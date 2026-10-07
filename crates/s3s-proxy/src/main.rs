// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use s3s::auth::SimpleAuth;
use s3s::config::{S3Config, StaticConfigProvider};
use s3s::host::SingleDomain;
use s3s::service::S3ServiceBuilder;
use tokio::net::TcpListener;

use std::error::Error;
use std::io::IsTerminal;
use std::sync::Arc;

use aws_credential_types::provider::ProvideCredentials;

use clap::Parser;
use s3s::route::S3Route;
use tracing::info;
use tracing::warn;

use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;

mod admin_route;
mod auth_passthrough;
mod hop_by_hop;
mod post_object_passthrough;
mod proxy_service;
mod route_chain;
mod sts_route;

// A CLI options struct: every passthrough rule is a flag by design, so the
// switches outnumber what `struct_excessive_bools` accepts.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Parser)]
struct Opt {
    #[clap(long, default_value = "localhost")]
    host: String,

    #[clap(long, default_value = "8014")]
    port: u16,

    #[clap(long)]
    domain: Option<String>,

    #[clap(long)]
    endpoint_url: String,

    /// Enable Signature Version 2 (`SigV2`) support.
    ///
    /// `SigV2` is disabled by default for security. Use this flag to explicitly
    /// opt-in when testing clients that require `SigV2`.
    #[clap(long)]
    enable_sig_v2: bool,

    /// Forward `MinIO` admin, health, and metrics endpoints (`/minio/admin/*`,
    /// `/minio/health/*`, and `/minio/v2/metrics/*`) to the backend.
    ///
    /// Disabled by default; only meaningful when the backend is a `MinIO`
    /// server.
    #[clap(long)]
    enable_minio_route: bool,

    /// Forward requests whose credentials this proxy does not know instead of
    /// rejecting them.
    ///
    /// A user created through the backend's admin API, or the temporary
    /// credentials an STS `AssumeRole` call hands out, cannot be verified
    /// locally. When this is enabled such a request is forwarded verbatim and the
    /// backend decides: the proxy no longer authenticates it. Disabled by
    /// default.
    #[clap(long)]
    enable_auth_passthrough: bool,

    /// Forward POST Object form uploads to the backend without reading them.
    ///
    /// A form upload authenticates with a policy inside the form, and the core
    /// parses that form before the S3 service is called, so a proxy cannot
    /// re-frame it at the service layer. With this enabled the request is
    /// forwarded as it arrived, at the HTTP layer, and the backend decides.
    ///
    /// Known limitation: without this flag a form upload becomes a `PutObject`
    /// whose body length the proxy cannot know, so a backend that requires a
    /// declared length rejects the forwarded request (`MinIO` answers 411,
    /// `Amazon S3` 501) and the object is never created. A proxy deployment that
    /// must accept form uploads therefore runs with this flag.
    #[clap(long)]
    enable_post_object_passthrough: bool,

    /// Disable the STS protocol part of the authentication passthrough.
    ///
    /// `POST /` with a form body asks the backend to issue temporary
    /// credentials. It is forwarded when the request is signed with a credential
    /// this proxy knows, which is what an STS client uses, and can be turned off
    /// on its own.
    #[clap(long, requires = "enable_auth_passthrough")]
    no_auth_passthrough_sts: bool,

    /// Disable the object-level part of the authentication passthrough.
    ///
    /// Requests carrying an unknown credential are then rejected locally again.
    #[clap(long, requires = "enable_auth_passthrough")]
    no_auth_passthrough_object: bool,

    /// Allow an extra query parameter on object-level requests forwarded by the
    /// authentication passthrough.
    ///
    /// The default set covers object reads, writes, deletes, listings and
    /// multipart uploads; a query parameter outside it keeps the request on the
    /// typed path. Repeat the flag to allow more than one.
    #[clap(
        long = "auth-passthrough-allow-query",
        value_name = "KEY",
        requires = "enable_auth_passthrough"
    )]
    auth_passthrough_allow_query: Vec<String>,
}

fn setup_tracing() {
    use tracing_subscriber::EnvFilter;

    let env_filter = EnvFilter::from_default_env();
    let enable_color = std::io::stdout().is_terminal();

    tracing_subscriber::fmt()
        .pretty()
        .with_env_filter(env_filter)
        .with_ansi(enable_color)
        .init();
}

