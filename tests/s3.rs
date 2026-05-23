//! Live integration tests against an S3-compatible backend.
//!
//! Gated by the `live-tests` cargo feature (which implies
//! `backend-s3`). Requires a running S3-compatible server with a
//! pre-created bucket; see `dev/README.md` for the dev MinIO setup.
//!
//! Run with:
//!
//! ```sh
//! cargo test --features live-tests
//! ```
//!
//! Configuration via `APOTHECA_S3_*` env vars (see `dev/README.md`);
//! defaults match the dev `docker-compose.yml` MinIO instance.

#![cfg(feature = "live-tests")]

use apotheca::{
    Cella, DepositOutcome, GetError, GetPinaxError, Name, S3Config, SetPinaxOutcome, StatError,
};

// -- harness ---------------------------------------------------------------

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Unique per-test prefix so concurrent tests don't collide on shared
/// names. Each test gets its own subtree under
/// `apotheca-tests/<test-name>-<rand>/`.
fn unique_prefix(test_name: &str) -> String {
    let mut buf = [0u8; 8];
    getrandom::getrandom(&mut buf).unwrap();
    format!("apotheca-tests/{test_name}-{}", hex::encode(buf))
}

fn open_cella(test_name: &str) -> Cella {
    let endpoint = env_or("APOTHECA_S3_ENDPOINT", "http://localhost:9000");
    let bucket = env_or("APOTHECA_S3_BUCKET", "apotheca-test");
    let access_key = env_or("APOTHECA_S3_ACCESS_KEY_ID", "minioadmin");
    let secret_key = env_or("APOTHECA_S3_SECRET_ACCESS_KEY", "minioadmin");
    let region = env_or("APOTHECA_S3_REGION", "us-east-1");

    let config = S3Config::new()
        .with_endpoint(endpoint)
        .with_bucket(bucket)
        .with_region(region)
        .with_access_key_id(access_key)
        .with_secret_access_key(secret_key)
        .with_allow_http(true)
        .with_virtual_hosted_style_request(false)
        .with_prefix(unique_prefix(test_name));

    Cella::open_s3(config).expect("open S3 cella")
}

// -- Depositum surface -----------------------------------------------------

#[test]
fn s3_deposit_then_get_returns_same_bytes() {
    let cella = open_cella("deposit_get");
    let name = Name::new(b"hello.txt").unwrap();
    assert_eq!(
        cella.deposit(&name, b"hello world").unwrap(),
        DepositOutcome::Ok
    );
    assert_eq!(cella.get(&name).unwrap(), b"hello world");
}

#[test]
fn s3_deposit_idempotent_same_bytes() {
    let cella = open_cella("deposit_idempotent");
    let name = Name::new(b"foo").unwrap();
    assert_eq!(cella.deposit(&name, b"data").unwrap(), DepositOutcome::Ok);
    assert_eq!(cella.deposit(&name, b"data").unwrap(), DepositOutcome::Ok);
    assert_eq!(cella.get(&name).unwrap(), b"data");
}

#[test]
fn s3_deposit_different_bytes_collides_and_does_not_mutate() {
    let cella = open_cella("deposit_collision");
    let name = Name::new(b"foo").unwrap();
    assert_eq!(
        cella.deposit(&name, b"original").unwrap(),
        DepositOutcome::Ok
    );
    assert_eq!(
        cella.deposit(&name, b"different").unwrap(),
        DepositOutcome::Collision
    );
    // SPEC §2.1: stored bytes MUST NOT be modified.
    assert_eq!(cella.get(&name).unwrap(), b"original");
}

#[test]
fn s3_deposit_cas_stores_and_idempotent_redeposit() {
    let cella = open_cella("deposit_cas");
    let name = Name::new(b"cas-blob").unwrap();
    assert_eq!(
        cella.deposit_cas(&name, b"payload").unwrap(),
        DepositOutcome::Ok
    );
    // SPEC §2.6: idempotent re-deposit returns Ok without read-back.
    assert_eq!(
        cella.deposit_cas(&name, b"payload").unwrap(),
        DepositOutcome::Ok
    );
    assert_eq!(cella.get(&name).unwrap(), b"payload");
}

#[test]
fn s3_get_missing_name_is_not_found() {
    let cella = open_cella("get_missing");
    let name = Name::new(b"missing").unwrap();
    assert!(matches!(cella.get(&name), Err(GetError::NotFound)));
}

#[test]
fn s3_stat_missing_name_is_not_found() {
    let cella = open_cella("stat_missing");
    let name = Name::new(b"missing").unwrap();
    assert!(matches!(cella.stat(&name), Err(StatError::NotFound)));
}

