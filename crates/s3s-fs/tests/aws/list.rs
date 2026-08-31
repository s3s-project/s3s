// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use crate::case;
use crate::suite::{List, create_bucket, delete_bucket, delete_object};

use std::sync::Arc;

use aws_sdk_s3::primitives::ByteStream;

use s3s_test::Result;
use s3s_test::tcx::TestContext;
use tracing::debug;
use uuid::Uuid;

pub fn register(tcx: &mut TestContext) {
    case!(tcx, FsServer, List, test_list_objects_v2);
    case!(tcx, FsServer, List, test_list_objects_v2_with_prefixes);
    case!(tcx, FsServer, List, test_list_objects_v1_with_prefixes);
    case!(tcx, FsServer, List, test_list_objects_v1_next_marker_with_delimiter);
    case!(tcx, FsServer, List, test_list_objects_v1_marker_pagination);
    case!(tcx, FsServer, List, test_list_objects_v1_max_keys_one);
    case!(tcx, FsServer, List, test_list_objects_v1_delimiter_multi_prefix_pagination);
    case!(tcx, FsServer, List, test_list_objects_v2_delimiter_multi_prefix_pagination);
    case!(tcx, FsServer, List, test_list_objects_v2_max_keys);
    case!(tcx, FsServer, List, test_list_objects_v2_start_after);
    case!(tcx, FsServer, List, test_list_objects_v2_continuation_token_with_delimiter);
    case!(tcx, FsServer, List, test_list_objects_v2_prefix_string_matching);
    case!(tcx, FsServer, List, test_list_objects_v2_continuation_token_pagination);
    case!(tcx, FsServer, List, test_list_objects_v2_continuation_token_and_start_after_uses_max);
    case!(tcx, FsServer, List, test_list_objects_v2_max_keys_zero);
}

impl List {
    async fn test_list_objects_v2(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-list-objects-v2-{}", Uuid::new_v4());
        let bucket_str = bucket.as_str();
        create_bucket(c, bucket_str).await?;

        let test_prefix = "/this/is/a/test/";
        let key1 = "this/is/a/test/path/file1.txt";
        let key2 = "this/is/a/test/path/file2.txt";
        {
            let content = "hello world\nनमस्ते दुनिया\n";
            let crc32c = base64_simd::STANDARD.encode_to_string(crc32c::crc32c(content.as_bytes()).to_be_bytes());
            c.put_object()
                .bucket(bucket_str)
                .key(key1)
                .body(ByteStream::from_static(content.as_bytes()))
                .checksum_crc32_c(crc32c.as_str())
                .send()
                .await?;
            c.put_object()
                .bucket(bucket_str)
                .key(key2)
                .body(ByteStream::from_static(content.as_bytes()))
                .checksum_crc32_c(crc32c.as_str())
                .send()
                .await?;
        }

        let result = c.list_objects_v2().bucket(bucket_str).prefix(test_prefix).send().await;

        let response = result?;

        let contents: Vec<_> = response.contents().iter().filter_map(|obj| obj.key()).collect();
        assert_ne!(contents.len(), 0);
        assert!(contents.contains(&key1));
        assert!(contents.contains(&key2));

        Ok(())
    }

