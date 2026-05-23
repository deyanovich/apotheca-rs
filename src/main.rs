//! `apo` — CLI surface for apotheca (SPEC §7).

use clap::{Parser, Subcommand};
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use apotheca::{
    Cella, DepositOutcome, Digest256, GetError, GetPinaxError, Name, SetPinaxError,
    SetPinaxOutcome, StatError,
};

#[derive(Parser)]
#[command(name = "apo", version, about = "apotheca named write-once store")]
struct Cli {
    /// Cella root directory. Defaults to $HOME/.apotheca/.
    #[arg(long, global = true)]
    cella: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Store bytes under a name in the depositum namespace (write-once).
    Deposit {
        /// Name to store under. Defaults to the basename of <path>.
        /// Required when reading from stdin.
        #[arg(long)]
        name: Option<OsString>,
        /// Path to read bytes from. Use "-" to read from standard input.
        path: OsString,
    },
    /// Read depositum bytes for a name to standard output.
    Get { name: OsString },
    /// Print depositum metadata (size, sha256) for a name.
    Stat { name: OsString },
    /// Operate on the pinax (compare-and-swap) namespace.
    Pinax {
        #[command(subcommand)]
        cmd: PinaxCmd,
    },
}

#[derive(Subcommand)]
enum PinaxCmd {
    /// Read pinax bytes for a name to standard output.
    Get { name: OsString },
    /// Set a pinax via compare-and-swap.
    Set {
        /// Pinax name.
        #[arg(long)]
        name: OsString,
        /// Require the pinax to be absent.
        #[arg(long, conflicts_with = "expect")]
        expect_absent: bool,
        /// Require the pinax's stored digest to equal this hex sha256 (64 lowercase hex digits).
        #[arg(long, conflicts_with = "expect_absent")]
        expect: Option<String>,
        /// Path to read bytes from. Use "-" to read from standard input.
        path: OsString,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let cella_root = cli.cella.unwrap_or_else(default_cella_root);

    let cella = match Cella::open(&cella_root) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(io::stderr(), "apo: open cella {}: {e}", cella_root.display());
            return ExitCode::from(1);
        }
    };

    match cli.cmd {
        Cmd::Deposit { name, path } => cmd_deposit(&cella, name, path),
        Cmd::Get { name } => cmd_get(&cella, name),
        Cmd::Stat { name } => cmd_stat(&cella, name),
        Cmd::Pinax { cmd } => match cmd {
            PinaxCmd::Get { name } => cmd_pinax_get(&cella, name),
            PinaxCmd::Set { name, expect_absent, expect, path } => {
                cmd_pinax_set(&cella, name, expect_absent, expect, path)
            }
        },
    }
}

fn cmd_deposit(cella: &Cella, name_arg: Option<OsString>, path: OsString) -> ExitCode {
    let from_stdin = path.as_bytes() == b"-";

    let name_bytes: Vec<u8> = if let Some(n) = name_arg.as_ref() {
        n.as_bytes().to_vec()
    } else if from_stdin {
        let _ = writeln!(io::stderr(), "apo: --name is required when reading from stdin");
        return ExitCode::from(1);
    } else {
        match Path::new(&path).file_name() {
            Some(b) => b.as_bytes().to_vec(),
            None => {
                let _ = writeln!(io::stderr(), "apo: cannot derive name from path {:?}", path);
                return ExitCode::from(1);
            }
        }
    };

    let name = match Name::new(&name_bytes) {
        Ok(n) => n,
        Err(e) => {
            let _ = writeln!(io::stderr(), "apo: invalid name: {e}");
            return ExitCode::from(1);
        }
    };

    let bytes = match read_input(from_stdin, &path) {
        Ok(v) => v,
        Err(code) => return code,
    };

    match cella.deposit(&name, &bytes) {
        Ok(DepositOutcome::Ok) => ExitCode::from(0),
        Ok(DepositOutcome::Collision) => {
            let _ = writeln!(io::stderr(), "apo: collision: name already present with different bytes");
            ExitCode::from(1)
        }
        Err(e) => {
            let _ = writeln!(io::stderr(), "apo: deposit: {e}");
            ExitCode::from(1)
        }
    }
}

fn cmd_get(cella: &Cella, name_os: OsString) -> ExitCode {
    let name = match Name::new(name_os.as_bytes()) {
        Ok(n) => n,
        Err(e) => {
            let _ = writeln!(io::stderr(), "apo: invalid name: {e}");
            return ExitCode::from(1);
        }
    };

    match cella.get(&name) {
        Ok(bytes) => {
            let mut out = io::stdout().lock();
            if let Err(e) = out.write_all(&bytes) {
                let _ = writeln!(io::stderr(), "apo: write stdout: {e}");
                return ExitCode::from(1);
            }
            ExitCode::from(0)
        }
        Err(GetError::NotFound) => {
            let _ = writeln!(io::stderr(), "apo: not found");
            ExitCode::from(1)
        }
        Err(GetError::IntegrityError) => {
            let _ = writeln!(io::stderr(), "apo: integrity error: stored bytes do not match digest");
            ExitCode::from(1)
        }
        Err(e) => {
            let _ = writeln!(io::stderr(), "apo: get: {e}");
            ExitCode::from(1)
        }
    }
}

