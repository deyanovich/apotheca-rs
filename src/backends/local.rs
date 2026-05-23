//! Local-filesystem backend (SPEC §6).
//!
//! On-disk layout:
//! ```text
//! <root>/
//!   deposita/<name>/{bytes,meta}    # depositum namespace (write-once)
//!   pinakes/<name>                  # pinax namespace (CAS)
//!   pinakes/<name>.lock             # per-pinax flock lockfile
//!   tmp/<staging-id>/...            # staging area
//! ```

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use crate::{
    sha256, DepositError, DepositOutcome, Digest256, GetError, GetPinaxError, Meta, Name,
    SetPinaxError, SetPinaxOutcome, StatError,
};

pub(crate) struct LocalBackend {
    root: PathBuf,
}

impl LocalBackend {
    /// Open (or create) a local cella at the given root. Idempotent:
    /// ensures `<root>/deposita/`, `<root>/pinakes/`, and `<root>/tmp/`
    /// exist.
    pub(crate) fn open<P: AsRef<Path>>(root: P) -> io::Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("deposita"))?;
        fs::create_dir_all(root.join("pinakes"))?;
        fs::create_dir_all(root.join("tmp"))?;
        Ok(LocalBackend { root })
    }

    fn deposita_dir(&self) -> PathBuf {
        self.root.join("deposita")
    }

    fn pinakes_dir(&self) -> PathBuf {
        self.root.join("pinakes")
    }

    fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    fn depositum_dir(&self, name: &Name<'_>) -> PathBuf {
        let mut p = self.deposita_dir();
        p.push(OsStr::from_bytes(name.as_bytes()));
        p
    }

    fn pinax_path(&self, name: &Name<'_>) -> PathBuf {
        let mut p = self.pinakes_dir();
        p.push(OsStr::from_bytes(name.as_bytes()));
        p
    }

    fn pinax_lock_path(&self, name: &Name<'_>) -> PathBuf {
        let mut p = self.pinakes_dir();
        let mut nb = name.as_bytes().to_vec();
        nb.extend_from_slice(b".lock");
        p.push(OsStr::from_bytes(&nb));
        p
    }

    /// SPEC §2.1, §6.4. Atomic deposit.
    pub(crate) fn deposit(
        &self,
        name: &Name<'_>,
        bytes: &[u8],
    ) -> Result<DepositOutcome, DepositError> {
        let d = sha256(bytes);

        // §6.4 step 2: pre-check. If the depositum exists, decide without staging.
        let depositum = self.depositum_dir(name);
        match read_depositum_meta(&depositum) {
            Ok(Some(existing)) => {
                return Ok(if existing.sha256 == d {
                    DepositOutcome::Ok
                } else {
                    DepositOutcome::Collision
                });
            }
            Ok(None) => {}
            Err(e) => return Err(e),
        }

        // §6.4 steps 3–6: stage.
        let staging_id = fresh_staging_id();
        let staging_dir = self.tmp_dir().join(&staging_id);
        fs::create_dir(&staging_dir).map_err(DepositError::Io)?;

        if let Err(e) = stage_depositum(
            &staging_dir,
            bytes,
            &Meta {
                size: bytes.len() as u64,
                sha256: d,
            },
        ) {
            let _ = fs::remove_dir_all(&staging_dir);
            return Err(DepositError::Io(e));
        }

        // §6.4 step 7: linearisation point.
        match fs::rename(&staging_dir, &depositum) {
            Ok(()) => {
                // §6.4 step 8.
                fsync_dir(&self.deposita_dir()).map_err(DepositError::Io)?;
                Ok(DepositOutcome::Ok)
            }
            Err(e) => {
                // Concurrent deposit may have won the race; recheck §5.3.
                let _ = fs::remove_dir_all(&staging_dir);
                match read_depositum_meta(&depositum) {
                    Ok(Some(existing)) => Ok(if existing.sha256 == d {
                        DepositOutcome::Ok
                    } else {
                        DepositOutcome::Collision
                    }),
                    Ok(None) => Err(DepositError::Io(e)),
                    Err(err) => Err(err),
                }
            }
        }
    }

    /// SPEC §2.6. Trusted fast-path. The local backend delegates to
    /// `deposit`'s read-then-decide path: filesystem round-trips are
    /// cheap enough that the optimisation has no measurable effect, and
    /// reusing the existing path is simpler and preserves the same
    /// crash-safety guarantees.
    pub(crate) fn deposit_cas(
        &self,
        name: &Name<'_>,
        bytes: &[u8],
    ) -> Result<DepositOutcome, DepositError> {
        self.deposit(name, bytes)
    }

    /// SPEC §2.2, §6.6. Verifies before returning.
    pub(crate) fn get(&self, name: &Name<'_>) -> Result<Vec<u8>, GetError> {
        let depositum = self.depositum_dir(name);
        let meta = match read_depositum_meta(&depositum) {
            Ok(Some(m)) => m,
            Ok(None) => return Err(GetError::NotFound),
            Err(DepositError::Io(e)) => return Err(GetError::Io(e)),
            Err(DepositError::MalformedMeta) => return Err(GetError::MalformedMeta),
            Err(DepositError::InvalidName(e)) => return Err(GetError::InvalidName(e)),
        };
        let bytes = fs::read(depositum.join("bytes")).map_err(GetError::Io)?;
        if bytes.len() as u64 != meta.size {
            return Err(GetError::IntegrityError);
        }
        if sha256(&bytes) != meta.sha256 {
            return Err(GetError::IntegrityError);
        }
        Ok(bytes)
    }

    /// SPEC §2.3, §6.6. Does not read or re-hash bytes.
    pub(crate) fn stat(&self, name: &Name<'_>) -> Result<Meta, StatError> {
        let depositum = self.depositum_dir(name);
        match read_depositum_meta(&depositum) {
            Ok(Some(m)) => Ok(m),
            Ok(None) => Err(StatError::NotFound),
            Err(DepositError::Io(e)) => Err(StatError::Io(e)),
            Err(DepositError::MalformedMeta) => Err(StatError::MalformedMeta),
            Err(DepositError::InvalidName(e)) => Err(StatError::InvalidName(e)),
        }
    }

    /// SPEC §2.4, §6.9. Reads pinax bytes and verifies.
    ///
    /// On the local backend the digest is recomputed from the bytes just
    /// read, so `IntegrityError` is unreachable here (SPEC §6.9).
    pub(crate) fn get_pinax(&self, name: &Name<'_>) -> Result<Vec<u8>, GetPinaxError> {
        let path = self.pinax_path(name);
        match fs::read(&path) {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(GetPinaxError::NotFound),
            Err(e) => Err(GetPinaxError::Io(e)),
        }
    }

    /// SPEC §2.5, §6.8. Compare-and-swap.
    pub(crate) fn set_pinax(
        &self,
        name: &Name<'_>,
        bytes: &[u8],
        expected: Option<Digest256>,
    ) -> Result<SetPinaxOutcome, SetPinaxError> {
        let d = sha256(bytes);

        let lock_path = self.pinax_lock_path(name);
        let pinax_path = self.pinax_path(name);

        // §6.8 step 2: acquire exclusive flock. The lockfile is created
        // on demand. The lock is released when `_lock` is dropped.
        let _lock = open_exclusive_lock(&lock_path).map_err(SetPinaxError::Io)?;

        // §6.8 step 3: determine actual.
        let actual = match fs::read(&pinax_path) {
            Ok(content) => Some(sha256(&content)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(SetPinaxError::Io(e)),
        };

        // §6.8 step 4: precondition check.
        if actual != expected {
            return Ok(SetPinaxOutcome::Conflict { actual });
        }

        // §6.8 step 5: idempotent re-set with identical bytes.
        if actual == Some(d) {
            return Ok(SetPinaxOutcome::Ok);
        }

        // §6.8 step 6: stage in tmp.
        let staging_id = fresh_staging_id();
        let staging_path = self.tmp_dir().join(&staging_id);
        {
            let mut f = File::create(&staging_path).map_err(SetPinaxError::Io)?;
            f.write_all(bytes).map_err(SetPinaxError::Io)?;
            f.sync_all().map_err(SetPinaxError::Io)?;
        }

        // §6.8 step 7: rename-over linearisation point.
        if let Err(e) = fs::rename(&staging_path, &pinax_path) {
            let _ = fs::remove_file(&staging_path);
            return Err(SetPinaxError::Io(e));
        }
        fsync_dir(&self.pinakes_dir()).map_err(SetPinaxError::Io)?;

        Ok(SetPinaxOutcome::Ok)
    }
}

