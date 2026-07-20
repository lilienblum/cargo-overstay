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

#[derive(Debug)]
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

struct Inventory {
    candidates: Vec<Candidate>,
    gone: Vec<String>,
}

impl Inventory {
    fn measure(store: &crate::store::Store) -> Self {
        let mut candidates = Vec::new();
        let mut gone = Vec::new();
        for (id, entry) in store.entries().into_iter().enumerate() {
            let target = PathBuf::from(&entry.target_dir);
            if !target.exists() {
                gone.push(entry.target_dir);
                continue;
            }
            let size = crate::size::dir_size(&target);
            let volume = {
                use std::os::unix::fs::MetadataExt;
                std::fs::metadata(&target)
                    .ok()
                    .map(|metadata| metadata.dev())
            };
            let free_space = crate::size::free_space(&target);
            candidates.push(Candidate {
                id: id as i64,
                target_dir: target,
                last_used: entry.last_used,
                size,
                volume,
                free_space,
            });
        }
        Self { candidates, gone }
    }

    fn exceeds_limits(&self, policy: &Policy) -> bool {
        let total = self
            .candidates
            .iter()
            .map(|candidate| candidate.size)
            .fold(0u64, u64::saturating_add);
        total > policy.max_total_cache
            || self
                .candidates
                .iter()
                .any(|candidate| candidate.size > policy.max_project_size)
    }
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
    // unnecessary whole-target eviction when trimming succeeds. `run_gc`
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
) -> LimitOutcome {
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
    for (candidate, size) in &mut remaining {
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
            *size,
            policy,
            now,
            false,
        );
        total = total.saturating_sub(reclaimed);
        freed += reclaimed;
        *size = (*size).saturating_sub(reclaimed);
        evicted += 1;
    }
    let oversized_target = remaining
        .iter()
        .any(|(candidate, size)| candidate.target_dir.exists() && *size > policy.max_project_size);
    LimitOutcome {
        freed,
        evicted,
        limits_satisfied: total <= policy.max_total_cache && !oversized_target,
    }
}

struct LimitOutcome {
    freed: u64,
    evicted: usize,
    limits_satisfied: bool,
}

#[allow(dead_code)]
pub struct GcReport {
    pub freed: u64,
    pub evicted: usize,
    pub scanned: usize,
    pub limits_satisfied: bool,
}

// Built-in defaults. The size strings are the single source of truth;
// `default_policy` parses them back into bytes before config overrides apply.
const MAX_TOTAL_CACHE_STR: &str = "75GiB";
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
/// Minimum delay before retrying a pass that could not satisfy size limits
/// because protected, fresh, or locked targets remained.
pub const BUDGET_RETRY_SECS: i64 = 15 * 60;

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

pub fn run_gc(
    store: &crate::store::Store,
    current_target: Option<&Path>,
    now: i64,
    policy: &Policy,
) -> GcReport {
    run_gc_with_inventory(
        store,
        current_target,
        now,
        policy,
        Inventory::measure(store),
    )
}

/// Runs maintenance when its normal interval, low disk, a pending retry, or
/// post-build size growth makes work due. Returns `None` when the cheap checks
/// and, when necessary, one reusable inventory show there is nothing to do.
pub fn run_scheduled_gc(
    store: &crate::store::Store,
    current_target: Option<&Path>,
    probe_path: Option<&Path>,
    now: i64,
    policy: &Policy,
) -> Option<GcReport> {
    let maintenance = store.maintenance_state();
    let periodic_due = should_run_gc(maintenance.last_gc, now, THROTTLE_HOURS * 3600);
    let low_disk_due = should_run_gc_low_disk(
        maintenance.last_gc,
        now,
        probe_path.and_then(crate::size::free_space),
        policy.low_disk_trigger,
    );
    let retry_due =
        maintenance.budget_pending && should_run_gc(maintenance.last_gc, now, BUDGET_RETRY_SECS);
    if periodic_due || low_disk_due || retry_due {
        return Some(run_gc(store, current_target, now, policy));
    }
    if maintenance.budget_pending || current_target.is_none() {
        return None;
    }

    let inventory = Inventory::measure(store);
    inventory
        .exceeds_limits(policy)
        .then(|| run_gc_with_inventory(store, current_target, now, policy, inventory))
}

fn run_gc_with_inventory(
    store: &crate::store::Store,
    current_target: Option<&Path>,
    now: i64,
    policy: &Policy,
    inventory: Inventory,
) -> GcReport {
    let Inventory { candidates, gone } = inventory;
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
    let limits = enforce_total_budget(&candidates, policy, current_target, now);
    freed += limits.freed;
    evicted += limits.evicted;

    // Prune rows whose target no longer exists: the originally-missing ones plus
    // any we just fully reclaimed. Then record the run. Both are best-effort.
    let mut to_remove = gone;
    for c in &candidates {
        if !c.target_dir.exists() {
            to_remove.push(c.target_dir.to_string_lossy().into_owned());
        }
    }
    let _ = store.finish_maintenance(now, !limits.limits_satisfied, &to_remove);

    GcReport {
        freed,
        evicted,
        scanned,
        limits_satisfied: limits.limits_satisfied,
    }
}

#[cfg(test)]
#[path = "cleanup_tests.rs"]
mod tests;