fn cmd_stat(cella: &Cella, name_os: OsString) -> ExitCode {
    let name = match Name::new(name_os.as_bytes()) {
        Ok(n) => n,
        Err(e) => {
            let _ = writeln!(io::stderr(), "apo: invalid name: {e}");
            return ExitCode::from(1);
        }
    };

    match cella.stat(&name) {
        Ok(meta) => {
            // SPEC §7.3: aligned columns.
            let mut out = io::stdout().lock();
            if writeln!(out, "size   {}", meta.size).is_err()
                || writeln!(out, "sha256 {}", hex::encode(meta.sha256)).is_err()
            {
                return ExitCode::from(1);
            }
            ExitCode::from(0)
        }
        Err(StatError::NotFound) => {
            let _ = writeln!(io::stderr(), "apo: not found");
            ExitCode::from(1)
        }
        Err(e) => {
            let _ = writeln!(io::stderr(), "apo: stat: {e}");
            ExitCode::from(1)
        }
    }
}

fn cmd_pinax_get(cella: &Cella, name_os: OsString) -> ExitCode {
    let name = match Name::new(name_os.as_bytes()) {
        Ok(n) => n,
        Err(e) => {
            let _ = writeln!(io::stderr(), "apo: invalid name: {e}");
            return ExitCode::from(1);
        }
    };

    match cella.get_pinax(&name) {
        Ok(bytes) => {
            let mut out = io::stdout().lock();
            if let Err(e) = out.write_all(&bytes) {
                let _ = writeln!(io::stderr(), "apo: write stdout: {e}");
                return ExitCode::from(1);
            }
            ExitCode::from(0)
        }
        Err(GetPinaxError::NotFound) => {
            let _ = writeln!(io::stderr(), "apo: not found");
            ExitCode::from(1)
        }
        Err(GetPinaxError::IntegrityError) => {
            let _ = writeln!(io::stderr(), "apo: integrity error: stored bytes do not match digest");
            ExitCode::from(1)
        }
        Err(e) => {
            let _ = writeln!(io::stderr(), "apo: pinax get: {e}");
            ExitCode::from(1)
        }
    }
}

fn cmd_pinax_set(
    cella: &Cella,
    name_os: OsString,
    expect_absent: bool,
    expect: Option<String>,
    path: OsString,
) -> ExitCode {
    if !expect_absent && expect.is_none() {
        let _ = writeln!(
            io::stderr(),
            "apo: pinax set requires --expect-absent or --expect <hex>"
        );
        return ExitCode::from(1);
    }

    let name = match Name::new(name_os.as_bytes()) {
        Ok(n) => n,
        Err(e) => {
            let _ = writeln!(io::stderr(), "apo: invalid name: {e}");
            return ExitCode::from(1);
        }
    };

    let expected: Option<Digest256> = if expect_absent {
        None
    } else {
        let hex = expect.as_deref().unwrap();
        match parse_hex_digest(hex) {
            Some(d) => Some(d),
            None => {
                let _ = writeln!(
                    io::stderr(),
                    "apo: --expect must be 64 lowercase hex digits"
                );
                return ExitCode::from(1);
            }
        }
    };

    let from_stdin = path.as_bytes() == b"-";
    let bytes = match read_input(from_stdin, &path) {
        Ok(v) => v,
        Err(code) => return code,
    };

    match cella.set_pinax(&name, &bytes, expected) {
        Ok(SetPinaxOutcome::Ok) => ExitCode::from(0),
        Ok(SetPinaxOutcome::Conflict { actual }) => {
            let msg = match actual {
                None => "conflict: actual=absent".to_string(),
                Some(d) => format!("conflict: actual={}", hex::encode(d)),
            };
            let _ = writeln!(io::stderr(), "apo: {msg}");
            ExitCode::from(1)
        }
        Err(SetPinaxError::InvalidName(e)) => {
            let _ = writeln!(io::stderr(), "apo: invalid name: {e}");
            ExitCode::from(1)
        }
        Err(SetPinaxError::Io(e)) => {
            let _ = writeln!(io::stderr(), "apo: pinax set: {e}");
            ExitCode::from(1)
        }
    }
}

fn read_input(from_stdin: bool, path: &OsString) -> Result<Vec<u8>, ExitCode> {
    if from_stdin {
        let mut v = Vec::new();
        if let Err(e) = io::stdin().lock().read_to_end(&mut v) {
            let _ = writeln!(io::stderr(), "apo: read stdin: {e}");
            return Err(ExitCode::from(1));
        }
        Ok(v)
    } else {
        match std::fs::read(path) {
            Ok(v) => Ok(v),
            Err(e) => {
                let _ = writeln!(io::stderr(), "apo: read {:?}: {e}", path);
                Err(ExitCode::from(1))
            }
        }
    }
}

fn parse_hex_digest(s: &str) -> Option<Digest256> {
    if s.len() != 64 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let mut buf = [0u8; 32];
    hex::decode_to_slice(s, &mut buf).ok()?;
    Some(buf)
}

fn default_cella_root() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_else(|| OsString::from("."));
    let mut p = PathBuf::from(home);
    p.push(".apotheca");
    p
}
