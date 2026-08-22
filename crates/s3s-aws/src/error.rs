// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 Nugine

macro_rules! wrap_sdk_error {
    ($e:expr) => {{
        use aws_sdk_s3::error::SdkError;
        use aws_sdk_s3::operation::RequestId;
        use s3s::{S3Error, S3ErrorCode};

        let mut err = S3Error::new(S3ErrorCode::InternalError);
        let source = $e;
        tracing::debug!("sdk error: {:?}", source);

        if let SdkError::ServiceError(ref e) = source {
            let meta = e.err().meta();
            if let Some(val) = meta.code().and_then(|s| S3ErrorCode::from_bytes(s.as_bytes())) {
                err.set_code(val);
            }
            if let Some(val) = meta.message() {
                err.set_message(val.to_owned());
            }
            if let Some(val) = meta.request_id() {
                err.set_request_id(val);
            }
            crate::error::SetStatusCode(&mut err, e).call();
        }
        err.set_source(Box::new(source));

        err
    }};
}

// FIXME: this is actually an overloaded function

pub struct SetStatusCode<'a, 'b, E, R>(
    pub &'a mut s3s::S3Error,
    pub &'b aws_smithy_runtime_api::client::result::ServiceError<E, R>,
);

impl<E> SetStatusCode<'_, '_, E, aws_smithy_runtime_api::client::orchestrator::HttpResponse> {
    pub fn call(self) {
        let Self(err, e) = self;
        err.set_status_code(hyper_status_code_from_aws(e.raw().status()));
        // TODO: headers?
    }
}

impl<E> SetStatusCode<'_, '_, E, aws_smithy_types::event_stream::RawMessage> {
    #[allow(clippy::unused_self)]
    pub fn call(self) {}
}

fn hyper_status_code_from_aws(status_code: aws_smithy_runtime_api::http::StatusCode) -> hyper::StatusCode {
    hyper::StatusCode::from_u16(status_code.as_u16()).unwrap()
}
