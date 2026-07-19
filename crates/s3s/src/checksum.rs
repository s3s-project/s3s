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
use crate::crypto::Md5;
use crate::crypto::Sha1;
use crate::crypto::Sha256;
use crate::crypto::Sha512;
use crate::crypto::XxHash3;
use crate::crypto::XxHash64;
use crate::crypto::XxHash128;
use crate::dto::Checksum;

use stdx::default::default;

#[derive(Default)]
pub struct ChecksumHasher {
    pub crc32: Option<Crc32>,
    pub crc32c: Option<Crc32c>,
    pub sha1: Option<Sha1>,
    pub sha256: Option<Sha256>,
    pub sha512: Option<Sha512>,
    pub crc64nvme: Option<Crc64Nvme>,
    pub md5: Option<Md5>,
    pub xxhash64: Option<XxHash64>,
    pub xxhash3: Option<XxHash3>,
    pub xxhash128: Option<XxHash128>,
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
        if let Some(sha512) = &mut self.sha512 {
            sha512.update(data);
        }
        if let Some(crc64nvme) = &mut self.crc64nvme {
            crc64nvme.update(data);
        }
        if let Some(md5) = &mut self.md5 {
            md5.update(data);
        }
        if let Some(xxhash64) = &mut self.xxhash64 {
            xxhash64.update(data);
        }
        if let Some(xxhash3) = &mut self.xxhash3 {
            xxhash3.update(data);
        }
        if let Some(xxhash128) = &mut self.xxhash128 {
            xxhash128.update(data);
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
        if let Some(sha512) = self.sha512 {
            let sum = sha512.finalize();
            ans.checksum_sha512 = Some(Self::base64(sum.as_ref()));
        }
        if let Some(crc64nvme) = self.crc64nvme {
            let sum = crc64nvme.finalize();
            ans.checksum_crc64nvme = Some(Self::base64(&sum));
        }
        if let Some(md5) = self.md5 {
            let sum = md5.finalize();
            ans.checksum_md5 = Some(Self::base64(&sum));
        }
        if let Some(xxhash64) = self.xxhash64 {
            let sum = xxhash64.finalize();
            ans.checksum_xxhash64 = Some(Self::base64(&sum));
        }
        if let Some(xxhash3) = self.xxhash3 {
            let sum = xxhash3.finalize();
            ans.checksum_xxhash3 = Some(Self::base64(&sum));
        }
        if let Some(xxhash128) = self.xxhash128 {
            let sum = xxhash128.finalize();
            ans.checksum_xxhash128 = Some(Self::base64(&sum));
        }
        ans
    }

