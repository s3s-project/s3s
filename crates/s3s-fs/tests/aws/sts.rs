// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use crate::case;
use crate::suite::Sts;

use std::sync::Arc;

use s3s_test::Result;
use s3s_test::tcx::TestContext;
use tracing::debug;

pub fn register(tcx: &mut TestContext) {
    case!(tcx, FsServer, Sts, test_sts_assume_role_not_implemented);
}

impl Sts {
    async fn test_sts_assume_role_not_implemented(self: Arc<Self>) -> Result<()> {
        let sts_client = &self.sts;

        // Attempt to call AssumeRole - should fail with NotImplemented
        let result = sts_client
            .assume_role()
            .role_arn("arn:aws:iam::123456789012:role/test-role")
            .role_session_name("test-session")
            .send()
            .await;

        // Verify the operation returned an error
        assert!(result.is_err(), "Expected AssumeRole to fail with NotImplemented error");

        // Check that the error is NotImplemented
        let error = result.unwrap_err();
        let error_str = format!("{error:?}");
        debug!("AssumeRole error (expected): {error_str}");

        // The error should contain "NotImplemented" or similar indication
        assert!(
            error_str.contains("NotImplemented") || error_str.contains("not implemented"),
            "Expected NotImplemented error, got: {error_str}"
        );

        Ok(())
    }
}
