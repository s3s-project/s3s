//! `PostResponse` structure for S3 POST object uploads
//!
//! See: <https://docs.aws.amazon.com/AmazonS3/latest/API/RESTObjectPOST.html>

use crate::dto::ETag;
use crate::xml::{SerResult, Serialize, Serializer};

/// Response returned for POST object uploads when `success_action_status` is 200 or 201
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostResponse {
    /// Location of the uploaded object
    pub location: String,
    /// Bucket name
    pub bucket: String,
    /// Object key
    pub key: String,
    /// `ETag` of the uploaded object
    pub e_tag: ETag,
}

impl Serialize for PostResponse {
    fn serialize<W: std::io::Write>(&self, s: &mut Serializer<W>) -> SerResult {
        s.element("PostResponse", |s| {
            s.content("Location", &self.location)?;
            s.content("Bucket", &self.bucket)?;
            s.content("Key", &self.key)?;
            s.content("ETag", &self.e_tag)?;
            Ok(())
        })?;
        Ok(())
    }
}
