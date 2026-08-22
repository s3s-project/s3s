// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

pub use s3s_model::smithy::*;

pub trait SmithyTraitsExt {
    #[doc(hidden)]
    fn base(&self) -> &Traits;

    fn minio(&self) -> bool {
        self.base().get("s3s#minio").is_some()
    }

    fn sealed(&self) -> bool {
        self.base().get("s3s#sealed").is_some()
    }
}

impl SmithyTraitsExt for Traits {
    fn base(&self) -> &Traits {
        self
    }
}
