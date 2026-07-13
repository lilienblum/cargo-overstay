//! In-place shrinking of an over-cap `target/` directory: delete
//! least-recently-used compilation units until the directory fits the cap,
//! instead of removing it wholesale.
//!
//! A unit is identified the way cargo names it: a `.fingerprint/<name>-<16
//! hex chars>` entry plus every artifact in the profile root, `deps/`,
//! `build/`, and `examples/` carrying the same hash suffix. A unit's
//! last-used time is the newest mtime/atime across its fingerprint files
//! (cargo re-reads these on every build) and its artifacts (so binaries and
//! dylibs read outside cargo — test runs, rust-analyzer — also count as
//! use). On `relatime` mounts atime advances at day granularity, which is
//! fine for this LRU; on `noatime` mounts it degrades to compile-time
//! ordering — the trim then favors deleting the oldest-built units, which
//! cargo transparently rebuilds.
//!
//! Hash-suffixed artifacts whose fingerprint entry is gone are dead to cargo
//! (it never reuses an artifact without its fingerprint), so they are
//! reclaimed first, before any live unit. Files carrying no recognized hash
//! suffix are never touched, and orphan reclaim only engages when the
//! profile has at least one recognized fingerprint entry — if cargo ever
//! changes the naming convention (undocumented internals, stable since
//! ~2019), nothing matches and the trim frees less rather than more.
//!
//! Trimming coordinates with cargo itself: each profile's `.cargo-lock` is
//! flocked (non-blocking) before its units are touched, so a running build
//! makes the trim skip that profile, and a build starting mid-trim queues on
//! cargo's own lock instead of racing the deletions.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};

/// A deletable unit: one compilation unit (artifacts + fingerprint entry,
/// artifacts listed first so an interrupted deletion leaves an orphan the
/// next pass can reclaim), one orphaned-artifact group, or one
/// `incremental/` session dir.
struct Unit {
    /// Live units have a fingerprint entry; orphans (`false`) sort first.
    live: bool,
    last_used: i64,
    size: u64,
    paths: Vec<PathBuf>,
}

/// When even deleting every eligible unit cannot reach the cap (the excess
/// lives in files the trim does not recognize), gutting the whole build
/// cache would not fix anything — only units idle at least this long are
/// deleted then.
const UNREACHABLE_CAP_FLOOR_SECS: i64 = 86_400;

/// Shrinks `target` (whose measured size is `total`) toward `max_bytes`,
/// deleting orphans first and then whole units oldest-first, skipping any
/// unit used within `min_fresh_secs`. Stops when under the cap or out of
/// eligible units. Best-effort; returns the bytes freed.
pub fn trim_to_size(
    target: &Path,
    total: u64,
    max_bytes: u64,
    now: i64,
    min_fresh_secs: i64,
) -> u64 {
    if total <= max_bytes {
        return 0;
    }

    let mut units: Vec<Unit> = Vec::new();
    let mut locks: Vec<File> = Vec::new();
    for profile in profile_dirs(target) {
        match try_lock_profile(&profile) {
            ProfileLock::Busy => continue, // build running -> not ours to touch
            ProfileLock::Held(f) => locks.push(f),
            ProfileLock::Absent => {}
        }
        units.extend(profile_units(&profile, now));
        units.extend(incremental_units(&profile.join("incremental"), now));
    }

    // Orphans are dead weight cargo can never reuse — shed them before any
    // live unit; live units go oldest-first.
    units.sort_by_key(|u| (u.live, u.last_used));

    let deficit = total - max_bytes;
    let eligible: u64 = units
        .iter()
        .filter(|u| now - u.last_used >= min_fresh_secs)
        .map(|u| u.size)
        .sum();
    let floor = if eligible < deficit {
        min_fresh_secs.max(UNREACHABLE_CAP_FLOOR_SECS)
    } else {
        min_fresh_secs
    };

    let mut freed = 0u64;
    for unit in units {
        if total.saturating_sub(freed) <= max_bytes {
            break;
        }
        if now - unit.last_used < floor {
            continue;
        }
        for p in &unit.paths {
            // Cheap unlink first; directories fail it and fall back.
            if std::fs::remove_file(p).is_err() {
                let _ = std::fs::remove_dir_all(p);
            }
        }
        freed += unit.size;
    }
    freed
}

