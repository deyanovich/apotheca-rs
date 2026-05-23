//! Tokio runtime for remote backends. The public `Cella` API is sync;
//! remote backends use this isolated multi-thread runtime to drive
//! `object_store`'s async operations via `block_on`.
//!
//! Pattern follows `reqwest::blocking`: a single process-wide tokio
//! runtime, used by all remote `Cella` instances, lazily initialised on
//! first use. Calls from inside an existing tokio context don't
//! deadlock — the work runs on the apotheca runtime — but they do
//! block the calling thread; async users should wrap remote-backend
//! calls in `tokio::task::spawn_blocking`.

use std::sync::OnceLock;
use tokio::runtime::{Builder, Handle, Runtime};

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// Get a handle to the shared apotheca runtime, initialising it on
/// first call. Panics if runtime construction fails (which should only
/// happen on resource-exhaustion at process start).
pub(crate) fn handle() -> Handle {
    RUNTIME
        .get_or_init(|| {
            Builder::new_multi_thread()
                .enable_all()
                .thread_name("apotheca-rt")
                .build()
                .expect("build apotheca tokio runtime")
        })
        .handle()
        .clone()
}
