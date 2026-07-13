use std::fs;
use std::path::{Path, PathBuf};

pub fn resolve_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut nearest: Option<PathBuf> = None;
    let mut highest_ws: Option<PathBuf> = None;
    for ancestor in start.ancestors() {
        let manifest = ancestor.join("Cargo.toml");
        if manifest.is_file() {
            if nearest.is_none() {
                nearest = Some(ancestor.to_path_buf());
            }
            if let Ok(contents) = fs::read_to_string(&manifest) {
                if contents
                    .lines()
                    .any(|l| l.trim_start().starts_with("[workspace]"))
                {
                    highest_ws = Some(ancestor.to_path_buf());
                }
            }
        }
    }
    highest_ws.or(nearest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("overstay_ws_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn finds_nearest_manifest() {
        let root = tmp("single");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        assert_eq!(
            resolve_workspace_root(&root.join("src")),
            Some(root.clone())
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn promotes_to_workspace_root() {
        let root = tmp("ws");
        let member = root.join("crates/inner");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers=[\"crates/inner\"]\n",
        )
        .unwrap();
        std::fs::write(member.join("Cargo.toml"), "[package]\nname=\"inner\"\n").unwrap();
        assert_eq!(resolve_workspace_root(&member), Some(root.clone()));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn returns_none_outside_a_project() {
        let root = tmp("empty");
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(resolve_workspace_root(&root), None);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
