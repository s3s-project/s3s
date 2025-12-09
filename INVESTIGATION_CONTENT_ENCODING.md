# Content-Encoding Issue Investigation Report

## Issue Reference
- **Related Issue**: [rustfs/rustfs#1062](https://github.com/rustfs/rustfs/issues/1062)
- **s3s Issue**: #419

## Executive Summary

The `Content-Encoding` header (and other standard object attributes) are **not being preserved** when objects are stored and retrieved using `s3s-fs`. This is **NOT a bug in the s3s core library**, but rather **a limitation in the s3s-fs implementation** that affects all S3 backends built with s3s that don't explicitly handle these attributes.

## Problem Description

When a user uploads an object with:
```
PUT /bucket/key
Content-Encoding: br
Content-Type: application/json
Content-Disposition: attachment; filename="data.json"
Cache-Control: max-age=3600
```

And then retrieves it with:
```
GET /bucket/key
```

The response **does not include** the original `Content-Encoding`, `Content-Type`, `Content-Disposition`, or `Cache-Control` headers. Instead:
- These headers are missing or have default values
- The user may incorrectly believe these headers were converted to user metadata (`x-amz-meta-*`)

## Root Cause Analysis

### What Works Correctly (s3s Core)

The s3s core library **correctly handles** all standard HTTP headers:

1. **PutObject deserialization** (`crates/s3s/src/ops/generated.rs`):
   - Line ~5327: Parses `Content-Encoding` from the HTTP `Content-Encoding` header
   - Populates the `PutObjectInput.content_encoding` field
   - Similarly handles content_type, content_disposition, cache_control, expires, etc.

2. **GetObject serialization** (`crates/s3s/src/ops/generated.rs`):
   - Line 3030: Adds `Content-Encoding` to the HTTP response headers
   - Line 3040: Separately adds user metadata (x-amz-meta-*) headers
   - No code converts standard headers to user metadata

3. **Separation of concerns**:
   - Standard headers are in dedicated fields (e.g., `content_encoding: Option<ContentEncoding>`)
   - User metadata is in the `metadata: Option<Metadata>` field
   - These are completely separate in the data model

### What Doesn't Work (s3s-fs Implementation)

The s3s-fs implementation **only persists user metadata**, not standard object attributes:

1. **PutObject** (`crates/s3s-fs/src/s3.rs`, lines 461-608):
   ```rust
   async fn put_object(&self, req: S3Request<PutObjectInput>) -> ... {
       // ... file writing logic ...
       
       // Line 590-592: ONLY saves user metadata
       if let Some(ref metadata) = metadata {
           self.save_metadata(&bucket, &key, metadata, None).await?;
       }
       
       // Missing: No code to save content_encoding, content_type, etc.
   }
   ```

2. **GetObject** (`crates/s3s-fs/src/s3.rs`, lines 200-262):
   ```rust
   async fn get_object(&self, req: S3Request<GetObjectInput>) -> ... {
       // ... file reading logic ...
       
       // Line 235: Load only user metadata
       let object_metadata = self.load_metadata(&input.bucket, &input.key, None).await?;
       
       // Line 247-260: Create output with ONLY user metadata
       let output = GetObjectOutput {
           // ... other fields ...
           metadata: object_metadata,  // Only user metadata
           ..Default::default()
           // Missing: content_encoding, content_type, content_disposition, etc.
       };
   }
   ```

3. **HeadObject** (`crates/s3s-fs/src/s3.rs`, lines 277-302):
   - Line 291: Has a TODO comment about detecting content type
   - Line 292: Defaults to "application/octet-stream"
   - Same issue: doesn't return stored attributes

### Data Flow

```
Upload (PutObject):
HTTP Request → s3s core (✓ parses correctly) → s3s-fs (✗ doesn't store)
                                                          ↓
                                                   Only stores user metadata

Download (GetObject):
s3s-fs (✗ can't retrieve) → s3s core (✓ would serialize correctly) → HTTP Response
        ↑
Only loads user metadata
```

## Affected Standard Attributes

All of these fields from `PutObjectInput` are affected:
- `content_encoding` - **Primary issue reported**
- `content_type`
- `content_disposition`
- `content_language`
- `cache_control`
- `expires`
- `website_redirect_location`
- And potentially others

## Impact

1. **Immediate**: Objects lose important metadata upon storage
2. **Compatibility**: Breaks compatibility with AWS S3 and MinIO behavior
3. **Client Impact**: Clients expecting `Content-Encoding: br` may fail to decompress objects
4. **Web Applications**: Content-Type, Cache-Control, etc. are critical for web serving
5. **Scope**: Affects **all S3 implementations using s3s** that don't explicitly handle these attributes

## Test Case

A test case has been added to `crates/s3s-fs/tests/it_aws.rs::test_content_encoding_preservation` that demonstrates the issue:

```rust
// Upload with headers
c.put_object()
    .bucket(bucket)
    .key(key)
    .body(body)
    .content_encoding("br")
    .content_type("application/json")
    .content_disposition("attachment; filename=\"data.json\"")
    .cache_control("max-age=3600")
    .send()
    .await?;

// Retrieve - headers will be missing/None
let ans = c.get_object().bucket(bucket).key(key).send().await?;
assert!(ans.content_encoding().is_none());  // BUG: Should be "br"
assert!(ans.content_type().is_none());       // BUG: Should be "application/json"
```

## Recommended Solution

### For s3s-fs

1. **Extend metadata storage structure**:
   ```rust
   #[derive(Serialize, Deserialize)]
   struct ObjectMetadata {
       // User-defined metadata (x-amz-meta-*)
       user_metadata: Option<HashMap<String, String>>,
       
       // Standard object attributes
       content_encoding: Option<String>,
       content_type: Option<String>,
       content_disposition: Option<String>,
       content_language: Option<String>,
       cache_control: Option<String>,
       expires: Option<String>,
       website_redirect_location: Option<String>,
   }
   ```

2. **Update put_object** to save all attributes:
   ```rust
   let obj_meta = ObjectMetadata {
       user_metadata: input.metadata,
       content_encoding: input.content_encoding,
       content_type: input.content_type,
       // ... etc
   };
   self.save_object_metadata(&bucket, &key, &obj_meta, None).await?;
   ```

3. **Update get_object and head_object** to return all attributes:
   ```rust
   let obj_meta = self.load_object_metadata(&bucket, &key, None).await?;
   let output = GetObjectOutput {
       metadata: obj_meta.user_metadata,
       content_encoding: obj_meta.content_encoding,
       content_type: obj_meta.content_type,
       // ... etc
   };
   ```

4. **Ensure backward compatibility**:
   - When loading old metadata files (only user metadata), treat them gracefully
   - Consider a migration strategy for existing deployments

### For s3s Users

When implementing your own S3 backend using s3s, you **must** persist and retrieve these fields from `PutObjectInput`:
- All standard HTTP headers (content_encoding, content_type, etc.)
- Not just user metadata (metadata field)

### Documentation

Add a section to the s3s documentation explaining:
1. Which fields from operation inputs must be persisted
2. The difference between user metadata and standard attributes
3. Example implementation showing proper attribute storage

## Why This Wasn't Caught Earlier

1. **User metadata works**: Since user metadata (x-amz-meta-*) is stored/retrieved correctly, basic S3 functionality appears to work
2. **Not in common tests**: Many S3 tests focus on object content, not metadata preservation
3. **Implementation-specific**: Each backend (s3s-fs, custom implementations) must handle this separately

## Conclusion

This is a **straightforward implementation gap** in s3s-fs, not a fundamental design issue with s3s. The fix is well-defined:
1. Store standard object attributes alongside user metadata
2. Return these attributes in GetObject and HeadObject responses
3. Maintain backward compatibility with existing stored objects

The s3s core library is working correctly and doesn't need any changes.

## References

- AWS S3 PutObject API: https://docs.aws.amazon.com/AmazonS3/latest/API/API_PutObject.html
- AWS S3 GetObject API: https://docs.aws.amazon.com/AmazonS3/latest/API/API_GetObject.html
- HTTP Content-Encoding: https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Content-Encoding
- rustfs Issue #1062: https://github.com/rustfs/rustfs/issues/1062
