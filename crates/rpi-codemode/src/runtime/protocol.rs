//! Messages between the async host and the per-execution VM worker thread
//! (port of `packages/codemode/src/runtime/protocol.ts` @ a13d35a74).
//!
//! Tool arguments, results, and values cross as JSON strings: the worker
//! passes them into and out of the QuickJS VM as strings and never builds
//! structured values itself.

use crate::types::CodemodeOutputItem;

/// Which table a `call` targets (`target` in the worker protocol).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallTarget {
    Tool,
    Global,
}

/// Worker → host (`WorkerToHostMessage`).
#[derive(Debug, Clone)]
pub enum WorkerToHost {
    /// A script-issued call; `args` is JSON text (absent for `undefined`).
    Call {
        id: u32,
        target: CallTarget,
        name: String,
        args: Option<String>,
    },
    Output(CodemodeOutputItem),
    /// `writes` is a JSON array of `[key, json]` for `store()` and `[key]`
    /// for deletions; present on success only (possibly `[]`).
    Done {
        ok: bool,
        value: Option<String>,
        writes: Option<String>,
        /// `{ name?, message, stack? }` JSON of a script error.
        error: Option<String>,
    },
    /// The VM failed outside the script's control, for example a wasm trap.
    Crash {
        message: String,
    },
}

/// Host → worker (`HostToWorkerMessage`).
#[derive(Debug, Clone)]
pub enum HostToWorker {
    /// `payload` is the JSON result when `ok`, otherwise the error message.
    Result {
        id: u32,
        ok: bool,
        payload: Option<String>,
    },
}
