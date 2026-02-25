//! Multi-algorithm checksum computation for S3 objects.
//!
//! This module provides [`ChecksumHasher`], which can compute one or more
//! checksums simultaneously in a single pass over the data. The result is
//! a [`crate::dto::Checksum`] struct whose fields are populated with
//! base64-encoded digests for every algorithm that was enabled.

use crate::crypto::Checksum as _;
use crate::crypto::Crc32;
use crate::crypto::Crc32c;
use crate::crypto::Crc64Nvme;
use crate::crypto::Sha1;
use crate::crypto::Sha256;
use crate::dto::Checksum;

use stdx::default::default;

#[derive(Default)]
pub struct ChecksumHasher {
    pub crc32: Option<Crc32>,
    pub crc32c: Option<Crc32c>,
    pub sha1: Option<Sha1>,
    pub sha256: Option<Sha256>,
    pub crc64nvme: Option<Crc64Nvme>,
}

impl ChecksumHasher {
    pub fn update(&mut self, data: &[u8]) {
        if let Some(crc32) = &mut self.crc32 {
            crc32.update(data);
        }
        if let Some(crc32c) = &mut self.crc32c {
            crc32c.update(data);
        }
        if let Some(sha1) = &mut self.sha1 {
            sha1.update(data);
        }
        if let Some(sha256) = &mut self.sha256 {
            sha256.update(data);
        }
        if let Some(crc64nvme) = &mut self.crc64nvme {
            crc64nvme.update(data);
        }
    }

    #[must_use]
    pub fn finalize(self) -> Checksum {
        let mut ans: Checksum = default();
        if let Some(crc32) = self.crc32 {
            let sum = crc32.finalize();
            ans.checksum_crc32 = Some(Self::base64(&sum));
        }
        if let Some(crc32c) = self.crc32c {
            let sum = crc32c.finalize();
            ans.checksum_crc32c = Some(Self::base64(&sum));
        }
        if let Some(sha1) = self.sha1 {
            let sum = sha1.finalize();
            ans.checksum_sha1 = Some(Self::base64(sum.as_ref()));
        }
        if let Some(sha256) = self.sha256 {
            let sum = sha256.finalize();
            ans.checksum_sha256 = Some(Self::base64(sum.as_ref()));
        }
        if let Some(crc64nvme) = self.crc64nvme {
            let sum = crc64nvme.finalize();
            ans.checksum_crc64nvme = Some(Self::base64(&sum));
        }
        ans
    }

    fn base64(input: &[u8]) -> String {
        base64_simd::STANDARD.encode_to_string(input)
    }
}