/// Flocks every profile of `target`, for callers about to delete the whole
/// tree. `None` means a build currently owns one of the profiles — deleting
/// a target out from under a build is worse than waiting for the next pass.
/// The returned guards hold the locks until dropped.
pub fn lock_target(target: &Path) -> Option<Vec<File>> {
    let mut locks = Vec::new();
    for profile in profile_dirs(target) {
        match try_lock_profile(&profile) {
            ProfileLock::Busy => return None,
            ProfileLock::Held(f) => locks.push(f),
            ProfileLock::Absent => {}
        }
    }
    Some(locks)
}

enum ProfileLock {
    /// Lock acquired; held until the `File` drops.
    Held(File),
    /// No `.cargo-lock` (or flock unsupported) — proceed unguarded.
    Absent,
    /// A build holds the lock right now.
    Busy,
}

// flock(2), the same lock cargo holds on a profile for a build's duration.
// std already links libc, so a direct declaration keeps the crate
// zero-dependency (same pattern as statvfs in size.rs).
extern "C" {
    fn flock(fd: std::os::raw::c_int, operation: std::os::raw::c_int) -> std::os::raw::c_int;
}
const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;

fn try_lock_profile(profile: &Path) -> ProfileLock {
    let file = match File::open(profile.join(".cargo-lock")) {
        Ok(f) => f,
        Err(_) => return ProfileLock::Absent,
    };
    if flock_exclusive_nb(&file) {
        ProfileLock::Held(file)
    } else if std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
        ProfileLock::Busy
    } else {
        // flock unsupported on this filesystem: proceed unguarded.
        ProfileLock::Absent
    }
}

pub(crate) fn flock_exclusive_nb(file: &File) -> bool {
    use std::os::unix::io::AsRawFd;
    unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) == 0 }
}

/// Directories under `target` holding a compiled profile — `debug`,
/// `release`, custom profiles, and cross-compile layouts like
/// `<triple>/release` — identified by their `.fingerprint` child.
pub(crate) fn profile_dirs(target: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![(target.to_path_buf(), 0u8)];
    while let Some((dir, depth)) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !meta.is_dir() {
                continue;
            }
            let path = entry.path();
            if path.join(".fingerprint").is_dir() {
                found.push(path);
            } else if depth < 2 {
                stack.push((path, depth + 1));
            }
        }
    }
    found
}

/// A unit under construction: artifacts accumulate first, the fingerprint
/// entry (if any) is appended last so deletion order is artifacts-then-
/// fingerprint — an interruption leaves reclaimable orphans, not invisible
/// ones.
struct UnitBuild {
    last_used: i64,
    size: u64,
    artifacts: Vec<PathBuf>,
    fingerprint: Option<PathBuf>,
}

