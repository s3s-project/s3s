//! Region-aware S3 service example
//!
//! This example demonstrates how to access region information from S3 requests.
//! The region can be obtained from:
//! - The `S3Request.region` field (extracted from authentication headers)
//! - The `VirtualHost.region` field (extracted from the host header)
//!
//! This is a simple demonstration showing how to log and use region information.
//! In a real application, you could use regions to:
//! - Route requests to region-specific storage backends
//! - Apply region-specific policies or configurations
//! - Log for compliance or audit purposes
//! - Implement multi-region replication strategies

use s3s::dto::{GetObjectInput, GetObjectOutput, ListObjectsV2Input, ListObjectsV2Output};
use s3s::{S3, S3Request, S3Response, S3Result};

/// A region-aware S3 implementation
///
/// This demonstrates how to access region information from S3 requests.
/// In practice, you would use this information to route to different
/// storage backends, apply region-specific logic, etc.
#[derive(Debug, Clone)]
struct RegionAwareS3;

#[async_trait::async_trait]
impl S3 for RegionAwareS3 {
    async fn get_object(&self, req: S3Request<GetObjectInput>) -> S3Result<S3Response<GetObjectOutput>> {
        // Access region from the request
        // This is extracted from the Authorization header or presigned URL
        let region = req.region.as_deref().unwrap_or("unknown");

        println!(
            "GetObject request received:\n  Bucket: {}\n  Key: {}\n  Region: {}",
            req.input.bucket, req.input.key, region
        );

        // You can use the region to route to different storage backends,
        // apply region-specific policies, or log for compliance purposes
        match region {
            "us-east-1" => {
                println!("  -> Using US East 1 storage backend");
            }
            "eu-west-2" => {
                println!("  -> Using EU West 2 storage backend");
            }
            _ => {
                println!("  -> Using default storage backend for region: {}", region);
            }
        }

        Err(s3s::s3_error!(NoSuchKey, "Object not found"))
    }

    async fn list_objects_v2(
        &self,
        req: S3Request<ListObjectsV2Input>,
    ) -> S3Result<S3Response<ListObjectsV2Output>> {
        let region = req.region.as_deref().unwrap_or("unknown");
        let service = req.service.as_deref().unwrap_or("unknown");

        println!(
            "ListObjectsV2 request received:\n  Bucket: {}\n  Region: {}\n  Service: {}",
            req.input.bucket, region, service
        );

        // Create an empty list response
        let output = ListObjectsV2Output {
            name: Some(req.input.bucket.clone()),
            ..Default::default()
        };

        Ok(S3Response::new(output))
    }
}

fn main() {
    println!("Region-Aware S3 Implementation Example");
    println!("=====================================\n");
    println!("This example demonstrates how to access region information from S3 requests.");
    println!("\nRegion information is available in two places:");
    println!("1. S3Request.region - Extracted from Authorization header or presigned URL");
    println!("2. VirtualHost.region - Extracted from the host header (e.g., s3.us-east-1.amazonaws.com)\n");
    println!("In practice, you would:");
    println!("- Use regions to route to region-specific storage backends");
    println!("- Apply region-specific policies or configurations");
    println!("- Log for compliance or audit purposes");
    println!("- Implement multi-region replication strategies\n");
    println!("Example usage in an S3 handler:");
    println!("  let region = req.region.as_deref().unwrap_or(\"unknown\");");
    println!("  match region {{");
    println!("      \"us-east-1\" => /* use US East backend */,");
    println!("      \"eu-west-2\" => /* use EU West backend */,");
    println!("      _ => /* use default backend */,");
    println!("  }}");
}

