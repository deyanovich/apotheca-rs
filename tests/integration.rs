// Integration tests against the protocol surface (SPEC §2, §4.1, §6).

use apotheca::{
    Cella, DepositOutcome, GetError, GetPinaxError, Name, SetPinaxOutcome, StatError,
};
use tempfile::TempDir;

fn open() -> (TempDir, Cella) {
    let dir = TempDir::new().unwrap();
    let cella = Cella::open(dir.path()).unwrap();
    (dir, cella)
}

// -- Depositum namespace ----------------------------------------------------

#[test]
fn deposit_then_get_returns_same_bytes() {
    let (_d, cella) = open();
    let name = Name::new(b"hello.txt").unwrap();
    assert_eq!(cella.deposit(&name, b"hello world").unwrap(), DepositOutcome::Ok);
    assert_eq!(cella.get(&name).unwrap(), b"hello world");
}

#[test]
fn deposit_idempotent_same_bytes() {
    let (_d, cella) = open();
    let name = Name::new(b"foo").unwrap();
    assert_eq!(cella.deposit(&name, b"data").unwrap(), DepositOutcome::Ok);
    // Re-deposit with identical bytes succeeds and is a no-op.
    assert_eq!(cella.deposit(&name, b"data").unwrap(), DepositOutcome::Ok);
    assert_eq!(cella.get(&name).unwrap(), b"data");
}

#[test]
fn deposit_different_bytes_collides_and_does_not_mutate() {
    let (_d, cella) = open();
    let name = Name::new(b"foo").unwrap();
    assert_eq!(cella.deposit(&name, b"original").unwrap(), DepositOutcome::Ok);
    assert_eq!(cella.deposit(&name, b"different").unwrap(), DepositOutcome::Collision);
    // SPEC §2.1: stored bytes MUST NOT be modified.
    assert_eq!(cella.get(&name).unwrap(), b"original");
}

#[test]
fn deposit_cas_stores_and_idempotent_redeposit() {
    // SPEC §2.6: deposit_cas stores bytes under a caller-asserted CAS
    // precondition. On the local backend it delegates to deposit, so
    // idempotent re-deposit returns Ok.
    let (_d, cella) = open();
    let name = Name::new(b"cas-blob").unwrap();
    assert_eq!(cella.deposit_cas(&name, b"payload").unwrap(), DepositOutcome::Ok);
    assert_eq!(cella.deposit_cas(&name, b"payload").unwrap(), DepositOutcome::Ok);
    assert_eq!(cella.get(&name).unwrap(), b"payload");
}

#[test]
fn get_missing_name_is_not_found() {
    let (_d, cella) = open();
    let name = Name::new(b"missing").unwrap();
    assert!(matches!(cella.get(&name), Err(GetError::NotFound)));
}

#[test]
fn stat_missing_name_is_not_found() {
    let (_d, cella) = open();
    let name = Name::new(b"missing").unwrap();
    assert!(matches!(cella.stat(&name), Err(StatError::NotFound)));
}

