# apotheca

A named write-once store with a compare-and-swap pinax namespace. Bytes go in
by name, come out by name; *deposita* are never overwritten, *pinakes* update
via compare-and-swap.

This crate (`apotheca`, binary `apo`) is the Rust reference implementation of
the apotheca protocol. It implements both surfaces — the depositum surface
(`deposit`, `deposit_cas`, `get`, `stat`) and the pinax surface (`get_pinax`,
`set_pinax`) — on two backends: a default dependency-free local-filesystem
backend, and an S3-compatible backend (AWS S3, Cloudflare R2, MinIO) behind
the `backend-s3` Cargo feature. Multi-backend cellae, GCS / Azure / scp / sftp
transports, encryption-as-wrapper, and external configuration are not yet
implemented.

## Install

The binary:

```sh
cargo install apotheca
```

The library:

```toml
[dependencies]
apotheca = "0.3"

# Optional S3-compatible backend (AWS S3, Cloudflare R2, MinIO):
apotheca = { version = "0.3", features = ["backend-s3"] }
```

## CLI

`apo` exposes the protocol operations one-for-one. The default cella root is
`$HOME/.apotheca/`; override with `--cella <dir>`.

```sh
apo deposit <path>                   # store the file under its basename
apo deposit --name <n> <path>        # store the file under <n>
apo deposit --name <n> -             # store stdin under <n>
apo get <name>                       # depositum bytes to stdout
apo stat <name>                      # depositum size and sha256 to stdout

apo pinax get <name>                 # pinax bytes to stdout
apo pinax set --name <n> --expect-absent <path>      # set if absent
apo pinax set --name <n> --expect <hex>     <path>   # set if current digest matches
apo pinax set --name <n> --expect <hex>     -        # bytes from stdin
```

`deposit` is write-once: re-depositing identical bytes under an existing name
is a no-op (`Ok`); depositing different bytes under an existing name fails
with a collision and the stored bytes are not modified. `get` verifies bytes
against the stored sha256 before returning them; a mismatch is reported as an
integrity error rather than silently propagated. `stat` does not read or
re-hash the bytes.

`pinax set` is compare-and-swap. Exactly one of `--expect-absent` and
`--expect <hex>` is required: `--expect-absent` requires the name to be
absent in the pinax namespace; `--expect <hex>` requires the stored digest
(64 lowercase hex digits) to equal the value passed. On precondition failure,
exit is non-zero and stderr carries `conflict: actual=<hex>` or
`conflict: actual=absent` so the caller can rebuild on top of the winner and
retry. Re-setting a pinax to bytes that already match its stored digest is a
no-op (`Ok`).

The pinax namespace is disjoint from the depositum namespace: the same name
may refer to a pinax and a depositum simultaneously without collision.

Exit status is `0` on success, non-zero on collision, conflict, not-found,
integrity error, invalid name, or any I/O failure, with a diagnostic on
stderr.

## Library

```rust
use apotheca::{Cella, Name, DepositOutcome, SetPinaxOutcome};

let cella = Cella::open("/path/to/cella")?;
let name = Name::new(b"hello")?;

// Depositum namespace: write-once-by-name.
match cella.deposit(&name, b"world")? {
    DepositOutcome::Ok => {}              // stored, or idempotent re-deposit
    DepositOutcome::Collision => {}       // present with different bytes
}
let bytes = cella.get(&name)?;            // verified before return
let meta  = cella.stat(&name)?;           // { size, sha256 } without reading bytes

// Content-addressed callers (e.g. syntheca) MAY use `deposit_cas`:
// when `name` is itself derived from the bytes, the caller asserts
// the CAS invariant and apotheca skips the existence-collision read.
cella.deposit_cas(&name, b"world")?;

// Pinax namespace: compare-and-swap.
let head = Name::new(b"head")?;
cella.set_pinax(&head, b"v1", None)?;     // first publish (expected = absent)
let current = cella.get_pinax(&head)?;
match cella.set_pinax(&head, b"v2", Some(sha256(&current)))? {
    SetPinaxOutcome::Ok => {}                            // swap succeeded
    SetPinaxOutcome::Conflict { actual } => {            // someone else wrote first
        // re-read current state and retry against `actual`
    }
}
# fn sha256(_: &[u8]) -> [u8; 32] { unimplemented!() }
# Ok::<(), Box<dyn std::error::Error>>(())
```

Names are octet sequences, not Unicode strings: `Name::new` takes `&[u8]` and
applies no normalisation. Names are non-empty, contain no `/` or NUL, are not
`.` or `..`, and are at most 255 octets. Name policy applies identically to
the depositum and pinax namespaces.

