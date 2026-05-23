//! S3-compatible backend (AWS S3, Cloudflare R2, MinIO) via Apache
//! Arrow's `object_store` crate.
//!
//! Key layout under the cella's bucket prefix:
//! ```text
//! <prefix>/deposita/<name>     # depositum bytes
//! <prefix>/pinakes/<name>      # pinax bytes
//! ```
//!
//! Both kinds carry the depositum's sha256 in
//! `x-amz-meta-apotheca-checksum` user metadata (SPEC §3.3). The
//! native `x-amz-checksum-sha256` field is also set on PUT, as
//! defense-in-depth, by `with_checksum_algorithm(Checksum::SHA256)`
//! on the underlying builder.
//!
//! No `tmp/` staging: atomicity is the conditional put itself
//! (`PutMode::Create` for deposita; `PutMode::Update(UpdateVersion)`
//! for pinax CAS). Failed puts leave no residue.

use std::borrow::Cow;
use std::sync::Arc;

use bytes::Bytes;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjectPath;
use object_store::{
    Attribute, Attributes, GetOptions, ObjectStore, PutMode, PutOptions, PutPayload, UpdateVersion,
};
use tokio::runtime::Handle;

use crate::{
    sha256, DepositError, DepositOutcome, Digest256, GetError, GetPinaxError, Meta, Name,
    SetPinaxError, SetPinaxOutcome, StatError,
};

/// The user-metadata key (sans backend prefix) carrying the apotheca
/// digest. SPEC §3.3.
const APOTHECA_CHECKSUM: &str = "apotheca-checksum";

/// Configuration for opening an S3-compatible cella.
pub struct S3Config {
    pub builder: AmazonS3Builder,
    /// Optional key prefix; if `Some("path/to/cella")`, the cella's
    /// keys are `path/to/cella/deposita/<name>` etc. If `None`, the
    /// cella occupies the bucket root.
    pub prefix: Option<String>,
}

impl S3Config {
    pub fn new() -> Self {
        S3Config {
            builder: AmazonS3Builder::new(),
            prefix: None,
        }
    }

    pub fn with_bucket(mut self, bucket: impl Into<String>) -> Self {
        self.builder = self.builder.with_bucket_name(bucket);
        self
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.builder = self.builder.with_endpoint(endpoint);
        self
    }

    pub fn with_region(mut self, region: impl Into<String>) -> Self {
        self.builder = self.builder.with_region(region);
        self
    }

    pub fn with_access_key_id(mut self, id: impl Into<String>) -> Self {
        self.builder = self.builder.with_access_key_id(id);
        self
    }

    pub fn with_secret_access_key(mut self, key: impl Into<String>) -> Self {
        self.builder = self.builder.with_secret_access_key(key);
        self
    }

    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    /// Allow plaintext `http://` endpoints. Required for MinIO and
    /// other self-hosted S3-compatible servers running without TLS;
    /// remains `false` (the default) for AWS S3 and R2.
    pub fn with_allow_http(mut self, allow: bool) -> Self {
        self.builder = self.builder.with_allow_http(allow);
        self
    }

    /// Use path-style addressing instead of virtual-hosted-style
    /// (`http://endpoint/bucket/key` vs `http://bucket.endpoint/key`).
    /// MinIO and many self-hosted S3-compatible servers expect
    /// path-style; AWS S3 prefers virtual-hosted.
    pub fn with_virtual_hosted_style_request(mut self, virtual_hosted: bool) -> Self {
        self.builder = self
            .builder
            .with_virtual_hosted_style_request(virtual_hosted);
        self
    }
}

impl Default for S3Config {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum S3OpenError {
    Build(object_store::Error),
}

impl std::fmt::Display for S3OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            S3OpenError::Build(e) => write!(f, "build S3 client: {e}"),
        }
    }
}

impl std::error::Error for S3OpenError {}

pub(crate) struct S3Backend {
    store: Arc<dyn ObjectStore>,
    prefix: Option<String>,
    rt: Handle,
}

impl S3Backend {
    pub(crate) fn open(config: S3Config) -> Result<Self, S3OpenError> {
        // Server-side checksum validation as defense-in-depth on PUT
        // (SPEC §3.3, MAY clause). Conformance keys off the
        // `x-amz-meta-apotheca-checksum` field set per-request, not
        // this; this just lets S3 reject corrupted uploads server-side
        // as an extra protection.
        let store = config
            .builder
            .with_checksum_algorithm(object_store::aws::Checksum::SHA256)
            .build()
            .map_err(S3OpenError::Build)?;
        Ok(S3Backend {
            store: Arc::new(store),
            prefix: config.prefix,
            rt: crate::runtime::handle(),
        })
    }

