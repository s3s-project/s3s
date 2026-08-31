// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use crate::case;
use crate::suite::{Bucket, Essential, REGION, create_bucket, delete_bucket};

use std::sync::Arc;

use aws_sdk_s3::types::BucketLocationConstraint;
use aws_sdk_s3::types::CreateBucketConfiguration;

use s3s_test::Result;
use s3s_test::tcx::TestContext;
use tracing::debug;
use uuid::Uuid;

pub fn register(tcx: &mut TestContext) {
    case!(tcx, FsServer, Bucket, test_list_buckets);
    case!(tcx, FsServerRelaxed, Essential, test_relaxed_bucket_validation);
    case!(tcx, FsServer, Bucket, test_default_bucket_validation);
}

impl Bucket {
    async fn test_list_buckets(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;
        let response1 = c.list_buckets().send().await?;
        drop(response1);

        let bucket1 = format!("test-list-buckets-1-{}", Uuid::new_v4());
        let bucket1_str = bucket1.as_str();
        let bucket2 = format!("test-list-buckets-2-{}", Uuid::new_v4());
        let bucket2_str = bucket2.as_str();

        create_bucket(c, bucket1_str).await?;
        create_bucket(c, bucket2_str).await?;

        let response2 = c.list_buckets().send().await?;
        let bucket_names: Vec<_> = response2.buckets().iter().filter_map(|bucket| bucket.name()).collect();
        assert!(bucket_names.contains(&bucket1_str));
        assert!(bucket_names.contains(&bucket2_str));

        Ok(())
    }

    async fn test_default_bucket_validation(self: Arc<Self>) -> Result<()> {
        let c = &self.s3; // Uses default validation

        // Test with invalid bucket names that should be rejected by AWS rules
        let invalid_bucket_names = [
            "UPPERCASE-BUCKET",       // Uppercase not allowed
            "bucket_with_underscore", // Underscores not allowed
            "bucket..double.dots",    // Consecutive dots not allowed
        ];

        for bucket_name in invalid_bucket_names {
            // Try to create bucket with invalid name - should fail with default validation
            let location = BucketLocationConstraint::from(REGION);
            let cfg = CreateBucketConfiguration::builder().location_constraint(location).build();

            let result = c
                .create_bucket()
                .create_bucket_configuration(cfg)
                .bucket(bucket_name)
                .send()
                .await;

            // Should fail due to bucket name validation
            assert!(result.is_err(), "Expected error for invalid bucket name: {bucket_name}");

            let error_str = format!("{:?}", result.unwrap_err());
            debug!("Default validation rejected bucket name {bucket_name}: {error_str}");
        }

        Ok(())
    }
}

impl Essential {
    async fn test_relaxed_bucket_validation(self: Arc<Self>) -> Result<()> {
        let c = &self.s3;

        // Test with bucket names that should pass with relaxed validation
        let relaxed_bucket_names = [
            "UPPERCASE-BUCKET",       // Uppercase not normally allowed
            "bucket_with_underscore", // Underscores not allowed
        ];

        for bucket_name in relaxed_bucket_names {
            let location = BucketLocationConstraint::from(REGION);
            let cfg = CreateBucketConfiguration::builder().location_constraint(location).build();

            let result = c
                .create_bucket()
                .create_bucket_configuration(cfg)
                .bucket(bucket_name)
                .send()
                .await;

            // Should not fail due to bucket name validation
            match result {
                Ok(_) => {
                    debug!("Successfully created bucket with relaxed validation: {bucket_name}");

                    // Verify the bucket was actually created by checking bucket existence
                    let head_result = c.head_bucket().bucket(bucket_name).send().await;
                    assert!(head_result.is_ok(), "Failed to head bucket {bucket_name} after creation");

                    // Clean up the bucket
                    let delete_result = delete_bucket(c, bucket_name).await;
                    assert!(delete_result.is_ok(), "Failed to delete bucket {bucket_name}");
                }
                Err(e) => {
                    let error_str = format!("{e:?}");
                    debug!("Bucket creation failed for other reasons (expected): {bucket_name} - {error_str}");
                    // Verify it's not a bucket name validation error
                    assert!(!error_str.contains("InvalidBucketName") && !error_str.contains("bucket name"));
                }
            }
        }

        Ok(())
    }
}