Errors split into `DepositError`, `GetError`, `StatError`, `GetPinaxError`,
`SetPinaxError`, each with variants for `InvalidName`, `Io`, `MalformedMeta`
where applicable, plus the operation-specific outcomes (`NotFound`,
`IntegrityError` on read paths). The `Conflict` outcome of `set_pinax` is an
`Ok` variant of `SetPinaxOutcome`, not an error — it carries the observed
`actual` digest for the caller's compare-and-swap retry loop.

## Backends

The local-filesystem backend (above) is the default and requires no extra
features. An S3-compatible backend covering AWS S3, Cloudflare R2, and MinIO
is available behind the `backend-s3` Cargo feature; enabling it pulls in
[`object_store`](https://crates.io/crates/object_store) and a tokio runtime
(the `Cella` surface stays sync — a dedicated runtime drives the async
calls internally).

```rust
# #[cfg(feature = "backend-s3")] {
use apotheca::{Cella, S3Config};

let config = S3Config::new()
    .with_bucket("my-cella")
    .with_region("us-east-1")
    .with_access_key_id(std::env::var("AWS_ACCESS_KEY_ID")?)
    .with_secret_access_key(std::env::var("AWS_SECRET_ACCESS_KEY")?);
//  .with_endpoint("http://localhost:9000")           // R2 / MinIO
//  .with_allow_http(true)                            // MinIO (no TLS)
//  .with_virtual_hosted_style_request(false)         // MinIO (path-style)
//  .with_prefix("foo/")                              // namespace inside the bucket

let cella = Cella::open_s3(config)?;
# }
# Ok::<(), Box<dyn std::error::Error>>(())
```

`Cella` operations behave identically across backends; the integrity field
travels as `x-amz-meta-apotheca-checksum` on S3 / R2 / MinIO and as the
local backend's `meta` file on the filesystem (SPEC §3.3).

## Local backend layout

A local cella is a directory containing `deposita/`, `pinakes/`, and `tmp/`.

```
<cella>/
  deposita/                           # write-once depositum namespace
    <name>/
      bytes
      meta                            # "size <decimal>\nsha256 <hex>\n"
  pinakes/                            # compare-and-swap pinax namespace
    <name>                            # one regular file per pinax; content = bytes
    <name>.lock                       # per-name advisory lockfile (created on demand)
  tmp/                                # staging area, shared
    <staging-id>/                     # depositum staging: directory rename
    <staging-id>                      # pinax staging:    file rename
```

Each depositum lives at `deposita/<name>/` with two files: `bytes` (the
depositum's bytes) and `meta` (UTF-8 text giving size and sha256). Bytes and
meta are staged together in a `tmp/<staging-id>/` directory, fsynced, then
renamed into place as a single linearisation point — `deposit` is
all-or-nothing.

Each pinax is a single file at `pinakes/<name>`; the digest is recomputed from
the file content on each read (small payloads, cheap). `set_pinax` holds an
exclusive `flock(2)` on `pinakes/<name>.lock` for the read-current /
compare-and-swap / rename window; the rename is the linearisation point.
Readers don't take the lock.

After a crash, partially-written files are left in `tmp/` and never visible
through `get`, `get_pinax`, or `stat`.

## Status and scope

Reference implementation. Conformant with apotheca v1.0-rc2 on both
surfaces: depositum operations and integrity, pinax operations with
compare-and-swap and integrity, atomicity (crash-safe rename-based deposit;
flock-guarded rename-over for set_pinax), name policy (SPEC §4.1), local
backend layout, and the `apo` CLI surface. The optional §2.6 `deposit_cas`
operation is implemented on both backends; the `apo` binary stays on the
mandatory surface (no `deposit-cas` verb) and exposes `deposit_cas` only
through the library.

Out of scope here: enumeration (deliberately — apotheca has no `ls`/`list`
operation, and never will, so consumers have to maintain their own manifests),
deletion (apotheca is write-once for deposita, compare-and-swap-replaceable
but never deleted for pinakes; GC is a higher-layer concern operating on
backends directly), backends beyond local-filesystem and S3-compatible
(GCS, Azure, scp, sftp), multi-backend composition, encryption,
configuration files, and multi-segment names.

## License

Licensed under either of MIT (LICENSE-MIT) or Apache-2.0 (LICENSE-APACHE) at
your option.

## See also

The protocol specification, decision rationale, and broader project framing
live in the apotheca project group at
<https://gitlab.com/pantheca/apotheca>. Sibling substrate `syntheca` (a thin
content-addressing layer above apotheca) lives at
<https://gitlab.com/pantheca/syntheca>.
