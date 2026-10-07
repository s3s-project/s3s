// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

mod generated;

#[cfg(feature = "minio")]
mod listen;

#[cfg(feature = "minio")]
mod minio_error;

#[cfg(feature = "minio")]
mod minio_list;

mod meta;

#[cfg(feature = "minio")]
use minio::s3::MinioClient;

/// An S3 service adapter that forwards requests through the AWS SDK for S3.
///
/// Build one via [`ProxyBuilder`].
pub struct Proxy {
    client: aws_sdk_s3::Client,
    #[cfg(feature = "minio")]
    minio: MinioClient,
}

impl Proxy {
    /// Returns a builder for a [`Proxy`].
    #[must_use]
    pub fn builder(client: aws_sdk_s3::Client) -> ProxyBuilder {
        ProxyBuilder {
            client,
            #[cfg(feature = "minio")]
            minio: None,
        }
    }
}

/// Builder for [`Proxy`].
pub struct ProxyBuilder {
    client: aws_sdk_s3::Client,
    #[cfg(feature = "minio")]
    minio: Option<MinioClient>,
}

impl ProxyBuilder {
    /// Set the `MinIO` client used for `MinIO`-only extensions.
    ///
    /// This method is only available with the `minio` feature.
    #[cfg(feature = "minio")]
    #[must_use]
    pub fn minio_client(mut self, minio: MinioClient) -> Self {
        self.minio = Some(minio);
        self
    }

    /// Build the [`Proxy`].
    ///
    /// With the `minio` feature, a `MinIO` client must have been set via
    /// [`Self::minio_client`]; a missing one is a programming error.
    #[must_use]
    pub fn build(self) -> Proxy {
        Proxy {
            client: self.client,
            #[cfg(feature = "minio")]
            minio: self.minio.expect("minio client is required with the minio feature"),
        }
    }
}

