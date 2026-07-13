//! The cargo shim: forwards every invocation to the real cargo, records
//! workspace usage, and hands throttled cleanup to a detached child.

use std::ffi::{OsStr, OsString};
use std::process::Stdio;

/// Hidden internal verb used to run the throttled post-build cleanup in a
/// detached child process, off the critical path of a normal `cargo` command.
/// This is an internal implementation detail, not a user-facing command.
///
/// Ordering contract: `dispatch` (main.rs) must check this verb BEFORE the
/// argv0 name dispatch. The child is re-exec'd via `current_exe`, which
/// resolves the shim symlink on Linux (`cargo-overstay`) but can keep the
/// symlink path on macOS (`cargo`) — under either name, this verb has to win,
/// and a violation fails invisibly (the child's stderr is null and nothing
/// observes its exit).
pub(crate) const GC_DETACHED_VERB: &str = "__gc-detached";

/// Args are `OsString`, not `String`: the shim must forward argument vectors
/// the real cargo accepts, including non-UTF8 bytes (legal in unix argv).
pub(crate) fn passthrough(args: &[OsString]) -> i32 {
    // Without a trustworthy own path there is no way to step over the shim
    // symlink when resolving the real cargo — searching anyway would find
    // ourselves first on PATH and fork-bomb. Refuse instead.
    let self_exe = match std::env::current_exe() {
        Ok(p) if !p.as_os_str().is_empty() => p,
        _ => {
            eprintln!(
                "cargo-overstay: cannot determine own executable path; \
                 refusing to search for the real `cargo`"
            );
            return 127;
        }
    };
    let path_var = std::env::var_os("PATH").unwrap_or_default();

    let cargo = match crate::cargo::find_real_cargo(&self_exe, &path_var) {
        Some(c) => c,
        None => {
            eprintln!("cargo-overstay: could not find the real `cargo` on PATH");
            return 127;
        }
    };

    let cwd = std::env::current_dir().ok();
    let workspace = cwd
        .as_deref()
        .and_then(crate::workspace::resolve_workspace_root);
    // Lossy view for sniffing --target-dir out of the args; forwarding below
    // still uses the exact bytes.
    let lossy_args: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let target_dir = workspace
        .as_deref()
        .zip(cwd.as_deref())
        .map(|(ws, dir)| crate::cargo::invocation_target_dir(&lossy_args, dir, ws));
    let now = crate::size::now_unix();

    // Best-effort, single store open: record usage and check whether throttled
    // cleanup is due. Never blocks or fails the build.
    let due = record_usage_and_check_due(
        workspace.as_deref(),
        target_dir.as_deref(),
        cwd.as_deref(),
        now,
    );

    // Run the real cargo and remember its exit code.
    let code = crate::cargo::run(&cargo, args).unwrap_or(1);

    // Best-effort: hand off cleanup to a detached child so the prompt returns
    // immediately. The child re-execs this same binary with a hidden verb; we
    // deliberately do not wait on it (its parent exiting right after is fine
    // on unix — init reaps it).
    if due {
        let target_arg = target_dir
            .map(std::path::PathBuf::into_os_string)
            .unwrap_or_default();
        let _ = std::process::Command::new(&self_exe)
            .arg(GC_DETACHED_VERB)
            .arg(target_arg)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }

    code
}

fn record_usage_and_check_due(
    workspace: Option<&std::path::Path>,
    target_dir: Option<&std::path::Path>,
    cwd: Option<&std::path::Path>,
    now: i64,
) -> bool {
    let store = crate::store::Store::open(&crate::paths::state_path());
    if let (Some(ws), Some(target)) = (workspace, target_dir) {
        let _ = store.touch(&ws.to_string_lossy(), &target.to_string_lossy(), now);
    }
    let last_gc = store.last_gc();
    if crate::cleanup::should_run_gc(last_gc, now, crate::cleanup::THROTTLE_HOURS * 3600) {
        return true;
    }
    // Inside the normal throttle window, still run if the disk is low. Probe
    // the target dir, falling back to the cwd (the target may not exist yet).
    let free = target_dir
        .and_then(crate::size::free_space)
        .or_else(|| cwd.and_then(crate::size::free_space));
    crate::cleanup::should_run_gc_low_disk(
        last_gc,
        now,
        free,
        crate::cleanup::default_policy().low_disk_trigger,
    )
}

/// Runs the throttled cleanup pass in what is meant to be a detached child
/// process (spawned by `passthrough`, never waited on). Best-effort end to
/// end: nothing is watching this process's exit code anyway. `run_gc` itself
/// updates `last_gc`, so the next invocation sees cleanup as not due.
pub(crate) fn run_detached_gc(target_arg: Option<&OsStr>) -> i32 {
    let now = crate::size::now_unix();
    let current_target = target_arg
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from);

    let store = crate::store::Store::open(&crate::paths::state_path());
    crate::cleanup::run_gc(&store, current_target.as_deref(), now);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_detached_gc_is_best_effort_and_returns_zero() {
        let _env = crate::paths::env_lock();
        let dir = std::env::temp_dir().join(format!("overstay_maindet_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let state_path = dir.join("test.state");

        std::env::set_var("CARGO_OVERSTAY_STATE", &state_path);
        let code = run_detached_gc(None);
        assert_eq!(code, 0);
        // run_gc should have opened the store and advanced last_gc.
        let last_gc = crate::store::Store::open(&state_path).last_gc();
        assert!(last_gc > 0);

        std::env::remove_var("CARGO_OVERSTAY_STATE");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
