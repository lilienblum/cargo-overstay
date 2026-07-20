use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub enum Reason {
    Inactive,
    OverProjectCap,
    OverBudget,
    LowDisk,
}

impl Reason {
    /// Whether this reason reclaims the whole target dir. `OverProjectCap`
    /// is the one partial trim; everything else removes the tree.
    fn full_reclaim(&self) -> bool {
        !matches!(self, Reason::OverProjectCap)
    }
}

pub struct Policy {
    pub max_total_cache: u64,
    pub max_project_size: u64,
    pub inactive_secs: i64,
    pub low_disk_trigger: u64,
    pub low_disk_recover: u64,
}

#[derive(Clone)]
pub struct Candidate {
    pub id: i64,
    pub target_dir: PathBuf,
    pub last_used: i64,
    pub size: u64,
    /// st_dev of the target's volume; None when the probe failed.
    pub volume: Option<u64>,
    /// Free bytes reported for the target's volume; None when the probe failed.
    pub free_space: Option<u64>,
}

#[derive(Debug, PartialEq)]
pub struct Eviction {
    pub id: i64,
    pub target_dir: PathBuf,
    pub reason: Reason,
}

pub fn should_run_gc(last_gc: i64, now: i64, interval_secs: i64) -> bool {
    now - last_gc >= interval_secs
}

/// Whether low free disk space makes cleanup due despite the normal 6-hour
/// throttle. Still enforces LOW_DISK_THROTTLE_SECS since the last pass so a
/// full disk with nothing evictable doesn't re-run GC on every cargo call.
pub fn should_run_gc_low_disk(last_gc: i64, now: i64, free: Option<u64>, trigger: u64) -> bool {
    matches!(free, Some(f) if f < trigger) && now - last_gc >= LOW_DISK_THROTTLE_SECS
}

pub fn select_evictions(
    candidates: &[Candidate],
    policy: &Policy,
    now: i64,
    current_target: Option<&Path>,
) -> Vec<Eviction> {
    let is_current = |c: &Candidate| current_target.is_some_and(|p| p == c.target_dir);
    let mut evictions: Vec<Eviction> = Vec::new();
    let mut evicted: HashSet<i64> = HashSet::new();

    // Pass 1: inactivity, then per-project cap. The current target is exempt
    // from full-reclaim reasons but still gets the cap's partial trim — an
    // actively used project can be the very thing filling the disk.
    for c in candidates {
        if is_current(c) {
            if c.size > policy.max_project_size {
                evictions.push(Eviction {
                    id: c.id,
                    target_dir: c.target_dir.clone(),
                    reason: Reason::OverProjectCap,
                });
                evicted.insert(c.id);
            }
            continue;
        }
        if now - c.last_used > policy.inactive_secs {
            evictions.push(Eviction {
                id: c.id,
                target_dir: c.target_dir.clone(),
                reason: Reason::Inactive,
            });
            evicted.insert(c.id);
        } else if c.size > policy.max_project_size {
            evictions.push(Eviction {
                id: c.id,
                target_dir: c.target_dir.clone(),
                reason: Reason::OverProjectCap,
            });
            evicted.insert(c.id);
        }
    }

    // Pass 2: global budget. This is an optimistic first pass: over-cap
    // targets are counted at the size their trim is meant to reach, avoiding
    // unnecessary whole-target eviction when trimming succeeds. `run_gc_with`
    // remeasures afterward and enforces the budget against actual bytes, since
    // fresh, locked, or unrecognized artifacts can make a trim fall short.
    let total: u64 = candidates
        .iter()
        .filter(|c| !is_current(c))
        .map(|c| c.size.min(policy.max_project_size))
        .sum();
    if total > policy.max_total_cache {
        // Same capped basis as `total`, so an inactive over-cap target's
        // reclaim is not credited beyond what it contributed.
        let mut freed: u64 = evictions
            .iter()
            .filter(|e| e.reason.full_reclaim())
            .filter_map(|e| candidates.iter().find(|c| c.id == e.id))
            .map(|c| c.size.min(policy.max_project_size))
            .sum();
        let mut remaining: Vec<&Candidate> = candidates
            .iter()
            .filter(|c| !is_current(c) && !evicted.contains(&c.id))
            .collect();
        remaining.sort_by_key(|c| c.last_used); // oldest first
        for c in remaining {
            if total.saturating_sub(freed) <= policy.max_total_cache {
                break;
            }
            evictions.push(Eviction {
                id: c.id,
                target_dir: c.target_dir.clone(),
                reason: Reason::OverBudget,
            });
            evicted.insert(c.id);
            freed += c.size;
        }
    }

    // Pass 3: low disk. For each volume reporting free space below the
    // trigger, evict LRU until the recovery target is predicted free.
    // Full reclaims from earlier passes count toward recovery;
    // OverProjectCap is a partial trim, so it does not.
    let mut volumes: Vec<u64> = candidates.iter().filter_map(|c| c.volume).collect();
    volumes.sort_unstable();
    volumes.dedup();
    for vol in volumes {
        let free = candidates
            .iter()
            .filter(|c| c.volume == Some(vol))
            .filter_map(|c| c.free_space)
            .min();
        let free = match free {
            Some(f) if f < policy.low_disk_trigger => f,
            _ => continue,
        };
        let mut freed: u64 = evictions
            .iter()
            .filter(|e| e.reason.full_reclaim())
            .filter_map(|e| candidates.iter().find(|c| c.id == e.id))
            .filter(|c| c.volume == Some(vol))
            .map(|c| c.size)
            .sum();
        let mut remaining: Vec<&Candidate> = candidates
            .iter()
            .filter(|c| c.volume == Some(vol) && !is_current(c) && !evicted.contains(&c.id))
            .collect();
        remaining.sort_by_key(|c| c.last_used); // oldest first
        for c in remaining {
            if free.saturating_add(freed) >= policy.low_disk_recover {
                break;
            }
            evictions.push(Eviction {
                id: c.id,
                target_dir: c.target_dir.clone(),
                reason: Reason::LowDisk,
            });
            evicted.insert(c.id);
            freed += c.size;
        }
    }

    evictions
}

