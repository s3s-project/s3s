use s3s::auth::SimpleAuth;
use s3s::host::SingleDomain;
use s3s::service::S3ServiceBuilder;
use s3s_fs::FileSystem;

use std::env;
use std::fs;
use std::sync::Once;

use anyhow::Result;
use futures_util::stream::StreamExt;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use opendal::Operator;
use opendal::services::S3;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::sync::MutexGuard;
use tracing::{debug, error};
use uuid::Uuid;

const FS_ROOT: &str = concat!(env!("CARGO_TARGET_TMPDIR"), "/s3s-fs-tests-opendal");
const DOMAIN_NAME: &str = "localhost:8015";
const REGION: &str = "us-west-2";
const ACCESS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

async fn ensure_server_ready() {
    use std::sync::LazyLock;
    static INIT: Once = Once::new();
    static SERVER_INIT: LazyLock<tokio::sync::Mutex<bool>> = LazyLock::new(|| tokio::sync::Mutex::new(false));

    let mut server_started = SERVER_INIT.lock().await;
    if *server_started {
        // Server already running, just verify it's accessible
        if tokio::net::TcpStream::connect(DOMAIN_NAME).await.is_ok() {
            return;
        }
        // If we can't connect, we'll restart the server
        *server_started = false;
    }

    if !*server_started {
        INIT.call_once(|| {
            use tracing_subscriber::EnvFilter;
            tracing_subscriber::fmt()
                .pretty()
                .with_env_filter(EnvFilter::from_default_env())
                .with_test_writer()
                .init();
        });

        // Setup S3 provider
        fs::create_dir_all(FS_ROOT).unwrap();
        let fs = FileSystem::new(FS_ROOT).unwrap();

        // Setup S3 service
        let service = {
            let mut b = S3ServiceBuilder::new(fs);
            b.set_auth(SimpleAuth::from_single(ACCESS_KEY, SECRET_KEY));
            b.set_host(SingleDomain::new(DOMAIN_NAME).unwrap());
            b.build()
        };

        // Start HTTP server
        let listener = TcpListener::bind(DOMAIN_NAME).await.unwrap();
        debug!("Server listening on {}", DOMAIN_NAME);

        tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let service_clone = service.clone();

                // Create a new http_server instance for each connection
                let http_server = ConnBuilder::new(TokioExecutor::new());
                let conn = http_server.serve_connection(TokioIo::new(socket), service_clone).into_owned();
                tokio::spawn(async move {
                    let _ = conn.await;
                });
            }
        });

        *server_started = true;
    }

    // Wait for server to be ready by attempting to connect
    let mut attempts = 0;
    while attempts < 50 {
        if tokio::net::TcpStream::connect(DOMAIN_NAME).await.is_ok() {
            debug!("Server is ready after {} attempts", attempts);
            return;
        }
        attempts += 1;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("Server failed to start after 50 attempts (2.5 seconds)");
}

fn create_operator() -> Operator {
    let builder = S3::default()
        .endpoint(&format!("http://{DOMAIN_NAME}"))
        .region(REGION)
        .access_key_id(ACCESS_KEY)
        .secret_access_key(SECRET_KEY)
        .bucket("test-bucket");

    Operator::new(builder).unwrap().finish()
}

async fn serial() -> MutexGuard<'static, ()> {
    use std::sync::LazyLock;
    static LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
    LOCK.lock().await
}

macro_rules! log_and_unwrap {
    ($result:expr) => {
        match $result {
            Ok(ans) => ans,
            Err(err) => {
                error!(?err);
                return Err(err.into());
            }
        }
    };
}

#[tokio::test]
#[tracing::instrument]
async fn test_operator_info() -> Result<()> {
    let _guard = serial().await;
    ensure_server_ready().await;

    let op = create_operator();
    let info = op.info();

    debug!("Operator scheme: {:?}", info.scheme());
    debug!("Operator capabilities: {:?}", info.full_capability());

    // Basic smoke test - operator should be created successfully
    assert_eq!(info.scheme(), "s3");

    Ok(())
}

