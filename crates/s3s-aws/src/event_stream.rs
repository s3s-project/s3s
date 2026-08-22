// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use sync_wrapper::{SyncFuture, SyncWrapper};
use transform_stream::AsyncStream;

type AwsSelectObjectContentEventStream = aws_sdk_s3::primitives::event_stream::EventReceiver<
    aws_sdk_s3::types::SelectObjectContentEventStream,
    aws_sdk_s3::types::error::SelectObjectContentEventStreamError,
>;

pub fn from_aws(src: AwsSelectObjectContentEventStream) -> s3s::dto::SelectObjectContentEventStream {
    let mut src = SyncWrapper::new(src);
    s3s::dto::SelectObjectContentEventStream::new(AsyncStream::new(|mut y| async move {
        loop {
            let recv = SyncFuture::new(src.get_mut().recv());
            let ans = recv.await;
            match ans {
                Ok(Some(ev)) => y.yield_(crate::conv::try_from_aws(ev)).await,
                Ok(None) => break,
                Err(err) => y.yield_err(wrap_sdk_error!(err)).await,
            }
        }
    }))
}
