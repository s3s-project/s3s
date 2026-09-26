// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Behaviour of the generic Tower service entry point.

use bytes::{Buf, Bytes};
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use quinn::rustls::pki_types::CertificateDer;

use std::error::Error;
use std::sync::Arc;

use crate::common::{
    Client, TestResult, client_endpoint, connect_client, receive_response, send, server_endpoint, shutdown_server,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serves_generic_tower_service_and_passes_remote_address() -> TestResult {
    let (endpoint, certificate) = server_endpoint()?;
    let server_address = endpoint.local_addr()?;

    let (remote_tx, remote_rx) = tokio::sync::watch::channel(None);
    let make_service = move |remote_addr| {
        let _ = remote_tx.send(Some(remote_addr));

        tower::service_fn(|_: Request<s3s_http3::RequestBody>| async {
            Ok::<_, std::convert::Infallible>(Response::new(s3s::Body::from(Bytes::from_static(b"generic response"))))
        })
    };

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(s3s_http3::serve_with(endpoint, make_service, async move {
        let _ = shutdown_rx.await;
    }));

    let client_endpoint = client_endpoint(certificate)?;
    let expected_remote = client_endpoint.local_addr()?;
    let connection = client_endpoint.connect(server_address, "localhost")?.await?;

    let (mut h3_connection, mut send_request) = h3::client::builder().build(h3_quinn::Connection::new(connection)).await?;

    let driver = tokio::spawn(async move {
        let _ = std::future::poll_fn(|cx| h3_connection.poll_close(cx)).await;
    });

    let (response, body, trailers) = send(
        &mut send_request,
        Request::builder().method(Method::GET).uri("http://localhost/").body(())?,
        std::iter::empty::<Bytes>(),
    )
    .await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body, b"generic response");
    assert!(trailers.is_none());
    assert_eq!(*remote_rx.borrow(), Some(expected_remote));

    shutdown_server(shutdown_tx, send_request, server_task, client_endpoint, driver).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generic_service_errors_reset_stream() -> TestResult {
    let (endpoint, certificate) = server_endpoint()?;
    let server_address = endpoint.local_addr()?;

    let make_service = |_remote_addr| {
        tower::service_fn(|_: Request<s3s_http3::RequestBody>| async { Err::<Response<s3s::Body>, _>("service failed") })
    };

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(s3s_http3::serve_with(endpoint, make_service, async move {
        let _ = shutdown_rx.await;
    }));

    let client_endpoint = client_endpoint(certificate)?;
    let connection = client_endpoint.connect(server_address, "localhost")?.await?;
    let (mut h3_connection, mut send_request) = h3::client::builder().build(h3_quinn::Connection::new(connection)).await?;

    let driver = tokio::spawn(async move {
        let _ = std::future::poll_fn(|cx| h3_connection.poll_close(cx)).await;
    });

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        send(
            &mut send_request,
            Request::builder().method(Method::GET).uri("http://localhost/").body(())?,
            std::iter::empty::<Bytes>(),
        ),
    )
    .await?;

    let Err(error) = result else {
        return Err(std::io::Error::other("service unexpectedly returned a response").into());
    };

    assert!(
        matches!(
            error.downcast_ref::<h3::error::StreamError>(),
            Some(h3::error::StreamError::RemoteTerminate { code, .. })
                if *code == h3::error::Code::H3_INTERNAL_ERROR
        ),
        "unexpected HTTP/3 error: {error:?}",
    );

    shutdown_server(shutdown_tx, send_request, server_task, client_endpoint, driver).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streams_generic_response_data_and_trailers() -> TestResult {
    let (endpoint, certificate) = server_endpoint()?;
    let server_address = endpoint.local_addr()?;

    let make_service = |_remote_addr| {
        tower::service_fn(|_: Request<s3s_http3::RequestBody>| async {
            let frames = [
                Ok::<_, std::convert::Infallible>(http_body::Frame::data(Bytes::from_static(b"streamed "))),
                Ok(http_body::Frame::data(Bytes::from_static(b"response"))),
                Ok(http_body::Frame::trailers({
                    let mut trailers = HeaderMap::new();
                    trailers.insert("x-stream-status", HeaderValue::from_static("complete"));
                    trailers
                })),
            ];

            let body = http_body_util::StreamBody::new(futures_util::stream::iter(frames));
            Ok::<_, std::convert::Infallible>(Response::new(body))
        })
    };

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(s3s_http3::serve_with(endpoint, make_service, async move {
        let _ = shutdown_rx.await;
    }));

    let client_endpoint = client_endpoint(certificate)?;
    let connection = client_endpoint.connect(server_address, "localhost")?.await?;
    let (mut h3_connection, mut send_request) = h3::client::builder().build(h3_quinn::Connection::new(connection)).await?;

    let driver = tokio::spawn(async move {
        let _ = std::future::poll_fn(|cx| h3_connection.poll_close(cx)).await;
    });

    let (response, body, trailers) = send(
        &mut send_request,
        Request::builder().method(Method::GET).uri("http://localhost/").body(())?,
        std::iter::empty::<Bytes>(),
    )
    .await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body, b"streamed response");
    assert_eq!(
        trailers.and_then(|headers| headers.get("x-stream-status").cloned()),
        Some(HeaderValue::from_static("complete")),
    );

    shutdown_server(shutdown_tx, send_request, server_task, client_endpoint, driver).await
}

