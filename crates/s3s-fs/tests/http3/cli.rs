// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! End-to-end tests for the `s3s-fs` binary: the command line surface, and the
//! TCP and HTTP/3 listeners sharing one address.

use crate::common::{CleanupGuard, TestResult, connect_client, load_certificate, send};

use bytes::Bytes;
use http::{Method, Request, StatusCode, Version};

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_s3s-fs");
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// A temporary directory the binary uses as its data root.
fn temp_dir(name: &str) -> TestResult<(PathBuf, CleanupGuard)> {
    CleanupGuard::new(name)
}

/// Runs a command and fails instead of hanging when it does not exit.
fn output_of(mut command: Command) -> TestResult<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let deadline = Instant::now() + COMMAND_TIMEOUT;

    loop {
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }

        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other("the command did not exit").into());
        }

        std::thread::sleep(Duration::from_millis(20));
    }
}

/// An address the operating system picks for the TCP listener; the QUIC
/// endpoint uses the same one.
fn free_port() -> TestResult<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// The `s3s-fs` binary, started on a free port.
struct Server {
    child: Child,
    addr: SocketAddr,
    certificate: Option<PathBuf>,
    stdout: PathBuf,
    stderr: PathBuf,
    _cleanup: CleanupGuard,
}

impl Server {
    async fn start(name: &str, http3: bool) -> TestResult<Self> {
        let (dir, cleanup) = temp_dir(name)?;
        let data = dir.join("data");
        fs::create_dir_all(&data)?;

        let stdout = dir.join("stdout.log");
        let stderr = dir.join("stderr.log");
        let certificate = dir.join("cert.pem");
        let port = free_port()?;

        let mut command = Command::new(BIN);
        command
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            // No credentials: the requests below are unsigned, and the
            // authenticated path is covered by the S3 API tests.
            .arg(port.to_string())
            .arg(&data)
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::from(fs::File::create(&stdout)?))
            .stderr(Stdio::from(fs::File::create(&stderr)?));

        if http3 {
            command.arg("--http3").arg("--cert-out").arg(&certificate);
        }

        let child = command.spawn()?;

        let mut server = Self {
            child,
            addr: format!("127.0.0.1:{port}").parse()?,
            certificate: http3.then_some(certificate),
            stdout,
            stderr,
            _cleanup: cleanup,
        };

        server.wait_until_ready().await?;

