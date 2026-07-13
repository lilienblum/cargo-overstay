use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && (m.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

/// `path_var` is `OsStr` and split with `env::split_paths`: one non-UTF8
/// byte anywhere in PATH must not take the shim down with it.
pub fn which(name: &str, path_var: &OsStr, exclude: Option<&Path>) -> Option<PathBuf> {
    let exclude_canon = exclude.and_then(|p| std::fs::canonicalize(p).ok());
    for dir in std::env::split_paths(path_var).filter(|d| !d.as_os_str().is_empty()) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            if let Some(ref ex) = exclude_canon {
                if std::fs::canonicalize(&candidate).ok().as_ref() == Some(ex) {
                    continue;
                }
            }
            return Some(candidate);
        }
    }
    None
}

pub fn find_real_cargo(self_exe: &Path, path_var: &OsStr) -> Option<PathBuf> {
    which("cargo", path_var, Some(self_exe))
}

pub fn run(program: &Path, args: &[OsString]) -> std::io::Result<i32> {
    use std::os::unix::process::ExitStatusExt;
    let status = Command::new(program).args(args).status()?;
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
}

pub fn invocation_target_dir(args: &[String], cwd: &Path, workspace: &Path) -> PathBuf {
    invocation_target_dir_from(
        args,
        cwd,
        workspace,
        std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from),
        config_file_target_dir(cwd),
    )
}

fn invocation_target_dir_from(
    args: &[String],
    cwd: &Path,
    workspace: &Path,
    cargo_target_dir: Option<PathBuf>,
    config_file_target_dir: Option<PathBuf>,
) -> PathBuf {
    target_dir_arg(args)
        .or_else(|| config_arg_target_dir(args))
        .or(cargo_target_dir)
        .map(|path| absolute_path(path, cwd))
        .or(config_file_target_dir)
        .unwrap_or_else(|| workspace.join("target"))
}

fn target_dir_arg(args: &[String]) -> Option<PathBuf> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if let Some(value) = arg.strip_prefix("--target-dir=") {
            if !value.is_empty() {
                return Some(PathBuf::from(value));
            }
        } else if arg == "--target-dir" {
            if let Some(value) = iter.next() {
                if !value.is_empty() {
                    return Some(PathBuf::from(value));
                }
            }
        }
    }
    None
}

fn config_arg_target_dir(args: &[String]) -> Option<PathBuf> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        let value = if let Some(value) = arg.strip_prefix("--config=") {
            Some(value)
        } else if arg == "--config" {
            iter.next().map(String::as_str)
        } else {
            None
        };
        if let Some(path) = value.and_then(target_dir_from_config_value) {
            return Some(path);
        }
    }
    None
}

fn target_dir_from_config_value(value: &str) -> Option<PathBuf> {
    let (key, value) = value.split_once('=')?;
    if key.trim() != "build.target-dir" {
        return None;
    }
    let value = strip_toml_quotes(value.trim());
    if value.is_empty() {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

fn config_file_target_dir(cwd: &Path) -> Option<PathBuf> {
    let mut found = None;
    for ancestor in cwd.ancestors().collect::<Vec<_>>().into_iter().rev() {
        for name in ["config", "config.toml"] {
            let path = ancestor.join(".cargo").join(name);
            if let Some(target) = read_config_target_dir(&path) {
                found = Some(absolute_path(target, config_path_base(&path)));
            }
        }
    }
    found
}

fn read_config_target_dir(path: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut in_build = false;
    let mut found = None;
    for raw_line in content.lines() {
        let line = trim_toml_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_build = line.trim_matches(&['[', ']'][..]).trim() == "build";
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key == "build.target-dir" || (in_build && key == "target-dir") {
            let value = strip_toml_quotes(value.trim());
            if !value.is_empty() {
                found = Some(PathBuf::from(value));
            }
        }
    }
    found
}

fn trim_toml_comment(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    for (i, ch) in line.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double => return &line[..i],
            _ => {}
        }
    }
    line
}

fn strip_toml_quotes(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
        .unwrap_or(value)
}