struct GenericHarness {
    server_address: std::net::SocketAddr,
    certificate: CertificateDer<'static>,
    client_endpoint: quinn::Endpoint,
    driver: tokio::task::JoinHandle<()>,
    shutdown: tokio::sync::oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
    client: Client,
}

impl GenericHarness {
    async fn new<B>(
        service: tower::util::BoxCloneService<Request<s3s_http3::RequestBody>, Response<B>, Box<dyn Error + Send + Sync>>,
    ) -> TestResult<Self>
    where
        B: http_body::Body<Data = Bytes> + Send + 'static,
        B::Error: std::fmt::Debug + Send + 'static,
    {
        let (endpoint, certificate) = server_endpoint()?;
        let server_address = endpoint.local_addr()?;

        let make_service = move |_remote_addr: std::net::SocketAddr| service.clone();

        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(s3s_http3::serve_with(endpoint, make_service, async move {
            let _ = shutdown_rx.await;
        }));

        let (client_endpoint, driver, client) = connect_client(server_address, certificate.clone()).await?;

        Ok(Self {
            server_address,
            certificate,
            client_endpoint,
            driver,
            shutdown,
            server,
            client,
        })
    }

    async fn finish(self) -> TestResult<()> {
        shutdown_server(self.shutdown, self.client, self.server, self.client_endpoint, self.driver).await
    }

    /// Opens an additional connection to the same server.
    async fn connect_again(&self) -> TestResult<(quinn::Endpoint, tokio::task::JoinHandle<()>, Client)> {
        connect_client(self.server_address, self.certificate.clone()).await
    }
}

fn is_internal_error(error: &h3::error::StreamError) -> bool {
    matches!(error, h3::error::StreamError::RemoteTerminate { code, .. } if *code == h3::error::Code::H3_INTERNAL_ERROR)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generic_service_reads_the_request_body() -> TestResult {
    let service = tower::service_fn(|request: Request<s3s_http3::RequestBody>| async move {
        let collected = http_body_util::BodyExt::collect(request.into_body()).await?;
        Ok::<_, Box<dyn Error + Send + Sync>>(Response::new(s3s::Body::from(collected.to_bytes())))
    });
    let mut harness = GenericHarness::new(tower::util::BoxCloneService::new(service)).await?;

    let payload = Bytes::from_static(b"generic request body");
    let request = Request::builder()
        .method(Method::PUT)
        .uri("http://localhost/probe")
        .header("content-length", payload.len().to_string())
        .body(())?;

    let (response, body, trailers) = send(&mut harness.client, request, [payload.clone()]).await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body, payload.to_vec());
    assert!(trailers.is_none());

    harness.finish().await
}

