use std::fs;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub const SIDECAR_VERSION: u32 = 1;
pub const SIDECAR_SUFFIX: &str = ".tabs.toml";
pub const MAX_RESTORED_PAGES: usize = 64;

// Deliberately NOT `#[serde(deny_unknown_fields)]`: this is a machine-written
// convenience cache (open-page order + cursor position), not a user-edited
// config file, so it must stay forgiving of a newer build's extra field
// rather than fail closed on it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sidecar {
    pub version: u32,
    #[serde(default)]
    pub active: usize,
    #[serde(default)]
    pub open: Vec<SidecarPage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarPage {
    pub file: String,
    #[serde(default)]
    pub line: usize,
    #[serde(default)]
    pub col: usize,
}

impl Sidecar {
    pub fn empty() -> Sidecar {
        Sidecar {
            version: SIDECAR_VERSION,
            active: 0,
            open: Vec::new(),
        }
    }
}

/// Infallible: an absent file, zero-length file, non-UTF-8 bytes, invalid
/// TOML, a truncated document, or a wrong-typed field all degrade to
/// `Sidecar::empty()` rather than erroring or panicking. This is a
/// convenience cache for open-page order and cursor position, never a
/// blocking error on session open. Does not itself check `version` against
/// `SIDECAR_VERSION` -- an unrecognised version parses fine here and is the
/// caller's job to reject (see `PageTabs::restore_in`).
pub fn load(path: &Path) -> Sidecar {
    let Ok(text) = fs::read_to_string(path) else {
        return Sidecar::empty();
    };
    toml::from_str(&text).unwrap_or_else(|_| Sidecar::empty())
}

/// Best-effort atomic write: every failure (serialization, directory
/// creation, opening the tmp file, writing, renaming) is swallowed. This is a
/// cache write on the way out of a session/app, not something that should
/// ever block or fail a quit.
pub fn store(path: &Path, sidecar: &Sidecar) {
    let Ok(text) = toml::to_string(sidecar) else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    // Same owner-only directory permissions as `PagesDir`'s own directories
    // (0700 on unix), not a bare `fs::create_dir_all`.
    if super::create_dir_private(parent).is_err() {
        return;
    }

    let tmp_path = {
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        std::path::PathBuf::from(tmp)
    };

    let mut open_options = fs::OpenOptions::new();
    open_options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_options.mode(0o600);
    }

    let Ok(mut tmp_file) = open_options.open(&tmp_path) else {
        return;
    };
    if tmp_file.write_all(text.as_bytes()).is_err() {
        return;
    }
    drop(tmp_file);

    let _ = fs::rename(&tmp_path, path);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_is_empty(s: &Sidecar) {
        assert_eq!(s.version, SIDECAR_VERSION);
        assert_eq!(s.active, 0);
        assert!(s.open.is_empty());
    }

    #[test]
    fn load_of_an_absent_file_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.tabs.toml");
        assert_is_empty(&load(&path));
    }

    #[test]
    fn load_of_a_zero_length_file_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("empty.tabs.toml");
        fs::write(&path, b"").unwrap();
        assert_is_empty(&load(&path));
    }

    #[test]
    fn load_of_a_document_truncated_mid_table_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("truncated.tabs.toml");
        fs::write(
            &path,
            b"version = 1\nactive = 0\n[[open]]\nfile = \"a.sql\"\nline",
        )
        .unwrap();
        assert_is_empty(&load(&path));
    }

    #[test]
    fn load_of_a_wrong_typed_field_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wrong-type.tabs.toml");
        fs::write(
            &path,
            b"version = 1\nactive = 0\n[[open]]\nfile = \"a.sql\"\nline = \"x\"\n",
        )
        .unwrap();
        assert_is_empty(&load(&path));
    }

    #[test]
    fn load_of_non_utf8_bytes_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bad-bytes.tabs.toml");
        fs::write(&path, [0xff, 0xfe, 0xfd]).unwrap();
        assert_is_empty(&load(&path));
    }

    #[test]
    fn load_of_an_unrecognised_version_parses_as_is_version_rejection_is_the_callers_job() {
        // `load` itself does NOT normalize an unrecognised `version` to
        // `empty()` -- its own doc comment says this is `PageTabs::restore_in`'s
        // job. Pinning this here so a future change to that split doesn't
        // silently regress unnoticed.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("future-version.tabs.toml");
        fs::write(&path, b"version = 999\nactive = 0\nopen = []\n").unwrap();

        let loaded = load(&path);
        assert_eq!(loaded.version, 999);
        assert_eq!(loaded.active, 0);
        assert!(loaded.open.is_empty());
    }

    #[test]
    fn store_then_load_round_trips_order_active_line_and_col() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sidecar.tabs.toml");
        let sidecar = Sidecar {
            version: SIDECAR_VERSION,
            active: 1,
            open: vec![
                SidecarPage {
                    file: "a.sql".to_string(),
                    line: 3,
                    col: 7,
                },
                SidecarPage {
                    file: "b.sql".to_string(),
                    line: 0,
                    col: 0,
                },
            ],
        };

        store(&path, &sidecar);
        let loaded = load(&path);

        assert_eq!(loaded.version, SIDECAR_VERSION);
        assert_eq!(loaded.active, 1);
        assert_eq!(loaded.open.len(), 2);
        assert_eq!(loaded.open[0].file, "a.sql");
        assert_eq!(loaded.open[0].line, 3);
        assert_eq!(loaded.open[0].col, 7);
        assert_eq!(loaded.open[1].file, "b.sql");
        assert_eq!(loaded.open[1].line, 0);
        assert_eq!(loaded.open[1].col, 0);
    }

    #[test]
    #[cfg(unix)]
    fn store_writes_the_file_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sidecar.tabs.toml");

        store(&path, &Sidecar::empty());

        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    #[cfg(unix)]
    fn store_creates_the_parent_directory_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("state");
        let path = parent.join("sidecar.tabs.toml");

        store(&path, &Sidecar::empty());

        let mode = fs::metadata(&parent).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn store_creates_missing_parent_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join("nested")
            .join("dir")
            .join("sidecar.tabs.toml");

        store(&path, &Sidecar::empty());

        assert!(path.exists());
    }
}
