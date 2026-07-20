//! The `cargo overstay <command>` dispatch: `purge` and `ls`.

use std::ffi::OsString;
use std::path::Path;

pub(crate) fn run_cli(args: &[OsString]) -> i32 {
    match args.first().and_then(|a| a.to_str()) {
        Some("purge") => crate::purge::run(&args[1..]),
        Some("ls") => ls(),
        _ => {
            print_usage();
            2
        }
    }
}

fn print_usage() {
    let style = crate::style::Style::stderr();
    eprintln!("{}", style.heading("Usage"));
    eprintln!(
        "  {} {}",
        style.command("cargo overstay"),
        style.muted("<command>")
    );
    eprintln!();
    eprintln!("{}", style.heading("Commands"));
    eprintln!(
        "  {} delete tracked build targets",
        style.command(format!("{:<36}", "purge"))
    );
    eprintln!(
        "  {} also scan dir (default: home)",
        style.command(format!("{:<36}", "purge --include-untracked [dir]"))
    );
    eprintln!(
        "  {} list tracked projects with sizes",
        style.command(format!("{:<36}", "ls"))
    );
}

fn ls() -> i32 {
    let style = crate::style::Style::stdout();
    let policy = match crate::config::load_policy() {
        Ok(policy) => policy,
        Err(error) => {
            let error_style = crate::style::Style::stderr();
            eprintln!("{} {error}", error_style.error("cargo-overstay:"));
            return 2;
        }
    };
    let store = crate::store::Store::open(&crate::paths::state_path());
    let now = crate::size::now_unix();
    let mut rows = store.entries();
    if rows.is_empty() {
        println!(
            "{}",
            style.muted("No tracked projects yet — build something through cargo first.")
        );
        return 0;
    }
    rows.sort_by_key(|e| std::cmp::Reverse(e.last_used));

    println!("{}", style.heading("Tracked targets"));
    let mut total = 0u64;
    for e in &rows {
        let target = Path::new(&e.target_dir);
        let size = crate::size::dir_size(target);
        total += size;
        let size_cell = format!("{:>10}", crate::size::format_size(size));
        let size_cell = if size > policy.max_project_size {
            style.warning(size_cell)
        } else {
            style.accent(size_cell)
        };
        let mut line = format!(
            "{}  {}  {}",
            size_cell,
            style.muted(format!("{:>8}", format_age(now - e.last_used))),
            style.path(&e.path)
        );
        if target != Path::new(&e.path).join("target") {
            line.push_str(&format!(
                "  {} {}{}",
                style.muted("(target:"),
                style.path(&e.target_dir),
                style.muted(")")
            ));
        }
        println!("{line}");
    }
    println!();
    let total_cell = format!("{:>10}", crate::size::format_size(total));
    let total_cell = if total > policy.max_total_cache {
        style.error(total_cell)
    } else {
        style.success(total_cell)
    };
    println!(
        "{}  {} {}",
        total_cell,
        style.strong("total"),
        style.muted(format!(
            "(budget {})",
            crate::size::format_size(policy.max_total_cache)
        ))
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