    fn base64(input: &[u8]) -> String {
        base64_simd::STANDARD.encode_to_string(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_hasher_no_checksums() {
        let hasher = ChecksumHasher::default();
        let checksum = hasher.finalize();
        assert!(checksum.checksum_crc32.is_none());
        assert!(checksum.checksum_crc32c.is_none());
        assert!(checksum.checksum_sha1.is_none());
        assert!(checksum.checksum_sha256.is_none());
        assert!(checksum.checksum_sha512.is_none());
        assert!(checksum.checksum_crc64nvme.is_none());
        assert!(checksum.checksum_md5.is_none());
        assert!(checksum.checksum_xxhash64.is_none());
        assert!(checksum.checksum_xxhash3.is_none());
        assert!(checksum.checksum_xxhash128.is_none());
    }

    #[test]
    fn crc32_only() {
        let mut hasher = ChecksumHasher {
            crc32: Some(Crc32::new()),
            ..Default::default()
        };
        hasher.update(b"hello");
        let checksum = hasher.finalize();
        assert!(checksum.checksum_crc32.is_some());
        assert!(checksum.checksum_crc32c.is_none());
        assert!(checksum.checksum_sha1.is_none());
        assert!(checksum.checksum_sha256.is_none());
        assert!(checksum.checksum_crc64nvme.is_none());
    }

    #[test]
    fn crc32c_only() {
        let mut hasher = ChecksumHasher {
            crc32c: Some(Crc32c::new()),
            ..Default::default()
        };
        hasher.update(b"hello");
        let checksum = hasher.finalize();
        assert!(checksum.checksum_crc32.is_none());
        assert!(checksum.checksum_crc32c.is_some());
    }

    #[test]
    fn sha1_only() {
        let mut hasher = ChecksumHasher {
            sha1: Some(Sha1::new()),
            ..Default::default()
        };
        hasher.update(b"hello");
        let checksum = hasher.finalize();
        assert!(checksum.checksum_sha1.is_some());
    }

    #[test]
    fn sha256_only() {
        let mut hasher = ChecksumHasher {
            sha256: Some(Sha256::new()),
            ..Default::default()
        };
        hasher.update(b"hello");
        let checksum = hasher.finalize();
        assert!(checksum.checksum_sha256.is_some());
    }

    #[test]
    fn crc64nvme_only() {
        let mut hasher = ChecksumHasher {
            crc64nvme: Some(Crc64Nvme::new()),
            ..Default::default()
        };
        hasher.update(b"hello");
        let checksum = hasher.finalize();
        assert!(checksum.checksum_crc64nvme.is_some());
    }

    #[test]
    fn all_checksums() {
        let mut hasher = ChecksumHasher {
            crc32: Some(Crc32::new()),
            crc32c: Some(Crc32c::new()),
            sha1: Some(Sha1::new()),
            sha256: Some(Sha256::new()),
            sha512: Some(Sha512::new()),
            crc64nvme: Some(Crc64Nvme::new()),
            md5: Some(Md5::new()),
            xxhash64: Some(XxHash64::new()),
            xxhash3: Some(XxHash3::new()),
            xxhash128: Some(XxHash128::new()),
        };
        hasher.update(b"hello");
        let checksum = hasher.finalize();
        assert!(checksum.checksum_crc32.is_some());
        assert!(checksum.checksum_crc32c.is_some());
        assert!(checksum.checksum_sha1.is_some());
        assert!(checksum.checksum_sha256.is_some());
        assert!(checksum.checksum_sha512.is_some());
        assert!(checksum.checksum_crc64nvme.is_some());
        assert!(checksum.checksum_md5.is_some());
        assert!(checksum.checksum_xxhash64.is_some());
        assert!(checksum.checksum_xxhash3.is_some());
        assert!(checksum.checksum_xxhash128.is_some());
    }

    #[test]
    fn all_checksums_match_individual_digest_outputs() {
        let input = b"hello";

        let mut hasher = ChecksumHasher {
            crc32: Some(Crc32::new()),
            crc32c: Some(Crc32c::new()),
            sha1: Some(Sha1::new()),
            sha256: Some(Sha256::new()),
            sha512: Some(Sha512::new()),
            crc64nvme: Some(Crc64Nvme::new()),
            md5: Some(Md5::new()),
            xxhash64: Some(XxHash64::new()),
            xxhash3: Some(XxHash3::new()),
            xxhash128: Some(XxHash128::new()),
        };

        hasher.update(input);
        let checksum = hasher.finalize();

        assert_eq!(checksum.checksum_crc32, Some(ChecksumHasher::base64(&Crc32::checksum(input))));
        assert_eq!(checksum.checksum_crc32c, Some(ChecksumHasher::base64(&Crc32c::checksum(input))));
        assert_eq!(checksum.checksum_sha1, Some(ChecksumHasher::base64(Sha1::checksum(input).as_ref())));
        assert_eq!(checksum.checksum_sha256, Some(ChecksumHasher::base64(Sha256::checksum(input).as_ref())));
        assert_eq!(checksum.checksum_sha512, Some(ChecksumHasher::base64(Sha512::checksum(input).as_ref())));
        assert_eq!(checksum.checksum_crc64nvme, Some(ChecksumHasher::base64(&Crc64Nvme::checksum(input))));
        assert_eq!(checksum.checksum_md5, Some(ChecksumHasher::base64(&Md5::checksum(input))));
        assert_eq!(checksum.checksum_xxhash64, Some(ChecksumHasher::base64(&XxHash64::checksum(input))));
        assert_eq!(checksum.checksum_xxhash3, Some(ChecksumHasher::base64(&XxHash3::checksum(input))));
        assert_eq!(checksum.checksum_xxhash128, Some(ChecksumHasher::base64(&XxHash128::checksum(input))));
    }

    #[test]
    fn sha512_only() {
        let mut hasher = ChecksumHasher {
            sha512: Some(Sha512::new()),
            ..Default::default()
        };
        hasher.update(b"hello");
        let checksum = hasher.finalize();
        assert!(checksum.checksum_sha512.is_some());
    }

    #[test]
    fn md5_only() {
        let mut hasher = ChecksumHasher {
            md5: Some(Md5::new()),
            ..Default::default()
        };
        hasher.update(b"hello");
        let checksum = hasher.finalize();
        assert!(checksum.checksum_md5.is_some());
    }

    #[test]
    fn xxhash64_only() {
        let mut hasher = ChecksumHasher {
            xxhash64: Some(XxHash64::new()),
            ..Default::default()
        };
        hasher.update(b"hello");
        let checksum = hasher.finalize();
        assert!(checksum.checksum_xxhash64.is_some());
    }

    #[test]
    fn xxhash3_only() {
        let mut hasher = ChecksumHasher {
            xxhash3: Some(XxHash3::new()),
            ..Default::default()
        };
        hasher.update(b"hello");
        let checksum = hasher.finalize();
        assert!(checksum.checksum_xxhash3.is_some());
    }

    #[test]
    fn xxhash128_only() {
        let mut hasher = ChecksumHasher {
            xxhash128: Some(XxHash128::new()),
            ..Default::default()
        };
        hasher.update(b"hello");
        let checksum = hasher.finalize();
        assert!(checksum.checksum_xxhash128.is_some());
    }

    #[test]
    fn base64_encoding() {
        // base64 of [0, 1, 2, 3] is "AAECAw=="
        let encoded = ChecksumHasher::base64(&[0, 1, 2, 3]);
        assert_eq!(encoded, "AAECAw==");
    }
}
