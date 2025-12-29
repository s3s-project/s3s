#![cfg_attr(docsrs, feature(doc_cfg))]

use opendal::Operator;

/// An S3 backend backed by an `OpenDAL` [`Operator`].
///
/// This crate provides a thin adapter type that implements [`s3s::S3`]. By default, all S3
/// operations return `501 Not Implemented`. Specific operations will be implemented incrementally.
#[derive(Clone)]
pub struct Opendal {
    operator: Operator,
}

impl Opendal {
    #[must_use]
    pub fn new(operator: Operator) -> Self {
        Self { operator }
    }

    #[must_use]
    pub fn operator(&self) -> &Operator {
        &self.operator
    }

    #[must_use]
    pub fn into_operator(self) -> Operator {
        self.operator
    }
}

impl s3s::S3 for Opendal {}
