//! apotheca — named write-once store. See SPEC.md for the protocol.
//!
//! Public surface: `Cella` with five operations on the depositum
//! namespace (`deposit`, `deposit_cas`, `get`, `stat`) and two on the
//! pinax namespace (`get_pinax`, `set_pinax`). Conformance: v1.0-rc2
//! on both surfaces.
//!
//! Backends: local-filesystem (default) and S3-compatible (feature
//! `backend-s3`). The local backend stays dependency-free; remote
//! backends pull in `object_store` + tokio via Cargo features.

mod backends;
mod meta;
mod name;

#[cfg(feature = "backend-s3")]
mod runtime;

pub use meta::{Meta, MetaParseError};
pub use name::{Name, NameError};

#[cfg(feature = "backend-s3")]
pub use backends::s3::{S3Config, S3OpenError};

use std::io;
use std::path::Path;

/// SHA-256 digest, 32 octets (SPEC §1.5).
pub type Digest256 = [u8; 32];

/// Outcome of `Cella::deposit` (SPEC §2.1) and `Cella::deposit_cas`
/// (SPEC §2.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepositOutcome {
    /// `name` was absent (now stored), or was present with bytes whose
    /// digest equals sha256(bytes) — an idempotent re-deposit.
    Ok,
    /// `name` is present with bytes whose digest differs from
    /// sha256(bytes). The stored bytes are unchanged. Reachable from
    /// both `deposit` and `deposit_cas` (SPEC §2.1, §2.6 both
    /// mandate detection); `deposit_cas` does the check on its
    /// `AlreadyExists` branch rather than pre-emptively.
    Collision,
}

/// Outcome of `Cella::set_pinax` (SPEC §2.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetPinaxOutcome {
    /// Precondition held; bytes are now stored under `name` (or were
    /// already stored with the matching digest, idempotent re-set).
    Ok,
    /// Precondition did not hold. `actual = None` reports absent;
    /// `actual = Some(d)` reports present with stored digest `d`.
    Conflict { actual: Option<Digest256> },
}

#[derive(Debug)]
pub enum DepositError {
    InvalidName(NameError),
    MalformedMeta,
    Io(io::Error),
}

#[derive(Debug)]
pub enum GetError {
    InvalidName(NameError),
    NotFound,
    IntegrityError,
    MalformedMeta,
    Io(io::Error),
}

#[derive(Debug)]
pub enum StatError {
    InvalidName(NameError),
    NotFound,
    MalformedMeta,
    Io(io::Error),
}

#[derive(Debug)]
pub enum GetPinaxError {
    InvalidName(NameError),
    NotFound,
    IntegrityError,
    Io(io::Error),
}

#[derive(Debug)]
pub enum SetPinaxError {
    InvalidName(NameError),
    Io(io::Error),
}

impl std::fmt::Display for DepositError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DepositError::InvalidName(e) => write!(f, "invalid name: {e}"),
            DepositError::MalformedMeta => f.write_str("malformed meta file in cella"),
            DepositError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::fmt::Display for GetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GetError::InvalidName(e) => write!(f, "invalid name: {e}"),
            GetError::NotFound => f.write_str("not found"),
            GetError::IntegrityError => {
                f.write_str("integrity error: stored digest does not match bytes")
            }
            GetError::MalformedMeta => f.write_str("malformed meta file in cella"),
            GetError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::fmt::Display for StatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StatError::InvalidName(e) => write!(f, "invalid name: {e}"),
            StatError::NotFound => f.write_str("not found"),
            StatError::MalformedMeta => f.write_str("malformed meta file in cella"),
            StatError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::fmt::Display for GetPinaxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GetPinaxError::InvalidName(e) => write!(f, "invalid name: {e}"),
            GetPinaxError::NotFound => f.write_str("not found"),
            GetPinaxError::IntegrityError => {
                f.write_str("integrity error: stored digest does not match bytes")
            }
            GetPinaxError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::fmt::Display for SetPinaxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetPinaxError::InvalidName(e) => write!(f, "invalid name: {e}"),
            SetPinaxError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DepositError {}
impl std::error::Error for GetError {}
impl std::error::Error for StatError {}
impl std::error::Error for GetPinaxError {}
impl std::error::Error for SetPinaxError {}

/// A cella: an apotheca storage instance over one backend.
pub struct Cella {
    backend: backends::Backend,
}

