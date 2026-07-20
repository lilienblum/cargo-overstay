use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub struct Entry {
    pub path: String,
    pub target_dir: String,
    pub last_used: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MaintenanceState {
    pub last_gc: i64,
    pub budget_pending: bool,
}

/// A tiny persistent `target_dir -> (path, last_used)` map plus a `last_gc`
/// timestamp, stored as a text file and guarded by a std advisory file lock so
/// concurrent overstay processes (one per `cargo` invocation) never corrupt or
/// clobber it.
///
/// File format: line 1 is `<last_gc>\t<budget_pending>`; each remaining line
/// is `<last_used>\t<path>\t<target_dir>`. Legacy integer-only headers are
/// accepted with `budget_pending` defaulting to false.
///
/// Older state files used `<last_used>\t<path>` and are still accepted; their
/// target directory is interpreted as `<path>/target`.
pub struct Store {
    path: PathBuf,
    lock_path: PathBuf,
}

impl Store {
    pub fn open(path: &Path) -> Store {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let lock_path = path.with_extension("lock");
        Store {
            path: path.to_path_buf(),
            lock_path,
        }
    }

    /// Parse the state file. Missing/unreadable -> (0, empty). Lock-free:
    /// writers replace the file atomically via rename, so a reader always sees
    /// a complete old or new file, never a partial one. Malformed lines are
    /// skipped (best-effort).
    fn read(&self) -> (MaintenanceState, Vec<Entry>) {
        let content = match std::fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(_) => return (MaintenanceState::default(), Vec::new()),
        };
        let mut lines = content.lines();
        let maintenance = lines.next().map(parse_maintenance).unwrap_or_default();
        let mut entries = Vec::new();
        for line in lines {
            let Some(entry) = parse_entry(line) else {
                continue;
            };
            if let Some(index) = entries
                .iter()
                .position(|e: &Entry| e.target_dir == entry.target_dir)
            {
                if entry.last_used >= entries[index].last_used {
                    entries[index] = entry;
                }
            } else {
                entries.push(entry);
            }
        }
        (maintenance, entries)
    }

    pub fn maintenance_state(&self) -> MaintenanceState {
        self.read().0
    }

    pub fn entries(&self) -> Vec<Entry> {
        self.read().1
    }

    /// Exclusive read-modify-write under an advisory file lock, so concurrent
    /// overstay processes serialize and none clobbers another's update (each
    /// call re-reads the current file under the lock before editing). The lock
    /// is held only for this in-memory edit plus a small atomic rewrite — never
    /// during slow work — so it cannot meaningfully delay a build.
    ///
    /// When `blocking` is false the lock is taken with `try_lock`: if another
    /// process holds it, this call is a no-op (`Ok(())`). That is used on the
    /// build's critical path so a `touch` never waits — dropping the occasional
    /// timestamp update under contention is acceptable (the mtime guard still
    /// protects an actively-building project from eviction).
    fn with_lock(
        &self,
        blocking: bool,
        edit: impl FnOnce(&mut MaintenanceState, &mut Vec<Entry>),
    ) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&self.lock_path)?;
        if blocking {
            lock.lock()?; // released when `lock` drops at end of scope
        } else {
            // A held lock is usually gone within milliseconds (it is only
            // ever held for an in-memory edit plus a small rewrite, and a
            // subprocess spawned elsewhere in the process can pin a released
            // lock for the fork->exec window). Retry briefly before giving
            // up so transient contention doesn't drop the update; the bound
            // keeps the worst case well under human-noticeable delay.
            let mut acquired = false;
            for attempt in 0..5 {
                if attempt > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                match lock.try_lock() {
                    Ok(()) => {
                        acquired = true;
                        break;
                    }
                    Err(std::fs::TryLockError::WouldBlock) => continue,
                    Err(std::fs::TryLockError::Error(e)) => return Err(e),
                }
            }
            if !acquired {
                return Ok(()); // skip, best-effort
            }
        }
        let (mut maintenance, mut entries) = self.read();
        edit(&mut maintenance, &mut entries);
        self.write_atomic(maintenance, &entries)
    }

    fn write_atomic(&self, maintenance: MaintenanceState, entries: &[Entry]) -> io::Result<()> {
        let tmp = self.path.with_extension("tmp");
        let mut buf = String::new();
        buf.push_str(&maintenance.last_gc.to_string());
        buf.push('\t');
        buf.push(if maintenance.budget_pending { '1' } else { '0' });
        buf.push('\n');
        for e in entries {
            buf.push_str(&e.last_used.to_string());
            buf.push('\t');
            buf.push_str(&e.path);
            buf.push('\t');
            buf.push_str(&e.target_dir);
            buf.push('\n');
        }
        {
            let mut f = File::create(&tmp)?;
            f.write_all(buf.as_bytes())?;
        }
        // No fsync: this is best-effort tracking data, and the atomic rename
        // already guarantees a reader never sees a torn file. Skipping the
        // sync keeps the (locked) critical section off the disk-flush path.
        std::fs::rename(&tmp, &self.path)
    }

    /// Records `target_dir`'s last-used time and latest project path.
    /// Non-blocking: on the build's critical path, so it skips (best-effort)
    /// rather than wait if the lock is held.
    pub fn touch(&self, path: &str, target_dir: &str, now: i64) -> io::Result<()> {
        self.with_lock(false, |_maintenance, entries| {
            if let Some(e) = entries.iter_mut().find(|e| e.target_dir == target_dir) {
                e.path = path.to_string();
                e.last_used = now;
            } else {
                entries.push(Entry {
                    path: path.to_string(),
                    target_dir: target_dir.to_string(),
                    last_used: now,
                });
            }
        })
    }

    pub fn finish_maintenance(
        &self,
        now: i64,
        budget_pending: bool,
        removed_targets: &[String],
    ) -> io::Result<()> {
        self.with_lock(true, |maintenance, entries| {
            *maintenance = MaintenanceState {
                last_gc: now,
                budget_pending,
            };
            entries.retain(|entry| {
                !removed_targets
                    .iter()
                    .any(|target| target == &entry.target_dir)
            });
        })
    }

    pub fn remove_targets(&self, target_dirs: &[String]) -> io::Result<()> {
        if target_dirs.is_empty() {
            return Ok(());
        }
        self.with_lock(true, |_maintenance, entries| {
            entries.retain(|e| {
                !target_dirs
                    .iter()
                    .any(|target_dir| target_dir == &e.target_dir)
            });
        })
    }

    /// Coalesces detached maintenance workers without holding the short-lived
    /// state-file lock during filesystem walks.
    pub fn try_maintenance_lock(&self) -> io::Result<Option<File>> {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.path.with_extension("gc.lock"))?;
        match lock.try_lock() {
            Ok(()) => Ok(Some(lock)),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }
}

