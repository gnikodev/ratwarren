use std::ffi::OsString;
use std::path::PathBuf;

use directories::ProjectDirs;

use super::{CONFIG_FILE_NAME, ConfigError};

pub const CONFIG_ENV_VAR: &str = "RATWARREN_CONFIG";

pub fn config_file_path() -> Result<PathBuf, ConfigError> {
    let (path, _explicit) = resolve_config_path()?;
    Ok(path)
}

// Resolves the config path along with whether it came from an explicit
// RATWARREN_CONFIG override, so callers (Config::load) can decide whether a
// missing file means "first run" or "user pointed us at the wrong path".
pub(crate) fn resolve_config_path() -> Result<(PathBuf, bool), ConfigError> {
    let override_value = std::env::var_os(CONFIG_ENV_VAR);
    let explicit = override_value.is_some();
    let path = config_file_path_from(override_value)?;
    Ok((path, explicit))
}

pub(crate) fn config_file_path_from(
    override_value: Option<OsString>,
) -> Result<PathBuf, ConfigError> {
    if let Some(path) = override_value {
        return Ok(PathBuf::from(path));
    }

    let dirs = ProjectDirs::from("", "", "ratwarren").ok_or(ConfigError::NoConfigDir)?;
    Ok(dirs.config_dir().join(CONFIG_FILE_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_value_is_returned_verbatim() {
        let override_path = OsString::from("/custom/path/config.toml");

        let path =
            config_file_path_from(Some(override_path.clone())).expect("override always succeeds");

        assert_eq!(path, PathBuf::from(override_path));
    }

    #[test]
    fn none_falls_back_to_platform_project_dir() {
        let path = config_file_path_from(None).expect("platform config dir should resolve");

        assert!(
            path.ends_with("ratwarren/config.toml"),
            "expected path to end with ratwarren/config.toml, got: {}",
            path.display()
        );
    }
}
