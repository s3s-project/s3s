// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use s3s::auth::SimpleAuth;
use s3s::host::SingleDomain;
use s3s::service::S3ServiceBuilder;
use s3s_fs::FileSystem;

use std::fs;
use std::future::Future;
use std::sync::Arc;

use hyper_util::rt::TokioExecutor;
use hyper_util::rt::TokioIo;
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use opendal::Operator;
use opendal::services::S3;
use s3s_test::Result;
use s3s_test::TestFixture;
use s3s_test::TestSuite;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracing::debug;

pub(crate) const FS_ROOT: &str = concat!(env!("CARGO_TARGET_TMPDIR"), "/s3s-fs-tests-opendal");
pub(crate) const REGION: &str = "us-west-2";
pub(crate) const ACCESS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
pub(crate) const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

pub(crate) struct OpendalServer {
    pub(crate) operator: Operator,
    handle: JoinHandle<()>,
}

impl TestSuite for OpendalServer {
    #[tracing::instrument(skip_all)]
    async fn setup() -> Result<Self> {
        // opendal 0.58 requires a process-wide HTTP transport to be installed
        // before using HTTP-based services. `auto-register-services` (the
        // ctor-based auto-install) is disabled here via
        // `default-features = false`, so install the reqwest transport
        // explicitly. This call is idempotent and lazy.
        opendal::install_default();

        // Setup S3 provider
        fs::create_dir_all(FS_ROOT)?;
        let fs = FileSystem::new(FS_ROOT).unwrap();

        // Setup S3 service
        let service = {
            let mut b = S3ServiceBuilder::new(fs);
            b.set_auth(SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY));
            b.set_host(SingleDomain::new("localhost").unwrap());
            b.build()
        };

        // Start HTTP server on a random port
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let endpoint = format!("http://{addr}");
        debug!("Server listening on {addr}");

        let handle = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else { break };
                let service_clone = service.clone();

                // Create a new http_server instance for each connection
                let http_server = ConnBuilder::new(TokioExecutor::new());
                let conn = http_server.serve_connection(TokioIo::new(socket), service_clone).into_owned();
                tokio::spawn(async move {
                    let _ = conn.await;
                });
            }
        });

        // Wait for the server to be ready
        let mut attempts = 0;
        loop {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                debug!("Server is ready after {attempts} attempts");
                break;
            }
            attempts += 1;
            assert!(attempts < 50, "server failed to start after 50 attempts");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let operator = {
            let builder = S3::default()
                .endpoint(&endpoint)
                .region(REGION)
                .access_key_id(ACCESS_KEY)
                .secret_access_key(SECRET_KEY)
                .bucket("test-bucket");

            Operator::new(builder)?
        };

        Ok(Self { operator, handle })
    }

    fn teardown(self) -> impl Future<Output = Result> + Send + 'static {
        self.handle.abort();
        std::future::ready(Ok(()))
    }
}

#[macro_export]
macro_rules! case {
    ($tcx: expr, $s:ident, $x:ident, $c:ident) => {{
        #[allow(clippy::wildcard_imports)]
        use $crate::suite::*;
        let mut suite = $tcx.suite::<$s>(stringify!($s));
        let mut fixture = suite.fixture::<$x>(stringify!($x));
        fixture.case(stringify!($c), $x::$c);
    }};
}

pub(crate) struct OperatorFixture {
    pub(crate) op: Operator,
}

impl TestFixture<OpendalServer> for OperatorFixture {
    fn setup(suite: Arc<OpendalServer>) -> impl Future<Output = Result<Self>> + Send + 'static {
        let operator = suite.operator.clone();
        std::future::ready(Ok(Self { op: operator }))
    }
}