    async fn test_list_objects_v2_with_prefixes(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-list-prefixes-{}", Uuid::new_v4());
        let bucket_str = bucket.as_str();
        create_bucket(c, bucket_str).await?;

        // Create files in nested directory structure
        let content = "hello world\n";
        let files = [
            "README.md",                   // Root level file
            "test/subdirectory/README.md", // Nested file
            "test/file.txt",               // File in test/ directory
            "other/dir/file.txt",          // File in other/dir/ directory
        ];

        for key in &files {
            c.put_object()
                .bucket(bucket_str)
                .key(*key)
                .body(ByteStream::from_static(content.as_bytes()))
                .send()
                .await?;
        }

        // List without delimiter - should return all files recursively
        let result = c.list_objects_v2().bucket(bucket_str).send().await;

        let response = result?;
        let contents: Vec<_> = response.contents().iter().filter_map(|obj| obj.key()).collect();

        debug!("List without delimiter - objects: {:?}", contents);
        assert_eq!(contents.len(), 4);
        for key in &files {
            assert!(contents.contains(key), "Missing key: {key}");
        }

        // List with delimiter "/" - should return root files and common prefixes
        let result = c.list_objects_v2().bucket(bucket_str).delimiter("/").send().await;

        let response = result?;

        // Should have one file at root level
        let contents: Vec<_> = response.contents().iter().filter_map(|obj| obj.key()).collect();
        debug!("List with delimiter - objects: {:?}", contents);
        assert_eq!(contents.len(), 1);
        assert!(contents.contains(&"README.md"));

        // Should have two common prefixes: "test/" and "other/"
        let prefixes: Vec<_> = response.common_prefixes().iter().filter_map(|cp| cp.prefix()).collect();
        debug!("List with delimiter - prefixes: {:?}", prefixes);
        assert_eq!(prefixes.len(), 2);
        assert!(prefixes.contains(&"test/"));
        assert!(prefixes.contains(&"other/"));

        // List with prefix "test/" and delimiter "/" - should return files in test/ and subdirectories
        let result = c
            .list_objects_v2()
            .bucket(bucket_str)
            .prefix("test/")
            .delimiter("/")
            .send()
            .await;

        let response = result?;

        // Should have one file in test/ directory
        let contents: Vec<_> = response.contents().iter().filter_map(|obj| obj.key()).collect();
        debug!("List with prefix test/ - objects: {:?}", contents);
        assert_eq!(contents.len(), 1);
        assert!(contents.contains(&"test/file.txt"));

        // Should have one common prefix: "test/subdirectory/"
        let prefixes: Vec<_> = response.common_prefixes().iter().filter_map(|cp| cp.prefix()).collect();
        debug!("List with prefix test/ - prefixes: {:?}", prefixes);
        assert_eq!(prefixes.len(), 1);
        assert!(prefixes.contains(&"test/subdirectory/"));

        Ok(())
    }

    async fn test_list_objects_v1_with_prefixes(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-list-v1-prefixes-{}", Uuid::new_v4());
        let bucket_str = bucket.as_str();
        create_bucket(c, bucket_str).await?;

        // Create a simple structure
        let content = "hello world\n";
        let files = ["README.md", "dir/file.txt"];

        for key in &files {
            c.put_object()
                .bucket(bucket_str)
                .key(*key)
                .body(ByteStream::from_static(content.as_bytes()))
                .send()
                .await?;
        }

        // Test list_objects (v1) with delimiter
        let result = c.list_objects().bucket(bucket_str).delimiter("/").send().await;

        let response = result?;

        // Should have one file at root level
        let contents: Vec<_> = response.contents().iter().filter_map(|obj| obj.key()).collect();
        debug!("ListObjects v1 with delimiter - objects: {:?}", contents);
        assert_eq!(contents.len(), 1);
        assert!(contents.contains(&"README.md"));

        // Should have one common prefix: "dir/"
        let prefixes: Vec<_> = response.common_prefixes().iter().filter_map(|cp| cp.prefix()).collect();
        debug!("ListObjects v1 with delimiter - prefixes: {:?}", prefixes);
        assert_eq!(prefixes.len(), 1);
        assert!(prefixes.contains(&"dir/"));

        Ok(())
    }

