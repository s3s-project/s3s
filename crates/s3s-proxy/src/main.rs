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
    let client = aws_sdk_s3::Client::from_conf(aws_sdk_s3::config::Builder::from(&sdk_conf).force_path_style(true).build());

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

        // Enable parsing virtual-hosted-style requests
        if let Some(domain) = opt.domain {
            b.set_host(SingleDomain::new(&domain)?);
        }

        b.build()
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