// Hop-by-hop response headers are stripped on the generic path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generic_response_strips_hop_by_hop_headers() -> TestResult {
    let service = tower::service_fn(|_request: Request<s3s_http3::RequestBody>| async move {
        let mut response = Response::new(s3s::Body::from(Bytes::from_static(b"stripped")));
        let headers = response.headers_mut();
        headers.insert(http::header::CONNECTION, HeaderValue::from_static("keep-alive, x-hop"));
        headers.insert(http::HeaderName::from_static("x-hop"), HeaderValue::from_static("1"));
        headers.insert(http::header::TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
        headers.insert(http::header::TE, HeaderValue::from_static("trailers"));
        headers.insert(http::HeaderName::from_static("keep-alive"), HeaderValue::from_static("timeout=5"));
        headers.insert(http::header::UPGRADE, HeaderValue::from_static("h3"));
        headers.insert(http::HeaderName::from_static("x-kept"), HeaderValue::from_static("yes"));
        Ok::<_, Box<dyn Error + Send + Sync>>(response)
    });
    let mut harness = GenericHarness::new(tower::util::BoxCloneService::new(service)).await?;

    let request = Request::builder()
        .method(Method::GET)
        .uri("http://localhost/strip")
        .body(())?;
    let (response, body, _) = send(&mut harness.client, request, std::iter::empty::<Bytes>()).await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body, b"stripped");

    let headers = response.headers();
    for name in ["connection", "x-hop", "transfer-encoding", "te", "keep-alive", "upgrade"] {
        assert!(!headers.contains_key(name), "header {name} must be stripped: {headers:?}");
    }
    assert_eq!(headers.get("x-kept").map(http::HeaderValue::as_bytes), Some(&b"yes"[..]));

    harness.finish().await
}

// A response body error resets the stream with H3_INTERNAL_ERROR.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn response_body_error_resets_the_stream() -> TestResult {
    let service = tower::service_fn(|_request: Request<s3s_http3::RequestBody>| async move {
        let body = http_body_util::StreamBody::new(futures_util::stream::iter([
            Ok::<_, std::io::Error>(http_body::Frame::data(Bytes::from_static(b"partial"))),
            Err(std::io::Error::other("probe body failure")),
        ]));
        let body = http_body_util::BodyExt::boxed(body);
        Ok::<_, Box<dyn Error + Send + Sync>>(Response::new(body))
    });
    let mut harness = GenericHarness::new(tower::util::BoxCloneService::new(service)).await?;

    let request = Request::builder()
        .method(Method::GET)
        .uri("http://localhost/body-error")
        .body(())?;
    let mut stream = harness.client.send_request(request).await?;
    stream.finish().await?;

    // The server writes the head and one data frame before the body error resets the stream.
    // A reset discards data that was not acknowledged yet, so the client may see the data frame,
    // or only the reset; the terminal error is the invariant, and any data that does arrive must
    // be the frame the service sent.
    match stream.recv_response().await {
        Ok(response) => {
            assert_eq!(response.status(), StatusCode::OK);

            let mut received = Vec::new();
            let error = loop {
                match stream.recv_data().await {
                    Ok(Some(mut chunk)) => received.extend_from_slice(chunk.copy_to_bytes(chunk.remaining()).as_ref()),
                    Ok(None) => return Err(std::io::Error::other("the stream ended without an error").into()),
                    Err(error) => break error,
                }
            };

            assert!(received.is_empty() || received == b"partial", "unexpected body data: {received:?}");
            assert!(is_internal_error(&error), "unexpected HTTP/3 error: {error:?}");
        }
        Err(error) => {
            assert!(is_internal_error(&error), "unexpected HTTP/3 error: {error:?}");
        }
    }

    harness.finish().await
}

// Client trailers reach a generic service.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generic_service_receives_client_trailers() -> TestResult {
    let service = tower::service_fn(|request: Request<s3s_http3::RequestBody>| async move {
        let collected = http_body_util::BodyExt::collect(request.into_body()).await?;
        let value = collected
            .trailers()
            .and_then(|trailers| trailers.get("x-client-trailer"))
            .map_or_else(String::new, |value| String::from_utf8_lossy(value.as_bytes()).into_owned());
        Ok::<_, Box<dyn Error + Send + Sync>>(Response::new(s3s::Body::from(value)))
    });
    let mut harness = GenericHarness::new(tower::util::BoxCloneService::new(service)).await?;

    let request = Request::builder()
        .method(Method::PUT)
        .uri("http://localhost/trailers")
        .body(())?;
    let mut stream = harness.client.send_request(request).await?;
    stream.send_data(Bytes::from_static(b"body")).await?;
    let mut trailers = HeaderMap::new();
    trailers.insert(
        http::HeaderName::from_static("x-client-trailer"),
        HeaderValue::from_static("trailer-value"),
    );
    stream.send_trailers(trailers).await?;
    stream.finish().await?;

    let (response, body, _) = receive_response(stream).await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body, b"trailer-value");

    harness.finish().await
}

