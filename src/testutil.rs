//! Shared test fixtures. The cargo target-dir layout assumption (fingerprint
//! entry + hash-suffixed artifacts) is encoded HERE, once — tests in any
//! module build units through these helpers so a layout change is a
//! one-file update.

use std::fs;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

/// Sets a file's atime and mtime to `used_at` (unix seconds).
pub fn stamp(path: &Path, used_at: i64) {
    let t = UNIX_EPOCH + Duration::from_secs(used_at as u64);
    let f = fs::File::options().write(true).open(path).unwrap();
    f.set_times(fs::FileTimes::new().set_accessed(t).set_modified(t))
        .unwrap();
}

/// Creates a compilation unit in `profile` the way cargo lays it out: a
/// one-file `.fingerprint/<name>-<hash>` entry plus a
/// `deps/lib<name>-<hash>.rlib` of `artifact_bytes`, both stamped `used_at`
/// (unix seconds).
pub fn make_unit(profile: &Path, name: &str, hash: &str, artifact_bytes: usize, used_at: i64) {
    let fp = profile.join(".fingerprint").join(format!("{name}-{hash}"));
    fs::create_dir_all(&fp).unwrap();
    fs::write(fp.join("lib"), b"fp").unwrap();
    stamp(&fp.join("lib"), used_at);
    let deps = profile.join("deps");
    fs::create_dir_all(&deps).unwrap();
    let rlib = deps.join(format!("lib{name}-{hash}.rlib"));
    fs::write(&rlib, vec![0u8; artifact_bytes]).unwrap();
    stamp(&rlib, used_at);
}