#[test]
fn stat_returns_size_and_sha256() {
    let (_d, cella) = open();
    let name = Name::new(b"hello").unwrap();
    cella.deposit(&name, b"hello").unwrap();
    let meta = cella.stat(&name).unwrap();
    assert_eq!(meta.size, 5);
    // sha256("hello")
    assert_eq!(
        hex::encode(meta.sha256),
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
}

#[test]
fn empty_bytes_round_trip() {
    let (_d, cella) = open();
    let name = Name::new(b"empty").unwrap();
    cella.deposit(&name, b"").unwrap();
    assert_eq!(cella.get(&name).unwrap(), b"");
    let meta = cella.stat(&name).unwrap();
    assert_eq!(meta.size, 0);
    // sha256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
    assert_eq!(
        hex::encode(meta.sha256),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn integrity_error_when_bytes_tampered() {
    let (dir, cella) = open();
    let name = Name::new(b"foo").unwrap();
    cella.deposit(&name, b"original").unwrap();
    // Corrupt the on-disk bytes directly.
    std::fs::write(dir.path().join("deposita").join("foo").join("bytes"), b"tampered").unwrap();
    assert!(matches!(cella.get(&name), Err(GetError::IntegrityError)));
    // stat does not re-hash, so it still succeeds (SPEC §6.6).
    assert!(cella.stat(&name).is_ok());
}

#[test]
fn integrity_error_when_size_mismatched() {
    let (dir, cella) = open();
    let name = Name::new(b"foo").unwrap();
    cella.deposit(&name, b"original").unwrap();
    // Truncate.
    std::fs::write(dir.path().join("deposita").join("foo").join("bytes"), b"orig").unwrap();
    assert!(matches!(cella.get(&name), Err(GetError::IntegrityError)));
}

#[test]
fn name_validation_rejects() {
    use apotheca::NameError;
    assert_eq!(Name::new(b""), Err(NameError::Empty));
    assert_eq!(Name::new(b"a/b"), Err(NameError::ContainsSlash));
    assert_eq!(Name::new(b"a\0b"), Err(NameError::ContainsNul));
    assert_eq!(Name::new(b"."), Err(NameError::DotOrDotDot));
    assert_eq!(Name::new(b".."), Err(NameError::DotOrDotDot));
    let too_long = vec![b'a'; 256];
    assert_eq!(Name::new(&too_long), Err(NameError::TooLong));
}

#[test]
fn on_disk_layout_matches_spec() {
    let (dir, cella) = open();
    let name = Name::new(b"foo").unwrap();
    cella.deposit(&name, b"hello").unwrap();
    // SPEC §6.2 layout.
    let depositum = dir.path().join("deposita").join("foo");
    assert!(depositum.is_dir());
    assert!(depositum.join("bytes").is_file());
    assert!(depositum.join("meta").is_file());
    // SPEC §6.3 meta format.
    let meta_text = std::fs::read_to_string(depositum.join("meta")).unwrap();
    assert_eq!(
        meta_text,
        "size 5\nsha256 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824\n"
    );
    assert!(dir.path().join("tmp").is_dir());
    // SPEC §6.7: pinakes/ is created on cella open even before any pinax is set.
    assert!(dir.path().join("pinakes").is_dir());
}

#[test]
fn cella_reopen_sees_existing_deposita() {
    let dir = TempDir::new().unwrap();
    {
        let cella = Cella::open(dir.path()).unwrap();
        let name = Name::new(b"persistent").unwrap();
        cella.deposit(&name, b"hello").unwrap();
    }
    {
        let cella = Cella::open(dir.path()).unwrap();
        let name = Name::new(b"persistent").unwrap();
        assert_eq!(cella.get(&name).unwrap(), b"hello");
    }
}

#[test]
fn collision_after_reopen() {
    let dir = TempDir::new().unwrap();
    {
        let cella = Cella::open(dir.path()).unwrap();
        let name = Name::new(b"foo").unwrap();
        cella.deposit(&name, b"first").unwrap();
    }
    {
        let cella = Cella::open(dir.path()).unwrap();
        let name = Name::new(b"foo").unwrap();
        assert_eq!(cella.deposit(&name, b"second").unwrap(), DepositOutcome::Collision);
        assert_eq!(cella.get(&name).unwrap(), b"first");
    }
}

#[test]
fn names_with_arbitrary_octets_round_trip() {
    let (_d, cella) = open();
    let raw: &[u8] = &[0xff, 0x01, 0x7f, 0x80];
    let name = Name::new(raw).unwrap();
    cella.deposit(&name, b"opaque").unwrap();
    assert_eq!(cella.get(&name).unwrap(), b"opaque");
}

// -- Pinax namespace --------------------------------------------------------

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

#[test]
fn set_pinax_absent_then_get() {
    let (_d, cella) = open();
    let name = Name::new(b"head").unwrap();
    assert_eq!(
        cella.set_pinax(&name, b"v1", None).unwrap(),
        SetPinaxOutcome::Ok
    );
    assert_eq!(cella.get_pinax(&name).unwrap(), b"v1");
}

#[test]
fn get_pinax_missing_is_not_found() {
    let (_d, cella) = open();
    let name = Name::new(b"absent").unwrap();
    assert!(matches!(cella.get_pinax(&name), Err(GetPinaxError::NotFound)));
}

#[test]
fn set_pinax_expect_absent_when_present_is_conflict() {
    let (_d, cella) = open();
    let name = Name::new(b"head").unwrap();
    cella.set_pinax(&name, b"v1", None).unwrap();
    let outcome = cella.set_pinax(&name, b"v2", None).unwrap();
    let v1_digest = sha256(b"v1");
    assert_eq!(outcome, SetPinaxOutcome::Conflict { actual: Some(v1_digest) });
    // Stored bytes unchanged.
    assert_eq!(cella.get_pinax(&name).unwrap(), b"v1");
}

#[test]
fn set_pinax_with_correct_expected_replaces() {
    let (_d, cella) = open();
    let name = Name::new(b"head").unwrap();
    cella.set_pinax(&name, b"v1", None).unwrap();
    let v1 = sha256(b"v1");
    assert_eq!(
        cella.set_pinax(&name, b"v2", Some(v1)).unwrap(),
        SetPinaxOutcome::Ok
    );
    assert_eq!(cella.get_pinax(&name).unwrap(), b"v2");
}

#[test]
fn set_pinax_with_stale_expected_is_conflict() {
    let (_d, cella) = open();
    let name = Name::new(b"head").unwrap();
    cella.set_pinax(&name, b"v1", None).unwrap();
    let v1 = sha256(b"v1");
    cella.set_pinax(&name, b"v2", Some(v1)).unwrap();
    let v2 = sha256(b"v2");
    // Caller still thinks head is v1; should observe Conflict reporting v2.
    let outcome = cella.set_pinax(&name, b"v3", Some(v1)).unwrap();
    assert_eq!(outcome, SetPinaxOutcome::Conflict { actual: Some(v2) });
    assert_eq!(cella.get_pinax(&name).unwrap(), b"v2");
}

#[test]
fn set_pinax_expect_some_when_absent_is_conflict() {
    let (_d, cella) = open();
    let name = Name::new(b"never_set").unwrap();
    let bogus = [0u8; 32];
    let outcome = cella.set_pinax(&name, b"v1", Some(bogus)).unwrap();
    assert_eq!(outcome, SetPinaxOutcome::Conflict { actual: None });
    assert!(matches!(cella.get_pinax(&name), Err(GetPinaxError::NotFound)));
}

#[test]
fn set_pinax_idempotent_same_bytes() {
    let (_d, cella) = open();
    let name = Name::new(b"head").unwrap();
    cella.set_pinax(&name, b"v1", None).unwrap();
    let v1 = sha256(b"v1");
    // Identical bytes with matching expected: idempotent Ok.
    assert_eq!(
        cella.set_pinax(&name, b"v1", Some(v1)).unwrap(),
        SetPinaxOutcome::Ok
    );
    assert_eq!(cella.get_pinax(&name).unwrap(), b"v1");
}

#[test]
fn pinax_namespace_disjoint_from_depositum() {
    let (_d, cella) = open();
    let name = Name::new(b"shared").unwrap();
    // Same name, both namespaces.
    cella.deposit(&name, b"depositum-bytes").unwrap();
    cella.set_pinax(&name, b"pinax-bytes", None).unwrap();
    // Each namespace returns its own bytes.
    assert_eq!(cella.get(&name).unwrap(), b"depositum-bytes");
    assert_eq!(cella.get_pinax(&name).unwrap(), b"pinax-bytes");
}

#[test]
fn pinax_layout_matches_spec() {
    let (dir, cella) = open();
    let name = Name::new(b"head").unwrap();
    cella.set_pinax(&name, b"v1", None).unwrap();
    // SPEC §6.7: pinax stored as a single regular file at <root>/pinakes/<name>.
    let pinax_file = dir.path().join("pinakes").join("head");
    assert!(pinax_file.is_file());
    assert_eq!(std::fs::read(&pinax_file).unwrap(), b"v1");
    // The lockfile is created on demand at <name>.lock alongside.
    assert!(dir.path().join("pinakes").join("head.lock").is_file());
}

#[test]
fn pinax_persists_across_reopen() {
    let dir = TempDir::new().unwrap();
    let v1 = sha256(b"v1");
    {
        let cella = Cella::open(dir.path()).unwrap();
        let name = Name::new(b"head").unwrap();
        cella.set_pinax(&name, b"v1", None).unwrap();
    }
    {
        let cella = Cella::open(dir.path()).unwrap();
        let name = Name::new(b"head").unwrap();
        assert_eq!(cella.get_pinax(&name).unwrap(), b"v1");
        // compare-and-swap still works against the persisted digest.
        assert_eq!(
            cella.set_pinax(&name, b"v2", Some(v1)).unwrap(),
            SetPinaxOutcome::Ok
        );
        assert_eq!(cella.get_pinax(&name).unwrap(), b"v2");
    }
}