    async fn test_list_objects_v1_next_marker_with_delimiter(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-v1-next-marker-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(c, bucket).await?;

        // Objects: a.txt, dir/1.txt (→ common prefix "dir/"), z.txt
        // With delimiter="/", max_keys=2 → page 1 = [a.txt, dir/], truncated
        let keys = ["a.txt", "dir/1.txt", "z.txt"];
        for key in &keys {
            c.put_object()
                .bucket(bucket)
                .key(*key)
                .body(ByteStream::from_static(b"x"))
                .send()
                .await?;
        }

        let page1 = c.list_objects().bucket(bucket).delimiter("/").max_keys(2).send().await?;

        assert_eq!(page1.is_truncated(), Some(true));
        // Per S3 spec: when delimiter is set and is_truncated is true,
        // next_marker must be returned so the client can paginate.
        assert!(
            page1.next_marker().is_some(),
            "is_truncated is true with delimiter but next_marker is missing"
        );
        let page1_contents: Vec<_> = page1.contents().iter().filter_map(|obj| obj.key()).collect();
        let page1_prefixes: Vec<_> = page1.common_prefixes().iter().filter_map(|cp| cp.prefix()).collect();
        assert_eq!(page1_contents, vec!["a.txt"]);
        assert_eq!(page1_prefixes, vec!["dir/"]);
        let next_marker = page1.next_marker().expect("truncated v1 listing should include next_marker");

        let page2 = c
            .list_objects()
            .bucket(bucket)
            .delimiter("/")
            .max_keys(2)
            .marker(next_marker)
            .send()
            .await?;

        assert_eq!(page2.is_truncated(), Some(false));
        let page2_contents: Vec<_> = page2.contents().iter().filter_map(|obj| obj.key()).collect();
        let page2_prefixes: Vec<_> = page2.common_prefixes().iter().filter_map(|cp| cp.prefix()).collect();
        assert_eq!(page2_contents, vec!["z.txt"]);
        assert!(
            page2_prefixes.is_empty(),
            "second page should not repeat the already-returned common prefix"
        );

        // Cleanup
        for key in &keys {
            delete_object(c, bucket, key).await?;
        }
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    /// Walk all v1 pages using `marker` (no delimiter) and verify the full key set.
    async fn test_list_objects_v1_marker_pagination(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-v1-marker-pag-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(c, bucket).await?;

        let keys = ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"];
        for key in &keys {
            c.put_object()
                .bucket(bucket)
                .key(*key)
                .body(ByteStream::from_static(b"x"))
                .send()
                .await?;
        }

        // Walk all pages with max_keys=2
        let mut all_keys: Vec<String> = Vec::new();
        let mut marker: Option<String> = None;
        loop {
            let mut req = c.list_objects().bucket(bucket).max_keys(2);
            if let Some(m) = &marker {
                req = req.marker(m.clone());
            }
            let page = req.send().await?;

            all_keys.extend(page.contents().iter().filter_map(|o| o.key().map(String::from)));

            if page.is_truncated() != Some(true) {
                break;
            }
            marker = page.next_marker().map(String::from);
            assert!(marker.is_some(), "is_truncated is true but next_marker is missing");
        }

        assert_eq!(all_keys, vec!["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"]);

        for key in &keys {
            delete_object(c, bucket, key).await?;
        }
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    /// Walk all v1 pages with `max_keys=1` to verify single-item pages advance correctly.
    async fn test_list_objects_v1_max_keys_one(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-v1-maxkeys1-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(c, bucket).await?;

        let keys = ["a.txt", "b.txt", "c.txt"];
        for key in &keys {
            c.put_object()
                .bucket(bucket)
                .key(*key)
                .body(ByteStream::from_static(b"x"))
                .send()
                .await?;
        }

        let mut all_keys: Vec<String> = Vec::new();
        let mut marker: Option<String> = None;
        let mut page_count = 0;
        loop {
            let mut req = c.list_objects().bucket(bucket).max_keys(1);
            if let Some(m) = &marker {
                req = req.marker(m.clone());
            }
            let page = req.send().await?;

            let page_keys: Vec<_> = page.contents().iter().filter_map(|o| o.key().map(String::from)).collect();
            assert!(page_keys.len() <= 1, "max_keys=1 but got {} results", page_keys.len());
            all_keys.extend(page_keys);
            page_count += 1;

            if page.is_truncated() != Some(true) {
                break;
            }
            marker = page.next_marker().map(String::from);
            assert!(marker.is_some(), "is_truncated is true but next_marker is missing");
        }

        assert_eq!(all_keys, vec!["a.txt", "b.txt", "c.txt"]);
        assert_eq!(page_count, 3, "expected 3 single-item pages");

        for key in &keys {
            delete_object(c, bucket, key).await?;
        }
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    /// Walk all v1 pages with `delimiter` and multiple common prefixes,
    /// verifying no duplicates and complete coverage.
    async fn test_list_objects_v1_delimiter_multi_prefix_pagination(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-v1-multi-pfx-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(c, bucket).await?;

        // Objects sorted: a.txt, dir1/x.txt (→ "dir1/"), dir2/y.txt (→ "dir2/"), z.txt
        // With delimiter="/", this produces entries: a.txt, dir1/, dir2/, z.txt
        let keys = ["a.txt", "dir1/x.txt", "dir2/y.txt", "z.txt"];
        for key in &keys {
            c.put_object()
                .bucket(bucket)
                .key(*key)
                .body(ByteStream::from_static(b"x"))
                .send()
                .await?;
        }

        let mut all_objects: Vec<String> = Vec::new();
        let mut all_prefixes: Vec<String> = Vec::new();
        let mut marker: Option<String> = None;
        loop {
            let mut req = c.list_objects().bucket(bucket).delimiter("/").max_keys(2);
            if let Some(m) = &marker {
                req = req.marker(m.clone());
            }
            let page = req.send().await?;

            all_objects.extend(page.contents().iter().filter_map(|o| o.key().map(String::from)));
            all_prefixes.extend(page.common_prefixes().iter().filter_map(|p| p.prefix().map(String::from)));

            if page.is_truncated() != Some(true) {
                break;
            }
            marker = page.next_marker().map(String::from);
            assert!(marker.is_some(), "is_truncated is true but next_marker is missing");
        }

        assert_eq!(all_objects, vec!["a.txt", "z.txt"]);
        assert_eq!(all_prefixes, vec!["dir1/", "dir2/"]);

        for key in &keys {
            delete_object(c, bucket, key).await?;
        }
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    /// Walk all v2 pages with `delimiter` and multiple common prefixes,
    /// verifying no duplicates and complete coverage.
    async fn test_list_objects_v2_delimiter_multi_prefix_pagination(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-v2-multi-pfx-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(c, bucket).await?;

        let keys = ["a.txt", "dir1/x.txt", "dir2/y.txt", "z.txt"];
        for key in &keys {
            c.put_object()
                .bucket(bucket)
                .key(*key)
                .body(ByteStream::from_static(b"x"))
                .send()
                .await?;
        }

        let mut all_objects: Vec<String> = Vec::new();
        let mut all_prefixes: Vec<String> = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut req = c.list_objects_v2().bucket(bucket).delimiter("/").max_keys(2);
            if let Some(t) = &token {
                req = req.continuation_token(t.clone());
            }
            let page = req.send().await?;

            all_objects.extend(page.contents().iter().filter_map(|o| o.key().map(String::from)));
            all_prefixes.extend(page.common_prefixes().iter().filter_map(|p| p.prefix().map(String::from)));

            if page.is_truncated() != Some(true) {
                break;
            }
            token = page.next_continuation_token().map(String::from);
            assert!(token.is_some(), "is_truncated is true but next_continuation_token is missing");
        }

        assert_eq!(all_objects, vec!["a.txt", "z.txt"]);
        assert_eq!(all_prefixes, vec!["dir1/", "dir2/"]);

        for key in &keys {
            delete_object(c, bucket, key).await?;
        }
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    async fn test_list_objects_v2_max_keys(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-max-keys-{}", Uuid::new_v4());
        let bucket_str = bucket.as_str();
        create_bucket(c, bucket_str).await?;

        // Create 10 files
        let content = "test";
        for i in 0..10 {
            let key = format!("file{i:02}.txt");
            c.put_object()
                .bucket(bucket_str)
                .key(key)
                .body(ByteStream::from_static(content.as_bytes()))
                .send()
                .await?;
        }

        // Test max_keys=5
        let result = c.list_objects_v2().bucket(bucket_str).max_keys(5).send().await;
        let response = result?;

        // Should return exactly 5 objects
        let contents: Vec<_> = response.contents().iter().filter_map(|obj| obj.key()).collect();
        assert_eq!(contents.len(), 5, "Expected 5 objects, got {}", contents.len());
        assert_eq!(response.key_count(), Some(5));
        assert_eq!(response.max_keys(), Some(5));
        assert_eq!(response.is_truncated(), Some(true), "Should be truncated");

        // Test max_keys=20 (more than available)
        let result = c.list_objects_v2().bucket(bucket_str).max_keys(20).send().await;
        let response = result?;

        let contents: Vec<_> = response.contents().iter().filter_map(|obj| obj.key()).collect();
        assert_eq!(contents.len(), 10, "Expected 10 objects, got {}", contents.len());
        assert_eq!(response.key_count(), Some(10));
        assert_eq!(response.max_keys(), Some(20));
        assert_eq!(response.is_truncated(), Some(false), "Should not be truncated");

        Ok(())
    }

    /// Regression test for <https://github.com/s3s-project/s3s/issues/112>
    ///
    /// `list_objects_v2` prefix matching should use string-based matching (not `Path::starts_with`)
    /// and `start_after` should work correctly
    async fn test_list_objects_v2_start_after(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-start-after-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(c, bucket).await?;

        let content = "test content";
        let keys = ["aaa.txt", "bbb.txt", "ccc.txt", "ddd.txt"];
        for key in &keys {
            c.put_object()
                .bucket(bucket)
                .key(*key)
                .body(ByteStream::from_static(content.as_bytes()))
                .send()
                .await?;
        }

        // start_after="bbb.txt" should return only ccc.txt and ddd.txt
        let result = c.list_objects_v2().bucket(bucket).start_after("bbb.txt").send().await?;

        let contents: Vec<_> = result.contents().iter().filter_map(|obj| obj.key()).collect();
        assert_eq!(contents, vec!["ccc.txt", "ddd.txt"]);

        // Cleanup
        for key in &keys {
            delete_object(c, bucket, key).await?;
        }
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    async fn test_list_objects_v2_continuation_token_with_delimiter(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-continuation-token-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(c, bucket).await?;

        let content = "test";
        let keys = ["a.txt", "dir/1.txt", "z.txt"];
        for key in &keys {
            c.put_object()
                .bucket(bucket)
                .key(*key)
                .body(ByteStream::from_static(content.as_bytes()))
                .send()
                .await?;
        }

        let page1 = c.list_objects_v2().bucket(bucket).delimiter("/").max_keys(2).send().await?;
        assert_eq!(page1.is_truncated(), Some(true));
        assert_eq!(page1.contents().iter().filter_map(|obj| obj.key()).collect::<Vec<_>>(), vec!["a.txt"]);
        assert_eq!(
            page1
                .common_prefixes()
                .iter()
                .filter_map(|prefix| prefix.prefix())
                .collect::<Vec<_>>(),
            vec!["dir/"]
        );
        let next_token = page1
            .next_continuation_token()
            .expect("truncated v2 listing should include next_continuation_token");

        let page2 = c
            .list_objects_v2()
            .bucket(bucket)
            .delimiter("/")
            .max_keys(2)
            .continuation_token(next_token)
            .send()
            .await?;

        assert_eq!(page2.is_truncated(), Some(false));
        assert_eq!(page2.contents().iter().filter_map(|obj| obj.key()).collect::<Vec<_>>(), vec!["z.txt"]);
        assert!(
            page2.common_prefixes().is_empty(),
            "continuation token should advance past the previous common prefix"
        );

        for key in &keys {
            delete_object(c, bucket, key).await?;
        }
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    /// Regression test for <https://github.com/s3s-project/s3s/issues/112>
    ///
    /// Prefix matching must use string comparison, not `Path::starts_with` which is stricter.
    /// For example, prefix "dir/sub" should match key "dir/subdir/file.txt".
    async fn test_list_objects_v2_prefix_string_matching(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-prefix-match-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(c, bucket).await?;

        let content = "test";
        let keys = ["dir/subdir/file1.txt", "dir/subother/file2.txt", "dir/other/file3.txt"];
        for key in &keys {
            c.put_object()
                .bucket(bucket)
                .key(*key)
                .body(ByteStream::from_static(content.as_bytes()))
                .send()
                .await?;
        }

        // Prefix "dir/sub" should match "dir/subdir/..." and "dir/subother/..."
        // but NOT "dir/other/..."
        // Path::starts_with would fail here because it requires component boundaries
        let result = c.list_objects_v2().bucket(bucket).prefix("dir/sub").send().await?;

        let contents: Vec<_> = result.contents().iter().filter_map(|obj| obj.key()).collect();
        assert_eq!(contents.len(), 2, "Expected 2 objects matching prefix 'dir/sub', got {contents:?}");
        assert!(contents.contains(&"dir/subdir/file1.txt"));
        assert!(contents.contains(&"dir/subother/file2.txt"));

        // Cleanup
        for key in &keys {
            delete_object(c, bucket, key).await?;
        }
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    async fn test_list_objects_v2_continuation_token_pagination(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-continuation-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(c, bucket).await?;

        let keys = ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"];
        for key in &keys {
            c.put_object()
                .bucket(bucket)
                .key(*key)
                .body(ByteStream::from_static(b"x"))
                .send()
                .await?;
        }

        // Walk all pages with max_keys=2
        let mut all_keys: Vec<String> = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut req = c.list_objects_v2().bucket(bucket).max_keys(2);
            if let Some(t) = &token {
                req = req.continuation_token(t.clone());
            }
            let page = req.send().await?;

            all_keys.extend(page.contents().iter().filter_map(|o| o.key().map(String::from)));

            if page.is_truncated() != Some(true) {
                break;
            }
            token = page.next_continuation_token().map(String::from);
            assert!(token.is_some(), "is_truncated is true but next_continuation_token is missing");
        }

        assert_eq!(all_keys, vec!["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"]);

        // Cleanup
        for key in &keys {
            delete_object(c, bucket, key).await?;
        }
        delete_bucket(c, bucket).await?;

        Ok(())
    }

    /// When both `continuation_token` and `start_after` are present, the stricter
    /// (larger) bound wins so we never re-list keys the caller already skipped.
    async fn test_list_objects_v2_continuation_token_and_start_after_uses_max(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-ct-max-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(c, bucket).await?;

        let keys = ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"];
        for key in &keys {
            c.put_object()
                .bucket(bucket)
                .key(*key)
                .body(ByteStream::from_static(b"x"))
                .send()
                .await?;
        }

        // start_after="d.txt" is larger than continuation_token="b.txt", so we resume after d.txt
        let result = c
            .list_objects_v2()
            .bucket(bucket)
            .continuation_token("b.txt")
            .start_after("d.txt")
            .send()
            .await?;
        let result_keys: Vec<_> = result.contents().iter().filter_map(|o| o.key()).collect();
        assert_eq!(result_keys, vec!["e.txt"], "should resume after the larger value (d.txt)");

        // continuation_token="c.txt" is larger than start_after="a.txt", so we resume after c.txt
        let result = c
            .list_objects_v2()
            .bucket(bucket)
            .continuation_token("c.txt")
            .start_after("a.txt")
            .send()
            .await?;
        let result_keys: Vec<_> = result.contents().iter().filter_map(|o| o.key()).collect();
        assert_eq!(result_keys, vec!["d.txt", "e.txt"], "should resume after the larger value (c.txt)");

        // Cleanup
        for key in &keys {
            delete_object(c, bucket, key).await?;
        }

        delete_bucket(c, bucket).await?;

        Ok(())
    }

    /// `max_keys=0` return `is_truncated=false` with no continuation token
    async fn test_list_objects_v2_max_keys_zero(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let bucket = format!("test-max-keys-zero-{}", Uuid::new_v4());
        let bucket = bucket.as_str();
        create_bucket(c, bucket).await?;

        let keys = ["a.txt", "b.txt", "c.txt"];
        for key in &keys {
            c.put_object()
                .bucket(bucket)
                .key(*key)
                .body(ByteStream::from_static(b"x"))
                .send()
                .await?;
        }

        let result = c.list_objects_v2().bucket(bucket).max_keys(0).send().await?;

        assert_eq!(result.is_truncated(), Some(false), "max_keys=0 should not be truncated");
        assert!(
            result.next_continuation_token().is_none(),
            "max_keys=0 should not return a continuation token"
        );
        assert!(result.contents().is_empty(), "max_keys=0 should return no objects");

        // Cleanup
        for key in &keys {
            delete_object(c, bucket, key).await?;
        }
        delete_bucket(c, bucket).await?;

        Ok(())
    }
}