/// Compilation units of one profile dir: each `.fingerprint` entry plus the
/// same-hash artifacts next to it, and — once the layout is recognized —
/// orphan groups for hash-suffixed artifacts whose fingerprint is gone.
/// Files with no recognized hash suffix are never units.
fn profile_units(profile: &Path, now: i64) -> Vec<Unit> {
    let mut live: HashMap<String, UnitBuild> = HashMap::new();
    let entries = match std::fs::read_dir(profile.join(".fingerprint")) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(hash) = hash_suffix(&name) else {
            continue;
        };
        let path = entry.path();
        let (size, newest) = dir_stat(&path);
        live.insert(
            hash.to_string(),
            UnitBuild {
                last_used: newest,
                size,
                artifacts: Vec::new(),
                fingerprint: Some(path),
            },
        );
    }
    // No recognized fingerprint entries means the layout is not the one we
    // know — do not classify anything as an orphan.
    if live.is_empty() {
        return Vec::new();
    }

    let mut orphans: HashMap<String, UnitBuild> = HashMap::new();
    for dir in [
        profile.to_path_buf(),
        profile.join("deps"),
        profile.join("build"),
        profile.join("examples"),
    ] {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let stem = name.split('.').next().unwrap_or("");
            let Some(hash) = hash_suffix(stem) else {
                continue;
            };
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let path = entry.path();
            let (size, newest) = if meta.is_dir() {
                dir_stat(&path)
            } else {
                (meta.len(), file_time(&meta))
            };
            let unit = live.get_mut(hash).unwrap_or_else(|| {
                orphans.entry(hash.to_string()).or_insert(UnitBuild {
                    last_used: 0,
                    size: 0,
                    artifacts: Vec::new(),
                    fingerprint: None,
                })
            });
            unit.size += size;
            // Artifacts being read (linked, executed, loaded by an IDE)
            // keeps the unit fresh, not just fingerprint reads.
            unit.last_used = unit.last_used.max(newest);
            unit.artifacts.push(path);
        }
    }

    live.into_values()
        .chain(orphans.into_values())
        .map(|b| Unit {
            live: b.fingerprint.is_some(),
            // Clamp future timestamps (clock skew, restored archives): they
            // count as "in use now", never as unevictable forever.
            last_used: b.last_used.min(now),
            size: b.size,
            paths: {
                let mut paths = b.artifacts;
                paths.extend(b.fingerprint);
                paths
            },
        })
        .collect()
}

/// Each `incremental/<crate>-<fingerprint>` session dir is its own unit.
/// They are pure cache — often the largest low-value space in a target —
/// so they age out like everything else. Only dirs containing cargo's
/// `s-<stamp>` session entries qualify; anything else under `incremental/`
/// is not ours to delete.
fn incremental_units(incremental: &Path, now: i64) -> Vec<Unit> {
    let mut units = Vec::new();
    let entries = match std::fs::read_dir(incremental) {
        Ok(e) => e,
        Err(_) => return units,
    };
    for entry in entries.flatten() {
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_dir() {
            continue;
        }
        let path = entry.path();
        let is_session = std::fs::read_dir(&path)
            .map(|es| {
                es.flatten()
                    .any(|e| e.file_name().to_string_lossy().starts_with("s-"))
            })
            .unwrap_or(false);
        if !is_session {
            continue;
        }
        let (size, newest) = dir_stat(&path);
        units.push(Unit {
            live: true,
            last_used: newest.min(now),
            size,
            paths: vec![path],
        });
    }
    units
}

/// The trailing `-<16 hex chars>` cargo appends to unit names, if present.
fn hash_suffix(stem: &str) -> Option<&str> {
    let (_, hash) = stem.rsplit_once('-')?;
    (hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hash)
}

/// Size and newest file mtime/atime under `path`, in one walk. Only file
/// times count: scanning a directory updates the directory's own atime, so
/// dir atimes always look fresh once the GC has walked them. A tree holding
/// no files falls back to the dir's own mtime.
fn dir_stat(path: &Path) -> (u64, i64) {
    let mut size = 0u64;
    let mut newest = 0i64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                size += meta.len();
                newest = newest.max(file_time(&meta));
            }
        }
    }
    if newest == 0 {
        newest = std::fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok())
            .map(crate::size::unix_secs)
            .unwrap_or(0);
    }
    (size, newest)
}

