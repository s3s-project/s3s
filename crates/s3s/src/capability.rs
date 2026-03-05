use std::collections::BTreeSet;

/// A semantic feature that a backend can explicitly declare support for.
///
/// Each variant represents a group of S3 semantics that changes the observable
/// behavior of an operation. Backends that do not override [`S3::capabilities`]
/// are treated as supporting none of these features (i.e., [`Capabilities::empty()`]).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Capability {
    /// `GetObject` and `HeadObject` with a `partNumber` query parameter.
    ///
    /// When set, the server must return only the specified part of a multipart
    /// object instead of the entire object.
    GetObjectPartNumber,
}

/// A set of [`Capability`] values declared by an S3 backend.
#[derive(Debug, Clone, Default)]
pub struct Capabilities(BTreeSet<Capability>);

impl Capabilities {
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn with(mut self, cap: Capability) -> Self {
        self.0.insert(cap);
        self
    }

    #[must_use]
    pub fn contains(&self, cap: Capability) -> bool {
        self.0.contains(&cap)
    }

    #[must_use]
    pub fn missing(&self, required: &Self) -> Vec<Capability> {
        required.0.iter().filter(|c| !self.0.contains(c)).copied().collect()
    }
}

/// Check whether the backend supports the required capabilities.
///
/// # Errors
///
/// Returns an error if `supported` does not include all capabilities
/// listed in `required`.
pub fn check(required: &Capabilities, supported: &Capabilities) -> crate::error::S3Result<()> {
    let missing = supported.missing(required);
    if missing.is_empty() {
        return Ok(());
    }
    Err(crate::s3_error!(
        NotImplemented,
        "Backend does not declare support for capabilities: {:?}",
        missing
    ))
}
