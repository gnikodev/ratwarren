use std::ffi::OsString;
use std::path::PathBuf;

use directories::ProjectDirs;

use super::{CONFIG_FILE_NAME, ConfigError};

pub const CONFIG_ENV_VAR: &str = "RATWARREN_CONFIG";
pub const DATA_ENV_VAR: &str = "RATWARREN_DATA_DIR";
pub const SANITIZED_STEM_MAX: usize = 40;

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

pub fn data_dir() -> Result<PathBuf, ConfigError> {
    data_dir_from(std::env::var_os(DATA_ENV_VAR))
}

// Unlike `config_file_path_from`, an empty override does NOT fall through
// verbatim: an empty RATWARREN_CONFIG fails loudly on open() the first time
// it's used, but an empty RATWARREN_DATA_DIR would resolve to `PathBuf::from("")`
// and silently create `pages`/`state` directories under the current working
// directory instead -- a much quieter and more surprising failure mode, so it
// falls back to the platform default instead.
pub(crate) fn data_dir_from(override_value: Option<OsString>) -> Result<PathBuf, ConfigError> {
    if let Some(path) = override_value
        && !path.is_empty()
    {
        return Ok(PathBuf::from(path));
    }

    let dirs = ProjectDirs::from("", "", "ratwarren").ok_or(ConfigError::NoDataDir)?;
    Ok(dirs.data_dir().to_path_buf())
}

pub fn pages_root() -> Result<PathBuf, ConfigError> {
    Ok(data_dir()?.join("pages"))
}

pub fn pages_dir_for(connection_name: &str) -> Result<PathBuf, ConfigError> {
    Ok(pages_root()?.join(sanitize_connection_name(connection_name)))
}

pub fn state_dir() -> Result<PathBuf, ConfigError> {
    Ok(data_dir()?.join("state"))
}

/// Connection names are free-form config strings and can contain `/`, `..`,
/// or other path-hostile characters. Lowercases + replaces anything outside
/// `[a-z0-9_]` with `_`, truncates to `SANITIZED_STEM_MAX` characters, and
/// appends a hash suffix so two names that sanitize to the same stem (or an
/// empty stem) never collide on disk.
pub fn sanitize_connection_name(name: &str) -> String {
    let hash = fnv1a64(name.as_bytes());
    let mut stem = String::new();
    for ch in name.chars() {
        if stem.len() == SANITIZED_STEM_MAX {
            break;
        }
        stem.push(match ch {
            'a'..='z' | '0'..='9' | '_' => ch,
            'A'..='Z' => ch.to_ascii_lowercase(),
            _ => '_',
        });
    }
    if stem.is_empty() {
        stem.push_str("conn");
    }
    format!("{stem}-{}", &format!("{hash:016x}")[..12])
}