fn config_path_base(config_path: &Path) -> &Path {
    config_path
        .parent()
        .and_then(|parent| {
            if parent.file_name().is_some_and(|name| name == ".cargo") {
                parent.parent()
            } else {
                Some(parent)
            }
        })
        .unwrap_or_else(|| Path::new(""))
}

fn absolute_path(path: PathBuf, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn make_exe(path: &std::path::Path) {
        std::fs::write(path, "#!/bin/sh\ntrue\n").unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn which_finds_executable_and_skips_exclude() {
        let dir = std::env::temp_dir().join(format!("overstay_which_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bin_a = dir.join("a/cargo");
        let bin_b = dir.join("b/cargo");
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::create_dir_all(dir.join("b")).unwrap();
        make_exe(&bin_a);
        make_exe(&bin_b);
        let path_var = format!("{}:{}", dir.join("a").display(), dir.join("b").display());
        let path_var = OsStr::new(&path_var);
        // no exclude -> first (a)
        assert_eq!(which("cargo", path_var, None), Some(bin_a.clone()));
        // exclude a -> b
        assert_eq!(which("cargo", path_var, Some(&bin_a)), Some(bin_b.clone()));
        assert_eq!(which("nope", path_var, None), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn run_propagates_exit_code() {
        let code = run(
            std::path::Path::new("/bin/sh"),
            &["-c".into(), "exit 7".into()],
        )
        .unwrap();
        assert_eq!(code, 7);
    }

    #[test]
    fn run_propagates_signal_exit_code() {
        let code = run(
            std::path::Path::new("/bin/sh"),
            &["-c".into(), "kill -TERM $$".into()],
        )
        .unwrap();
        assert_eq!(code, 143); // 128 + SIGTERM(15)
    }

    #[test]
    fn target_dir_arg_accepts_split_and_equals_forms() {
        assert_eq!(
            target_dir_arg(&["build".into(), "--target-dir".into(), "out".into()]),
            Some(PathBuf::from("out"))
        );
        assert_eq!(
            target_dir_arg(&["check".into(), "--target-dir=out".into()]),
            Some(PathBuf::from("out"))
        );
    }

    #[test]
    fn target_dir_arg_ignores_args_after_double_dash() {
        assert_eq!(
            target_dir_arg(&[
                "rustc".into(),
                "--".into(),
                "--target-dir".into(),
                "out".into()
            ]),
            None
        );
    }

    #[test]
    fn invocation_target_dir_uses_arg_before_env() {
        let cwd = Path::new("/work/project");
        assert_eq!(
            invocation_target_dir_from(
                &["build".into(), "--target-dir".into(), "arg-out".into()],
                cwd,
                cwd,
                Some(PathBuf::from("env-out")),
                None,
            ),
            PathBuf::from("/work/project/arg-out")
        );
    }

    #[test]
    fn invocation_target_dir_uses_env_when_arg_is_absent() {
        let cwd = Path::new("/work/project");
        assert_eq!(
            invocation_target_dir_from(
                &["build".into()],
                cwd,
                cwd,
                Some(PathBuf::from("env-out")),
                None,
            ),
            PathBuf::from("/work/project/env-out")
        );
    }

    #[test]
    fn invocation_target_dir_reads_config_arg() {
        let cwd = Path::new("/work/project");
        assert_eq!(
            invocation_target_dir_from(
                &[
                    "build".into(),
                    "--config".into(),
                    "build.target-dir='cfg-out'".into()
                ],
                cwd,
                cwd,
                None,
                None,
            ),
            PathBuf::from("/work/project/cfg-out")
        );
    }

    #[test]
    fn invocation_target_dir_reads_discovered_config_file() {
        let dir = std::env::temp_dir().join(format!("overstay_cfg_{}", std::process::id()));
        let project = dir.join("project");
        let nested = project.join("crates/inner");
        std::fs::create_dir_all(nested.join(".cargo")).unwrap();
        std::fs::write(
            nested.join(".cargo/config.toml"),
            "[build]\ntarget-dir = 'configured-target'\n",
        )
        .unwrap();

        assert_eq!(
            invocation_target_dir_from(
                &["build".into()],
                &nested,
                &project,
                None,
                config_file_target_dir(&nested),
            ),
            nested.join("configured-target")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