pub fn is_idle_enough(modified_unix: i64, now: i64, min_idle_secs: i64) -> bool {
    now - modified_unix >= min_idle_secs
}

pub fn is_target_evictable(target: &Path, now: i64, min_idle_secs: i64) -> bool {
    let meta = match std::fs::metadata(target) {
        Ok(m) => m,
        Err(_) => return false, // missing target -> nothing to reclaim
    };
    if !meta.is_dir() {
        return false;
    }
    let modified_unix = meta.modified().map(crate::size::unix_secs).unwrap_or(0);
    is_idle_enough(modified_unix, now, min_idle_secs)
}

/// Frees space for one eviction. Over-cap targets — including the current
/// one — are trimmed in place, LRU units first, never removed wholesale;
/// every other reason reclaims the whole target dir. `size` is the target's
/// already-measured size (no re-walk). Returns bytes freed.
fn reclaim(
    target: &Path,
    reason: &Reason,
    size: u64,
    policy: &Policy,
    now: i64,
    is_current: bool,
) -> u64 {
    if !reason.full_reclaim() {
        return crate::trim::trim_to_size(
            target,
            size,
            policy.max_project_size,
            now,
            MIN_IDLE_SECS,
        );
    }
    // Structural backstop: never rm the current target, whatever selection
    // produced — a selection bug must not cost the tree we just built.
    if is_current {
        return 0;
    }
    // A held cargo build lock means a build is running in this target right
    // now; deleting it out from under the build is worse than waiting for
    // the next pass. The guards stay held while the tree goes away.
    let Some(_locks) = crate::trim::lock_target(target) else {
        return 0;
    };
    match std::fs::remove_dir_all(target) {
        Ok(()) => size,
        Err(_) => size.saturating_sub(crate::size::dir_size(target)),
    }
}

/// Remeasures surviving targets after the planned cleanup and removes LRU
/// targets until their actual combined size fits the global budget. The
/// current target contributes to the total but is never removed, so callers
/// can observe a remaining excess when protected data alone exceeds the cap.
fn enforce_total_budget(
    candidates: &[Candidate],
    policy: &Policy,
    current_target: Option<&Path>,
    now: i64,
) -> (u64, usize) {
    let is_current = |c: &Candidate| current_target.is_some_and(|p| p == c.target_dir);
    let mut remaining: Vec<(&Candidate, u64)> = candidates
        .iter()
        .filter(|c| c.target_dir.is_dir())
        .map(|c| (c, crate::size::dir_size(&c.target_dir)))
        .collect();
    let mut total: u64 = remaining.iter().map(|(_, size)| size).sum();
    remaining.sort_by_key(|(c, _)| c.last_used);

    let mut freed = 0u64;
    let mut evicted = 0usize;
    for (candidate, size) in remaining {
        if total <= policy.max_total_cache {
            break;
        }
        if is_current(candidate) || !is_target_evictable(&candidate.target_dir, now, MIN_IDLE_SECS)
        {
            continue;
        }
        let reclaimed = reclaim(
            &candidate.target_dir,
            &Reason::OverBudget,
            size,
            policy,
            now,
            false,
        );
        total = total.saturating_sub(reclaimed);
        freed += reclaimed;
        evicted += 1;
    }
    (freed, evicted)
}