// An unread request body is drained when the service answers early.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unread_request_body_is_drained_when_the_service_answers_early() -> TestResult {
    let service = tower::service_fn(|_request: Request<s3s_http3::RequestBody>| async move {
        Ok::<_, Box<dyn Error + Send + Sync>>(Response::new(s3s::Body::from(Bytes::from_static(b"early"))))
    });
    let mut harness = GenericHarness::new(tower::util::BoxCloneService::new(service)).await?;

    let chunk = Bytes::from(vec![b'x'; 64 * 1024]);
    let request = Request::builder()
        .method(Method::PUT)
        .uri("http://localhost/early")
        .header("content-length", (chunk.len() * 16).to_string())
        .body(())?;

    let (response, body, _) = send(&mut harness.client, request, std::iter::repeat_n(chunk, 16)).await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body, b"early");

    harness.finish().await
}

// A client reset in the middle of an upload is observed by the service.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_reset_mid_upload_is_observed_by_the_service() -> TestResult {
    let observed: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    let service = {
        let observed = Arc::clone(&observed);
        tower::service_fn(move |request: Request<s3s_http3::RequestBody>| {
            let observed = Arc::clone(&observed);
            async move {
                *observed.lock().expect("observed lock") = Some(String::from("started"));
                match http_body_util::BodyExt::collect(request.into_body()).await {
                    Ok(collected) => {
                        let bytes = collected.to_bytes();
                        *observed.lock().expect("observed lock") = Some(format!("ok:{}", bytes.len()));
                        Ok::<_, Box<dyn Error + Send + Sync>>(Response::new(s3s::Body::from(bytes)))
                    }
                    Err(error) => {
                        let has_source = error.source().is_some();
                        *observed.lock().expect("observed lock") = Some(format!("error:display={error};source={has_source}"));
                        Ok(Response::new(s3s::Body::from(Bytes::from_static(b"upload aborted"))))
                    }
                }
            }
        })
    };
    let mut harness = GenericHarness::new(tower::util::BoxCloneService::new(service)).await?;

    let request = Request::builder()
        .method(Method::PUT)
        .uri("http://localhost/reset")
        .header("content-length", (64 * 1024).to_string())
        .body(())?;
    let mut stream = harness.client.send_request(request).await?;
    stream.send_data(Bytes::from(vec![b'x'; 32 * 1024])).await?;

    // The reset must reach a service that already started reading the body, otherwise the request
    // task is cancelled before the service runs. Wait until the service records any state.
    let started = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if observed.lock().expect("observed lock").is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(started.is_ok(), "the service never started reading the request body");

    stream.stop_stream(h3::error::Code::H3_REQUEST_CANCELLED);
    // The h3 client turns a stream that ends without response headers into a connection error,
    // so this test asserts the server-side observation only (the request task must not wedge).
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), stream.recv_response()).await;

    let recorded = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let current = observed
                .lock()
                .expect("observed lock")
                .clone()
                .filter(|recorded| recorded.starts_with("error:"));
            if let Some(recorded) = current {
                break recorded;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| std::io::Error::other("the service did not observe the aborted upload"))?;

    assert!(recorded.contains("Remote reset"), "unexpected body error: {recorded}");
    assert!(recorded.contains("source=true"), "unexpected body error: {recorded}");

    // The server must still accept new connections after an aborted upload.
    let (extra_endpoint, extra_driver, mut extra_client) = harness.connect_again().await?;
    let request = Request::builder()
        .method(Method::GET)
        .uri("http://localhost/after-reset")
        .body(())?;
    let (response, _, _) = send(&mut extra_client, request, std::iter::empty::<Bytes>()).await?;
    assert_eq!(response.status(), StatusCode::OK);
    drop(extra_client);
    extra_endpoint.close(0u32.into(), b"test complete");
    extra_driver.abort();
    let _ = extra_driver.await;

    harness.finish().await
}