#[cfg(all(test, feature = "minio"))]
mod tests {
    use super::Proxy;

    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use hyper::StatusCode;
    use hyper::http::{Extensions, HeaderMap, Method, Uri};
    use s3s::S3;
    use s3s::S3Request;
    use s3s::dto::{DeleteObjectInput, ETagCondition, GetObjectInput};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    /// Serves exactly one request: records its head and answers the given raw response.
    ///
    /// The response is a full HTTP/1.1 head, so a test can answer a status the SDK models as an
    /// error (a `304` has no modeled output and no modeled error) and attach the headers a real
    /// backend would send.
    async fn serve_recorded(response: &'static str) -> (SocketAddr, Arc<Mutex<String>>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let seen = Arc::new(Mutex::new(String::new()));
        let recorder = Arc::clone(&seen);

        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut head = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                let read = socket.read(&mut chunk).await.expect("read");
                if read == 0 {
                    break;
                }
                head.extend_from_slice(&chunk[..read]);
            }
            *recorder.lock().expect("lock") = String::from_utf8_lossy(&head).into_owned();
            socket.write_all(response.as_bytes()).await.expect("write response");
        });

        (addr, seen, handle)
    }

    /// Serves exactly one request: records its head and answers `204 No Content`.
    async fn record_one_request() -> (SocketAddr, Arc<Mutex<String>>, tokio::task::JoinHandle<()>) {
        serve_recorded("HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n").await
    }

    fn proxy_for(addr: SocketAddr) -> Proxy {
        let config = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .credentials_provider(aws_sdk_s3::config::Credentials::new("test", "test", None, None, "test"))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .endpoint_url(format!("http://{addr}"))
            .force_path_style(true)
            .build();
        let provider = minio::s3::creds::StaticProvider::new("test", "test", None);
        let minio = minio::s3::MinioClient::new("http://127.0.0.1:1".parse().expect("url"), Some(provider), None, None)
            .expect("minio client");
        Proxy::builder(aws_sdk_s3::Client::from_conf(config))
            .minio_client(minio)
            .build()
    }

    fn delete_object_request(bucket: &str, key: &str) -> S3Request<DeleteObjectInput> {
        let input = DeleteObjectInput {
            bucket: bucket.to_owned(),
            key: key.to_owned(),
            ..Default::default()
        };
        S3Request {
            input,
            method: Method::DELETE,
            uri: Uri::from_static("/"),
            headers: HeaderMap::new(),
            extensions: Extensions::default(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        }
    }

    /// The entity tag a conditional read was matched against.
    const NOT_MODIFIED_ETAG: &str = "\"37b51d194a7513e45b56f6524f2d51f2\"";
    /// The modification time a conditional read reports.
    const NOT_MODIFIED_LAST_MODIFIED: &str = "Wed, 07 Oct 2026 18:33:28 GMT";
    /// A real backend answers a conditional read hit with the entity headers the condition was
    /// matched against, plus headers that describe *its* body.
    const NOT_MODIFIED_RESPONSE: &str = concat!(
        "HTTP/1.1 304 Not Modified\r\n",
        "etag: \"37b51d194a7513e45b56f6524f2d51f2\"\r\n",
        "last-modified: Wed, 07 Oct 2026 18:33:28 GMT\r\n",
        "content-length: 0\r\n",
        "content-type: application/xml\r\n",
        "\r\n",
    );

    fn get_object_request(bucket: &str, key: &str, if_none_match: &str) -> S3Request<GetObjectInput> {
        let input = GetObjectInput {
            bucket: bucket.to_owned(),
            key: key.to_owned(),
            if_none_match: Some(ETagCondition::parse_http_header(if_none_match.as_bytes()).expect("etag condition")),
            ..Default::default()
        };
        S3Request {
            input,
            method: Method::GET,
            uri: Uri::from_static("/"),
            headers: HeaderMap::new(),
            extensions: Extensions::default(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        }
    }

    /// A conditional read hit is a `304 Not Modified` carrying the entity tag the condition was
    /// matched against; a client caches on that tag. The SDK has no modeled output for a `304`,
    /// so the proxy reports the status through the error channel: the entity headers have to
    /// survive on the error, or the answer carries nothing the client can use.
    #[tokio::test]
    async fn get_object_not_modified_keeps_entity_headers() {
        let (addr, _seen, server) = serve_recorded(NOT_MODIFIED_RESPONSE).await;
        let proxy = proxy_for(addr);

        let err = proxy
            .get_object(get_object_request("bucket", "key", NOT_MODIFIED_ETAG))
            .await
            .expect_err("the SDK reports a 304 as an error");

        assert_eq!(err.status_code(), Some(StatusCode::NOT_MODIFIED));
        let headers = err.headers().expect("a 304 must carry the entity headers");
        assert_eq!(
            headers.get(hyper::header::ETAG).and_then(|value| value.to_str().ok()),
            Some(NOT_MODIFIED_ETAG)
        );
        assert_eq!(
            headers
                .get(hyper::header::LAST_MODIFIED)
                .and_then(|value| value.to_str().ok()),
            Some(NOT_MODIFIED_LAST_MODIFIED)
        );
        // Negative controls: headers that describe the backend's body or connection must not be
        // copied, because the client never receives that body.
        assert!(!headers.contains_key(hyper::header::CONTENT_LENGTH), "{headers:?}");
        assert!(!headers.contains_key(hyper::header::CONTENT_TYPE), "{headers:?}");

        server.await.expect("server task");
    }

    /// The `MinIO` extension member has no `aws-sdk-s3` counterpart, so the header
    /// has to reach the backend through the operation customization hook.
    #[tokio::test]
    async fn delete_object_forwards_the_force_delete_header() {
        let (addr, seen, server) = record_one_request().await;
        let proxy = proxy_for(addr);

        let mut req = delete_object_request("bucket", "key");
        req.input.force_delete = Some(true);
        proxy.delete_object(req).await.expect("delete object");

        server.await.expect("server task");
        let head = seen.lock().expect("lock").clone().to_ascii_lowercase();
        assert!(head.contains("\r\nx-minio-force-delete: true\r\n"), "{head}");
    }

    /// Negative control: without the extension member the header must not be sent.
    #[tokio::test]
    async fn delete_object_omits_the_force_delete_header() {
        let (addr, seen, server) = record_one_request().await;
        let proxy = proxy_for(addr);

        proxy
            .delete_object(delete_object_request("bucket", "key"))
            .await
            .expect("delete object");

        server.await.expect("server task");
        let head = seen.lock().expect("lock").clone().to_ascii_lowercase();
        assert!(!head.contains("x-minio-force-delete"), "{head}");
    }
}
