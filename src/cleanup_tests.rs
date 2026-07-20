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
fn cand_disk(id: i64, path: &str, last_used: i64, size: u64, volume: u64, free: u64) -> Candidate {
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
    let report = run_gc(&store, None, far, &default_policy());
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
    let report = run_gc(&store, None, far, &default_policy());
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
    let report = run_gc(&store, Some(&target), far, &policy);
    assert!(target.join("big.bin").exists());
    assert_eq!(report.freed, 0);
    assert!(!report.limits_satisfied);
    assert!(store.maintenance_state().budget_pending);
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
    let report = run_gc(&store, Some(&target), now, &policy);
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
    let base = std::env::temp_dir().join(format!("overstay_gc_budget_trim_{}", std::process::id()));
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
    let report = run_gc(&store, Some(&current_target), now, &policy);

    assert_eq!(
        (current_target.exists(), old_target.exists()),
        (true, false)
    );
    assert!(!report.limits_satisfied);
    assert!(store.maintenance_state().budget_pending);
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn scheduled_gc_runs_for_new_overage_inside_normal_throttle() {
    use crate::store::Store;
    let base = std::env::temp_dir().join(format!("overstay_gc_scheduled_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let current = base.join("current");
    let current_target = target_dir(&current);
    let old = base.join("old");
    let old_target = target_dir(&old);
    std::fs::create_dir_all(&current_target).unwrap();
    std::fs::create_dir_all(&old_target).unwrap();
    std::fs::write(current_target.join("keep.bin"), vec![0u8; 1024]).unwrap();
    std::fs::write(old_target.join("drop.bin"), vec![0u8; 4096]).unwrap();

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
    store.finish_maintenance(now - 1, false, &[]).unwrap();

    let report = run_scheduled_gc(
        &store,
        Some(&current_target),
        Some(&base),
        now,
        &policy(2 * 1024, u64::MAX, i64::MAX),
    );

    assert!(report.is_some());
    assert!(current_target.exists());
    assert!(!old_target.exists());
    assert!(!store.maintenance_state().budget_pending);
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn scheduled_gc_retries_unresolved_budget_on_short_interval() {
    use crate::store::Store;
    let base = std::env::temp_dir().join(format!("overstay_gc_retry_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let current = base.join("current");
    let current_target = target_dir(&current);
    std::fs::create_dir_all(&current_target).unwrap();
    std::fs::write(current_target.join("keep.bin"), vec![0u8; 4096]).unwrap();

    let store = Store::open(&base.join("state"));
    let now = 2_000_000_000i64;
    store
        .touch(
            &current.to_string_lossy(),
            &current_target.to_string_lossy(),
            now,
        )
        .unwrap();
    let policy = policy(1, u64::MAX, i64::MAX);
    let report = run_gc(&store, Some(&current_target), now, &policy);
    assert!(!report.limits_satisfied);

    let early = run_scheduled_gc(
        &store,
        Some(&current_target),
        None,
        now + BUDGET_RETRY_SECS - 1,
        &policy,
    );
    let retry = run_scheduled_gc(
        &store,
        Some(&current_target),
        None,
        now + BUDGET_RETRY_SECS,
        &policy,
    );

    assert!(early.is_none());
    assert!(retry.is_some());
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn run_gc_keeps_eviction_until_budget_when_lru_target_is_locked() {
    use crate::store::Store;
    let base = std::env::temp_dir().join(format!("overstay_gc_budget_lock_{}", std::process::id()));
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
    run_gc(&store, None, now, &policy(4096, u64::MAX, i64::MAX));

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
    let report = run_gc(&store, None, far, &default_policy());
    assert_eq!(report.freed, 0);
    assert!(target.join("debug/big.bin").exists());

    // Lock released -> the next pass reclaims.
    drop(build);
    let report = run_gc(&store, None, far, &default_policy());
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
    let report = run_gc(&store, Some(&target), far, &default_policy());
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