fn file_time(meta: &std::fs::Metadata) -> i64 {
    let modified = meta.modified().map(crate::size::unix_secs).unwrap_or(0);
    let accessed = meta.accessed().map(crate::size::unix_secs).unwrap_or(0);
    modified.max(accessed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // A fixed "now" far enough in the future that freshly created files look
    // old unless a test explicitly stamps them fresh.
    const NOW: i64 = 2_000_000_000;
    const DAY: i64 = 86_400;
    const FLOOR: i64 = 600;

    fn trim(target: &Path, max_bytes: u64) -> u64 {
        trim_to_size(target, crate::size::dir_size(target), max_bytes, NOW, FLOOR)
    }

    fn stamp(path: &Path, used_secs_ago: i64) {
        crate::testutil::stamp(path, NOW - used_secs_ago);
    }

    fn temp_target(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("overstay_trim_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A compilation unit last used `used_secs_ago`; layout lives in testutil.
    fn make_unit(
        profile: &Path,
        name: &str,
        hash: &str,
        artifact_bytes: usize,
        used_secs_ago: i64,
    ) {
        crate::testutil::make_unit(profile, name, hash, artifact_bytes, NOW - used_secs_ago);
    }

    #[test]
    fn hash_suffix_requires_16_hex_chars() {
        assert_eq!(
            hash_suffix("serde-1a2b3c4d5e6f7890"),
            Some("1a2b3c4d5e6f7890")
        );
        assert_eq!(
            hash_suffix("my-multi-dash-crate-abcdefabcdefabcd"),
            Some("abcdefabcdefabcd")
        );
        assert_eq!(hash_suffix("foo-123"), None); // too short
        assert_eq!(hash_suffix("foo-1a2b3c4d5e6fg890"), None); // non-hex
        assert_eq!(hash_suffix("nodash"), None);
        assert_eq!(hash_suffix(""), None);
    }

    #[test]
    fn trims_lru_units_and_spares_fresh_and_unmatched() {
        let target = temp_target("lru");
        let profile = target.join("debug");
        make_unit(&profile, "oldest", "aaaaaaaaaaaaaaaa", 8192, 2 * DAY);
        make_unit(&profile, "newer", "bbbbbbbbbbbbbbbb", 8192, DAY);
        fs::write(profile.join("loose.bin"), vec![0u8; 512]).unwrap();

        // Cap forces exactly one eviction; LRU order means "oldest" goes.
        let freed = trim(&target, 9000);
        assert!(freed >= 8192, "freed {freed}");
        assert!(!profile
            .join("deps/liboldest-aaaaaaaaaaaaaaaa.rlib")
            .exists());
        assert!(!profile
            .join(".fingerprint/oldest-aaaaaaaaaaaaaaaa")
            .exists());
        assert!(profile.join("deps/libnewer-bbbbbbbbbbbbbbbb.rlib").exists());
        assert!(profile.join(".fingerprint/newer-bbbbbbbbbbbbbbbb").exists());
        assert!(profile.join("loose.bin").exists());
        fs::remove_dir_all(&target).unwrap();
    }

    #[test]
    fn freshness_floor_protects_hot_units() {
        let target = temp_target("fresh");
        let profile = target.join("debug");
        make_unit(&profile, "hot", "cccccccccccccccc", 8192, FLOOR - 60);

        // Over the cap, but the only unit was used within the floor.
        let freed = trim(&target, 1);
        assert_eq!(freed, 0);
        assert!(profile.join("deps/libhot-cccccccccccccccc.rlib").exists());
        fs::remove_dir_all(&target).unwrap();
    }

    #[test]
    fn artifact_use_keeps_a_unit_fresh() {
        let target = temp_target("artifresh");
        let profile = target.join("debug");
        // Fingerprint is old, but the rlib was read a minute ago (e.g. by
        // rust-analyzer or a directly-executed binary).
        make_unit(&profile, "used", "dddddddddddddd0d", 8192, 2 * DAY);
        stamp(&profile.join("deps/libused-dddddddddddddd0d.rlib"), 60);

        let freed = trim(&target, 1);
        assert_eq!(freed, 0);
        assert!(profile.join("deps/libused-dddddddddddddd0d.rlib").exists());
        fs::remove_dir_all(&target).unwrap();
    }

    #[test]
    fn orphans_are_reclaimed_before_live_units() {
        let target = temp_target("orphan");
        let profile = target.join("debug");
        make_unit(&profile, "live", "aaaaaaaaaaaaaaaa", 8192, 2 * DAY);
        // Younger than the live unit, but its fingerprint is gone — cargo
        // can never reuse it, so it must go first.
        let orphan = profile.join("deps/libgone-ffffffffffffffff.rlib");
        fs::write(&orphan, vec![0u8; 8192]).unwrap();
        stamp(&orphan, DAY);

        let freed = trim(&target, 8300);
        assert!(freed >= 8192, "freed {freed}");
        assert!(!orphan.exists());
        assert!(profile.join("deps/liblive-aaaaaaaaaaaaaaaa.rlib").exists());
        fs::remove_dir_all(&target).unwrap();
    }

    #[test]
    fn unrecognized_layout_reclaims_no_orphans() {
        let target = temp_target("layout");
        let profile = target.join("debug");
        // A .fingerprint dir with no recognizable entries: everything that
        // merely looks hash-suffixed must survive.
        fs::create_dir_all(profile.join(".fingerprint/strange_new_naming")).unwrap();
        let artifact = profile.join("deps/libx-ffffffffffffffff.rlib");
        fs::create_dir_all(profile.join("deps")).unwrap();
        fs::write(&artifact, vec![0u8; 8192]).unwrap();
        stamp(&artifact, 2 * DAY);

        let freed = trim(&target, 1);
        assert_eq!(freed, 0);
        assert!(artifact.exists());
        fs::remove_dir_all(&target).unwrap();
    }

    #[test]
    fn examples_artifacts_are_part_of_the_unit() {
        let target = temp_target("examples");
        let profile = target.join("debug");
        make_unit(&profile, "demo", "eeeeeeeeeeeeeeee", 1024, 2 * DAY);
        let example = profile.join("examples/demo-eeeeeeeeeeeeeeee");
        fs::create_dir_all(profile.join("examples")).unwrap();
        fs::write(&example, vec![0u8; 4096]).unwrap();
        stamp(&example, 2 * DAY);

        let freed = trim(&target, 1);
        assert!(freed >= 5120, "freed {freed}");
        assert!(!example.exists());
        assert!(!profile.join(".fingerprint/demo-eeeeeeeeeeeeeeee").exists());
        fs::remove_dir_all(&target).unwrap();
    }

    #[test]
    fn under_cap_is_a_no_op() {
        let target = temp_target("undercap");
        let profile = target.join("debug");
        make_unit(&profile, "any", "dddddddddddddddd", 1024, 2 * DAY);

        let freed = trim(&target, u64::MAX);
        assert_eq!(freed, 0);
        assert!(profile.join("deps/libany-dddddddddddddddd.rlib").exists());
        fs::remove_dir_all(&target).unwrap();
    }

    #[test]
    fn ages_out_incremental_session_dirs() {
        let target = temp_target("incr");
        let profile = target.join("debug");
        fs::create_dir_all(profile.join(".fingerprint")).unwrap();
        let session = profile.join("incremental/app-2xk9qj3l");
        fs::create_dir_all(&session).unwrap();
        fs::write(session.join("s-abc123-def.lock"), b"").unwrap();
        fs::write(session.join("cache.bin"), vec![0u8; 4096]).unwrap();
        stamp(&session.join("cache.bin"), 2 * DAY);
        stamp(&session.join("s-abc123-def.lock"), 2 * DAY);

        let freed = trim(&target, 1);
        assert!(freed >= 4096, "freed {freed}");
        assert!(!session.exists());
        fs::remove_dir_all(&target).unwrap();
    }

    #[test]
    fn foreign_dirs_under_incremental_survive() {
        let target = temp_target("incrforeign");
        let profile = target.join("debug");
        fs::create_dir_all(profile.join(".fingerprint")).unwrap();
        // No s-* session entries -> not a cargo session dir -> not ours.
        let foreign = profile.join("incremental/user-stash");
        fs::create_dir_all(&foreign).unwrap();
        fs::write(foreign.join("notes.txt"), vec![0u8; 4096]).unwrap();
        stamp(&foreign.join("notes.txt"), 2 * DAY);

        let freed = trim(&target, 1);
        assert_eq!(freed, 0);
        assert!(foreign.join("notes.txt").exists());
        fs::remove_dir_all(&target).unwrap();
    }

    #[test]
    fn finds_cross_compile_profiles() {
        let target = temp_target("cross");
        let profile = target.join("aarch64-unknown-linux-gnu/release");
        make_unit(&profile, "xdep", "eeeeeeeeeeeeeeee", 8192, 2 * DAY);

        let freed = trim(&target, 1);
        assert!(freed >= 8192, "freed {freed}");
        assert!(!profile.join("deps/libxdep-eeeeeeeeeeeeeeee.rlib").exists());
        fs::remove_dir_all(&target).unwrap();
    }

    #[test]
    fn unreachable_cap_only_sheds_stale_units() {
        let target = temp_target("unreach");
        let profile = target.join("debug");
        // 8KiB of unmatched mass makes a 100-byte cap unreachable; the trim
        // must not gut recently-used units chasing it.
        make_unit(&profile, "recent", "aaaaaaaaaaaaaaaa", 8192, 2 * 3600);
        make_unit(&profile, "stale", "bbbbbbbbbbbbbbbb", 8192, 2 * DAY);
        fs::write(profile.join("loose.bin"), vec![0u8; 8192]).unwrap();

        let freed = trim(&target, 100);
        assert!(freed >= 8192, "freed {freed}");
        assert!(!profile.join("deps/libstale-bbbbbbbbbbbbbbbb.rlib").exists());
        assert!(profile
            .join("deps/librecent-aaaaaaaaaaaaaaaa.rlib")
            .exists());
        fs::remove_dir_all(&target).unwrap();
    }

    #[test]
    fn future_timestamps_count_as_in_use_now() {
        let target = temp_target("future");
        let profile = target.join("debug");
        make_unit(&profile, "skewed", "cccccccccccccccc", 8192, -DAY); // future

        let freed = trim(&target, 1);
        assert_eq!(freed, 0);
        assert!(profile
            .join("deps/libskewed-cccccccccccccccc.rlib")
            .exists());
        fs::remove_dir_all(&target).unwrap();
    }

    #[test]
    fn locked_profile_is_skipped() {
        let target = temp_target("locked");
        let profile = target.join("debug");
        make_unit(&profile, "busy", "aaaaaaaaaaaaaaaa", 8192, 2 * DAY);
        fs::write(profile.join(".cargo-lock"), b"").unwrap();

        // Hold cargo's build lock the way a running build would.
        let build = fs::File::open(profile.join(".cargo-lock")).unwrap();
        assert!(flock_exclusive_nb(&build));
        let freed = trim(&target, 1);
        assert_eq!(freed, 0);
        assert!(profile.join("deps/libbusy-aaaaaaaaaaaaaaaa.rlib").exists());

        // Lock released -> the next pass trims.
        drop(build);
        let freed = trim(&target, 1);
        assert!(freed >= 8192, "freed {freed}");
        fs::remove_dir_all(&target).unwrap();
    }

    #[test]
    fn lock_target_reports_running_builds() {
        let target = temp_target("locktgt");
        let profile = target.join("debug");
        fs::create_dir_all(profile.join(".fingerprint")).unwrap();
        fs::write(profile.join(".cargo-lock"), b"").unwrap();

        let build = fs::File::open(profile.join(".cargo-lock")).unwrap();
        assert!(flock_exclusive_nb(&build));
        assert!(lock_target(&target).is_none());

        drop(build);
        assert!(lock_target(&target).is_some());
        fs::remove_dir_all(&target).unwrap();
    }
}
