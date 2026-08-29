pub mod sidecar;
pub mod tabs;

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

pub use tabs::{Page, PageTabs, SaveOutcome};

/// A validated `.sql` page file name: never empty, never a path (no `/`,
/// `\`, `..`, leading `.`), always ends in `.sql` with a non-empty stem.
/// `new` is the sole constructor and the sole traversal guard for every
/// filesystem operation `PagesDir` performs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageName(String);

impl PageName {
    pub fn new(s: &str) -> Result<PageName, PagesError> {
        let invalid = |reason: &'static str| PagesError::InvalidName {
            name: s.to_string(),
            reason,
        };

        if s.is_empty() {
            return Err(invalid("must not be empty"));
        }
        if s.len() > 255 {
            return Err(invalid("must not be longer than 255 bytes"));
        }
        if s.contains('/') || s.contains('\\') || s.contains('\0') {
            return Err(invalid("must not contain '/', '\\', or a NUL byte"));
        }
        if s == "." || s == ".." {
            return Err(invalid("must not be \".\" or \"..\""));
        }
        if s.starts_with('.') {
            return Err(invalid("must not start with '.'"));
        }
        let Some(stem) = s.strip_suffix(".sql") else {
            return Err(invalid("must end with \".sql\""));
        };
        if stem.is_empty() {
            return Err(invalid("must have a non-empty name before \".sql\""));
        }

        // Structural backstop against anything the checks above don't catch
        // by content alone (e.g. a Windows drive-prefix string like
        // "C:foo.sql", which `Path::new` parses as a `Prefix` component on
        // that platform rather than a plain file name).
        let mut components = Path::new(s).components();
        let is_single_normal_component = matches!(
            components.next(),
            Some(Component::Normal(c)) if c == OsStr::new(s)
        ) && components.next().is_none();
        if !is_single_normal_component {
            return Err(invalid("is not a valid single path component"));
        }