/// `Ok(Some(meta))` — depositum present.
/// `Ok(None)` — depositum absent.
/// `Err(_)` — unexpected I/O failure or malformed meta.
fn read_depositum_meta(depositum: &Path) -> Result<Option<Meta>, DepositError> {
    let meta_path = depositum.join("meta");
    let text = match fs::read_to_string(&meta_path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(DepositError::Io(e)),
    };
    Meta::parse(&text).map(Some).map_err(|_| DepositError::MalformedMeta)
}

fn stage_depositum(staging_dir: &Path, bytes: &[u8], meta: &Meta) -> io::Result<()> {
    {
        let mut f = File::create(staging_dir.join("bytes"))?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    {
        let mut f = File::create(staging_dir.join("meta"))?;
        f.write_all(meta.format().as_bytes())?;
        f.sync_all()?;
    }
    fsync_dir(staging_dir)?;
    Ok(())
}

/// Fsync a directory by opening it for read and calling sync_all. The
/// documented Linux pattern for ensuring directory entry changes hit
/// disk.
fn fsync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn fresh_staging_id() -> String {
    let mut buf = [0u8; 16];
    getrandom::getrandom(&mut buf).expect("getrandom must succeed");
    hex::encode(buf)
}

/// Open `path` (creating it if absent) and acquire an exclusive advisory
/// lock on it via `flock(2)`. The lock is held for the lifetime of the
/// returned `File` and released when it is dropped (kernel releases all
/// flocks on close of the last reference to the open file description).
fn open_exclusive_lock(path: &Path) -> io::Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(f)
}