/// Upper bound, in bytes, on a request body the authentication passthrough
/// buffers before forwarding it.
///
/// A forwarded request keeps its bytes (the signature covers the payload), so
/// the body is read into memory; the bound is the configured maximum object size,
/// with a fixed fallback when the configuration disables that limit. It bounds
/// one request, not the proxy: the default is 5 GiB, so a client can make the
/// proxy hold that much per forwarded request.
fn passthrough_max_body_size(config: &S3Config) -> u64 {
    /// The AWS single-PUT object size limit, used when the configuration does
    /// not set one.
    const FALLBACK_MAX_BODY_SIZE: u64 = 5 * 1024 * 1024 * 1024;

    config.put_object_max_size.unwrap_or(FALLBACK_MAX_BODY_SIZE)
}

/// Builds the S3 service: the authenticated typed path plus its custom routes.
///
/// Custom routes forward requests the typed path cannot serve: the `MinIO` admin
/// API (signed with a credential the proxy knows, so it passes the service's own
/// verification) and the STS protocol shape.
fn build_service(
    opt: &Opt,
    credentials: Option<&aws_credential_types::Credentials>,
    proxy: s3s_aws::Proxy,
    minio_client: Option<&reqwest::Client>,
    sts_route: Option<sts_route::StsRoute>,
    config: Arc<S3Config>,
) -> Result<s3s::service::S3Service, Box<dyn Error + Send + Sync>> {
    let mut b = S3ServiceBuilder::new(proxy);

    // Enable authentication
    if let Some(cred) = credentials {
        b.set_auth(SimpleAuth::from_single(cred.access_key_id(), cred.secret_access_key()));
    }

    // Apply the configuration the caller built, which is also what bounds a
    // forwarded request body.
    b.set_config(Arc::new(StaticConfigProvider::new(config)));

    // Forward MinIO admin API requests to the backend through a custom route.
    // Admin requests are SigV4-protected and pass the S3 signature verification,
    // so routing them through the S3 service reuses its authentication: unsigned
    // admin requests are denied before they reach the backend.
    let mut routes: Vec<Box<dyn S3Route>> = Vec::new();
    if let Some(client) = minio_client {
        routes.push(Box::new(admin_route::MinioAdminRoute::new(
            reqwest::Url::parse(&opt.endpoint_url)?,
            client.clone(),
        )));
    }
    if let Some(route) = sts_route {
        routes.push(Box::new(route));
    }
    if !routes.is_empty() {
        b.set_route(route_chain::RouteChain::new(routes));
    }

    // Enable parsing virtual-hosted-style requests
    if let Some(domain) = &opt.domain {
        b.set_host(SingleDomain::new(domain)?);
    }

    Ok(b.build())
}

