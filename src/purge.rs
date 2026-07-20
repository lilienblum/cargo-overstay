//! `cargo overstay purge`: delete store-known build targets, optionally
//! scanning the filesystem for cargo targets overstay has never seen.
//!
//! Deletion confidence is tiered. A dir is deleted outright only when
//! cargo's own droppings prove cargo built it (`CACHEDIR.TAG` mentioning
//! cargo, `.rustc_info.json`, or a compiled profile) — whether it came from
//! a store row or from the scan finding a `target/` with a sibling
//! `Cargo.toml`. Marker-less hits from either source (a stale store path
//! reused by something else, a fresh manifest with an unbuilt target) are
//! listed and gated behind one batch confirmation. A scanned `target/`
//! with no sibling manifest — someone's JS build output, say — is never
//! listed, never touched. Every deletion first takes cargo's build lock,
//! so running builds are skipped.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};

pub(crate) struct Summary {
    pub freed: u64,
    pub deleted: usize,
    pub locked: usize,
}

/// A candidate target with no cargo markers inside.
pub(crate) struct UnsureHit {
    pub target: PathBuf,
    pub size: u64,
}

#[derive(Debug, PartialEq)]
struct Options {
    scan_root: Option<PathBuf>,
}

fn parse_args(args: &[OsString]) -> Result<Options, String> {
    let mut include_untracked = false;
    let mut root = None;
    let mut options = true;
    for arg in args {
        if options && arg == OsStr::new("--") {
            options = false;
        } else if options && arg == OsStr::new("--include-untracked") {
            include_untracked = true;
        } else if options && arg.to_string_lossy().starts_with('-') {
            return Err(format!("unknown option: {}", arg.to_string_lossy()));
        } else if root.is_some() {
            return Err(format!("unexpected argument: {}", arg.to_string_lossy()));
        } else {
            root = Some(PathBuf::from(arg));
        }
    }

    if root.is_some() && !include_untracked {
        return Err("a scan directory requires --include-untracked".to_string());
    }
    let scan_root = include_untracked
        .then(|| root.unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default())));
    Ok(Options { scan_root })
}

pub fn run(args: &[OsString]) -> i32 {
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!(
                "cargo-overstay: {error}\n\
                 usage: cargo overstay purge [--include-untracked [dir]]"
            );
            return 2;
        }
    };
    if let Some(root) = &options.scan_root {
        if !root.is_dir() {
            eprintln!("cargo-overstay: {} is not a directory", root.display());
            return 2;
        }
    }
    let store = crate::store::Store::open(&crate::paths::state_path());
    let summary = purge(&store, options.scan_root.as_deref(), &mut confirm_on_stdin);
    println!(
        "freed {} from {} target dir{}{}",
        crate::size::format_size(summary.freed),
        summary.deleted,
        if summary.deleted == 1 { "" } else { "s" },
        if summary.locked > 0 {
            format!(" ({} skipped: build running)", summary.locked)
        } else {
            String::new()
        },
    );
    0
}

fn confirm_on_stdin(hits: &[UnsureHit]) -> bool {
    let total: u64 = hits.iter().map(|h| h.size).sum();
    println!(
        "\n{} target dir{} without cargo build markers:",
        hits.len(),
        if hits.len() == 1 { "" } else { "s" }
    );
    for h in hits {
        println!(
            "  {:>10}  {}",
            crate::size::format_size(h.size),
            h.target.display()
        );
    }
    print!(
        "delete these too? ({} total) [y/N] ",
        crate::size::format_size(total)
    );
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes")
}

pub(crate) fn purge(
    store: &crate::store::Store,
    scan_root: Option<&Path>,
    confirm: &mut dyn FnMut(&[UnsureHit]) -> bool,
) -> Summary {
    let mut summary = Summary {
        freed: 0,
        deleted: 0,
        locked: 0,
    };

    // Phase 1: every target the store knows about. `seen` keeps an optional
    // scan from re-finding targets this phase handled (or skipped as locked).
    // Rows were recorded from real cargo runs, but the path may have been
    // reused since — a row whose dir no longer carries cargo markers is
    // demoted to the confirmation bucket rather than deleted outright.
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut unsure: Vec<UnsureHit> = Vec::new();
    for e in store.entries() {
        let target = PathBuf::from(&e.target_dir);
        if target.is_dir() {
            if let Ok(canon) = target.canonicalize() {
                seen.insert(canon);
            }
            if is_cargo_target(&target) {
                delete_target(&target, &mut summary);
            } else {
                unsure.push(UnsureHit {
                    size: crate::size::dir_size(&target),
                    target,
                });
            }
        }
    }

    // Phase 2 is opt-in because it discovers targets outside overstay's store.
    if let Some(scan_root) = scan_root {
        let (sure, scanned_unsure) = scan(scan_root, &seen);
        for target in &sure {
            delete_target(target, &mut summary);
        }
        unsure.extend(scanned_unsure);
    }
    if !unsure.is_empty() && confirm(&unsure) {
        for hit in &unsure {
            delete_target(&hit.target, &mut summary);
        }
    }

    // Prune rows whose target is gone — deleted above or already missing.
    let prune: Vec<String> = store
        .entries()
        .into_iter()
        .filter(|e| !Path::new(&e.target_dir).exists())
        .map(|e| e.target_dir)
        .collect();
    let _ = store.remove_targets(&prune);
    summary
}

