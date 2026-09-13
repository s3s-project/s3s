// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use bytes::Bytes;
use h2::client::SendRequest;
use h2::{RecvStream, client};
use http::{Method, Request, Response, StatusCode, Uri, Version};
use hyper::body::Incoming;
use hyper::service::{Service, service_fn};
use hyper_util::rt::{TokioExecutor, TokioIo};
use s3s::auth::SimpleAuth;
use s3s::config::{S3Config, StaticConfigProvider};
use s3s::dto::{GetObjectInput, GetObjectOutput};
use s3s::service::S3ServiceBuilder;
use s3s::{S3, S3Request, S3Response, S3Result};
use std::future::poll_fn;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

const ACCESS_KEY: &str = "test-access";
const SECRET_KEY: &str = "test-secret";
const AMZ_DATE: &str = "20260828T000000Z";
const REGION: &str = "us-east-1";
const SERVICE: &str = "s3";
const EMPTY_SHA256: &str = s3s_sigv4::EMPTY_STRING_SHA256_HASH;
const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

#[derive(Clone, Default)]
struct TestS3 {
    get_object_calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl S3 for TestS3 {
    async fn get_object(&self, _req: S3Request<GetObjectInput>) -> S3Result<S3Response<GetObjectOutput>> {
        self.get_object_calls.fetch_add(1, Ordering::SeqCst);
        Ok(S3Response::new(GetObjectOutput::default()))
    }
}

#[derive(Debug)]
struct RequestObservation {
    version: Version,
    has_content_length: bool,
    exact_body_length: Option<u64>,
    body_is_end_stream: bool,
}

struct Http2Harness {
    authority: String,
    client: SendRequest<Bytes>,
    observations: mpsc::UnboundedReceiver<RequestObservation>,
    dispatch: mpsc::UnboundedSender<()>,
    get_object_calls: Arc<AtomicUsize>,
    client_driver: JoinHandle<Result<(), String>>,
    server_connection: JoinHandle<Result<(), String>>,
}

impl Http2Harness {
    async fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("test listener should bind");
        let address = listener.local_addr().expect("test listener should have an address");
        let authority = address.to_string();

        let test_s3 = TestS3::default();
        let get_object_calls = Arc::clone(&test_s3.get_object_calls);
        let mut builder = S3ServiceBuilder::new(test_s3);
        builder.set_auth(SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY));
        let mut config = S3Config::default();
        config.presigned_url_max_skew_time_secs = u32::MAX;
        config.expected_region = Some(REGION.parse().expect("valid test region"));
        builder.set_config(Arc::new(StaticConfigProvider::new(Arc::new(config))));
        let s3_service = builder.build();

        let (observation_tx, observations) = mpsc::unbounded_channel();
        let (dispatch, dispatch_rx) = mpsc::unbounded_channel();
        let dispatch_rx = Arc::new(Mutex::new(dispatch_rx));

        let server_connection = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("test server should accept one connection");
            let service = service_fn(move |request: Request<Incoming>| {
                let observation_tx = observation_tx.clone();
                let dispatch_rx = Arc::clone(&dispatch_rx);
                let s3_service = s3_service.clone();
                async move {
                    observation_tx
                        .send(RequestObservation {
                            version: request.version(),
                            has_content_length: request.headers().contains_key(http::header::CONTENT_LENGTH),
                            exact_body_length: http_body::Body::size_hint(request.body()).exact(),
                            body_is_end_stream: http_body::Body::is_end_stream(request.body()),
                        })
                        .expect("test should still be receiving request observations");

                    dispatch_rx
                        .lock()
                        .await
                        .recv()
                        .await
                        .expect("test should authorize request dispatch");

                    Service::call(&s3_service, request).await
                }
            });

            hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(stream), service)
                .await
                .map_err(|err| format!("HTTP/2 test server connection error: {err}"))
        });

        let stream = TcpStream::connect(address).await.expect("test client should connect");
        let (client, connection) = client::handshake(stream).await.expect("HTTP/2 handshake should succeed");
        let client_driver = tokio::spawn(async move {
            connection
                .await
                .map_err(|err| format!("HTTP/2 test client connection error: {err}"))
        });

        Self {
            authority,
            client,
            observations,
            dispatch,
            get_object_calls,
            client_driver,
            server_connection,
        }
    }

    async fn get_object(&mut self, payload_sha256: &'static str, body: Bytes) -> Result<Response<RecvStream>, h2::Error> {
        poll_fn(|cx| self.client.poll_ready(cx))
            .await
            .expect("HTTP/2 connection should be ready for another request");

        let uri = format!("http://{}/test-bucket/test-key.txt", self.authority)
            .parse()
            .expect("test URI should be valid");
        let request = signed_get_object_request(uri, payload_sha256);

        // Keep END_STREAM off the initial HEADERS so Hyper cannot infer an empty body from framing.
        let (response, mut send_stream) = self
            .client
            .send_request(request, false)
            .expect("HTTP/2 request headers should be accepted");

        let observation = self
            .observations
            .recv()
            .await
            .expect("server should observe the HTTP/2 request");
        assert_eq!(observation.version, Version::HTTP_2);
        assert!(!observation.has_content_length);
        assert_eq!(observation.exact_body_length, None);
        assert!(!observation.body_is_end_stream);

        send_stream
            .send_data(body, true)
            .expect("HTTP/2 request body should be accepted");
        self.dispatch
            .send(())
            .expect("server should still be waiting to dispatch the request");

        response.await
    }

    fn get_object_calls(&self) -> usize {
        self.get_object_calls.load(Ordering::SeqCst)
    }

    /// Closes the client side of the connection, then asserts that both connection
    /// drivers finished without an error. Panics in spawned tasks do not fail the
    /// test, so the connection results must be awaited here to be asserted at all.
    ///
    /// Call this only after every response has been drained: a stream still in
    /// flight keeps the client connection open and this future would block. The
    /// panic path needs no explicit teardown because dropping the `#[tokio::test]`
    /// runtime cancels both tasks.
    async fn shutdown(self) {
        drop(self.client);
        self.client_driver
            .await
            .expect("HTTP/2 test client driver task should not panic")
            .expect("HTTP/2 test client should complete without a connection error");
        self.server_connection
            .await
            .expect("HTTP/2 test server task should not panic")
            .expect("HTTP/2 test server should complete without a connection error");
    }
}