/// Sets up the authentication passthrough.
///
/// The object-level rule is enforced in the proxy service: it must run before
/// the S3 service, which authenticates every request it sees. The STS protocol
/// rule is a route, because that request is signed with a credential the proxy
/// knows and therefore passes the S3 service's own verification.
#[allow(clippy::type_complexity)]
fn setup_auth_passthrough(
    opt: &Opt,
    credentials: Option<&aws_credential_types::Credentials>,
    config: &S3Config,
) -> Result<(Option<auth_passthrough::AuthPassthrough>, Option<sts_route::StsRoute>), Box<dyn Error + Send + Sync>> {
    if !opt.enable_auth_passthrough {
        return Ok((None, None));
    }
    let Some(cred) = credentials else {
        return Err("--enable-auth-passthrough requires credentials: without them the proxy cannot tell a known access key from an unknown one".into());
    };

    let sts_enabled = !opt.no_auth_passthrough_sts;
    let object_enabled = !opt.no_auth_passthrough_object;
    warn!(
        "authentication passthrough enabled: a request carrying a credential this proxy does not know is no longer authenticated locally, the backend decides"
    );
    let max_body_size = passthrough_max_body_size(config);
    info!(
        sts = sts_enabled,
        object = object_enabled,
        extra_query_keys = ?opt.auth_passthrough_allow_query,
        max_body_size,
        "authentication passthrough rules"
    );

    let endpoint_url = reqwest::Url::parse(&opt.endpoint_url)?;
    // No overall timeout: a forwarded object transfer may take as long as the
    // client and the backend need.
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()?;

    let sts = sts_enabled.then(|| sts_route::StsRoute::new(endpoint_url.clone(), client.clone(), max_body_size));
    let object = object_enabled.then(|| {
        auth_passthrough::AuthPassthrough::new(
            endpoint_url,
            client,
            cred.access_key_id().to_owned(),
            opt.auth_passthrough_allow_query.clone(),
            max_body_size,
        )
    });
    Ok((object, sts))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    setup_tracing();
    let opt = Opt::parse();

    // Setup S3 provider
    let sdk_conf = aws_config::from_env().endpoint_url(&opt.endpoint_url).load().await;
    let client = {
        let builder = aws_sdk_s3::config::Builder::from(&sdk_conf).force_path_style(true);
        #[cfg(feature = "minio")]
        let builder = builder.interceptor(s3s_aws::minio_compat::MinioBoolCompatInterceptor::new());
        aws_sdk_s3::Client::from_conf(builder.build())
    };

    // One credential serves all three roles: the MinIO client, the proxy's own
    // authentication of clients, and the access key it knows when the
    // authentication passthrough asks whether a credential is known.
    let credentials = match sdk_conf.credentials_provider() {
        Some(provider) => Some(provider.provide_credentials().await?),
        None => None,
    };

    #[cfg(feature = "minio")]
    let proxy = {
        // MinIO-only extensions (e.g. ListenBucketNotification) have no
        // aws-sdk-s3 counterpart; forward them through the official MinIO SDK.
        let cred = credentials.as_ref().ok_or("missing credentials provider")?;
        let provider =
            minio::s3::creds::StaticProvider::new(cred.access_key_id(), cred.secret_access_key(), cred.session_token());
        let minio_client = minio::s3::MinioClient::new(opt.endpoint_url.parse()?, Some(provider), None, None)?;
        s3s_aws::Proxy::builder(client).minio_client(minio_client).build()
    };
    #[cfg(not(feature = "minio"))]
    let proxy = s3s_aws::Proxy::builder(client).build();

    // HTTP client shared by the MinIO passthrough layers. The admin route
    // (inside the S3 service) and the health/metrics passthrough (in the
    // proxy service) both forward to the backend through it.
    let minio_client = if opt.enable_minio_route {
        Some(
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .connect_timeout(std::time::Duration::from_secs(5))
                .build()?,
        )
    } else {
        None
    };

    // One configuration serves both the S3 service and the passthrough bound, so
    // the two cannot drift apart.
    let mut config = S3Config::default();
    config.enable_sig_v2 = opt.enable_sig_v2;
    // The `minio` build is the one that serves a MinIO backend, and MinIO clients sign some
    // credential scopes with an empty region: `mc admin`, the admin APIs of the language SDKs,
    // and the credentials `AssumeRole` returns. The service they talk to accepts them, so this
    // build accepts them too; every other region rule stays in force.
    config.sig_v4_allow_empty_region = cfg!(feature = "minio");
    let config = Arc::new(config);

    let (auth_passthrough, sts_route) = setup_auth_passthrough(&opt, credentials.as_ref(), &config)?;

    let service = build_service(&opt, credentials.as_ref(), proxy, minio_client.as_ref(), sts_route, config)?;

    // Wrap in the proxy service, optionally forwarding MinIO health/metrics
    // endpoints to the backend at the HTTP layer, bypassing the S3 service so
    // its signature verification never rejects the Bearer token the prometheus
    // endpoints require.
    let mut service = if let Some(client) = minio_client {
        proxy_service::ProxyService::with_minio_health(service, reqwest::Url::parse(&opt.endpoint_url)?, client)
    } else {
        proxy_service::ProxyService::new(service)
    };
    if let Some(passthrough) = auth_passthrough {
        service = service.with_auth_passthrough(passthrough);
    }
    if opt.enable_post_object_passthrough {
        warn!("POST Object passthrough enabled: a form upload is forwarded unread and the backend decides");
        let endpoint_url = reqwest::Url::parse(&opt.endpoint_url)?;
        // No overall timeout: a forwarded upload may take as long as the client
        // and the backend need.
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()?;
        service = service.with_post_object_passthrough(post_object_passthrough::PostObjectPassthrough::new(endpoint_url, client));
    }

    // Run server
    let listener = TcpListener::bind((opt.host.as_str(), opt.port)).await?;

    let http_server = ConnBuilder::new(TokioExecutor::new());
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();

    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());

    info!("server is running at http://{}:{}/", opt.host, opt.port);
    info!("server is forwarding requests to {}", opt.endpoint_url);

    loop {
        let (socket, _) = tokio::select! {
            res =  listener.accept() => {
                match res {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!("error accepting connection: {err}");
                        continue;
                    }
                }
            }
            _ = ctrl_c.as_mut() => {
                break;
            }
        };

        let conn = http_server.serve_connection(TokioIo::new(socket), service.clone());
        let conn = graceful.watch(conn.into_owned());
        tokio::spawn(async move {
            let _ = conn.await;
        });
    }

    tokio::select! {
        () = graceful.shutdown() => {
             tracing::debug!("Gracefully shutdown!");
        },
        () = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
             tracing::debug!("Waited 10 seconds for graceful shutdown, aborting...");
        }
    }

    info!("server is stopped");

    Ok(())
}