fn parse_maintenance(line: &str) -> MaintenanceState {
    let mut parts = line.splitn(2, '\t');
    MaintenanceState {
        last_gc: parts
            .next()
            .and_then(|value| value.trim().parse::<i64>().ok())
            .unwrap_or_default(),
        budget_pending: parts.next().is_some_and(|value| value.trim() == "1"),
    }
}

fn parse_entry(line: &str) -> Option<Entry> {
    let mut parts = line.splitn(3, '\t');
    let ts = parts.next()?;
    let path = parts.next()?;
    let target_dir = parts.next().map(str::to_string).unwrap_or_else(|| {
        Path::new(path)
            .join("target")
            .to_string_lossy()
            .into_owned()
    });
    let last_used = ts.trim().parse::<i64>().ok()?;
    Some(Entry {
        path: path.to_string(),
        target_dir,
        last_used,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> (Store, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("overstay_store_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state");
        (Store::open(&path), dir)
    }

    #[test]
    fn touch_upserts_and_updates_last_used() {
        let (store, dir) = temp_store("touch");
        store.touch("a", "a/target", 100).unwrap();
        store.touch("a", "a/target", 200).unwrap();
        store.touch("b", "b/target", 150).unwrap();
        let entries = store.entries();
        assert_eq!(entries.len(), 2);
        let a = entries.iter().find(|e| e.path == "a").unwrap();
        let b = entries.iter().find(|e| e.path == "b").unwrap();
        assert_eq!(a.last_used, 200);
        assert_eq!(a.target_dir, "a/target");
        assert_eq!(b.last_used, 150);
        assert_eq!(b.target_dir, "b/target");
        assert_eq!(store.maintenance_state().last_gc, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn touch_tracks_multiple_target_dirs_for_one_workspace() {
        let (store, dir) = temp_store("multi_target");
        store.touch("a", "a/target", 100).unwrap();
        store.touch("a", "/tmp/a-target", 200).unwrap();

        let entries = store.entries();
        assert_eq!(entries.len(), 2);
        assert!(entries
            .iter()
            .any(|e| e.path == "a" && e.target_dir == "a/target" && e.last_used == 100));
        assert!(entries
            .iter()
            .any(|e| e.path == "a" && e.target_dir == "/tmp/a-target" && e.last_used == 200));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn touch_upserts_by_target_dir() {
        let (store, dir) = temp_store("shared_target");
        store.touch("old", "/tmp/shared-target", 100).unwrap();
        store.touch("new", "/tmp/shared-target", 200).unwrap();

        let entries = store.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "new");
        assert_eq!(entries[0].target_dir, "/tmp/shared-target");
        assert_eq!(entries[0].last_used, 200);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn maintenance_state_and_touch_preserve_each_other() {
        let (store, dir) = temp_store("preserve");
        store.touch("a", "a/target", 100).unwrap();
        store.finish_maintenance(500, true, &[]).unwrap();
        store.touch("b", "b/target", 200).unwrap();
        assert_eq!(
            store.maintenance_state(),
            MaintenanceState {
                last_gc: 500,
                budget_pending: true
            }
        );
        let entries = store.entries();
        assert_eq!(entries.len(), 2);
        assert!(entries
            .iter()
            .any(|e| e.path == "a" && e.target_dir == "a/target" && e.last_used == 100));
        assert!(entries
            .iter()
            .any(|e| e.path == "b" && e.target_dir == "b/target" && e.last_used == 200));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn touch_rides_out_briefly_held_lock() {
        let (store, dir) = temp_store("contended");
        // Hold the advisory lock from an independent handle for a few ms —
        // as a subprocess spawn elsewhere in the process briefly does by
        // inheriting lock fds — then release. touch must land anyway.
        let lock_path = dir.join("state.lock");
        let holder = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .unwrap();
        holder.lock().unwrap();
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(4));
            drop(holder); // releases the lock
        });
        store.touch("a", "a/target", 100).unwrap();
        t.join().unwrap();
        assert_eq!(store.entries().len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn remove_drops_only_named_targets() {
        let (store, dir) = temp_store("remove");
        store.touch("a", "a/target", 100).unwrap();
        store.touch("a", "/tmp/a-target", 150).unwrap();
        store.touch("b", "b/target", 200).unwrap();
        store.touch("c", "c/target", 300).unwrap();
        store
            .remove_targets(&["a/target".to_string(), "c/target".to_string()])
            .unwrap();
        let entries = store.entries();
        assert_eq!(entries.len(), 2);
        assert!(entries
            .iter()
            .any(|e| e.path == "a" && e.target_dir == "/tmp/a-target"));
        assert!(entries
            .iter()
            .any(|e| e.path == "b" && e.target_dir == "b/target"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_legacy_rows_with_default_target_dir() {
        let (store, dir) = temp_store("legacy");
        std::fs::write(&store.path, "0\n123\t/work/proj\n").unwrap();

        let entries = store.entries();
        assert_eq!(store.maintenance_state(), MaintenanceState::default());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "/work/proj");
        assert_eq!(entries[0].target_dir, "/work/proj/target");
        assert_eq!(entries[0].last_used, 123);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_duplicate_target_rows_as_one_freshest_entry() {
        let (store, dir) = temp_store("duplicate_targets");
        std::fs::write(
            &store.path,
            "0\n100\t/old/project\t/shared-target\n200\t/new/project\t/shared-target\n",
        )
        .unwrap();

        let entries = store.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "/new/project");
        assert_eq!(entries[0].target_dir, "/shared-target");
        assert_eq!(entries[0].last_used, 200);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fresh_store_on_nonexistent_path_reads_empty() {
        let dir =
            std::env::temp_dir().join(format!("overstay_store_{}_nonexistent", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir.join("state"));
        assert_eq!(store.maintenance_state().last_gc, 0);
        assert!(store.entries().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn maintenance_lock_coalesces_workers() {
        let (store, dir) = temp_store("maintenance_lock");
        let first = store.try_maintenance_lock().unwrap().unwrap();

        assert!(store.try_maintenance_lock().unwrap().is_none());
        drop(first);
        assert!(store.try_maintenance_lock().unwrap().is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn finish_maintenance_updates_state_and_removes_targets_atomically() {
        let (store, dir) = temp_store("finish_maintenance");
        store.touch("a", "a/target", 100).unwrap();
        store.touch("b", "b/target", 200).unwrap();

        store
            .finish_maintenance(500, true, &["a/target".to_string()])
            .unwrap();

        assert_eq!(
            store.maintenance_state(),
            MaintenanceState {
                last_gc: 500,
                budget_pending: true
            }
        );
        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.entries()[0].target_dir, "b/target");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