        Ok(PageName(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `s` minus the trailing `.sql`.
    pub fn stem(&self) -> &str {
        self.0.strip_suffix(".sql").unwrap_or(&self.0)
    }
}

/// A connection's saved-pages directory: `<data>/pages/<sanitized-name>/`.
/// Every path this hands back is `root.join(a PageName)`, so `PageName`'s
/// validation is the only traversal guard this type relies on.
pub struct PagesDir {
    root: PathBuf,
}

impl PagesDir {
    pub fn for_connection(connection_name: &str) -> Result<PagesDir, PagesError> {
        let root = crate::config::paths::pages_dir_for(connection_name)?;
        create_dir_private(&root)?;
        Ok(PagesDir { root })
    }

    /// Test seam: no filesystem access, no directory creation.
    pub fn at(root: PathBuf) -> PagesDir {
        PagesDir { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path_of(&self, name: &PageName) -> PathBuf {
        self.root.join(name.as_str())
    }

    pub fn exists(&self, name: &PageName) -> bool {
        self.path_of(name).exists()
    }

    pub fn list(&self) -> Result<Vec<PageName>, PagesError> {
        let entries = fs::read_dir(&self.root).map_err(|source| PagesError::Read {
            path: self.root.clone(),
            source,
        })?;

        let mut names = Vec::new();
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file() {
                continue;
            }
            let file_name = entry.file_name();
            let Some(name_str) = file_name.to_str() else {
                continue;
            };
            if let Ok(name) = PageName::new(name_str) {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }

    pub fn load(&self, name: &PageName) -> Result<String, PagesError> {
        let path = self.path_of(name);
        let bytes =
            fs::read(&path).map_err(|source| self.read_error(name, path.clone(), source))?;
        String::from_utf8(bytes).map_err(|_| PagesError::NotUtf8 { path })
    }

    pub fn save(&self, name: &PageName, contents: &str) -> Result<(), PagesError> {
        create_dir_private(&self.root)?;
        let path = self.path_of(name);

        let tmp_path = {
            let mut tmp = path.as_os_str().to_owned();
            tmp.push(".tmp");
            PathBuf::from(tmp)
        };

        let mut open_options = fs::OpenOptions::new();
        open_options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open_options.mode(0o600);
        }

        let mut tmp_file = open_options
            .open(&tmp_path)
            .map_err(|source| PagesError::Write {
                path: tmp_path.clone(),
                source,
            })?;
        tmp_file
            .write_all(contents.as_bytes())
            .map_err(|source| PagesError::Write {
                path: tmp_path.clone(),
                source,
            })?;
        drop(tmp_file);

        fs::rename(&tmp_path, &path).map_err(|source| PagesError::Write { path, source })
    }

    // fs::rename would silently clobber an existing `to` -- refuse up front
    // instead.
    pub fn rename(&self, from: &PageName, to: &PageName) -> Result<(), PagesError> {
        if self.exists(to) {
            return Err(PagesError::AlreadyExists(to.as_str().to_string()));
        }
        let from_path = self.path_of(from);
        let to_path = self.path_of(to);
        fs::rename(&from_path, &to_path).map_err(|source| self.read_error(from, from_path, source))
    }

    pub fn delete(&self, name: &PageName) -> Result<(), PagesError> {
        let path = self.path_of(name);
        fs::remove_file(&path).map_err(|source| self.read_error(name, path, source))
    }

    fn read_error(&self, name: &PageName, path: PathBuf, source: std::io::Error) -> PagesError {
        if source.kind() == std::io::ErrorKind::NotFound {
            PagesError::NotFound(name.as_str().to_string())
        } else {
            PagesError::Read { path, source }
        }
    }
}

fn create_dir_private(path: &Path) -> Result<(), PagesError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    // Set the mode at creation time (not via a post-hoc chmod) so this
    // directory is never briefly world-readable. `Config::save_to`'s own
    // `fs::create_dir_all(parent)` call does NOT set a directory mode at all
    // (it relies on the umask) -- this is a deliberate hardening improvement
    // over that precedent, not parity with it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|source| PagesError::Write {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- PageName / path traversal guard ---

    #[test]
    fn rejects_path_traversal_and_absolute_and_hidden_and_malformed_names() {
        let rejected = [
            "../evil.sql",
            "a/b.sql",
            "/abs.sql",
            "..\\x.sql",
            "",
            ".",
            "..",
            ".sql",
            ".hidden.sql",
            "foo.txt",
        ];
        for name in rejected {
            assert!(
                PageName::new(name).is_err(),
                "expected {name:?} to be rejected"
            );
        }
    }

    #[test]
    fn rejects_a_very_long_name() {
        let long = format!("{}.sql", "a".repeat(300));
        assert!(PageName::new(&long).is_err());
    }

    #[test]
    fn accepts_well_formed_names() {
        for name in ["queries.sql", "a_b-c1.sql", "Upper.sql"] {
            assert!(
                PageName::new(name).is_ok(),
                "expected {name:?} to be accepted"
            );
        }
    }

    #[test]
    fn every_accepted_name_resolves_directly_under_the_dir_root() {
        let dir = PagesDir::at(PathBuf::from("/some/pages/root"));
        for name in ["queries.sql", "a_b-c1.sql", "Upper.sql"] {
            let page = PageName::new(name).unwrap();
            let path = dir.path_of(&page);
            assert_eq!(
                path.parent(),
                Some(dir.root()),
                "path_of({name:?}) must resolve directly under the dir root, got {}",
                path.display()
            );
        }
    }

    #[test]
    fn stem_strips_the_sql_suffix() {
        let page = PageName::new("queries.sql").unwrap();
        assert_eq!(page.stem(), "queries");
    }

    // --- PagesDir CRUD, using a real tempdir (never the platform data dir) ---

    fn temp_dir() -> (tempfile::TempDir, PagesDir) {
        let tmp = tempfile::tempdir().expect("tempdir creation");
        let dir = PagesDir::at(tmp.path().to_path_buf());
        (tmp, dir)
    }

    #[test]
    fn save_then_load_round_trips_and_list_reflects_it() {
        let (_tmp, dir) = temp_dir();
        let name = PageName::new("a.sql").unwrap();

        dir.save(&name, "SELECT 1;").expect("save should succeed");
        let loaded = dir.load(&name).expect("load should succeed");
        assert_eq!(loaded, "SELECT 1;");

        let listed = dir.list().expect("list should succeed");
        assert_eq!(listed, vec![name]);
    }

    #[test]
    fn list_on_a_fresh_directory_is_empty() {
        let (_tmp, dir) = temp_dir();
        assert_eq!(dir.list().expect("list should succeed"), Vec::new());
    }

    #[test]
    fn list_ignores_non_sql_and_hidden_files() {
        let (tmp, dir) = temp_dir();
        fs::write(tmp.path().join("notes.txt"), "irrelevant").unwrap();
        fs::write(tmp.path().join(".hidden.sql"), "irrelevant").unwrap();
        let name = PageName::new("real.sql").unwrap();
        dir.save(&name, "SELECT 1;").unwrap();

        assert_eq!(dir.list().expect("list should succeed"), vec![name]);
    }

    #[test]
    fn load_of_a_missing_page_is_not_found() {
        let (_tmp, dir) = temp_dir();
        let name = PageName::new("missing.sql").unwrap();
        let err = dir.load(&name).expect_err("missing page must error");
        assert!(matches!(err, PagesError::NotFound(n) if n == "missing.sql"));
    }

    #[test]
    fn rename_refuses_to_clobber_an_existing_destination() {
        let (_tmp, dir) = temp_dir();
        let from = PageName::new("from.sql").unwrap();
        let to = PageName::new("to.sql").unwrap();
        dir.save(&from, "one").unwrap();
        dir.save(&to, "two").unwrap();

        let err = dir.rename(&from, &to).expect_err("must refuse to clobber");
        assert!(matches!(err, PagesError::AlreadyExists(n) if n == "to.sql"));
        assert_eq!(dir.load(&from).unwrap(), "one", "source must be untouched");
        assert_eq!(
            dir.load(&to).unwrap(),
            "two",
            "destination must be untouched"
        );
    }

    #[test]
    fn rename_moves_the_file_when_the_destination_is_free() {
        let (_tmp, dir) = temp_dir();
        let from = PageName::new("from.sql").unwrap();
        let to = PageName::new("to.sql").unwrap();
        dir.save(&from, "content").unwrap();

        dir.rename(&from, &to).expect("rename should succeed");

        assert!(!dir.exists(&from));
        assert_eq!(dir.load(&to).unwrap(), "content");
    }

    #[test]
    fn delete_removes_the_file() {
        let (_tmp, dir) = temp_dir();
        let name = PageName::new("gone.sql").unwrap();
        dir.save(&name, "content").unwrap();

        dir.delete(&name).expect("delete should succeed");

        assert!(!dir.exists(&name));
    }

    // --- Byte-exact round trip ---

    #[test]
    fn save_then_load_is_byte_identical_for_various_contents() {
        let cases = [
            "no trailing newline",
            "trailing newline\n",
            "all\r\ncrlf\r\nlines\r\n",
            "mixed\r\ncrlf\nand lf\r\nlines\n",
            "",
            "emoji 🦀 and non-ascii café naïve\n",
        ];
        let (_tmp, dir) = temp_dir();
        for (i, content) in cases.iter().enumerate() {
            let name = PageName::new(&format!("case{i}.sql")).unwrap();
            dir.save(&name, content).expect("save should succeed");
            let loaded = dir.load(&name).expect("load should succeed");
            assert_eq!(
                &loaded, content,
                "byte-exact round trip failed for case {i}"
            );
        }
    }

    #[test]
    fn load_of_non_utf8_bytes_returns_not_utf8_not_a_panic() {
        let (tmp, dir) = temp_dir();
        let path = tmp.path().join("bad.sql");
        fs::write(&path, [0xff, 0xfe, 0xfd]).unwrap();
        let name = PageName::new("bad.sql").unwrap();

        let err = dir
            .load(&name)
            .expect_err("non-utf8 content must error, not panic");
        assert!(matches!(err, PagesError::NotUtf8 { .. }));
    }

    #[test]
    #[cfg(unix)]
    fn save_writes_the_file_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let (tmp, dir) = temp_dir();
        let name = PageName::new("perm.sql").unwrap();
        dir.save(&name, "content").unwrap();

        let mode = fs::metadata(tmp.path().join("perm.sql"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    #[cfg(unix)]
    fn for_connection_creates_the_directory_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir creation");
        let root = tmp.path().join("nested").join("pages-dir");
        create_dir_private(&root).expect("dir creation should succeed");

        let mode = fs::metadata(&root).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PagesError {
    #[error(transparent)]
    Path(#[from] crate::config::ConfigError),
    #[error("invalid page name {name:?}: {reason}")]
    InvalidName { name: String, reason: &'static str },
    #[error("a page named {0:?} already exists")]
    AlreadyExists(String),
    #[error("no page named {0:?}")]
    NotFound(String),
    #[error("{} is not valid UTF-8", path.display())]
    NotUtf8 { path: PathBuf },
    #[error("failed to read {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write {}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}
