// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use s3s_fs::FileSystem;
use s3s_fs::Result;

use s3s::auth::SimpleAuth;
use s3s::host::MultiDomain;
use s3s::service::S3ServiceBuilder;

use std::io::IsTerminal;
use std::ops::Not;
use std::path::PathBuf;

use tokio::net::TcpListener;

use clap::{CommandFactory, Parser};
use tracing::info;

use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;

#[derive(Debug, Parser)]
#[command(version)]
struct Opt {
    /// Host name to listen on.
    #[arg(long, default_value = "localhost")]
    host: String,

    /// Port number to listen on.
    #[arg(long, default_value = "8014")] // The original design was finished on 2020-08-14.
    port: u16,

    /// Access key used for authentication.
    #[arg(long)]
    access_key: Option<String>,

    /// Secret key used for authentication.
    #[arg(long)]
    secret_key: Option<String>,

    /// Domain names used for virtual-hosted-style requests.
    #[arg(long)]
    domain: Vec<String>,

    /// Serve the S3 API over HTTP/3 (QUIC) in addition to TCP.
    #[arg(long)]
    http3: bool,

    /// PEM file with the certificate chain used by TLS-based transports.
    #[arg(long, requires = "http3")]
    cert: Option<PathBuf>,

    /// Private key in PEM used by TLS-based transports.
    #[arg(long, requires = "http3")]
    key: Option<PathBuf>,

    /// Write the certificate that is actually used to this PEM file.
    #[arg(long, requires = "http3")]
    cert_out: Option<PathBuf>,

    /// Root directory of stored data.
    root: PathBuf,
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

fn check_cli_args(opt: &Opt) {
    use clap::error::ErrorKind;

    let mut cmd = Opt::command();

    // TODO: how to specify the requirements with clap derive API?
    if let (Some(_), None) | (None, Some(_)) = (&opt.access_key, &opt.secret_key) {
        let msg = "access key and secret key must be specified together";
        cmd.error(ErrorKind::MissingRequiredArgument, msg).exit();
    }

    if opt.http3 && cfg!(not(all(feature = "binary", feature = "http3"))) {
        let msg = "this binary was built without the http3 feature";
        cmd.error(ErrorKind::InvalidValue, msg).exit();
    }

    if let (Some(_), None) | (None, Some(_)) = (&opt.cert, &opt.key) {
        let msg = "certificate and private key must be specified together";
        cmd.error(ErrorKind::MissingRequiredArgument, msg).exit();
    }

    for s in &opt.domain {
        if s.contains('/') {
            let msg = format!("expected domain name, found URL-like string: {s:?}");
            cmd.error(ErrorKind::InvalidValue, msg).exit();
        }
    }
}

fn main() -> Result {
    let opt = Opt::parse();
    check_cli_args(&opt);

    setup_tracing();

    run(opt)
}

#[tokio::main]
async fn run(opt: Opt) -> Result {
    // Setup S3 provider
    let fs = FileSystem::new(opt.root)?;

    // Setup S3 service
    let service = {
        let mut b = S3ServiceBuilder::new(fs);

        // Enable authentication
        if let (Some(ak), Some(sk)) = (opt.access_key, opt.secret_key) {
            b.set_auth(SimpleAuth::from_single(ak, sk));
            info!("authentication is enabled");
        }

        // Enable parsing virtual-hosted-style requests
        if opt.domain.is_empty().not() {
            b.set_host(MultiDomain::new(&opt.domain)?);
            info!("virtual-hosted-style requests are enabled");
        }

        b.build()
    };

    // Run server
    let listener = TcpListener::bind((opt.host.as_str(), opt.port)).await?;
    let local_addr = listener.local_addr()?;

    let http_server = ConnBuilder::new(TokioExecutor::new());
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();

    // The QUIC endpoint shares the address and the shutdown of the TCP listener.
    #[cfg(all(feature = "binary", feature = "http3"))]
    let (http3_stop, http3_shutdown) = tokio::sync::oneshot::channel::<()>();

    #[cfg(all(feature = "binary", feature = "http3"))]
    let http3_task = if opt.http3 {
        let config = s3s_fs::http3::Http3Config::new(local_addr, opt.cert, opt.key, opt.cert_out);
        let server = s3s_fs::http3::Http3Server::bind(config).await?;
        Some(tokio::spawn(server.serve(service.clone(), async move {
            let _ = http3_shutdown.await;
        })))
    } else {
        None
    };

    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());

    info!("server is running at http://{local_addr}");

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

    #[cfg(all(feature = "binary", feature = "http3"))]
    let _ = http3_stop.send(());

    tokio::select! {
        () = graceful.shutdown() => {
             tracing::debug!("Gracefully shutdown!");
        },
        () = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
             tracing::debug!("Waited 10 seconds for graceful shutdown, aborting...");
        }
    }

    // Both transports are stopped: the QUIC endpoint drained while the TCP
    // connections were closing.
    //
    // A failed task must not be reported as a clean shutdown: the shutdown
    // signal makes the task end with `Ok`, so anything else is a real failure.
    #[cfg(all(feature = "binary", feature = "http3"))]
    if let Some(task) = http3_task {
        join_http3_task(task).await?;
    }

    info!("server is stopped");
    Ok(())
}

/// Waits for the HTTP/3 server task.
///
/// The task is stopped by the shutdown signal and ends normally, so a join
/// error means it panicked or was cancelled instead of stopping gracefully.
#[cfg(all(feature = "binary", feature = "http3"))]
async fn join_http3_task(task: tokio::task::JoinHandle<()>) -> Result {
    task.await.map_err(|err| {
        let what = if err.is_panic() { "panicked" } else { "was cancelled" };
        s3s_fs::Error::from_string(format!("the HTTP/3 server task {what}: {err}"))
    })
}

#[cfg(all(test, feature = "binary", feature = "http3"))]
mod tests {
    use super::join_http3_task;
    use std::future::pending;

    #[tokio::test]
    async fn http3_task_stopped_gracefully_is_not_an_error() {
        let (stop, shutdown) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _ = shutdown.await;
        });

        stop.send(()).expect("the shutdown receiver must be alive");
        assert!(join_http3_task(task).await.is_ok());
    }

    #[tokio::test]
    async fn http3_task_panic_is_reported() {
        let task = tokio::spawn(async {
            panic!("the server task panicked");
        });

        let err = join_http3_task(task).await.expect_err("a panic must be reported");
        assert!(format!("{err:?}").contains("panicked"), "{err:?}");
    }

    #[tokio::test]
    async fn http3_task_cancellation_is_reported() {
        let task = tokio::spawn(pending::<()>());

        task.abort();
        let err = join_http3_task(task).await.expect_err("a cancellation must be reported");
        assert!(format!("{err:?}").contains("cancelled"), "{err:?}");
    }
}
