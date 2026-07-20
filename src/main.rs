//! `cargo-overstay`, a multi-call binary: invoked as `cargo` (through the
//! shim symlink) it forwards to the real cargo and runs maintenance
//! transparently; invoked under any other name it is the
//! `cargo overstay <command>` CLI.

use std::ffi::{OsStr, OsString};
use std::path::Path;

mod cargo;
mod cleanup;
mod cli;
mod config;
mod paths;
mod purge;
mod shim;
mod size;
mod store;
#[cfg(test)]
mod testutil;
mod trim;
mod workspace;

fn main() {
    let mut argv = std::env::args_os();
    let argv0 = argv.next();
    let args: Vec<OsString> = argv.collect();
    std::process::exit(match dispatch(argv0.as_deref(), &args) {
        Route::DetachedGc(target) => shim::run_detached_gc(target),
        Route::Passthrough => shim::passthrough(&args),
        Route::Cli(rest) => cli::run_cli(rest),
    });
}

/// Where one invocation goes, decided purely from argv — see `dispatch`.
#[derive(Debug)]
enum Route<'a> {
    DetachedGc(Option<&'a OsStr>),
    Passthrough,
    Cli(&'a [OsString]),
}

fn dispatch<'a>(argv0: Option<&OsStr>, args: &'a [OsString]) -> Route<'a> {
    // The detached-GC child is re-exec'd via `current_exe`, which can carry
    // either of the shim symlink's names depending on platform, so its verb
    // must outrank the name dispatch below.
    if args.first().map(OsString::as_os_str) == Some(OsStr::new(shim::GC_DETACHED_VERB)) {
        return Route::DetachedGc(args.get(1).map(OsString::as_os_str));
    }

    // A program named exactly `cargo` — the shim symlink — is a pure
    // passthrough, so no overstay verb can ever shadow a cargo subcommand.
    let name = argv0.map(Path::new).and_then(Path::file_name);
    if name == Some(OsStr::new("cargo")) {
        return Route::Passthrough;
    }

    // Otherwise this is the CLI. Cargo's external-subcommand convention
    // passes the matched subcommand — this binary's name minus its `cargo-`
    // prefix — as the first argument; strip it so `cargo overstay purge`
    // and a direct `cargo-overstay purge` look the same.
    let subcommand = name
        .and_then(OsStr::to_str)
        .and_then(|n| n.strip_prefix("cargo-"));
    let convention_call = subcommand
        .zip(args.first())
        .is_some_and(|(sub, first)| first.as_os_str() == OsStr::new(sub));
    if convention_call {
        Route::Cli(&args[1..])
    } else {
        Route::Cli(args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn gc_verb_outranks_the_name_dispatch() {
        let args = os(&["__gc-detached", "/some/target"]);
        // Even under the `cargo` name (macOS current_exe can keep the
        // symlink path), the detached child must reach the GC.
        let route = dispatch(Some(OsStr::new("/x/bin/cargo")), &args);
        assert!(matches!(route, Route::DetachedGc(Some(t)) if t == OsStr::new("/some/target")));
    }

    #[test]
    fn cargo_name_is_pure_passthrough_even_for_cli_verbs() {
        let args = os(&["purge"]);
        let route = dispatch(Some(OsStr::new("/x/.cargo-overstay/bin/cargo")), &args);
        assert!(matches!(route, Route::Passthrough));
    }

    #[test]
    fn cargo_convention_token_is_stripped() {
        let args = os(&["overstay", "ls"]);
        let route = dispatch(Some(OsStr::new("/usr/bin/cargo-overstay")), &args);
        assert!(matches!(route, Route::Cli(rest) if rest == &args[1..]));
    }

    #[test]
    fn direct_cli_invocation_is_not_stripped() {
        let args = os(&["purge", "/scan/root"]);
        let route = dispatch(Some(OsStr::new("cargo-overstay")), &args);
        assert!(matches!(route, Route::Cli(rest) if rest == &args[..]));
    }

    #[test]
    fn alias_binaries_strip_their_own_token() {
        // `ln -s cargo-overstay cargo-os` + `cargo os ls` must still work:
        // the stripped token derives from argv0, not a hardcoded name.
        let args = os(&["os", "ls"]);
        let route = dispatch(Some(OsStr::new("/x/bin/cargo-os")), &args);
        assert!(matches!(route, Route::Cli(rest) if rest == &args[1..]));
    }

    #[test]
    fn missing_argv0_still_reaches_the_cli() {
        let args = os(&["ls"]);
        let route = dispatch(None, &args);
        assert!(matches!(route, Route::Cli(rest) if rest == &args[..]));
    }
}
