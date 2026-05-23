//! Backend implementations. Each backend implements the apotheca
//! operations against a particular substrate (local filesystem, S3,
//! ...). `Cella` (in `crate`) dispatches its public methods to the
//! appropriate backend variant.

pub(crate) mod local;

#[cfg(feature = "backend-s3")]
pub(crate) mod s3;

/// Internal backend dispatch. Public methods on `Cella` match on this
/// enum to route to the backend's implementation.
pub(crate) enum Backend {
    Local(local::LocalBackend),
    #[cfg(feature = "backend-s3")]
    S3(s3::S3Backend),
}