#[allow(dead_code)]
pub struct GcReport {
    pub freed: u64,
    pub evicted: usize,
    pub scanned: usize,
}

// Fixed defaults — overstay is zero-config. The size strings are the single
// source of truth; `default_policy` parses them back into bytes.
pub(crate) const MAX_TOTAL_CACHE_STR: &str = "75GiB";
const MAX_PROJECT_SIZE_STR: &str = "10GiB";
const INACTIVE_DAYS: i64 = 30;
// Low-disk trigger: evict LRU on a volume once its free space falls below the
// trigger, until the recovery target is predicted free. Recover > trigger
// gives hysteresis so builds near the line don't re-fire cleanup every run.
const LOW_DISK_TRIGGER_STR: &str = "10GiB";
const LOW_DISK_RECOVER_STR: &str = "20GiB";

/// Minimum seconds a target (for whole-dir reclaim) or a compilation unit
/// (for the in-place trim's freshness floor) must have been idle before it
/// may be deleted. One knob, two layers: raising it makes both reclaim paths
/// more conservative.
pub(crate) const MIN_IDLE_SECS: i64 = 10 * 60;

/// Minimum hours between background cleanup passes.
pub const THROTTLE_HOURS: i64 = 6;
/// Minimum seconds between passes when the trigger is low free disk space —
/// short enough to react, long enough that a full-but-unevictable disk does
/// not spawn a GC child on every cargo call.
pub const LOW_DISK_THROTTLE_SECS: i64 = 15 * 60;

pub fn default_policy() -> Policy {
    Policy {
        // These parse calls cannot fail — the strings are valid constants, and
        // `default_policy_parses` guards that.
        max_total_cache: crate::size::parse_size(MAX_TOTAL_CACHE_STR).unwrap(),
        max_project_size: crate::size::parse_size(MAX_PROJECT_SIZE_STR).unwrap(),
        inactive_secs: INACTIVE_DAYS * 86400,
        low_disk_trigger: crate::size::parse_size(LOW_DISK_TRIGGER_STR).unwrap(),
        low_disk_recover: crate::size::parse_size(LOW_DISK_RECOVER_STR).unwrap(),
    }
}

pub fn run_gc(store: &crate::store::Store, current_target: Option<&Path>, now: i64) -> GcReport {
    run_gc_with(store, current_target, now, &default_policy())
}