/// rm -rf one target behind cargo's build lock; prints what happened.
fn delete_target(target: &Path, summary: &mut Summary) {
    let Some(_locks) = crate::trim::lock_target(target) else {
        println!(
            "{:>10}  {}  (skipped: build running)",
            "-",
            target.display()
        );
        summary.locked += 1;
        return;
    };
    let before = crate::size::dir_size(target);
    let _ = std::fs::remove_dir_all(target);
    let freed = before.saturating_sub(crate::size::dir_size(target));
    println!(
        "{:>10}  {}",
        crate::size::format_size(freed),
        target.display()
    );
    summary.freed += freed;
    summary.deleted += 1;
}

const SKIP_DIRS: [&str; 2] = ["node_modules", "Library"];

/// Walks `root` for `target/` dirs with a sibling `Cargo.toml`, splitting
/// them into marker-verified (sure) and marker-less (unsure). Hidden dirs
/// and `SKIP_DIRS` are not entered, symlinks are never followed, and
/// `target` dirs themselves are never descended into.
fn scan(root: &Path, seen: &HashSet<PathBuf>) -> (Vec<PathBuf>, Vec<UnsureHit>) {
    let mut sure = Vec::new();
    let mut unsure = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let has_manifest = dir.join("Cargo.toml").is_file();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            // file_type() does not follow symlinks, so a symlinked dir is
            // neither descended into nor deleted through.
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            let path = entry.path();
            if has_manifest && name == "target" {
                let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
                if !seen.contains(&canon) {
                    if is_cargo_target(&path) {
                        sure.push(path);
                    } else {
                        unsure.push(UnsureHit {
                            size: crate::size::dir_size(&path),
                            target: path,
                        });
                    }
                }
                continue;
            }
            stack.push(path);
        }
    }
    (sure, unsure)
}

