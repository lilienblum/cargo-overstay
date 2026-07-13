use std::path::Path;

/// A `SystemTime` as unix seconds; 0 on epoch-underflow or error.
pub fn unix_secs(t: std::time::SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn now_unix() -> i64 {
    unix_secs(std::time::SystemTime::now())
}

/// Human-readable size in binary units, one decimal.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [(u64, &str); 4] = [
        (1 << 40, "TiB"),
        (1 << 30, "GiB"),
        (1 << 20, "MiB"),
        (1 << 10, "KiB"),
    ];
    for (factor, unit) in UNITS {
        if bytes >= factor {
            return format!("{:.1} {}", bytes as f64 / factor as f64, unit);
        }
    }
    format!("{bytes} B")
}

pub fn parse_size(input: &str) -> Result<u64, &'static str> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty size");
    }
    let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let (num_part, unit_part) = s.split_at(split);
    let num: f64 = num_part.trim().parse().map_err(|_| "invalid size number")?;
    let mult: f64 = match unit_part.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kib" => 1024.0,
        "kb" => 1_000.0,
        "m" | "mib" => 1024f64.powi(2),
        "mb" => 1_000_000.0,
        "g" | "gib" => 1024f64.powi(3),
        "gb" => 1_000_000_000.0,
        "t" | "tib" => 1024f64.powi(4),
        "tb" => 1_000_000_000_000.0,
        _ => return Err("unknown size unit"),
    };
    Ok((num * mult) as u64)
}

/// Sum the sizes of regular files under `path`. Symlinks are not followed (so
/// there are no cycles and no double-counting), and unreadable entries are
/// skipped. A missing path yields 0. Iterative (an explicit stack) so a deep
/// tree can't overflow the call stack.
pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            // `DirEntry::metadata` does not traverse symlinks, so symlinked
            // dirs are not descended into and cannot cause cycles.
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total += meta.len();
            }
            // Symlinks (to files or dirs) are intentionally not followed.
        }
    }
    total
}

// Free-space probe via a direct statvfs(3) declaration — std already links
// libc, so this stays zero-dependency. The struct layout is OS-specific:
// macOS uses 32-bit block counts, 64-bit Linux uses 64-bit ones.
// Darwin caveat: fsblkcnt_t is 32-bit, so f_bavail wraps every 16 TiB of
// free space (at 4 KiB frsize). A wrapped reading could make a huge volume
// look "low"; the damage is bounded by the eviction safeguards (current
// target protected, idle gate, stops at the recovery target).

#[cfg(target_os = "macos")]
#[repr(C)]
struct StatVfs {
    f_bsize: u64,
    f_frsize: u64,
    f_blocks: u32,
    f_bfree: u32,
    f_bavail: u32,
    f_files: u32,
    f_ffree: u32,
    f_favail: u32,
    f_fsid: u64,
    f_flag: u64,
    f_namemax: u64,
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
#[repr(C)]
struct StatVfs {
    f_bsize: u64,
    f_frsize: u64,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_favail: u64,
    f_fsid: u64,
    f_flag: u64,
    f_namemax: u64,
    __f_spare: [i32; 6],
}

#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", target_pointer_width = "64")
))]
extern "C" {
    fn statvfs(path: *const std::os::raw::c_char, buf: *mut StatVfs) -> i32;
}

/// Bytes available to unprivileged processes on the volume holding `path`
/// (`f_bavail * f_frsize`). `None` on any failure; callers treat `None` as
/// "low-disk handling inactive".
#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", target_pointer_width = "64")
))]
pub fn free_space(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut buf = std::mem::MaybeUninit::<StatVfs>::zeroed();
    let rc = unsafe { statvfs(c_path.as_ptr(), buf.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    let s = unsafe { buf.assume_init() };
    Some((s.f_bavail as u64).saturating_mul(s.f_frsize))
}

#[cfg(not(any(
    target_os = "macos",
    all(target_os = "linux", target_pointer_width = "64")
)))]
pub fn free_space(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes_in_binary_units() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(10 * 1024), "10.0 KiB");
        assert_eq!(format_size(3 * 1024 * 1024 / 2), "1.5 MiB");
        assert_eq!(format_size(20 * 1024 * 1024 * 1024), "20.0 GiB");
        assert_eq!(format_size(2 * (1u64 << 40)), "2.0 TiB");
    }

    #[test]
    fn parses_binary_and_decimal_units() {
        assert_eq!(parse_size("20GiB").unwrap(), 20 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("75GB").unwrap(), 75 * 1_000_000_000);
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("512 MiB").unwrap(), 512 * 1024 * 1024);
        assert!(parse_size("nonsense").is_err());
    }

    #[test]
    fn measures_directory_size() {
        let dir = std::env::temp_dir().join(format!("overstay_size_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.bin"), vec![0u8; 1000]).unwrap();
        std::fs::write(dir.join("sub/b.bin"), vec![0u8; 500]).unwrap();
        assert_eq!(dir_size(&dir), 1500);
        assert_eq!(dir_size(&dir.join("does-not-exist")), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn free_space_reports_positive_for_temp_dir() {
        let free = free_space(&std::env::temp_dir());
        assert!(matches!(free, Some(n) if n > 0));
    }

    #[test]
    fn free_space_is_none_for_missing_path() {
        assert_eq!(free_space(Path::new("/definitely/not/a/real/path")), None);
    }
}
