// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct BuildError {
    #[from]
    kind: BuildErrorKind,
}

#[derive(Debug, thiserror::Error)]
enum BuildErrorKind {
    #[error("Missing field: {field:?}")]
    MissingField { field: &'static str },
    // #[error("BuildError: {source}")]
    // Other { source: StdError },
}

impl BuildError {
    pub(crate) fn missing_field(field: &'static str) -> Self {
        Self {
            kind: BuildErrorKind::MissingField { field },
        }
    }

    // pub(crate) fn other(source: StdError) -> Self {
    //     Self {
    //         kind: BuildErrorKind::Other { source },
    //     }
    // }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_field_formats_and_exposes_source() {
        let err = BuildError::missing_field("bucket");

        assert_eq!(err.to_string(), "Missing field: \"bucket\"");
        assert!(std::error::Error::source(&err).is_none());
    }
}
