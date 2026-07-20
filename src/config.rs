//! Optional TOML overrides for the automatic cleanup policy.

use std::io::ErrorKind;
use std::path::Path;

const MAX_TOTAL_SIZE: &str = "max_total_size";
const MAX_TARGET_SIZE: &str = "max_target_size";

pub fn load_policy() -> Result<crate::cleanup::Policy, String> {
    load_policy_from(&crate::paths::config_path())
}

fn load_policy_from(path: &Path) -> Result<crate::cleanup::Policy, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(crate::cleanup::default_policy());
        }
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    parse_policy(&content).map_err(|error| format!("invalid {}: {error}", path.display()))
}

fn parse_policy(content: &str) -> Result<crate::cleanup::Policy, String> {
    let table: toml::Table = content.parse().map_err(|error| format!("TOML: {error}"))?;
    if let Some(key) = table
        .keys()
        .find(|key| !matches!(key.as_str(), MAX_TOTAL_SIZE | MAX_TARGET_SIZE))
    {
        return Err(format!("unknown setting `{key}`"));
    }

    let mut policy = crate::cleanup::default_policy();
    policy.max_total_cache = size_setting(&table, MAX_TOTAL_SIZE, policy.max_total_cache)?;
    policy.max_project_size = size_setting(&table, MAX_TARGET_SIZE, policy.max_project_size)?;
    Ok(policy)
}

fn size_setting(table: &toml::Table, key: &str, default: u64) -> Result<u64, String> {
    let Some(value) = table.get(key) else {
        return Ok(default);
    };
    let Some(raw) = value.as_str() else {
        return Err(format!("`{key}` must be a string such as \"150GiB\""));
    };
    let size = crate::size::parse_size(raw).map_err(|error| format!("invalid `{key}`: {error}"))?;
    if size == 0 {
        return Err(format!("`{key}` must be greater than zero"));
    }
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_policy_overrides_total_and_target_sizes() {
        let policy = parse_policy(
            r#"
                max_total_size = "150GiB"
                max_target_size = "25 GiB"
            "#,
        )
        .unwrap();

        assert_eq!(
            (policy.max_total_cache, policy.max_project_size),
            (150 * 1024 * 1024 * 1024, 25 * 1024 * 1024 * 1024)
        );
    }

    #[test]
    fn parse_policy_uses_defaults_for_missing_settings() {
        let policy = parse_policy("").unwrap();
        let defaults = crate::cleanup::default_policy();

        assert_eq!(
            (policy.max_total_cache, policy.max_project_size),
            (defaults.max_total_cache, defaults.max_project_size)
        );
    }

    #[test]
    fn parse_policy_rejects_unknown_settings() {
        let error = parse_policy("max_totl_size = \"150GiB\"").unwrap_err();

        assert_eq!(error, "unknown setting `max_totl_size`");
    }

    #[test]
    fn parse_policy_rejects_non_string_sizes() {
        let error = parse_policy("max_total_size = 150").unwrap_err();

        assert_eq!(
            error,
            "`max_total_size` must be a string such as \"150GiB\""
        );
    }

    #[test]
    fn parse_policy_rejects_zero_sizes() {
        let error = parse_policy("max_target_size = \"0GiB\"").unwrap_err();

        assert_eq!(error, "`max_target_size` must be greater than zero");
    }

    #[test]
    fn load_policy_reads_config_file() {
        let path = std::env::temp_dir().join(format!(
            "overstay_valid_config_{}_config.toml",
            std::process::id()
        ));
        std::fs::write(&path, "max_total_size = \"180GiB\"").unwrap();

        let policy = load_policy_from(&path).unwrap();

        assert_eq!(policy.max_total_cache, 180 * 1024 * 1024 * 1024);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn missing_config_uses_default_policy() {
        let path = std::env::temp_dir().join(format!(
            "overstay_missing_config_{}_config.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let policy = load_policy_from(&path).unwrap();
        let defaults = crate::cleanup::default_policy();

        assert_eq!(
            (policy.max_total_cache, policy.max_project_size),
            (defaults.max_total_cache, defaults.max_project_size)
        );
    }
}