        Ok(server)
    }

    /// Waits until both listeners are up: the TCP listener accepts connections
    /// and the QUIC endpoint has exported the certificate it uses.
    async fn wait_until_ready(&mut self) -> TestResult {
        let deadline = Instant::now() + STARTUP_TIMEOUT;

        loop {
            if let Some(status) = self.child.try_wait()? {
                return Err(io::Error::other(format!("the server exited early: {status}\n{}", self.logs())).into());
            }

            let tcp_is_up = tokio::net::TcpStream::connect(self.addr).await.is_ok();
            let quic_is_up = self.certificate.as_ref().is_none_or(|path| path.exists());

            if tcp_is_up && quic_is_up {
                return Ok(());
            }

            if Instant::now() >= deadline {
                return Err(io::Error::other(format!("the server did not become ready\n{}", self.logs())).into());
            }

            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn certificate(&self) -> TestResult<&Path> {
        self.certificate
            .as_deref()
            .ok_or_else(|| io::Error::other("the server was not started with HTTP/3").into())
    }

    fn logs(&self) -> String {
        format!(
            "--- stdout ---\n{}--- stderr ---\n{}",
            read_to_string(&self.stdout),
            read_to_string(&self.stderr)
        )
    }

    /// Sends SIGINT and waits for a clean exit.
    #[cfg(unix)]
    async fn stop(mut self) -> TestResult<String> {
        let kill = Command::new("kill").arg("-INT").arg(self.child.id().to_string()).status()?;

        if !kill.success() {
            return Err(io::Error::other("failed to send SIGINT to the server").into());
        }

        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;

        loop {
            if let Some(status) = self.child.try_wait()? {
                let logs = self.logs();

                if !status.success() {
                    return Err(io::Error::other(format!("the server exited with {status}\n{logs}")).into());
                }

                return Ok(logs);
            }

            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                return Err(io::Error::other(format!("the server did not stop\n{}", self.logs())).into());
            }

            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_to_string(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// Sends one HTTP/1.1 request over TCP and returns the status code and the
/// whole response.
async fn http1_get(addr: SocketAddr, path: &str) -> TestResult<(u16, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    let request = format!("GET {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;

    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut response)).await??;

    let response = String::from_utf8(response)?;
    let status = response
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| io::Error::other("the response has no status code"))?
        .parse::<u16>()?;

    Ok((status, response))
}

/// Opens an HTTP/3 client that trusts the certificate the server exported.
async fn http3_client(
    addr: SocketAddr,
    certificate: &Path,
) -> TestResult<(quinn::Endpoint, tokio::task::JoinHandle<()>, crate::common::Client)> {
    let certificate = load_certificate(certificate)?;
    connect_client(addr, certificate).await
}

#[test]
fn help_lists_the_transport_and_tls_options() -> TestResult {
    let output = output_of({
        let mut command = Command::new(BIN);
        command.arg("--help");
        command
    })?;

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    let help = String::from_utf8(output.stdout)?;

    for option in ["--http3", "--cert <", "--key <", "--cert-out <"] {
        assert!(help.contains(option), "{option} is missing from:\n{help}");
    }

    // The address is configured once for both transports, so no transport
    // specific address or certificate option may exist.
    for option in [
        "--http3-port",
        "--http3-bind",
        "--http3-host",
        "--http3-addr",
        "--http3-cert",
        "--http3-key",
    ] {
        assert!(!help.contains(option), "{option} must not exist:\n{help}");
    }

    Ok(())
}

#[test]
fn rejects_tls_options_without_http3() -> TestResult {
    let (dir, _cleanup) = temp_dir("cli-tls-alone")?;

    for option in ["--cert", "--key", "--cert-out"] {
        let output = output_of({
            let mut command = Command::new(BIN);
            command.arg(option).arg("unused.pem").arg(&dir);
            command
        })?;

        assert!(!output.status.success(), "{option} without --http3 must be rejected");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("--http3"), "{option}: unexpected error:\n{stderr}");
    }

    Ok(())
}

#[test]
fn rejects_a_certificate_without_a_key() -> TestResult {
    let (dir, _cleanup) = temp_dir("cli-cert-alone")?;

    let output = output_of({
        let mut command = Command::new(BIN);
        command.args(["--http3", "--cert"]).arg("unused.pem").arg(&dir);
        command
    })?;

    assert!(!output.status.success(), "a certificate without a key must be rejected");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("certificate and private key"), "unexpected error:\n{stderr}");

    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serves_tcp_and_http3_on_one_address() -> TestResult {
    let server = Server::start("cli-same-address", true).await?;
    let addr = server.addr;

    // The TCP listener keeps serving HTTP/1.1 on the same address.
    let (status, response) = http1_get(addr, "/bucket").await?;
    assert_eq!(status, StatusCode::NOT_FOUND.as_u16(), "{response}");

    // The QUIC endpoint serves the same service over HTTP/3.
    let (endpoint, driver, mut client) = http3_client(addr, server.certificate()?).await?;

    let object = Bytes::from_static(b"hello over HTTP/3");

    let create_bucket = Request::builder()
        .method(Method::PUT)
        .uri("http://localhost/bucket")
        .body(())?;
    let (response, _, _) = send(&mut client, create_bucket, std::iter::empty::<Bytes>()).await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.version(), Version::HTTP_3);

    let create_object = Request::builder()
        .method(Method::PUT)
        .uri("http://localhost/bucket/key")
        .header("content-length", object.len())
        .body(())?;
    let (response, _, _) = send(&mut client, create_object, [object.clone()]).await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.version(), Version::HTTP_3);

    let read_object = Request::builder()
        .method(Method::GET)
        .uri("http://localhost/bucket/key")
        .body(())?;
    let (response, read, _) = send(&mut client, read_object, std::iter::empty::<Bytes>()).await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(read, object, "the payload must survive the round trip");
    assert_eq!(response.version(), Version::HTTP_3);

    let list_objects = Request::builder()
        .method(Method::GET)
        .uri("http://localhost/bucket?list-type=2")
        .body(())?;
    let (response, listing, _) = send(&mut client, list_objects, std::iter::empty::<Bytes>()).await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(String::from_utf8(listing)?.contains("<Key>key</Key>"));

    let delete_object = Request::builder()
        .method(Method::DELETE)
        .uri("http://localhost/bucket/key")
        .body(())?;
    let (response, _, _) = send(&mut client, delete_object, std::iter::empty::<Bytes>()).await?;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let read_deleted = Request::builder()
        .method(Method::GET)
        .uri("http://localhost/bucket/key")
        .body(())?;
    let (response, _, _) = send(&mut client, read_deleted, std::iter::empty::<Bytes>()).await?;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    endpoint.close(0u32.into(), b"test complete");
    driver.abort();
    let _ = driver.await;

    let logs = server.stop().await?;

    // Both transports used the address that the operating system returned for
    // the TCP listener, and the process stopped cleanly.
    let address = format!("127.0.0.1:{}", addr.port());
    assert!(logs.contains(&format!("http://{address}")), "the TCP address is missing:\n{logs}");
    assert!(logs.contains(&format!("https://{address}")), "the HTTP/3 address is missing:\n{logs}");
    assert!(logs.contains("server is stopped"), "the server did not stop cleanly:\n{logs}");

    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serves_tcp_without_http3() -> TestResult {
    let server = Server::start("cli-tcp-only", false).await?;

    let (status, response) = http1_get(server.addr, "/bucket").await?;
    assert_eq!(status, StatusCode::NOT_FOUND.as_u16(), "{response}");

    let logs = server.stop().await?;

    assert!(!logs.contains("HTTP/3"), "HTTP/3 must stay off without --http3:\n{logs}");
    assert!(logs.contains("server is stopped"), "the server did not stop cleanly:\n{logs}");

    Ok(())
}
