// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Tests for the HTTP/3 module itself: certificate handling, binding, serving
//! and the failure modes the module promises.

use crate::common::{CleanupGuard, TestResult, connect_client, library_error, load_certificate, send, shutdown_server};

use bytes::Bytes;
use http::{Method, Request, StatusCode, Version};
use s3s::host::SingleDomain;
use s3s::service::{S3Service, S3ServiceBuilder};
use s3s_fs::FileSystem;
use s3s_fs::http3::{Http3Config, Http3Server};

use std::fs;
use std::path::Path;

/// An address the operating system picks, so tests never collide.
fn ephemeral() -> std::net::SocketAddr {
    "127.0.0.1:0".parse().expect("a literal address")
}

/// Builds the file system service used by the module tests.
fn test_service(dir: &Path) -> TestResult<S3Service> {
    let root = dir.join("data");
    fs::create_dir_all(&root)?;

    let filesystem = FileSystem::new(&root).map_err(|error| library_error(&error))?;

    let mut builder = S3ServiceBuilder::new(filesystem);
    builder.set_host(SingleDomain::new("localhost")?);

    Ok(builder.build())
}

#[tokio::test]
async fn generates_a_self_signed_certificate_and_exports_only_it() -> TestResult {
    let (dir, _cleanup) = CleanupGuard::new("module-generated")?;
    let cert_out = dir.join("cert.pem");

    let config = Http3Config::new(ephemeral(), None, None, Some(cert_out.clone()));
    let server = Http3Server::bind(config).await.map_err(|error| library_error(&error))?;

    let pem = fs::read_to_string(&cert_out)?;
    assert!(pem.contains("-----BEGIN CERTIFICATE-----"), "{pem}");
    assert!(!pem.contains("PRIVATE KEY"), "the private key must never be exported");

    // The exported certificate is the one the endpoint uses, so a client can
    // trust it.
    let certificate = load_certificate(&cert_out)?;
    // `assert_ne!` on an empty array, with the element type spelled out: the nightly
    // `clippy::assert_is_empty` lint rejects `assert!(!x.is_empty())`.
    assert_ne!(certificate.as_ref(), [] as [u8; 0]);

    let addr = server.local_addr();
    assert!(addr.ip().is_loopback(), "{addr}");
    assert_ne!(addr.port(), 0);

    Ok(())
}

#[tokio::test]
async fn serves_with_the_configured_pem_files() -> TestResult {
    let (dir, _cleanup) = CleanupGuard::new("module-configured")?;
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    let cert_out = dir.join("exported.pem");

    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])?;
    fs::write(&cert_path, certified.cert.pem())?;
    fs::write(&key_path, certified.signing_key.serialize_pem())?;

    let config = Http3Config::new(ephemeral(), Some(cert_path), Some(key_path), Some(cert_out.clone()));
    let server = Http3Server::bind(config).await.map_err(|error| library_error(&error))?;

    assert_eq!(fs::read_to_string(&cert_out)?, certified.cert.pem());

    let service = test_service(&dir)?;
    let addr = server.local_addr();

    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(service, async move {
        let _ = shutdown_rx.await;
    }));

    let certificate = load_certificate(&cert_out)?;
    let (endpoint, driver, mut client) = connect_client(addr, certificate).await?;

    let request = Request::builder()
        .method(Method::GET)
        .uri("http://localhost/missing-bucket")
        .body(())?;
    let (response, body, _) = send(&mut client, request, std::iter::empty::<Bytes>()).await?;

    assert_eq!(response.version(), Version::HTTP_3);
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(String::from_utf8(body)?.contains("<Code>NoSuchBucket</Code>"));

    shutdown_server(shutdown, client, task, endpoint, driver).await
}

#[tokio::test]
async fn rejects_a_certificate_without_a_key() -> TestResult {
    let (dir, _cleanup) = CleanupGuard::new("module-pair")?;
    let cert_path = dir.join("cert.pem");

    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])?;
    fs::write(&cert_path, certified.cert.pem())?;

    let server = Http3Server::bind(Http3Config::new(ephemeral(), Some(cert_path), None, None)).await;

    assert!(server.is_err(), "a certificate without a key must be rejected");

    Ok(())
}

#[tokio::test]
async fn rejects_an_empty_certificate_file() -> TestResult {
    let (dir, _cleanup) = CleanupGuard::new("module-empty")?;
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");

    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])?;
    fs::write(&cert_path, "")?;
    fs::write(&key_path, certified.signing_key.serialize_pem())?;

    let error = Http3Server::bind(Http3Config::new(ephemeral(), Some(cert_path), Some(key_path), None))
        .await
        .err()
        .ok_or_else(|| std::io::Error::other("an empty certificate file must be rejected"))?;

    assert!(format!("{error:?}").contains("empty"), "{error:?}");

    Ok(())
}

#[tokio::test]
async fn fails_when_the_udp_address_is_already_in_use() -> TestResult {
    let config = Http3Config::new(ephemeral(), None, None, None);
    let first = Http3Server::bind(config).await.map_err(|error| library_error(&error))?;

    let addr = first.local_addr();
    let second = Http3Server::bind(Http3Config::new(addr, None, None, None)).await;

    assert!(second.is_err(), "binding an address that is already in use must fail");

    Ok(())
}
