use std::path::PathBuf;

pub fn data_dir() -> PathBuf {
    if let Ok(x) = std::env::var("XDG_DATA_HOME") {
        if !x.is_empty() {
            return PathBuf::from(x).join("cargo-overstay");
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return PathBuf::from(".cargo-overstay");
    }
    // Platform-native data dir: macOS uses Application Support; other unix uses
    // the XDG default of ~/.local/share.
    let base = if cfg!(target_os = "macos") {
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
    } else {
        PathBuf::from(home).join(".local").join("share")
    };
    base.join("cargo-overstay")
}

pub fn state_path() -> PathBuf {
    if let Ok(p) = std::env::var("CARGO_OVERSTAY_STATE") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    data_dir().join("state")
}

pub fn config_path() -> PathBuf {
    if let Ok(path) = std::env::var("CARGO_OVERSTAY_CONFIG") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    if let Ok(path) = std::env::var("XDG_CONFIG_HOME") {
        if !path.is_empty() {
            return PathBuf::from(path).join("cargo-overstay/config.toml");
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return PathBuf::from(".cargo-overstay/config.toml");
    }
    let base = if cfg!(target_os = "macos") {
        PathBuf::from(home).join("Library/Application Support")
    } else {
        PathBuf::from(home).join(".config")
    };
    base.join("cargo-overstay/config.toml")
}

/// Serializes tests that mutate process-global path override variables: the
/// test harness runs tests on parallel threads, so unsynchronized set/remove
/// in one test races reads in another.
#[cfg(test)]
pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_data_home_overrides_data_dir() {
        let _env = env_lock();
        std::env::set_var("XDG_DATA_HOME", "/tmp/xdg-test");
        std::env::remove_var("CARGO_OVERSTAY_STATE");
        assert_eq!(
            data_dir(),
            std::path::PathBuf::from("/tmp/xdg-test/cargo-overstay")
        );
        std::env::remove_var("XDG_DATA_HOME");
    }

    #[test]
    fn overstay_state_env_overrides_state_path() {
        let _env = env_lock();
        std::env::set_var("CARGO_OVERSTAY_STATE", "/tmp/custom/my.state");
        assert_eq!(
            state_path(),
            std::path::PathBuf::from("/tmp/custom/my.state")
        );
        std::env::remove_var("CARGO_OVERSTAY_STATE");
    }

    #[test]
    fn xdg_config_home_sets_config_path() {
        let _env = env_lock();
        std::env::set_var("XDG_CONFIG_HOME", "/tmp/xdg-config-test");
        std::env::remove_var("CARGO_OVERSTAY_CONFIG");
        assert_eq!(
            config_path(),
            PathBuf::from("/tmp/xdg-config-test/cargo-overstay/config.toml")
        );
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn overstay_config_env_overrides_config_path() {
        let _env = env_lock();
        std::env::set_var("CARGO_OVERSTAY_CONFIG", "/tmp/custom/config.toml");
        assert_eq!(config_path(), PathBuf::from("/tmp/custom/config.toml"));
        std::env::remove_var("CARGO_OVERSTAY_CONFIG");
    }
}