fn run_gc_with(
    store: &crate::store::Store,
    current_target: Option<&Path>,
    now: i64,
    policy: &Policy,
) -> GcReport {
    let entries = store.entries();

    // Measure sizes (slow work, done WITHOUT holding the store lock). Give each
    // candidate a unique synthetic id (its index) so select_evictions' dedup
    // works unchanged.
    let mut candidates = Vec::new();
    let mut gone: Vec<String> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let target = PathBuf::from(&e.target_dir);
        if !target.exists() {
            gone.push(e.target_dir.clone());
            continue;
        }
        let size = crate::size::dir_size(&target);
        let volume = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&target).ok().map(|m| m.dev())
        };
        let free_space = crate::size::free_space(&target);
        candidates.push(Candidate {
            id: i as i64,
            target_dir: target,
            last_used: e.last_used,
            size,
            volume,
            free_space,
        });
    }
    let scanned = candidates.len();

    let evictions = select_evictions(&candidates, policy, now, current_target);
    let mut freed = 0u64;
    let mut evicted = 0usize;
    for ev in &evictions {
        // The idle gate protects other sessions' in-use targets from deletion.
        // The current target skips it: its mtime is fresh by construction (we
        // just ran cargo there), and its reclaim is a trim, never an rm.
        // Both reclaim paths additionally take cargo's own build lock, so a
        // build running (or starting) in a target never races the deletes.
        let is_current = current_target.is_some_and(|p| p == ev.target_dir);
        if !is_current && !is_target_evictable(&ev.target_dir, now, MIN_IDLE_SECS) {
            continue;
        }
        let size = candidates
            .iter()
            .find(|c| c.id == ev.id)
            .map(|c| c.size)
            .unwrap_or(0);
        freed += reclaim(&ev.target_dir, &ev.reason, size, policy, now, is_current);
        evicted += 1;
    }

    // Trimming is deliberately best-effort: it cannot touch fresh, locked, or
    // unrecognized artifacts. Base the hard-budget fallback on a fresh walk,
    // not the optimistic per-project caps used during selection.
    let (budget_freed, budget_evicted) =
        enforce_total_budget(&candidates, policy, current_target, now);
    freed += budget_freed;
    evicted += budget_evicted;

    // Prune rows whose target no longer exists: the originally-missing ones plus
    // any we just fully reclaimed. Then record the run. Both are best-effort.
    let mut to_remove = gone;
    for c in &candidates {
        if !c.target_dir.exists() {
            to_remove.push(c.target_dir.to_string_lossy().into_owned());
        }
    }
    let _ = store.remove_targets(&to_remove);
    let _ = store.set_last_gc(now);

    GcReport {
        freed,
        evicted,
        scanned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: i64, path: &str, last_used: i64, size: u64) -> Candidate {
        Candidate {
            id,
            target_dir: PathBuf::from(path).join("target"),
            last_used,
            size,
            volume: None,
            free_space: None,
        }
    }
    fn cand_disk(
        id: i64,
        path: &str,
        last_used: i64,
        size: u64,
        volume: u64,
        free: u64,
    ) -> Candidate {
        Candidate {
            id,
            target_dir: PathBuf::from(path).join("target"),
            last_used,
            size,
            volume: Some(volume),
            free_space: Some(free),
        }
    }

    fn low_disk_policy(trigger: u64, recover: u64) -> Policy {
        Policy {
            max_total_cache: u64::MAX,
            max_project_size: u64::MAX,
            inactive_secs: i64::MAX,
            low_disk_trigger: trigger,
            low_disk_recover: recover,
        }
    }

    fn policy(total: u64, project: u64, inactive_secs: i64) -> Policy {
        Policy {
            max_total_cache: total,
            max_project_size: project,
            inactive_secs,
            low_disk_trigger: 0,
            low_disk_recover: 0,
        }
    }
    fn target_dir(project: &Path) -> PathBuf {
        project.join("target")
    }

    #[test]
    fn evicts_inactive() {
        let now = 1_000_000;
        let cs = vec![
            cand(1, "/old", now - 40 * 86400, 100),
            cand(2, "/fresh", now - 86400, 100),
        ];
        let ev = select_evictions(&cs, &policy(u64::MAX, u64::MAX, 30 * 86400), now, None);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].id, 1);
        assert_eq!(ev[0].reason, Reason::Inactive);
    }

    #[test]
    fn evicts_over_project_cap() {
        let now = 1_000_000;
        let cs = vec![cand(1, "/big", now, 30)];
        let ev = select_evictions(&cs, &policy(u64::MAX, 20, 30 * 86400), now, None);
        assert_eq!(
            ev,
            vec![Eviction {
                id: 1,
                target_dir: PathBuf::from("/big/target"),
                reason: Reason::OverProjectCap,
            }]
        );
    }

    #[test]
    fn evicts_lru_until_under_budget() {
        let now = 1_000_000;
        // total 300, budget 150 -> must free 150+. LRU order: c(oldest), b, a
        let cs = vec![
            cand(1, "/a", now - 10, 100),
            cand(2, "/b", now - 20, 100),
            cand(3, "/c", now - 30, 100),
        ];
        let ev = select_evictions(&cs, &policy(150, u64::MAX, 30 * 86400), now, None);
        // frees /c (30) then /b (20): 300 - 200 = 100 <= 150
        assert_eq!(ev.iter().map(|e| e.id).collect::<Vec<_>>(), vec![3, 2]);
        assert!(ev.iter().all(|e| e.reason == Reason::OverBudget));
    }

    #[test]
    fn budget_counts_over_cap_targets_at_their_post_trim_size() {
        let now = 1_000_000;
        // /big is 100 over a 50 cap -> will be trimmed to 50. Counted at 50,
        // the total (50 + 40 = 90) fits the 100 budget, so /other survives
        // (at full size, 140 > 100 would have evicted it).
        let cs = vec![
            cand(1, "/big", now - 10, 100),
            cand(2, "/other", now - 20, 40),
        ];
        let ev = select_evictions(&cs, &policy(100, 50, 30 * 86400), now, None);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].id, 1);
        assert_eq!(ev[0].reason, Reason::OverProjectCap);
    }

    #[test]
    fn current_target_over_cap_gets_partial_trim_eviction() {
        let now = 1_000_000;
        let cs = vec![cand(1, "/cur", now, 30)];
        let ev = select_evictions(
            &cs,
            &policy(u64::MAX, 20, 30 * 86400),
            now,
            Some(Path::new("/cur/target")),
        );
        assert_eq!(
            ev,
            vec![Eviction {
                id: 1,
                target_dir: PathBuf::from("/cur/target"),
                reason: Reason::OverProjectCap,
            }]
        );
    }

    #[test]
    fn never_evicts_current() {
        let now = 1_000_000;
        let cs = vec![cand(1, "/old", now - 40 * 86400, 100)];
        let ev = select_evictions(
            &cs,
            &policy(u64::MAX, u64::MAX, 30 * 86400),
            now,
            Some(Path::new("/old/target")),
        );
        assert!(ev.is_empty());
    }

    #[test]
    fn throttle_gate() {
        assert!(!should_run_gc(1000, 1000 + 3599, 3600));
        assert!(should_run_gc(1000, 1000 + 3600, 3600));
    }

    #[test]
    fn idle_gate() {
        assert!(!is_idle_enough(1000, 1000 + 599, 600)); // too fresh
        assert!(is_idle_enough(1000, 1000 + 600, 600));
    }

    #[test]
    fn evictable_respects_missing_and_fresh_target() {
        let base = std::env::temp_dir().join(format!("overstay_evict_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let proj = base.join("p");
        std::fs::create_dir_all(&proj).unwrap();
        // no target yet
        assert!(!is_target_evictable(&target_dir(&proj), 2_000_000_000, 600));
        // fresh target
        std::fs::create_dir_all(proj.join("target")).unwrap();
        // now is 'just after' its mtime -> not idle enough with 600s window
        assert!(!is_target_evictable(
            &target_dir(&proj),
            super_now(&proj),
            600
        ));
        std::fs::remove_dir_all(&base).unwrap();
    }

    // Helper: the target's own mtime as unix secs (so the test is clock-independent).
    fn super_now(proj: &std::path::Path) -> i64 {
        let m = std::fs::metadata(proj.join("target"))
            .unwrap()
            .modified()
            .unwrap();
        m.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64 + 10
    }

    #[test]
    fn run_gc_reclaims_inactive_and_removes_stale_rows() {
        use crate::store::Store;
        let base = std::env::temp_dir().join(format!("overstay_gc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let stale = base.join("stale"); // recorded but target already gone
        let old = base.join("old"); // inactive with a real target
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::create_dir_all(old.join("target")).unwrap();
        std::fs::write(old.join("target/blob.bin"), vec![0u8; 4096]).unwrap();

        let store = Store::open(&base.join("state"));
        let now = 2_000_000_000i64;
        store
            .touch(
                &stale.to_string_lossy(),
                &target_dir(&stale).to_string_lossy(),
                now - 100 * 86400,
            )
            .unwrap();
        store
            .touch(
                &old.to_string_lossy(),
                &target_dir(&old).to_string_lossy(),
                now - 100 * 86400,
            )
            .unwrap();

        // min idle is 600s but target mtime is ~now, so use a far-future
        // `now` so the idle gate passes. The default inactive window
        // (30 days) makes /old eligible for whole-dir reclaim.
        let far = now + 10 * 86400;
        let report = run_gc(&store, None, far);
        assert_eq!(report.scanned, 1); // stale row dropped before scan count
        assert!(report.freed >= 4096);
        assert!(!old.join("target").exists());
        let remaining = store.entries();
        assert!(remaining.iter().all(|e| e.path != stale.to_string_lossy()));
        assert!(remaining.iter().all(|e| e.path != old.to_string_lossy()));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn run_gc_reclaims_custom_target_dir() {
        use crate::store::Store;
        let base = std::env::temp_dir().join(format!("overstay_gc_custom_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let project = base.join("project");
        let default_target = project.join("target");
        let custom_target = base.join("custom-target");
        std::fs::create_dir_all(&default_target).unwrap();
        std::fs::create_dir_all(&custom_target).unwrap();
        std::fs::write(default_target.join("keep.bin"), vec![0u8; 1024]).unwrap();
        std::fs::write(custom_target.join("remove.bin"), vec![0u8; 4096]).unwrap();

        let store = Store::open(&base.join("state"));
        let now = 2_000_000_000i64;
        store
            .touch(
                &project.to_string_lossy(),
                &custom_target.to_string_lossy(),
                now - 100 * 86400,
            )
            .unwrap();

        let far = now + 10 * 86400;
        let report = run_gc(&store, None, far);
        assert_eq!(report.scanned, 1);
        assert!(report.freed >= 4096);
        assert!(!custom_target.exists());
        assert!(default_target.exists());
        assert!(store.entries().is_empty());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn run_gc_never_rms_current_target_even_over_cap() {
        use crate::store::Store;
        let base = std::env::temp_dir().join(format!("overstay_gc_cur_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let project = base.join("current");
        let target = project.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("big.bin"), vec![0u8; 4096]).unwrap();

        let store = Store::open(&base.join("state"));
        let now = 2_000_000_000i64;
        store
            .touch(&project.to_string_lossy(), &target.to_string_lossy(), now)
            .unwrap();

        // Cap of 1 byte puts the current target over the per-project cap.
        // Its reclaim is a trim, and the trim only deletes recognized
        // compilation units — a bare file in the target root must survive.
        // Far-future `now` so the idle gate is not what protects it.
        let policy = Policy {
            max_total_cache: u64::MAX,
            max_project_size: 1,
            inactive_secs: i64::MAX,
            low_disk_trigger: 0,
            low_disk_recover: 0,
        };
        let far = now + 10 * 86400;
        let report = run_gc_with(&store, Some(&target), far, &policy);
        assert!(target.join("big.bin").exists());
        assert_eq!(report.freed, 0);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn run_gc_trims_fresh_current_target_over_cap() {
        use crate::store::Store;
        let base = std::env::temp_dir().join(format!("overstay_gc_curtrim_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let project = base.join("current");
        let target = project.join("target");

        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("big.bin"), vec![0u8; 4096]).unwrap();

        let store = Store::open(&base.join("state"));
        let now = super_now(&project); // just after target mtime -> NOT idle
                                       // A stale unit, 2 days back: with the unmatched big.bin making the
                                       // 1-byte cap unreachable, the trim escalates its floor to a day —
                                       // the unit must be clearly past that.
        crate::testutil::make_unit(
            &target.join("debug"),
            "old",
            "aaaaaaaaaaaaaaaa",
            4096,
            now - 2 * 86400,
        );
        let fp = target.join("debug/.fingerprint/old-aaaaaaaaaaaaaaaa");
        let deps = target.join("debug/deps");
        store
            .touch(&project.to_string_lossy(), &target.to_string_lossy(), now)
            .unwrap();

        let policy = Policy {
            max_total_cache: u64::MAX,
            max_project_size: 1,
            inactive_secs: i64::MAX,
            low_disk_trigger: 0,
            low_disk_recover: 0,
        };
        let report = run_gc_with(&store, Some(&target), now, &policy);
        // The current target is trimmed despite being freshly modified: the
        // idle gate exists to avoid deleting another session's in-use target,
        // but trimming stale units of our own just-built target is safe.
        assert!(!fp.exists());
        assert!(!deps.join("libold-aaaaaaaaaaaaaaaa.rlib").exists());
        assert!(target.join("big.bin").exists()); // trimmed, not rm'd
        assert!(report.freed >= 4096);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn run_gc_enforces_budget_after_trim_falls_short() {
        use crate::store::Store;
        let base =
            std::env::temp_dir().join(format!("overstay_gc_budget_trim_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let current = base.join("current");
        let current_target = target_dir(&current);
        let old = base.join("old");
        let old_target = target_dir(&old);
        std::fs::create_dir_all(&current_target).unwrap();
        std::fs::create_dir_all(&old_target).unwrap();
        std::fs::write(current_target.join("keep.bin"), vec![0u8; 12 * 1024]).unwrap();
        std::fs::write(old_target.join("drop.bin"), vec![0u8; 8 * 1024]).unwrap();

        let store = Store::open(&base.join("state"));
        let now = 2_000_000_000i64;
        store
            .touch(
                &current.to_string_lossy(),
                &current_target.to_string_lossy(),
                now,
            )
            .unwrap();
        store
            .touch(
                &old.to_string_lossy(),
                &old_target.to_string_lossy(),
                now - 1,
            )
            .unwrap();

        let policy = policy(16 * 1024, 4 * 1024, i64::MAX);
        run_gc_with(&store, Some(&current_target), now, &policy);

        assert_eq!(
            (current_target.exists(), old_target.exists()),
            (true, false)
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn run_gc_keeps_eviction_until_budget_when_lru_target_is_locked() {
        use crate::store::Store;
        let base =
            std::env::temp_dir().join(format!("overstay_gc_budget_lock_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let busy = base.join("busy");
        let middle = base.join("middle");
        let newest = base.join("newest");
        for project in [&busy, &middle, &newest] {
            let target = target_dir(project);
            std::fs::create_dir_all(target.join("debug/.fingerprint")).unwrap();
            std::fs::write(target.join("blob.bin"), vec![0u8; 4096]).unwrap();
        }
        std::fs::write(target_dir(&busy).join("debug/.cargo-lock"), b"").unwrap();

        let store = Store::open(&base.join("state"));
        let now = 2_000_000_000i64;
        for (project, age) in [(&busy, 30), (&middle, 20), (&newest, 10)] {
            store
                .touch(
                    &project.to_string_lossy(),
                    &target_dir(project).to_string_lossy(),
                    now - age,
                )
                .unwrap();
        }

        let build = std::fs::File::open(target_dir(&busy).join("debug/.cargo-lock")).unwrap();
        assert!(crate::trim::flock_exclusive_nb(&build));
        run_gc_with(&store, None, now, &policy(4096, u64::MAX, i64::MAX));

        assert_eq!(
            (
                target_dir(&busy).exists(),
                target_dir(&middle).exists(),
                target_dir(&newest).exists(),
            ),
            (true, false, false)
        );
        drop(build);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn run_gc_skips_targets_a_build_holds_locked() {
        use crate::store::Store;
        let base = std::env::temp_dir().join(format!("overstay_gc_lock_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let project = base.join("busy");
        let target = project.join("target");
        std::fs::create_dir_all(target.join("debug/.fingerprint")).unwrap();
        std::fs::write(target.join("debug/.cargo-lock"), b"").unwrap();
        std::fs::write(target.join("debug/big.bin"), vec![0u8; 4096]).unwrap();

        let store = Store::open(&base.join("state"));
        let now = 2_000_000_000i64;
        store
            .touch(
                &project.to_string_lossy(),
                &target.to_string_lossy(),
                now - 100 * 86400, // long inactive -> whole-dir reclaim due
            )
            .unwrap();

        // Hold cargo's build lock the way a running build would: the idle
        // gate alone can't see a build that only writes into subdirs.
        let build = std::fs::File::open(target.join("debug/.cargo-lock")).unwrap();
        assert!(crate::trim::flock_exclusive_nb(&build));
        let far = now + 10 * 86400;
        let report = run_gc(&store, None, far);
        assert_eq!(report.freed, 0);
        assert!(target.join("debug/big.bin").exists());

        // Lock released -> the next pass reclaims.
        drop(build);
        let report = run_gc(&store, None, far);
        assert!(report.freed >= 4096);
        assert!(!target.exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn run_gc_does_not_reclaim_current_target_from_another_workspace_row() {
        use crate::store::Store;
        let base = std::env::temp_dir().join(format!("overstay_gc_shared_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let current_project = base.join("current");
        let target = base.join("shared-target");
        std::fs::create_dir_all(&current_project).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("keep.bin"), vec![0u8; 4096]).unwrap();

        let store = Store::open(&base.join("state"));
        let now = 2_000_000_000i64;
        store
            .touch(
                &base.join("old").to_string_lossy(),
                &target.to_string_lossy(),
                now - 100 * 86400,
            )
            .unwrap();

        let far = now + 10 * 86400;
        let report = run_gc(&store, Some(&target), far);
        assert_eq!(report.evicted, 0);
        assert!(target.exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn default_policy_parses() {
        // The constant size strings must parse, and match the documented defaults.
        let p = default_policy();
        assert_eq!(p.max_total_cache, 75 * 1024 * 1024 * 1024);
        assert_eq!(p.max_project_size, 10 * 1024 * 1024 * 1024);
        assert_eq!(p.inactive_secs, 30 * 86400);
        assert_eq!(p.low_disk_trigger, 10 * 1024 * 1024 * 1024);
        assert_eq!(p.low_disk_recover, 20 * 1024 * 1024 * 1024);
        assert!(p.low_disk_recover > p.low_disk_trigger);
    }

    #[test]
    fn low_disk_evicts_lru_until_recovery() {
        let now = 1_000_000;
        // Volume 7 reports 5 free; trigger 10, recover 20 -> need 15 more.
        // LRU order: /c (oldest), /b, /a. Evicting /c (10) + /b (10) reaches 25.
        let cs = vec![
            cand_disk(1, "/a", now - 10, 10, 7, 5),
            cand_disk(2, "/b", now - 20, 10, 7, 5),
            cand_disk(3, "/c", now - 30, 10, 7, 5),
        ];
        let ev = select_evictions(&cs, &low_disk_policy(10, 20), now, None);
        assert_eq!(ev.iter().map(|e| e.id).collect::<Vec<_>>(), vec![3, 2]);
        assert!(ev.iter().all(|e| e.reason == Reason::LowDisk));
    }

    #[test]
    fn low_disk_trigger_is_strict() {
        let now = 1_000_000;
        // free == trigger -> not low, nothing evicted.
        let cs = vec![cand_disk(1, "/a", now - 10, 10, 7, 10)];
        assert!(select_evictions(&cs, &low_disk_policy(10, 20), now, None).is_empty());
    }

    #[test]
    fn low_disk_only_touches_the_low_volume() {
        let now = 1_000_000;
        let cs = vec![
            cand_disk(1, "/low", now - 30, 10, 7, 5),    // volume 7: low
            cand_disk(2, "/fine", now - 40, 10, 8, 100), // volume 8: healthy
        ];
        let ev = select_evictions(&cs, &low_disk_policy(10, 20), now, None);
        assert_eq!(ev.iter().map(|e| e.id).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn low_disk_counts_prior_full_reclaims_toward_recovery() {
        let now = 1_000_000;
        // /old is Inactive (pass 1) and its 20 alone recovers the volume
        // (5 free + 20 freed >= 20), so no extra LowDisk evictions.
        let old = cand_disk(1, "/old", now - 40 * 86400, 20, 7, 5);
        let fresh = cand_disk(2, "/fresh", now - 10, 20, 7, 5);
        let policy = Policy {
            max_total_cache: u64::MAX,
            max_project_size: u64::MAX,
            inactive_secs: 30 * 86400,
            low_disk_trigger: 10,
            low_disk_recover: 20,
        };
        let ev = select_evictions(&[old, fresh], &policy, now, None);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].reason, Reason::Inactive);
    }

    #[test]
    fn low_disk_ignores_partial_trims_in_recovery_accounting() {
        let now = 1_000_000;
        // /big is over the project cap (partial trim) — its size must NOT
        // count toward low-disk recovery, so /other still gets evicted.
        let big = cand_disk(1, "/big", now - 100, 30, 7, 5);
        let other = cand_disk(2, "/other", now - 50, 20, 7, 5);
        let policy = Policy {
            max_total_cache: u64::MAX,
            max_project_size: 25,
            inactive_secs: i64::MAX,
            low_disk_trigger: 10,
            low_disk_recover: 20,
        };
        let ev = select_evictions(&[big, other], &policy, now, None);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].reason, Reason::OverProjectCap);
        assert_eq!(ev[1].id, 2);
        assert_eq!(ev[1].reason, Reason::LowDisk);
    }

    #[test]
    fn low_disk_skips_unprobed_candidates_and_current() {
        let now = 1_000_000;
        let mut unprobed = cand_disk(1, "/unprobed", now - 30, 10, 0, 0);
        unprobed.volume = None;
        unprobed.free_space = None;
        let current = cand_disk(2, "/current", now - 40, 10, 7, 5);
        let ev = select_evictions(
            &[unprobed, current],
            &low_disk_policy(10, 20),
            now,
            Some(Path::new("/current/target")),
        );
        assert!(ev.is_empty());
    }

    #[test]
    fn low_disk_throttle_gate() {
        let floor = LOW_DISK_THROTTLE_SECS;
        // Low disk + floor elapsed -> due.
        assert!(should_run_gc_low_disk(1000, 1000 + floor, Some(5), 10));
        // Low disk but within the floor -> not due.
        assert!(!should_run_gc_low_disk(1000, 1000 + floor - 1, Some(5), 10));
        // Plenty of space -> not due (boundary: free == trigger is not low).
        assert!(!should_run_gc_low_disk(1000, 1000 + floor, Some(10), 10));
        // Probe failed -> not due.
        assert!(!should_run_gc_low_disk(1000, 1000 + floor, None, 10));
    }
}