fn signed_get_object_request(uri: Uri, payload_sha256: &'static str) -> Request<()> {
    let amz_date = s3s_sigv4::AmzDate::parse(AMZ_DATE).expect("test date should be valid");
    let amz_date_str = amz_date.fmt_iso8601();
    let host = uri.authority().expect("test URI should have an authority").as_str();
    let signed_headers = [
        ("host", host),
        ("x-amz-content-sha256", payload_sha256),
        ("x-amz-date", amz_date_str.as_str()),
    ];
    let payload = if payload_sha256 == UNSIGNED_PAYLOAD {
        s3s_sigv4::Payload::Unsigned
    } else {
        s3s_sigv4::Payload::SingleChunk(payload_sha256)
    };
    let canonical_request =
        s3s_sigv4::create_canonical_request(Method::GET.as_str(), uri.path(), &[] as &[(&str, &str)], signed_headers, payload);
    let string_to_sign = s3s_sigv4::create_string_to_sign(&canonical_request, &amz_date, REGION, SERVICE);
    let signature = s3s_sigv4::calculate_signature(&string_to_sign, SECRET_KEY, &amz_date, REGION, SERVICE);
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={ACCESS_KEY}/{}/{REGION}/{SERVICE}/aws4_request, \
         SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={}",
        amz_date.fmt_date(),
        signature.as_str()
    );

    Request::builder()
        .method(Method::GET)
        .version(Version::HTTP_2)
        .uri(uri)
        .header(s3s::header::X_AMZ_CONTENT_SHA256, payload_sha256)
        .header(s3s::header::X_AMZ_DATE, AMZ_DATE)
        .header(s3s::header::AUTHORIZATION, authorization)
        .body(())
        .expect("signed test request should be valid")
}

async fn response_status(mut response: Response<RecvStream>) -> Result<StatusCode, h2::Error> {
    let status = response.status();
    let body = response.body_mut();
    while let Some(data) = body.data().await {
        let data = data?;
        body.flow_control().release_capacity(data.len())?;
    }
    body.trailers().await?;
    Ok(status)
}

async fn accept_response_or_stream_reset(response: Result<Response<RecvStream>, h2::Error>) {
    let result = match response {
        Ok(response) => response_status(response).await.map(|_| ()),
        Err(err) => Err(err),
    };
    if let Err(err) = result {
        assert!(err.is_reset(), "unexpected HTTP/2 connection error: {err}");
    }
}

#[tokio::test]
async fn bodyless_sigv4_requests_succeed_over_real_http2_without_content_length() {
    let mut harness = Http2Harness::start().await;

    for payload_sha256 in [EMPTY_SHA256, UNSIGNED_PAYLOAD] {
        let response = harness
            .get_object(payload_sha256, Bytes::new())
            .await
            .expect("bodyless signed request should receive an HTTP response");
        assert_eq!(
            response_status(response).await.expect("response body should complete"),
            StatusCode::OK,
            "bodyless request using {payload_sha256} should succeed"
        );
    }

    assert_eq!(harness.get_object_calls(), 2);
    harness.shutdown().await;
}

#[tokio::test]
async fn unexpected_request_body_does_not_poison_http2_connection() {
    let mut harness = Http2Harness::start().await;

    let unexpected_body_response = harness.get_object(UNSIGNED_PAYLOAD, Bytes::from_static(b"unexpected")).await;
    // Either a completed response or a stream reset safely ends this request; connection reuse is the contract under test.
    accept_response_or_stream_reset(unexpected_body_response).await;

    let calls_before_follow_up = harness.get_object_calls();
    let response = harness
        .get_object(UNSIGNED_PAYLOAD, Bytes::new())
        .await
        .expect("connection should accept a request after an unexpected body");
    assert_eq!(
        response_status(response)
            .await
            .expect("follow-up response body should complete"),
        StatusCode::OK
    );
    assert_eq!(harness.get_object_calls(), calls_before_follow_up + 1);
    harness.shutdown().await;
}