/// Proof cargo wrote this dir: its cache-directory tag, its rustc probe
/// cache, or a compiled profile.
fn is_cargo_target(target: &Path) -> bool {
    if std::fs::read_to_string(target.join("CACHEDIR.TAG")).is_ok_and(|tag| tag.contains("cargo")) {
        return true;
    }
    if target.join(".rustc_info.json").is_file() {
        return true;
    }
    !crate::trim::profile_dirs(target).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use std::fs;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("overstay_purge_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A project whose target carries cargo markers (the "sure" tier).
    fn cargo_project(project: &Path) -> PathBuf {
        fs::create_dir_all(project).unwrap();
        fs::write(project.join("Cargo.toml"), "[package]\n").unwrap();
        let target = project.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join(".rustc_info.json"), "{}").unwrap();
        fs::write(target.join("blob.bin"), vec![0u8; 4096]).unwrap();
        target
    }

    fn no_confirm(_: &[UnsureHit]) -> bool {
        panic!("confirmation must not be requested");
    }

    #[test]
    fn include_untracked_purges_tracked_and_scanned_targets() {
        let base = temp("known_sure");
        // Known project lives OUTSIDE the scan root; scanned one inside.
        let known = cargo_project(&base.join("known"));
        let root = base.join("scanroot");
        let found = cargo_project(&root.join("proj"));

        let store = Store::open(&base.join("state"));
        store
            .touch(
                &base.join("known").to_string_lossy(),
                &known.to_string_lossy(),
                1_000,
            )
            .unwrap();

        let summary = purge(&store, Some(&root), &mut no_confirm);
        assert!(!known.exists());
        assert!(!found.exists());
        assert_eq!(summary.deleted, 2);
        assert!(summary.freed >= 8192);
        assert!(store.entries().is_empty());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn tracked_only_purge_leaves_untracked_target_untouched() {
        let base = temp("tracked_only");
        let tracked = cargo_project(&base.join("tracked"));
        let untracked = cargo_project(&base.join("untracked"));
        let store = Store::open(&base.join("state"));
        store
            .touch(
                &base.join("tracked").to_string_lossy(),
                &tracked.to_string_lossy(),
                1_000,
            )
            .unwrap();

        let summary = purge(&store, None, &mut no_confirm);

        assert_eq!(
            (tracked.exists(), untracked.exists(), summary.deleted),
            (false, true, 1)
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn unsure_targets_are_gated_on_confirmation() {
        let base = temp("unsure");
        let project = base.join("proj");
        fs::create_dir_all(project.join("target")).unwrap();
        fs::write(project.join("Cargo.toml"), "[package]\n").unwrap();
        fs::write(project.join("target/stuff.bin"), vec![0u8; 1024]).unwrap();
        let store = Store::open(&base.join("state"));

        // Declined -> survives.
        let mut asked = 0;
        let summary = purge(&store, Some(&base), &mut |hits| {
            asked += 1;
            assert_eq!(hits.len(), 1);
            assert!(hits[0].size >= 1024);
            false
        });
        assert_eq!(asked, 1);
        assert_eq!(summary.deleted, 0);
        assert!(project.join("target/stuff.bin").exists());

        // Accepted -> deleted.
        let summary = purge(&store, Some(&base), &mut |_| true);
        assert_eq!(summary.deleted, 1);
        assert!(!project.join("target").exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn manifestless_target_dirs_are_invisible() {
        let base = temp("nomanifest");
        let junk = base.join("frontend/target");
        fs::create_dir_all(&junk).unwrap();
        fs::write(junk.join("bundle.js"), vec![0u8; 1024]).unwrap();
        let store = Store::open(&base.join("state"));

        let summary = purge(&store, Some(&base), &mut no_confirm);
        assert_eq!(summary.deleted, 0);
        assert!(junk.join("bundle.js").exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn hidden_dirs_and_symlinks_are_not_entered() {
        let base = temp("hidden");
        let hidden = cargo_project(&base.join(".stash/proj"));
        let outside = cargo_project(&base.join("outside-root"));
        let root = base.join("root");
        fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(base.join("outside-root"), root.join("link")).unwrap();
        let store = Store::open(&base.join("state"));

        let summary = purge(&store, Some(&base.join(".stash")), &mut no_confirm);
        // Scanning an explicit root works even if the root itself is hidden…
        assert_eq!(summary.deleted, 1);
        assert!(!hidden.exists());
        // …but a scan never crosses symlinks.
        let summary = purge(&store, Some(&root), &mut no_confirm);
        assert_eq!(summary.deleted, 0);
        assert!(outside.exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn locked_targets_survive_and_keep_their_row() {
        let base = temp("locked");
        let target = cargo_project(&base.join("busy"));
        fs::create_dir_all(target.join("debug/.fingerprint")).unwrap();
        fs::write(target.join("debug/.cargo-lock"), b"").unwrap();
        let store = Store::open(&base.join("state"));
        store
            .touch(
                &base.join("busy").to_string_lossy(),
                &target.to_string_lossy(),
                1_000,
            )
            .unwrap();

        let build = fs::File::open(target.join("debug/.cargo-lock")).unwrap();
        assert!(crate::trim::flock_exclusive_nb(&build));
        let summary = purge(&store, None, &mut no_confirm);
        assert_eq!(summary.locked, 1);
        assert_eq!(summary.deleted, 0);
        assert!(target.exists());
        assert_eq!(store.entries().len(), 1);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn store_rows_without_markers_are_gated_on_confirmation() {
        let base = temp("stale_row");
        // A recorded path that no longer looks cargo-built: no markers inside.
        let target = base.join("proj/target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("data.bin"), vec![0u8; 2048]).unwrap();
        let store = Store::open(&base.join("state"));
        store
            .touch(
                &base.join("proj").to_string_lossy(),
                &target.to_string_lossy(),
                1_000,
            )
            .unwrap();
        // Declined -> survives, row kept.
        let mut asked = 0;
        let summary = purge(&store, None, &mut |hits| {
            asked += 1;
            assert_eq!(hits.len(), 1);
            assert!(hits[0].size >= 2048);
            false
        });
        assert_eq!(asked, 1);
        assert_eq!(summary.deleted, 0);
        assert!(target.join("data.bin").exists());
        assert_eq!(store.entries().len(), 1);

        // Accepted -> deleted, row pruned.
        let summary = purge(&store, None, &mut |_| true);
        assert_eq!(summary.deleted, 1);
        assert!(!target.exists());
        assert!(store.entries().is_empty());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn scan_does_not_recount_known_targets() {
        let base = temp("recount");
        let root = base.join("root");
        let target = cargo_project(&root.join("proj"));
        let store = Store::open(&base.join("state"));
        store
            .touch(
                &root.join("proj").to_string_lossy(),
                &target.to_string_lossy(),
                1_000,
            )
            .unwrap();

        // Known target sits inside the scan root: phase 1 deletes it, the
        // scan must not report it again.
        let summary = purge(&store, Some(&root), &mut no_confirm);
        assert_eq!(summary.deleted, 1);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn parse_args_defaults_to_tracked_only() {
        assert_eq!(parse_args(&[]).unwrap(), Options { scan_root: None });
    }

    #[test]
    fn parse_args_accepts_include_untracked_with_root() {
        let args = [
            OsString::from("--include-untracked"),
            OsString::from("/work"),
        ];
        assert_eq!(
            parse_args(&args).unwrap(),
            Options {
                scan_root: Some(PathBuf::from("/work"))
            }
        );
    }

    #[test]
    fn parse_args_rejects_scan_root_without_opt_in() {
        let error = parse_args(&[OsString::from("/work")]).unwrap_err();
        assert_eq!(error, "a scan directory requires --include-untracked");
    }
}