    fn depositum_path(&self, name: &Name<'_>) -> Result<ObjectPath, NotUtf8> {
        self.scope_path("deposita", name)
    }

    fn pinax_path(&self, name: &Name<'_>) -> Result<ObjectPath, NotUtf8> {
        self.scope_path("pinakes", name)
    }

    fn scope_path(&self, scope: &str, name: &Name<'_>) -> Result<ObjectPath, NotUtf8> {
        let name_str = std::str::from_utf8(name.as_bytes()).map_err(|_| NotUtf8)?;
        let key = match &self.prefix {
            Some(p) => format!("{p}/{scope}/{name_str}"),
            None => format!("{scope}/{name_str}"),
        };
        Ok(ObjectPath::from(key))
    }

    /// HEAD-equivalent: fetches just the response metadata (incl. user
    /// attributes) without transferring bytes. Returns the parsed
    /// (size, sha256) on success, `Ok(None)` when the object is
    /// absent, `Err` on any other failure.
    fn head_meta(&self, path: &ObjectPath) -> Result<Option<(u64, Digest256)>, object_store::Error> {
        let opts = GetOptions {
            head: true,
            ..Default::default()
        };
        match self.rt.block_on(self.store.get_opts(path, opts)) {
            Ok(result) => {
                let digest = parse_checksum(&result.attributes).ok_or_else(|| {
                    object_store::Error::Generic {
                        store: "S3",
                        source: format!(
                            "missing or malformed {APOTHECA_CHECKSUM} on {path}"
                        )
                        .into(),
                    }
                })?;
                Ok(Some((result.meta.size, digest)))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub(crate) fn deposit(
        &self,
        name: &Name<'_>,
        bytes: &[u8],
    ) -> Result<DepositOutcome, DepositError> {
        let d = sha256(bytes);
        let path = self.depositum_path(name).map_err(deposit_not_utf8)?;

        // Step 1: pre-check. If present, decide by digest.
        match self.head_meta(&path) {
            Ok(Some((_, existing))) => {
                return Ok(if existing == d {
                    DepositOutcome::Ok
                } else {
                    DepositOutcome::Collision
                });
            }
            Ok(None) => {}
            Err(e) => return Err(deposit_obj_err(e)),
        }

        // Step 2: PutMode::Create. Atomic put-if-not-exists at the
        // backend; SPEC §5.3 race-resolution applies.
        let opts = put_options_create(d);
        let payload: PutPayload = Bytes::copy_from_slice(bytes).into();

        match self.rt.block_on(self.store.put_opts(&path, payload, opts)) {
            Ok(_) => Ok(DepositOutcome::Ok),
            Err(object_store::Error::AlreadyExists { .. }) => {
                // Concurrent deposit won the race; SPEC §5.3 says
                // both writers with matching digest see Ok, otherwise
                // at least one sees Collision.
                match self.head_meta(&path) {
                    Ok(Some((_, existing))) => Ok(if existing == d {
                        DepositOutcome::Ok
                    } else {
                        DepositOutcome::Collision
                    }),
                    Ok(None) => Err(DepositError::Io(io_from_obj(
                        "object disappeared after AlreadyExists",
                    ))),
                    Err(e) => Err(deposit_obj_err(e)),
                }
            }
            Err(e) => Err(deposit_obj_err(e)),
        }
    }

    /// SPEC §2.6. Trusted fast-path: skip the existence-collision
    /// read; issue a single put-if-not-exists at the backend; on
    /// `AlreadyExists`, return `Ok` because the caller's CAS
    /// invariant asserts the bytes already match.
    pub(crate) fn deposit_cas(
        &self,
        name: &Name<'_>,
        bytes: &[u8],
    ) -> Result<DepositOutcome, DepositError> {
        let d = sha256(bytes);
        let path = self.depositum_path(name).map_err(deposit_not_utf8)?;
        let opts = put_options_create(d);
        let payload: PutPayload = Bytes::copy_from_slice(bytes).into();

        match self.rt.block_on(self.store.put_opts(&path, payload, opts)) {
            Ok(_) => Ok(DepositOutcome::Ok),
            // SPEC §2.6: caller's precondition stands as the guarantee
            // that the stored bytes match; no read-back, return Ok.
            Err(object_store::Error::AlreadyExists { .. }) => Ok(DepositOutcome::Ok),
            Err(e) => Err(deposit_obj_err(e)),
        }
    }

    pub(crate) fn get(&self, name: &Name<'_>) -> Result<Vec<u8>, GetError> {
        let path = self.depositum_path(name).map_err(get_not_utf8)?;

        // SPEC §2.2: returned bytes MUST be verified against the
        // stored digest before delivery.
        let opts = GetOptions::default();
        let result = match self.rt.block_on(self.store.get_opts(&path, opts)) {
            Ok(r) => r,
            Err(object_store::Error::NotFound { .. }) => return Err(GetError::NotFound),
            Err(e) => return Err(get_obj_err(e)),
        };

        let stored_digest = parse_checksum(&result.attributes).ok_or(GetError::IntegrityError)?;
        let stored_size = result.meta.size;

        let bytes = match self.rt.block_on(result.bytes()) {
            Ok(b) => b,
            Err(e) => return Err(get_obj_err(e)),
        };

        if bytes.len() as u64 != stored_size {
            return Err(GetError::IntegrityError);
        }
        if sha256(&bytes) != stored_digest {
            return Err(GetError::IntegrityError);
        }
        Ok(bytes.to_vec())
    }

    pub(crate) fn stat(&self, name: &Name<'_>) -> Result<Meta, StatError> {
        let path = self.depositum_path(name).map_err(stat_not_utf8)?;
        match self.head_meta(&path) {
            Ok(Some((size, sha256))) => Ok(Meta { size, sha256 }),
            Ok(None) => Err(StatError::NotFound),
            Err(e) => Err(stat_obj_err(e)),
        }
    }

    /// SPEC §2.4. Same shape as `get`, scoped to the pinax key.
    pub(crate) fn get_pinax(&self, name: &Name<'_>) -> Result<Vec<u8>, GetPinaxError> {
        let path = self.pinax_path(name).map_err(get_pinax_not_utf8)?;
        let opts = GetOptions::default();
        let result = match self.rt.block_on(self.store.get_opts(&path, opts)) {
            Ok(r) => r,
            Err(object_store::Error::NotFound { .. }) => return Err(GetPinaxError::NotFound),
            Err(e) => return Err(get_pinax_obj_err(e)),
        };

        let stored_digest =
            parse_checksum(&result.attributes).ok_or(GetPinaxError::IntegrityError)?;
        let stored_size = result.meta.size;

        let bytes = match self.rt.block_on(result.bytes()) {
            Ok(b) => b,
            Err(e) => return Err(get_pinax_obj_err(e)),
        };

        if bytes.len() as u64 != stored_size {
            return Err(GetPinaxError::IntegrityError);
        }
        if sha256(&bytes) != stored_digest {
            return Err(GetPinaxError::IntegrityError);
        }
        Ok(bytes.to_vec())
    }

    /// SPEC §2.5. Two-round-trip compare-and-swap:
    /// 1. HEAD the pinax to learn its ETag and apotheca digest.
    /// 2. Compare the apotheca digest to `expected`. On mismatch,
    ///    return `Conflict { actual }` without writing.
    /// 3. PUT with `PutMode::Update(UpdateVersion)` (when present) or
    ///    `PutMode::Create` (when absent) — the backend's conditional
    ///    put handles the concurrent-writer race that may have
    ///    happened between (1) and (3).
    ///
    /// The S3 ETag drives backend-level concurrency; the apotheca
    /// digest drives protocol-level CAS. Two checks, two purposes.
    pub(crate) fn set_pinax(
        &self,
        name: &Name<'_>,
        bytes: &[u8],
        expected: Option<Digest256>,
    ) -> Result<SetPinaxOutcome, SetPinaxError> {
        let d = sha256(bytes);
        let path = self.pinax_path(name).map_err(set_pinax_not_utf8)?;

        // Step 1: HEAD.
        let head = match self.head_pinax(&path) {
            Ok(h) => h,
            Err(e) => return Err(set_pinax_obj_err(e)),
        };

        // Step 2: protocol-level CAS check.
        let actual: Option<Digest256> = head.as_ref().map(|h| h.digest);
        if actual != expected {
            return Ok(SetPinaxOutcome::Conflict { actual });
        }

        // Idempotent re-set with identical bytes.
        if actual == Some(d) {
            return Ok(SetPinaxOutcome::Ok);
        }

        // Step 3: conditional PUT.
        let opts = match head {
            None => put_options_create(d),
            Some(h) => put_options_update(d, h.etag),
        };
        let payload: PutPayload = Bytes::copy_from_slice(bytes).into();

        match self.rt.block_on(self.store.put_opts(&path, payload, opts)) {
            Ok(_) => Ok(SetPinaxOutcome::Ok),
            // A concurrent writer modified the pinax between our HEAD
            // and our PUT. Re-read to surface the current state.
            Err(object_store::Error::Precondition { .. })
            | Err(object_store::Error::AlreadyExists { .. }) => {
                match self.head_pinax(&path) {
                    Ok(Some(h)) => Ok(SetPinaxOutcome::Conflict {
                        actual: Some(h.digest),
                    }),
                    Ok(None) => Ok(SetPinaxOutcome::Conflict { actual: None }),
                    Err(e) => Err(set_pinax_obj_err(e)),
                }
            }
            Err(e) => Err(set_pinax_obj_err(e)),
        }
    }

    /// HEAD a pinax key, returning its ETag and apotheca digest. Used
    /// by `set_pinax` to drive both backend-level and protocol-level
    /// CAS checks from a single round-trip.
    fn head_pinax(
        &self,
        path: &ObjectPath,
    ) -> Result<Option<PinaxHead>, object_store::Error> {
        let opts = GetOptions {
            head: true,
            ..Default::default()
        };
        match self.rt.block_on(self.store.get_opts(path, opts)) {
            Ok(result) => {
                let digest = parse_checksum(&result.attributes).ok_or_else(|| {
                    object_store::Error::Generic {
                        store: "S3",
                        source: format!(
                            "missing or malformed {APOTHECA_CHECKSUM} on {path}"
                        )
                        .into(),
                    }
                })?;
                let etag =
                    result
                        .meta
                        .e_tag
                        .clone()
                        .ok_or_else(|| object_store::Error::Generic {
                            store: "S3",
                            source: format!("missing ETag on {path}").into(),
                        })?;
                Ok(Some(PinaxHead { digest, etag }))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

struct PinaxHead {
    digest: Digest256,
    etag: String,
}

// -- helpers ---------------------------------------------------------------

/// Internal marker for the "name isn't UTF-8" case. S3 keys are UTF-8;
/// apotheca names are octets (SPEC §4.1). Non-UTF-8 names on the S3
/// backend are an additional restriction beyond SPEC §4.1's
/// requirements; future work may add an encoding.
struct NotUtf8;

fn put_options_create(digest: Digest256) -> PutOptions {
    let mut attrs = Attributes::new();
    attrs.insert(
        Attribute::Metadata(Cow::Borrowed(APOTHECA_CHECKSUM)),
        hex::encode(digest).into(),
    );
    PutOptions {
        mode: PutMode::Create,
        attributes: attrs,
        ..Default::default()
    }
}

fn put_options_update(digest: Digest256, etag: String) -> PutOptions {
    let mut attrs = Attributes::new();
    attrs.insert(
        Attribute::Metadata(Cow::Borrowed(APOTHECA_CHECKSUM)),
        hex::encode(digest).into(),
    );
    PutOptions {
        mode: PutMode::Update(UpdateVersion {
            e_tag: Some(etag),
            version: None,
        }),
        attributes: attrs,
        ..Default::default()
    }
}

/// Extract the apotheca-checksum from response attributes, returning
/// the parsed 32-byte digest or None if the field is missing or
/// malformed. None is treated as `IntegrityError` (or NotFound on
/// HEAD) by callers per SPEC §3.1.
fn parse_checksum(attrs: &Attributes) -> Option<Digest256> {
    let raw = attrs.get(&Attribute::Metadata(Cow::Borrowed(APOTHECA_CHECKSUM)))?;
    let s = raw.as_ref();
    if s.len() != 64 {
        return None;
    }
    let mut buf = [0u8; 32];
    hex::decode_to_slice(s, &mut buf).ok()?;
    Some(buf)
}

fn deposit_not_utf8(_: NotUtf8) -> DepositError {
    DepositError::Io(io_from_obj("S3 backend requires UTF-8 names"))
}
fn get_not_utf8(_: NotUtf8) -> GetError {
    GetError::Io(io_from_obj("S3 backend requires UTF-8 names"))
}
fn stat_not_utf8(_: NotUtf8) -> StatError {
    StatError::Io(io_from_obj("S3 backend requires UTF-8 names"))
}
fn get_pinax_not_utf8(_: NotUtf8) -> GetPinaxError {
    GetPinaxError::Io(io_from_obj("S3 backend requires UTF-8 names"))
}
fn set_pinax_not_utf8(_: NotUtf8) -> SetPinaxError {
    SetPinaxError::Io(io_from_obj("S3 backend requires UTF-8 names"))
}

fn deposit_obj_err(e: object_store::Error) -> DepositError {
    DepositError::Io(io_from_obj(format!("{e}")))
}
fn get_obj_err(e: object_store::Error) -> GetError {
    GetError::Io(io_from_obj(format!("{e}")))
}
fn stat_obj_err(e: object_store::Error) -> StatError {
    StatError::Io(io_from_obj(format!("{e}")))
}
fn get_pinax_obj_err(e: object_store::Error) -> GetPinaxError {
    GetPinaxError::Io(io_from_obj(format!("{e}")))
}
fn set_pinax_obj_err(e: object_store::Error) -> SetPinaxError {
    SetPinaxError::Io(io_from_obj(format!("{e}")))
}

fn io_from_obj(msg: impl Into<String>) -> std::io::Error {
    std::io::Error::other(msg.into())
}
