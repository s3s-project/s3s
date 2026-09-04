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
use tracing::info;

use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;

mod admin_route;
mod proxy_service;

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

    #[cfg(feature = "minio")]
    let proxy = {
        // MinIO-only extensions (e.g. ListenBucketNotification) have no
        // aws-sdk-s3 counterpart; forward them through the official MinIO SDK.
        let cred = sdk_conf
            .credentials_provider()
            .ok_or("missing credentials provider")?
            .provide_credentials()
            .await?;
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

    // Setup S3 service
    let service = {
        let mut b = S3ServiceBuilder::new(proxy);

        // Enable authentication
        if let Some(cred_provider) = sdk_conf.credentials_provider() {
            let cred = cred_provider.provide_credentials().await?;
            b.set_auth(SimpleAuth::from_single(cred.access_key_id(), cred.secret_access_key()));
        }

        // Apply configuration
        {
            let mut config = S3Config::default();
            config.enable_sig_v2 = opt.enable_sig_v2;
            b.set_config(Arc::new(StaticConfigProvider::new(Arc::new(config))));
        }

        // Forward MinIO admin API requests to the backend through a custom
        // route. Admin requests are SigV4-protected and pass the S3 signature
        // verification, so routing them through the S3 service reuses its
        // authentication: unsigned admin requests are denied before they
        // reach the backend.
        if let Some(client) = &minio_client {
            b.set_route(admin_route::MinioAdminRoute::new(reqwest::Url::parse(&opt.endpoint_url)?, client.clone()));
        }

        // Enable parsing virtual-hosted-style requests
        if let Some(domain) = opt.domain {
            b.set_host(SingleDomain::new(&domain)?);
        }

        b.build()
    };

    // Wrap in the proxy service, optionally forwarding MinIO health/metrics
    // endpoints to the backend at the HTTP layer, bypassing the S3 service so
    // its signature verification never rejects the Bearer token the prometheus
    // endpoints require.
    let service = if let Some(client) = minio_client {
        proxy_service::ProxyService::with_minio_health(service, reqwest::Url::parse(&opt.endpoint_url)?, client)
    } else {
        proxy_service::ProxyService::new(service)
    };

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
