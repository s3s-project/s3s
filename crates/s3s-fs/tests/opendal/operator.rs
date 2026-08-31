// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use crate::case;
use crate::suite::OperatorFixture;

use std::sync::Arc;

use futures_util::stream::StreamExt;
use s3s_test::Result;
use s3s_test::tcx::TestContext;
use tracing::debug;
use uuid::Uuid;

pub fn register(tcx: &mut TestContext) {
    case!(tcx, OpendalServer, OperatorFixture, test_operator_info);
    case!(tcx, OpendalServer, OperatorFixture, test_write_and_read);
    case!(tcx, OpendalServer, OperatorFixture, test_stat);
    case!(tcx, OpendalServer, OperatorFixture, test_list);
    case!(tcx, OpendalServer, OperatorFixture, test_write_and_list_root);
    case!(tcx, OpendalServer, OperatorFixture, test_delete_non_existent);
    case!(tcx, OpendalServer, OperatorFixture, test_range_read);
}

impl OperatorFixture {
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    async fn test_operator_info(self: Arc<Self>) -> Result<()> {
        let op = &self.op;
        let info = op.info();

        debug!("Operator scheme: {:?}", info.scheme());
        debug!("Operator capabilities: {:?}", info.capability());

        // Basic smoke test - operator should be created successfully
        assert_eq!(info.scheme(), "s3");

        Ok(())
    }

    async fn test_write_and_read(self: Arc<Self>) -> Result<()> {
        let op = &self.op;
        let key = format!("test-write-read-{}", Uuid::new_v4());
        let content = "hello world\nनमस्ते दुनिया\n";

        // Write data
        op.write(&key, content).await?;
        debug!("Written data to key: {key}");

        // Read data back
        let data = op.read(&key).await?;
        let data_vec = data.to_vec();
        let read_content = std::str::from_utf8(&data_vec)?;

        assert_eq!(read_content, content);
        debug!("Read data matches written data");

        // Clean up
        op.delete(&key).await?;
        debug!("Deleted key: {key}");

        Ok(())
    }

    async fn test_stat(self: Arc<Self>) -> Result<()> {
        let op = &self.op;
        let key = format!("test-stat-{}", Uuid::new_v4());
        let content = "test content for stat";

        // Write data
        op.write(&key, content).await?;

        // Get metadata
        let metadata = op.stat(&key).await?;

        assert_eq!(metadata.content_length(), content.len() as u64);
        assert!(metadata.is_file());
        debug!("Metadata: {:?}", metadata);

        // Clean up
        op.delete(&key).await?;

        Ok(())
    }

    async fn test_list(self: Arc<Self>) -> Result<()> {
        let op = &self.op;
        let prefix = format!("test-list-{}/", Uuid::new_v4());
        let key1 = format!("{prefix}file1.txt");
        let key2 = format!("{prefix}file2.txt");
        let content = "test content";

        // Write test files
        op.write(&key1, content).await?;
        op.write(&key2, content).await?;

        // List files with prefix
        let mut lister = op.lister(&prefix).await?;
        let mut found_keys = Vec::new();

        while let Some(entry) = lister.next().await {
            let entry = entry?;
            let path = entry.path().to_string();
            found_keys.push(path.clone());
            debug!("Found entry: {}", path);

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
        op.delete(&key1).await?;
        op.delete(&key2).await?;

        Ok(())
    }

    async fn test_write_and_list_root(self: Arc<Self>) -> Result<()> {
        let op = &self.op;
        let key = format!("test-root-{}", Uuid::new_v4());
        op.write(&key, "test").await?;
        op.list("/").await?;

        // Clean up
        op.delete(&key).await?;

        Ok(())
    }

    async fn test_delete_non_existent(self: Arc<Self>) -> Result<()> {
        let op = &self.op;
        let key = format!("non-existent-{}", Uuid::new_v4());

        // Delete non-existent key should not error (S3 behavior)
        op.delete(&key).await?;
        debug!("Delete non-existent key succeeded");

        Ok(())
    }

    async fn test_range_read(self: Arc<Self>) -> Result<()> {
        let op = &self.op;
        let key = format!("test-range-{}", Uuid::new_v4());
        let content = "0123456789abcdefghijklmnopqrstuvwxyz";

        // Write data
        op.write(&key, content).await?;

        // Read range
        let range_data = op.read_with(&key).range(5..15).await?;
        let range_vec = range_data.to_vec();
        let range_content = std::str::from_utf8(&range_vec)?;

        assert_eq!(range_content, &content[5..15]);
        debug!("Range read: {} -> {}", 5, 15);

        // Clean up
        op.delete(&key).await?;

        Ok(())
    }
}