#[tokio::test]
#[tracing::instrument]
async fn test_write_and_read() -> Result<()> {
    let _guard = serial().await;
    ensure_server_ready().await;

    let op = create_operator();
    let key = format!("test-write-read-{}", Uuid::new_v4());
    let content = "hello world\nनमस्ते दुनिया\n";

    // Write data
    log_and_unwrap!(op.write(&key, content).await);
    debug!("Written data to key: {key}");

    // Read data back
    let data = log_and_unwrap!(op.read(&key).await);
    let data_vec = data.to_vec();
    let read_content = std::str::from_utf8(&data_vec)?;

    assert_eq!(read_content, content);
    debug!("Read data matches written data");

    // Clean up
    log_and_unwrap!(op.delete(&key).await);
    debug!("Deleted key: {key}");

    Ok(())
}

#[tokio::test]
#[tracing::instrument]
async fn test_stat() -> Result<()> {
    let _guard = serial().await;
    ensure_server_ready().await;

    let op = create_operator();
    let key = format!("test-stat-{}", Uuid::new_v4());
    let content = "test content for stat";

    // Write data
    log_and_unwrap!(op.write(&key, content).await);

    // Get metadata
    let metadata = log_and_unwrap!(op.stat(&key).await);

    assert_eq!(metadata.content_length(), content.len() as u64);
    assert!(metadata.is_file());
    debug!("Metadata: {:?}", metadata);

    // Clean up
    log_and_unwrap!(op.delete(&key).await);

    Ok(())
}

#[tokio::test]
#[tracing::instrument]
async fn test_list() -> Result<()> {
    let _guard = serial().await;
    ensure_server_ready().await;

    let op = create_operator();
    let prefix = format!("test-list-{}/", Uuid::new_v4());
    let key1 = format!("{prefix}file1.txt");
    let key2 = format!("{prefix}file2.txt");
    let content = "test content";

    // Write test files
    log_and_unwrap!(op.write(&key1, content).await);
    log_and_unwrap!(op.write(&key2, content).await);

    // List files with prefix
    let mut lister = log_and_unwrap!(op.lister(&prefix).await);
    let mut found_keys = Vec::new();

    while let Some(entry) = lister.next().await {
        let entry = log_and_unwrap!(entry);
        let path = entry.path().to_string();
        found_keys.push(path.clone());
        debug!("Found entry: {}", path);

        // Only collect entries that match our test keys to avoid false positives
        if path == key1 || path == key2 {
            // Found our keys, continue until we have both or no more entries
        }

        // Safety break to avoid infinite loop (should not be needed now)
        if found_keys.len() > 20 {
            debug!("Breaking after 20 entries to avoid infinite loop");
            break;
        }
    }

    assert!(found_keys.contains(&key1), "Did not find key1: {key1} in {found_keys:?}");
    assert!(found_keys.contains(&key2), "Did not find key2: {key2} in {found_keys:?}");
    debug!("Found {} files total, including our test files", found_keys.len());

    // Clean up
    log_and_unwrap!(op.delete(&key1).await);
    log_and_unwrap!(op.delete(&key2).await);

    Ok(())
}

#[tokio::test]
#[tracing::instrument]
async fn test_write_and_list_root() -> Result<()> {
    let _guard = serial().await;
    ensure_server_ready().await;

    let op = create_operator();
    log_and_unwrap!(op.write("a", "test").await);
    log_and_unwrap!(op.list("/").await);

    // Clean up
    log_and_unwrap!(op.delete("a").await);

    Ok(())
}

#[tokio::test]
#[tracing::instrument]
async fn test_delete_non_existent() -> Result<()> {
    let _guard = serial().await;
    ensure_server_ready().await;

    let op = create_operator();
    let key = format!("non-existent-{}", Uuid::new_v4());

    // Delete non-existent key should not error (S3 behavior)
    log_and_unwrap!(op.delete(&key).await);
    debug!("Delete non-existent key succeeded");

    Ok(())
}

#[tokio::test]
#[tracing::instrument]
async fn test_range_read() -> Result<()> {
    let _guard = serial().await;
    ensure_server_ready().await;

    let op = create_operator();
    let key = format!("test-range-{}", Uuid::new_v4());
    let content = "0123456789abcdefghijklmnopqrstuvwxyz";

    // Write data
    log_and_unwrap!(op.write(&key, content).await);

    // Read range
    let range_data = log_and_unwrap!(op.read_with(&key).range(5..15).await);
    let range_vec = range_data.to_vec();
    let range_content = std::str::from_utf8(&range_vec)?;

    assert_eq!(range_content, &content[5..15]);
    debug!("Range read: {} -> {}", 5, 15);

    // Clean up
    log_and_unwrap!(op.delete(&key).await);

    Ok(())
}