// Hand-rolled deliberately, not `std::hash::DefaultHasher`: that hasher's
// output is explicitly unspecified and can change across Rust releases, and
// a hash change here would silently orphan every user's existing saved pages
// (the directory their old sessions wrote to would no longer be the one a
// new build looks up).
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
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

    // --- data_dir_from ---

    #[test]
    fn data_dir_from_a_non_empty_override_is_returned_verbatim() {
        let override_path = OsString::from("/custom/data/dir");

        let path = data_dir_from(Some(override_path.clone())).expect("override always succeeds");

        assert_eq!(path, PathBuf::from(override_path));
    }

    #[test]
    fn data_dir_from_an_empty_override_falls_back_to_the_platform_default_instead_of_cwd() {
        // Unlike `config_file_path_from`, an empty override must NOT resolve
        // to `PathBuf::from("")` (the current working directory) -- that
        // would silently create `pages`/`state` under whatever directory the
        // binary happens to be launched from.
        let empty = OsString::from("");

        let path = data_dir_from(Some(empty)).expect("empty override should fall back, not fail");

        assert_ne!(path, PathBuf::from(""));
        assert!(
            path.ends_with("ratwarren"),
            "expected the platform data dir, got: {}",
            path.display()
        );
    }

    #[test]
    fn data_dir_from_none_falls_back_to_the_platform_default() {
        let path = data_dir_from(None).expect("platform data dir should resolve");

        assert!(
            path.ends_with("ratwarren"),
            "expected the platform data dir, got: {}",
            path.display()
        );
    }

    // --- pages_root / pages_dir_for / state_dir ---
    //
    // No env-var override is set here (see the module-level test-writer note
    // on why `RATWARREN_DATA_DIR` must never be set from a test): these pin
    // the same "no override -> platform default" composition the
    // `config_file_path_from(None)` test above pins for the config path.

    #[test]
    fn pages_root_is_pages_under_the_platform_data_dir() {
        let path = pages_root().expect("platform data dir should resolve");
        assert!(path.ends_with("ratwarren/pages"), "got: {}", path.display());
    }

    #[test]
    fn state_dir_is_state_under_the_platform_data_dir() {
        let path = state_dir().expect("platform data dir should resolve");
        assert!(path.ends_with("ratwarren/state"), "got: {}", path.display());
    }

    #[test]
    fn pages_dir_for_joins_the_sanitized_connection_name_under_pages_root() {
        let name = "My Prod DB";
        let path = pages_dir_for(name).expect("should resolve");
        let root = pages_root().expect("should resolve");
        assert_eq!(path, root.join(sanitize_connection_name(name)));
    }

    // --- sanitize_connection_name ---

    fn assert_well_formed(output: &str) {
        assert!(
            !output.starts_with('-'),
            "output must never start with '-': {output:?}"
        );
        assert_eq!(
            output.matches('-').count(),
            1,
            "output must contain exactly one '-' separating stem from hash: {output:?}"
        );
        assert!(
            output
                .chars()
                .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-')),
            "every output char must be in [a-z0-9_-]: {output:?}"
        );
    }

    #[test]
    fn dot_and_dotdot_produce_no_dot_and_are_not_dot_or_dotdot() {
        for input in ["..", "."] {
            let out = sanitize_connection_name(input);
            assert!(!out.contains('.'), "got {out:?} for input {input:?}");
            assert_ne!(out, ".");
            assert_ne!(out, "..");
            assert_well_formed(&out);
        }
    }

    #[test]
    fn path_hostile_inputs_produce_no_separator_in_the_output() {
        for input in ["a/b", "a\\b", "/", "../../etc/passwd"] {
            let out = sanitize_connection_name(input);
            assert!(
                !out.contains('/') && !out.contains('\\'),
                "got {out:?} for input {input:?}"
            );
            assert_well_formed(&out);
        }
    }

    #[test]
    fn leading_dot_input_does_not_produce_a_leading_dot_output() {
        let out = sanitize_connection_name(".hidden");
        assert!(!out.starts_with('.'), "got {out:?}");
        assert_well_formed(&out);
    }

    #[test]
    fn empty_name_produces_a_non_empty_conn_prefixed_output() {
        let out = sanitize_connection_name("");
        assert!(out.starts_with("conn-"), "got {out:?}");
        assert_well_formed(&out);
    }

    #[test]
    fn a_very_long_name_is_truncated_to_at_most_53_bytes() {
        let long = "a".repeat(10_000);
        let out = sanitize_connection_name(&long);
        assert!(
            out.len() <= 53,
            "expected output <= 53 bytes, got {} bytes: {out:?}",
            out.len()
        );
        assert_well_formed(&out);
    }

    #[test]
    fn lossy_mapping_collisions_are_broken_by_the_hash_suffix() {
        // "a/b", "a_b", and "a-b" all sanitize to the same stem ("a_b"), so
        // only the hash (computed on the original, unsanitized bytes) can
        // keep them from colliding on disk.
        let a = sanitize_connection_name("a/b");
        let b = sanitize_connection_name("a_b");
        let c = sanitize_connection_name("a-b");
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        for out in [&a, &b, &c] {
            assert_well_formed(out);
        }
    }

    #[test]
    fn case_folded_names_do_not_collide() {
        // The stem lowercases "Prod" and "prod" to the same value; the hash
        // is computed on the original (case-preserved) bytes, so the two
        // must still resolve to different directories.
        let upper = sanitize_connection_name("Prod");
        let lower = sanitize_connection_name("prod");
        assert_ne!(
            upper, lower,
            "case-folded names must not collide on the sanitized directory name"
        );
        assert_well_formed(&upper);
        assert_well_formed(&lower);
    }

    #[test]
    fn non_ascii_input_produces_a_non_empty_ascii_only_output() {
        for input in ["прод", "🦀🎉"] {
            let out = sanitize_connection_name(input);
            assert!(!out.is_empty());
            assert!(out.is_ascii(), "got {out:?} for input {input:?}");
            assert_well_formed(&out);
        }
    }
}