impl Cella {
    /// Open (or create) a local-filesystem cella rooted at `root`
    /// (SPEC §6.1). Idempotent: ensures the `deposita/`, `pinakes/`,
    /// and `tmp/` subdirectories exist.
    pub fn open<P: AsRef<Path>>(root: P) -> io::Result<Self> {
        Ok(Cella {
            backend: backends::Backend::Local(backends::local::LocalBackend::open(root)?),
        })
    }

    /// Open an S3-compatible cella (AWS S3, Cloudflare R2, MinIO).
    #[cfg(feature = "backend-s3")]
    pub fn open_s3(config: S3Config) -> Result<Self, S3OpenError> {
        Ok(Cella {
            backend: backends::Backend::S3(backends::s3::S3Backend::open(config)?),
        })
    }

    /// SPEC §2.1. Store `bytes` under `name`; reject differing bytes
    /// under an existing name as `Collision`.
    pub fn deposit(
        &self,
        name: &Name<'_>,
        bytes: &[u8],
    ) -> Result<DepositOutcome, DepositError> {
        match &self.backend {
            backends::Backend::Local(b) => b.deposit(name, bytes),
            #[cfg(feature = "backend-s3")]
            backends::Backend::S3(b) => b.deposit(name, bytes),
        }
    }

    /// SPEC §2.6. Fast-path for content-addressed callers. The caller
    /// asserts that if a depositum is present under `name`, its bytes
    /// equal `bytes`; apotheca skips the pre-PUT existence read.
    ///
    /// `Collision` detection is mandatory (SPEC §2.6) and reachable
    /// here just as from `deposit`: the local backend detects by
    /// delegating to `deposit`'s read-then-decide path, the S3
    /// backend by issuing a HEAD on `AlreadyExists`. The freedom
    /// `deposit_cas` retains over `deposit` is on the pre-emptive
    /// existence read, not on whether to detect collisions.
    ///
    /// The hash function used to derive `name` is opaque to apotheca:
    /// callers MAY use any hash family (e.g. syntheca uses BLAKE3 for
    /// names while apotheca's own integrity field stays SHA-256).
    pub fn deposit_cas(
        &self,
        name: &Name<'_>,
        bytes: &[u8],
    ) -> Result<DepositOutcome, DepositError> {
        match &self.backend {
            backends::Backend::Local(b) => b.deposit_cas(name, bytes),
            #[cfg(feature = "backend-s3")]
            backends::Backend::S3(b) => b.deposit_cas(name, bytes),
        }
    }

    /// SPEC §2.2. Returns the bytes under `name`, verified against the
    /// stored sha256.
    pub fn get(&self, name: &Name<'_>) -> Result<Vec<u8>, GetError> {
        match &self.backend {
            backends::Backend::Local(b) => b.get(name),
            #[cfg(feature = "backend-s3")]
            backends::Backend::S3(b) => b.get(name),
        }
    }

    /// SPEC §2.3. Returns the depositum's metadata (size + sha256)
    /// without transferring bytes.
    pub fn stat(&self, name: &Name<'_>) -> Result<Meta, StatError> {
        match &self.backend {
            backends::Backend::Local(b) => b.stat(name),
            #[cfg(feature = "backend-s3")]
            backends::Backend::S3(b) => b.stat(name),
        }
    }

    /// SPEC §2.4. Returns the pinax bytes under `name`, verified.
    pub fn get_pinax(&self, name: &Name<'_>) -> Result<Vec<u8>, GetPinaxError> {
        match &self.backend {
            backends::Backend::Local(b) => b.get_pinax(name),
            #[cfg(feature = "backend-s3")]
            backends::Backend::S3(b) => b.get_pinax(name),
        }
    }

    /// SPEC §2.5. Compare-and-swap pinax write: store `bytes` only
    /// when the stored digest equals `expected` (or, with
    /// `expected = None`, when `name` is absent).
    pub fn set_pinax(
        &self,
        name: &Name<'_>,
        bytes: &[u8],
        expected: Option<Digest256>,
    ) -> Result<SetPinaxOutcome, SetPinaxError> {
        match &self.backend {
            backends::Backend::Local(b) => b.set_pinax(name, bytes, expected),
            #[cfg(feature = "backend-s3")]
            backends::Backend::S3(b) => b.set_pinax(name, bytes, expected),
        }
    }
}

/// Shared SHA-256 helper used by backends.
pub(crate) fn sha256(bytes: &[u8]) -> Digest256 {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}
