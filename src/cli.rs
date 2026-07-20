//! The `cargo overstay <command>` dispatch: `purge` and `ls`.

use std::ffi::OsString;
use std::path::Path;

pub(crate) fn run_cli(args: &[OsString]) -> i32 {
    match args.first().and_then(|a| a.to_str()) {
        Some("purge") => crate::purge::run(&args[1..]),
        Some("ls") => ls(),
        _ => {
            eprintln!(
                "usage: cargo overstay <command>\n\
                 \n\
                 commands:\n\
                 \x20 purge                            delete tracked build targets\n\
                 \x20 purge --include-untracked [dir]  also scan `dir` (default: home)\n\
                 \x20                                  for untracked cargo targets\n\
                 \x20 ls                               list tracked projects with sizes"
            );
            2
        }
    }
}

fn ls() -> i32 {
    let store = crate::store::Store::open(&crate::paths::state_path());
    let now = crate::size::now_unix();
    let mut rows = store.entries();
    if rows.is_empty() {
        println!("no tracked projects yet — build something through cargo first");
        return 0;
    }
    rows.sort_by_key(|e| std::cmp::Reverse(e.last_used));

    let mut total = 0u64;
    for e in &rows {
        let target = Path::new(&e.target_dir);
        let size = crate::size::dir_size(target);
        total += size;
        let mut line = format!(
            "{:>10}  {:>8}  {}",
            crate::size::format_size(size),
            format_age(now - e.last_used),
            e.path
        );
        if target != Path::new(&e.path).join("target") {
            line.push_str(&format!("  (target: {})", e.target_dir));
        }
        println!("{line}");
    }
    println!(
        "{:>10}  total (budget {})",
        crate::size::format_size(total),
        crate::cleanup::MAX_TOTAL_CACHE_STR
    );
    0
}

fn format_age(secs: i64) -> String {
    let secs = secs.max(0);
    match secs {
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_verbs_exit_2() {
        assert_eq!(run_cli(&["bogus".into()]), 2);
        assert_eq!(run_cli(&[]), 2);
    }

    #[test]
    fn ages_format_coarsely() {
        assert_eq!(format_age(90), "1m ago");
        assert_eq!(format_age(7200), "2h ago");
        assert_eq!(format_age(3 * 86_400 + 5), "3d ago");
        assert_eq!(format_age(-5), "0m ago"); // clock skew
    }
}