// A panicking service does not break the server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_panic_does_not_break_the_server() -> TestResult {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let service = {
        let calls = Arc::clone(&calls);
        tower::service_fn(move |_request: Request<s3s_http3::RequestBody>| {
            let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                assert!(call != 0, "probe panic in the service");
                Ok::<_, Box<dyn Error + Send + Sync>>(Response::new(s3s::Body::from(Bytes::from_static(b"after panic"))))
            }
        })
    };
    let mut harness = GenericHarness::new(tower::util::BoxCloneService::new(service)).await?;

    let request = Request::builder()
        .method(Method::GET)
        .uri("http://localhost/panic")
        .body(())?;
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        send(&mut harness.client, request, std::iter::empty::<Bytes>()),
    )
    .await;

    // The panicking request leaves its stream without response headers, which the h3 client
    // escalates to a connection error; the server itself must still accept new connections.
    let (extra_endpoint, extra_driver, mut extra_client) = harness.connect_again().await?;

    let request = Request::builder()
        .method(Method::GET)
        .uri("http://localhost/after-panic")
        .body(())?;
    let (response, body, _) = send(&mut extra_client, request, std::iter::empty::<Bytes>()).await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body, b"after panic");

    drop(extra_client);
    extra_endpoint.close(0u32.into(), b"test complete");
    extra_driver.abort();
    let _ = extra_driver.await;

    harness.finish().await
}

// The public request body contract is observable (is_end_stream, poll past the end).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_body_contract_is_observable() -> TestResult {
    let service = tower::service_fn(|request: Request<s3s_http3::RequestBody>| async move {
        let mut body = std::pin::pin!(request.into_body());
        let before = http_body::Body::is_end_stream(&*body);

        loop {
            match std::future::poll_fn(|cx| http_body::Body::poll_frame(body.as_mut(), cx)).await {
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    return Ok::<_, Box<dyn Error + Send + Sync>>(Response::new(s3s::Body::from(format!("error={error}"))));
                }
                None => break,
            }
        }

        let after = http_body::Body::is_end_stream(&*body);
        let extra = std::future::poll_fn(|cx| http_body::Body::poll_frame(body.as_mut(), cx)).await;
        let text = format!("before={before};after={after};extra_none={}", extra.is_none());
        Ok(Response::new(s3s::Body::from(text)))
    });
    let mut harness = GenericHarness::new(tower::util::BoxCloneService::new(service)).await?;

    let payload = Bytes::from_static(b"contract");
    let request = Request::builder()
        .method(Method::PUT)
        .uri("http://localhost/contract")
        .header("content-length", payload.len().to_string())
        .body(())?;
    let (response, body, _) = send(&mut harness.client, request, [payload]).await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(String::from_utf8(body)?, "before=false;after=true;extra_none=true");

    harness.finish().await
}

// A body error exposes Display and source to the service.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn body_error_exposes_display_and_source() -> TestResult {
    let service = tower::service_fn(|request: Request<s3s_http3::RequestBody>| async move {
        match http_body_util::BodyExt::collect(request.into_body()).await {
            Ok(_) => {
                Ok::<_, Box<dyn Error + Send + Sync>>(Response::new(s3s::Body::from(Bytes::from_static(b"unexpected success"))))
            }
            Err(error) => {
                let has_source = error.source().is_some();
                Ok(Response::new(s3s::Body::from(format!("display={error};source={has_source}"))))
            }
        }
    });
    let mut harness = GenericHarness::new(tower::util::BoxCloneService::new(service)).await?;

    let request = Request::builder()
        .method(Method::PUT)
        .uri("http://localhost/short")
        .header("content-length", "64")
        .body(())?;
    let (response, body, _) = send(&mut harness.client, request, [Bytes::from_static(b"short")]).await?;

    assert_eq!(response.status(), StatusCode::OK);
    let text = String::from_utf8(body)?;
    assert!(text.contains("length mismatch"), "unexpected body: {text}");
    assert!(text.ends_with("source=true"), "unexpected body: {text}");

    harness.finish().await
}
