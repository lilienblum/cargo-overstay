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
            let style = crate::style::Style::stderr();
            eprintln!(
                "{} cannot determine own executable path; \
                 refusing to search for the real `cargo`",
                style.error("cargo-overstay:")
            );
            return 127;
        }
    };
    let path_var = std::env::var_os("PATH").unwrap_or_default();

    let cargo = match crate::cargo::find_real_cargo(&self_exe, &path_var) {
        Some(c) => c,
        None => {
            let style = crate::style::Style::stderr();
            eprintln!(
                "{} could not find the real `cargo` on PATH",
                style.error("cargo-overstay:")
            );
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
    // Record usage before Cargo runs so a concurrently finishing worker sees
    // this target as current. Invalid config disables automatic cleanup but
    // never blocks the actual Cargo command.
    let cleanup_enabled = record_usage_and_validate_config(
        workspace.as_deref(),
        target_dir.as_deref(),
        crate::size::now_unix(),
    );

    // Run the real cargo and remember its exit code.
    let code = crate::cargo::run(&cargo, args).unwrap_or(1);

    // Every configured invocation hands post-build scheduling to a detached
    // worker. A nonblocking maintenance lock coalesces concurrent workers; the
    // winner performs cheap throttle checks before any filesystem walk.
    if cleanup_enabled {
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

fn record_usage_and_validate_config(
    workspace: Option<&std::path::Path>,
    target_dir: Option<&std::path::Path>,
    now: i64,
) -> bool {
    let store = crate::store::Store::open(&crate::paths::state_path());
    if let (Some(ws), Some(target)) = (workspace, target_dir) {
        let _ = store.touch(&ws.to_string_lossy(), &target.to_string_lossy(), now);
    }
    match crate::config::load_policy() {
        Ok(_) => true,
        Err(error) => {
            let style = crate::style::Style::stderr();
            eprintln!(
                "{} {error}; automatic cleanup disabled",
                style.error("cargo-overstay:")
            );
            false
        }
    }
}

/// Runs the throttled cleanup pass in what is meant to be a detached child
/// process (spawned by `passthrough`, never waited on). Best-effort end to
/// end: nothing is watching this process's exit code anyway.
pub(crate) fn run_detached_gc(target_arg: Option<&OsStr>) -> i32 {
    let store = crate::store::Store::open(&crate::paths::state_path());
    let Ok(Some(_maintenance_lock)) = store.try_maintenance_lock() else {
        return 0;
    };
    let current_target = target_arg
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from);
    let policy = match crate::config::load_policy() {
        Ok(policy) => policy,
        Err(_) => return 0,
    };
    let cwd = std::env::current_dir().ok();
    let probe_path = current_target.as_deref().or(cwd.as_deref());

    crate::cleanup::run_scheduled_gc(
        &store,
        current_target.as_deref(),
        probe_path,
        crate::size::now_unix(),
        &policy,
    );
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
        let config_path = dir.join("missing-config.toml");

        std::env::set_var("CARGO_OVERSTAY_STATE", &state_path);
        std::env::set_var("CARGO_OVERSTAY_CONFIG", &config_path);
        let code = run_detached_gc(None);
        assert_eq!(code, 0);
        // run_gc should have opened the store and advanced last_gc.
        let last_gc = crate::store::Store::open(&state_path)
            .maintenance_state()
            .last_gc;
        assert!(last_gc > 0);

        std::env::remove_var("CARGO_OVERSTAY_STATE");
        std::env::remove_var("CARGO_OVERSTAY_CONFIG");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn invalid_config_disables_automatic_cleanup() {
        let _env = crate::paths::env_lock();
        let dir = std::env::temp_dir().join(format!("overstay_badconfig_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let state_path = dir.join("test.state");
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, "max_total_size = 150").unwrap();
        std::env::set_var("CARGO_OVERSTAY_STATE", &state_path);
        std::env::set_var("CARGO_OVERSTAY_CONFIG", &config_path);

        let enabled = record_usage_and_validate_config(None, None, 2_000_000_000);

        assert!(!enabled);
        std::env::remove_var("CARGO_OVERSTAY_STATE");
        std::env::remove_var("CARGO_OVERSTAY_CONFIG");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