#[test]
fn s3_stat_returns_size_and_sha256() {
    let cella = open_cella("stat_meta");
    let name = Name::new(b"sized").unwrap();
    cella.deposit(&name, b"hello").unwrap();
    let meta = cella.stat(&name).unwrap();
    assert_eq!(meta.size, 5);
    // sha256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
    assert_eq!(
        hex::encode(meta.sha256),
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
}

#[test]
fn s3_empty_bytes_roundtrip() {
    let cella = open_cella("empty");
    let name = Name::new(b"empty").unwrap();
    cella.deposit(&name, b"").unwrap();
    assert_eq!(cella.get(&name).unwrap(), b"");
    let meta = cella.stat(&name).unwrap();
    assert_eq!(meta.size, 0);
}

#[test]
fn s3_non_utf8_name_is_rejected() {
    // S3 keys must be UTF-8 (SPEC §4.1 allows arbitrary octets;
    // the S3 backend tightens this). Apotheca's local backend accepts
    // such a name; the S3 backend rejects it with an Io error.
    let cella = open_cella("non_utf8");
    let name = Name::new(&[0xff, 0xfe, 0xfd]).unwrap();
    assert!(cella.deposit(&name, b"x").is_err());
}

// -- Pinax surface ---------------------------------------------------------

#[test]
fn s3_set_pinax_absent_then_get() {
    let cella = open_cella("pinax_absent");
    let name = Name::new(b"head").unwrap();
    assert_eq!(
        cella.set_pinax(&name, b"v1", None).unwrap(),
        SetPinaxOutcome::Ok
    );
    assert_eq!(cella.get_pinax(&name).unwrap(), b"v1");
}

#[test]
fn s3_set_pinax_expect_absent_when_present_is_conflict() {
    let cella = open_cella("pinax_expect_absent_conflict");
    let name = Name::new(b"head").unwrap();
    cella.set_pinax(&name, b"v1", None).unwrap();
    // Now present — expect-absent must conflict.
    match cella.set_pinax(&name, b"v2", None).unwrap() {
        SetPinaxOutcome::Conflict { actual: Some(_) } => {}
        other => panic!("expected Conflict with Some(actual), got {other:?}"),
    }
    // Stored bytes unchanged.
    assert_eq!(cella.get_pinax(&name).unwrap(), b"v1");
}

#[test]
fn s3_set_pinax_with_correct_expected_replaces() {
    let cella = open_cella("pinax_correct_expected");
    let name = Name::new(b"head").unwrap();
    cella.set_pinax(&name, b"v1", None).unwrap();
    let d1 = cella.get_pinax(&name).map(|b| sha256_hex(&b)).unwrap();
    let d1_bytes = parse_hex(&d1);
    assert_eq!(
        cella.set_pinax(&name, b"v2", Some(d1_bytes)).unwrap(),
        SetPinaxOutcome::Ok
    );
    assert_eq!(cella.get_pinax(&name).unwrap(), b"v2");
}

#[test]
fn s3_set_pinax_with_stale_expected_is_conflict() {
    let cella = open_cella("pinax_stale_expected");
    let name = Name::new(b"head").unwrap();
    cella.set_pinax(&name, b"v1", None).unwrap();
    let d1 = parse_hex(&sha256_hex(b"v1"));
    cella
        .set_pinax(&name, b"v2", Some(d1))
        .unwrap(); // v1 → v2
    // Now stored is v2; expecting v1 (the stale digest) must conflict.
    match cella.set_pinax(&name, b"v3", Some(d1)).unwrap() {
        SetPinaxOutcome::Conflict { actual: Some(d) } => {
            assert_eq!(d, parse_hex(&sha256_hex(b"v2")));
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
    assert_eq!(cella.get_pinax(&name).unwrap(), b"v2");
}

#[test]
fn s3_set_pinax_expect_some_when_absent_is_conflict() {
    let cella = open_cella("pinax_expect_some_absent");
    let name = Name::new(b"head").unwrap();
    let phantom = parse_hex(&sha256_hex(b"never written"));
    match cella.set_pinax(&name, b"v1", Some(phantom)).unwrap() {
        SetPinaxOutcome::Conflict { actual: None } => {}
        other => panic!("expected Conflict with actual=None, got {other:?}"),
    }
    assert!(matches!(
        cella.get_pinax(&name),
        Err(GetPinaxError::NotFound)
    ));
}

#[test]
fn s3_set_pinax_idempotent_same_bytes() {
    let cella = open_cella("pinax_idempotent");
    let name = Name::new(b"head").unwrap();
    cella.set_pinax(&name, b"v1", None).unwrap();
    let d1 = parse_hex(&sha256_hex(b"v1"));
    // Re-setting v1 with the matching expected digest should be Ok
    // (idempotent re-set, no write).
    assert_eq!(
        cella.set_pinax(&name, b"v1", Some(d1)).unwrap(),
        SetPinaxOutcome::Ok
    );
    assert_eq!(cella.get_pinax(&name).unwrap(), b"v1");
}

// -- Namespace disjointness (SPEC §4.3) ------------------------------------

#[test]
fn s3_namespaces_disjoint() {
    let cella = open_cella("namespaces");
    let name = Name::new(b"shared").unwrap();
    cella.deposit(&name, b"depositum-bytes").unwrap();
    cella.set_pinax(&name, b"pinax-bytes", None).unwrap();
    // Both namespaces independently addressable under the same name.
    assert_eq!(cella.get(&name).unwrap(), b"depositum-bytes");
    assert_eq!(cella.get_pinax(&name).unwrap(), b"pinax-bytes");
}

// -- helpers ---------------------------------------------------------------

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

fn parse_hex(s: &str) -> [u8; 32] {
    let mut buf = [0u8; 32];
    hex::decode_to_slice(s, &mut buf).unwrap();
    buf
}
