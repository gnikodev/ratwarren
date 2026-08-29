pub mod paths;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const CONFIG_FILE_NAME: &str = "config.toml";
pub const KEYRING_SERVICE: &str = "ratwarren";
pub const DEFAULT_POSTGRES_PORT: u16 = 5432;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub connections: Vec<Connection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    pub name: String,
    // Flat label only — no nesting, no path syntax. Grouping is byte-exact
    // string equality; see Config::grouped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    // Address of Postgres as seen by whoever opens the TCP connection:
    // this machine when `tunnel` is None, the bastion when it is Some.
    pub host: String,
    #[serde(default = "default_postgres_port")]
    pub port: u16,
    pub database: String,
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<SecretRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<SshTunnel>,
}

/// A view over `Config::connections`, never serialized. `label` is None for
/// connections with no `group` key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionGroup<'a> {
    pub label: Option<&'a str>,
    pub connections: Vec<&'a Connection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum SecretRef {
    Keyring {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshTunnel {
    // SSH destination exactly as `ssh` understands it — a Host alias from
    // ~/.ssh/config is the expected common case.
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    // SSH port on the bastion, not the DB port. None => let ssh/ssh_config decide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

fn default_postgres_port() -> u16 {
    DEFAULT_POSTGRES_PORT
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not determine a config directory for this platform")]
    NoConfigDir,
    #[error("could not determine a data directory for this platform")]
    NoDataDir,
    #[error("failed to read config file {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write config file {}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("failed to serialize config: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("connection {name:?}: `{field}` must not be empty")]
    EmptyField { name: String, field: &'static str },
    #[error("connection {name:?}: `{field}` must not be 0")]
    ZeroPort { name: String, field: &'static str },
    #[error("connection {name:?}: `{field}` {reason}")]
    InvalidField {
        name: String,
        field: &'static str,
        reason: &'static str,
    },
    #[error("duplicate connection name {name:?}")]
    DuplicateConnectionName { name: String },
}

impl Config {
    pub fn parse_toml(text: &str) -> Result<Config, ConfigError> {
        let config: Config = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    pub fn to_toml(&self) -> Result<String, ConfigError> {
        self.validate()?;
        let text = toml::to_string_pretty(self)?;
        Ok(text)
    }

    pub fn load_from(path: &Path) -> Result<Config, ConfigError> {
        Config::load_resolved(path, false)
    }

    // Shared by `load()` and `load_from()`. `explicit` distinguishes "the user
    // pointed us at this path themselves" (missing file is an error) from
    // "this is the default location" (missing file means first run).
    pub(crate) fn load_resolved(path: &Path, explicit: bool) -> Result<Config, ConfigError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if !explicit && err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Config::default());
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        Config::parse_toml(&text)
    }

    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        let text = self.to_toml()?;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        }

        let tmp_path = {
            let mut tmp = path.as_os_str().to_owned();
            tmp.push(".tmp");
            PathBuf::from(tmp)
        };

        let mut open_options = fs::OpenOptions::new();
        open_options.write(true).create(true).truncate(true);
        // Set the mode at creation time (not via a post-hoc chmod) so the file is
        // never briefly world-readable before permissions are tightened.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open_options.mode(0o600);
        }

        let mut tmp_file = open_options
            .open(&tmp_path)
            .map_err(|source| ConfigError::Write {
                path: tmp_path.clone(),
                source,
            })?;
        tmp_file
            .write_all(text.as_bytes())
            .map_err(|source| ConfigError::Write {
                path: tmp_path.clone(),
                source,
            })?;
        drop(tmp_file);

        fs::rename(&tmp_path, path).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })?;

        Ok(())
    }

    pub fn load() -> Result<Config, ConfigError> {
        let (path, explicit) = paths::resolve_config_path()?;
        // When `explicit` (RATWARREN_CONFIG was set), a missing file is a real
        // error (e.g. a typo), not "first run" — silently defaulting here risks
        // `save()` clobbering the wrong path.
        Config::load_resolved(&path, explicit)
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        let path = paths::config_file_path()?;
        self.save_to(&path)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut seen_names: Vec<&str> = Vec::with_capacity(self.connections.len());

        for connection in &self.connections {
            connection.validate()?;

            if seen_names.contains(&connection.name.as_str()) {
                return Err(ConfigError::DuplicateConnectionName {
                    name: connection.name.clone(),
                });
            }
            seen_names.push(connection.name.as_str());
        }

        Ok(())
    }

    pub fn connection(&self, name: &str) -> Option<&Connection> {
        self.connections.iter().find(|c| c.name == name)
    }

    /// Connections bucketed by `group`, ready to render top-to-bottom with no
    /// further ordering logic.
    ///
    /// Bucket order is first appearance in the file, treating "no group" as an
    /// ordinary bucket key: the ungrouped bucket appears where its first
    /// ungrouped connection appears, not pinned to the top or bottom. Member
    /// order within a bucket is file order. Group labels compare byte-exact —
    /// no trimming, no case folding. An empty `connections` list yields an
    /// empty Vec.
    pub fn grouped(&self) -> Vec<ConnectionGroup<'_>> {
        let mut groups: Vec<ConnectionGroup<'_>> = Vec::new();

        for connection in &self.connections {
            let label = connection.group.as_deref();
            match groups.iter_mut().find(|g| g.label == label) {
                Some(group) => group.connections.push(connection),
                None => groups.push(ConnectionGroup {
                    label,
                    connections: vec![connection],
                }),
            }
        }

        groups
    }
}

impl Connection {
    fn validate(&self) -> Result<(), ConfigError> {
        let empty_field = |field: &'static str| ConfigError::EmptyField {
            name: self.name.clone(),
            field,
        };
        let zero_port = |field: &'static str| ConfigError::ZeroPort {
            name: self.name.clone(),
            field,
        };
        let invalid_field = |field: &'static str, reason: &'static str| ConfigError::InvalidField {
            name: self.name.clone(),
            field,
            reason,
        };

        if self.name.trim().is_empty() {
            return Err(empty_field("name"));
        }
        // Whitespace-only is rejected, but leading/trailing whitespace on an
        // otherwise non-empty label (e.g. " prod ") is accepted as-is and is
        // distinct from "prod" — trimming/normalizing labels is out of scope.
        if let Some(group) = &self.group
            && group.trim().is_empty()
        {
            return Err(empty_field("group"));
        }
        if self.host.trim().is_empty() {
            return Err(empty_field("host"));
        }
        if self.database.trim().is_empty() {
            return Err(empty_field("database"));
        }
        if self.user.trim().is_empty() {
            return Err(empty_field("user"));
        }
        if self.port == 0 {
            return Err(zero_port("port"));
        }

        if let Some(tunnel) = &self.tunnel {
            if tunnel.host.trim().is_empty() {
                return Err(empty_field("tunnel.host"));
            }
            // `tunnel.host` is passed as a positional argv element to `ssh` (Phase
            // 2); a leading `-` would be parsed as an ssh option, not a hostname.
            if tunnel.host.starts_with('-') {
                return Err(invalid_field("tunnel.host", "must not start with '-'"));
            }
            // `tunnel.host` is a bare ssh destination (a ~/.ssh/config Host alias
            // or hostname), passed as ssh's positional argument. ssh doesn't accept
            // `host:port` syntax there (that needs `-p` or a `ssh://` URI), so a
            // colon signals a copy-pasted connection-string-style value. IPv6
            // bastions aren't representable here — use a ~/.ssh/config Host alias
            // for those instead; that's a deliberate MVP0 limitation, not an
            // oversight.
            if tunnel.host.contains(':') {
                return Err(invalid_field("tunnel.host", "must not contain ':'"));
            }
            // `tunnel.host` is combined with `tunnel.user` into a single
            // `user@host` destination string by the tunnel command builder; an
            // '@' inside `host` itself would be ambiguous with that separator.
            // `tunnel.user` has no such ambiguity (it's passed as the literal
            // characters before the injected '@'), so email-style login names
            // like `alice@corp.com` (common on LDAP/SSO-fronted bastions) are
            // fine there — see TunnelSpec::from_parts (Phase 2), which is the
            // sole gate before argv construction and agrees.
            if tunnel.host.contains('@') {
                return Err(invalid_field(
                    "tunnel.host",
                    "must not contain '@' — put the login name in `user` instead",
                ));
            }
            if let Some(user) = &tunnel.user {
                if user.trim().is_empty() {
                    return Err(empty_field("tunnel.user"));
                }
                if user.starts_with('-') {
                    return Err(invalid_field("tunnel.user", "must not start with '-'"));
                }
            }
            if tunnel.port == Some(0) {
                return Err(zero_port("tunnel.port"));
            }

            // `host` becomes the third field of ssh's `-L local:host:hostport`
            // triple in Phase 2; a bare ':' there would silently corrupt the
            // forward spec. IPv6 literals are legitimate — the tunnel builder
            // brackets them. This check is gated on tunnel.is_some() because
            // without a tunnel, `host` goes straight to tokio-postgres, which
            // accepts IPv6 literals directly.
            if self.host.contains(':') && self.host.parse::<std::net::Ipv6Addr>().is_err() {
                return Err(invalid_field(
                    "host",
                    "must not contain ':' unless it is an IPv6 literal",
                ));
            }
        }

        if let Some(SecretRef::Keyring {
            account: Some(account),
        }) = &self.password
            && account.trim().is_empty()
        {
            return Err(empty_field("password.account"));
        }

        Ok(())
    }

    // Keyring account key to look up. None when no password is configured.
    // Defaults to `user@host:port/database` when SecretRef::Keyring.account is None.
    pub fn keyring_account(&self) -> Option<String> {
        match &self.password {
            Some(SecretRef::Keyring { account: Some(a) }) => Some(a.clone()),
            Some(SecretRef::Keyring { account: None }) => Some(format!(
                "{}@{}:{}/{}",
                self.user, self.host, self.port, self.database
            )),
            None => None,
        }
    }

    pub fn keyring_service(&self) -> &'static str {
        KEYRING_SERVICE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_connection() -> Connection {
        Connection {
            name: "prod".to_string(),
            group: Some("production".to_string()),
            host: "db.internal".to_string(),
            port: 6543,
            database: "app".to_string(),
            user: "app_user".to_string(),
            password: Some(SecretRef::Keyring {
                account: Some("prod-account".to_string()),
            }),
            tunnel: Some(SshTunnel {
                host: "bastion".to_string(),
                user: Some("deploy".to_string()),
                port: Some(2222),
            }),
        }
    }

    fn minimal_connection() -> Connection {
        Connection {
            name: "local".to_string(),
            group: None,
            host: "localhost".to_string(),
            port: DEFAULT_POSTGRES_PORT,
            database: "app".to_string(),
            user: "app_user".to_string(),
            password: None,
            tunnel: None,
        }
    }

    #[test]
    fn round_trips_a_full_connection_with_tunnel_and_password() {
        let config = Config {
            connections: vec![full_connection()],
        };

        let text = config.to_toml().expect("valid config serializes");
        let parsed = Config::parse_toml(&text).expect("round-tripped TOML parses");

        assert_eq!(parsed, config);
    }

    #[test]
    fn round_trips_a_minimal_connection_without_tunnel_or_password() {
        let config = Config {
            connections: vec![minimal_connection()],
        };

        let text = config.to_toml().expect("valid config serializes");
        let parsed = Config::parse_toml(&text).expect("round-tripped TOML parses");

        assert_eq!(parsed, config);
    }

    #[test]
    fn pre_grouping_config_parses_and_reserializes_without_a_group_key() {
        let text = r#"
            [[connections]]
            name = "local"
            host = "localhost"
            database = "app"
            user = "app_user"
        "#;

        let config = Config::parse_toml(text).expect("pre-grouping config is valid");
        let reserialized = config.to_toml().expect("valid config serializes");

        assert!(!reserialized.contains("group"));

        let reparsed = Config::parse_toml(&reserialized).expect("reserialized TOML parses");
        assert_eq!(reparsed, config);
    }

    #[test]
    fn omitted_group_parses_as_none() {
        let text = r#"
            [[connections]]
            name = "local"
            host = "localhost"
            database = "app"
            user = "app_user"
        "#;

        let config = Config::parse_toml(text).expect("group is optional");

        assert!(config.connections[0].group.is_none());
    }

    #[test]
    fn non_string_group_is_rejected_as_a_type_error() {
        let text = r#"
            [[connections]]
            name = "local"
            host = "localhost"
            database = "app"
            user = "app_user"
            group = 3
        "#;

        let err = Config::parse_toml(text).expect_err("group must be a string, not a number");
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    fn connection_with_group(name: &str, group: Option<&str>) -> Connection {
        let mut connection = minimal_connection();
        connection.name = name.to_string();
        connection.group = group.map(str::to_string);
        connection
    }

    #[test]
    fn grouped_orders_buckets_by_first_appearance() {
        let a = connection_with_group("a", Some("group1"));
        let b = connection_with_group("b", None);
        let c = connection_with_group("c", Some("group2"));
        let d = connection_with_group("d", Some("group1"));

        let config = Config {
            connections: vec![a.clone(), b.clone(), c.clone(), d.clone()],
        };

        let groups = config.grouped();
        let labels: Vec<Option<&str>> = groups.iter().map(|g| g.label).collect();
        assert_eq!(labels, vec![Some("group1"), None, Some("group2")]);

        let group1 = groups
            .iter()
            .find(|g| g.label == Some("group1"))
            .expect("group1 present");
        assert_eq!(group1.connections, vec![&a, &d]);
    }

    // The other ordering tests build `Config { connections: vec![...] }` by
    // hand, which never exercises whether TOML array-of-tables document order
    // actually survives deserialization into that same `Vec` order -- that
    // link is `grouped()`'s entire documented contract, so it needs at least
    // one test that goes through `parse_toml` rather than a hand-built `Vec`.
    #[test]
    fn grouped_orders_by_document_order_through_parse_toml() {
        let text = r#"
            [[connections]]
            name = "a"
            group = "group1"
            host = "h"
            database = "d"
            user = "u"

            [[connections]]
            name = "b"
            host = "h"
            database = "d"
            user = "u"

            [[connections]]
            name = "c"
            group = "group2"
            host = "h"
            database = "d"
            user = "u"

            [[connections]]
            name = "d"
            group = "group1"
            host = "h"
            database = "d"
            user = "u"
        "#;

        let config = Config::parse_toml(text).expect("valid config");
        let groups = config.grouped();
        let labels: Vec<Option<&str>> = groups.iter().map(|g| g.label).collect();
        assert_eq!(labels, vec![Some("group1"), None, Some("group2")]);

        let group1 = groups
            .iter()
            .find(|g| g.label == Some("group1"))
            .expect("group1 present");
        let names: Vec<&str> = group1.connections.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["a", "d"]);
    }

    #[test]
    fn grouped_places_the_ungrouped_bucket_at_its_first_appearance() {
        let a = connection_with_group("a", None);
        let b = connection_with_group("b", Some("group1"));

        let config = Config {
            connections: vec![a, b],
        };

        let groups = config.grouped();
        assert_eq!(groups[0].label, None);
    }

    #[test]
    fn grouped_is_empty_for_a_config_with_no_connections() {
        let config = Config::default();

        assert!(config.grouped().is_empty());
    }

    #[test]
    fn grouped_treats_labels_as_byte_exact() {
        let a = connection_with_group("a", Some("prod"));
        let b = connection_with_group("b", Some("Prod"));

        let config = Config {
            connections: vec![a, b],
        };

        let groups = config.grouped();
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn grouped_single_connection_with_a_group_is_its_own_bucket() {
        let a = connection_with_group("a", Some("solo"));

        let config = Config {
            connections: vec![a.clone()],
        };

        let groups = config.grouped();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].label, Some("solo"));
        assert_eq!(groups[0].connections, vec![&a]);
    }

    #[test]
    fn grouped_puts_all_connections_sharing_one_group_in_a_single_bucket() {
        let a = connection_with_group("a", Some("shared"));
        let b = connection_with_group("b", Some("shared"));
        let c = connection_with_group("c", Some("shared"));

        let config = Config {
            connections: vec![a.clone(), b.clone(), c.clone()],
        };

        let groups = config.grouped();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].label, Some("shared"));
        assert_eq!(groups[0].connections, vec![&a, &b, &c]);
    }

    #[test]
    fn grouped_does_not_confuse_a_group_label_with_a_connections_name() {
        // "alpha" is a connection *name* here, not a group. A second
        // connection's *group* happens to be the literal string "alpha".
        // Confirm the two fields are never cross-matched.
        let alpha_named = connection_with_group("alpha", None);
        let beta_grouped_alpha = connection_with_group("beta", Some("alpha"));

        let config = Config {
            connections: vec![alpha_named.clone(), beta_grouped_alpha.clone()],
        };

        let groups = config.grouped();
        assert_eq!(groups.len(), 2);

        let ungrouped = groups
            .iter()
            .find(|g| g.label.is_none())
            .expect("ungrouped bucket present");
        assert_eq!(ungrouped.connections, vec![&alpha_named]);

        let alpha_group = groups
            .iter()
            .find(|g| g.label == Some("alpha"))
            .expect("\"alpha\" group present");
        assert_eq!(alpha_group.connections, vec![&beta_grouped_alpha]);
    }

    #[test]
    fn grouped_handles_a_long_alternating_sequence_without_off_by_one_errors() {
        let connections: Vec<Connection> = vec![
            connection_with_group("c0", Some("g1")),
            connection_with_group("c1", None),
            connection_with_group("c2", Some("g2")),
            connection_with_group("c3", Some("g1")),
            connection_with_group("c4", None),
            connection_with_group("c5", Some("g3")),
            connection_with_group("c6", Some("g2")),
            connection_with_group("c7", None),
            connection_with_group("c8", Some("g1")),
        ];

        let config = Config {
            connections: connections.clone(),
        };

        let groups = config.grouped();
        let labels: Vec<Option<&str>> = groups.iter().map(|g| g.label).collect();
        assert_eq!(labels, vec![Some("g1"), None, Some("g2"), Some("g3")]);

        let names_for = |label: Option<&str>| -> Vec<&str> {
            groups
                .iter()
                .find(|g| g.label == label)
                .expect("group present")
                .connections
                .iter()
                .map(|c| c.name.as_str())
                .collect()
        };

        assert_eq!(names_for(Some("g1")), vec!["c0", "c3", "c8"]);
        assert_eq!(names_for(None), vec!["c1", "c4", "c7"]);
        assert_eq!(names_for(Some("g2")), vec!["c2", "c6"]);
        assert_eq!(names_for(Some("g3")), vec!["c5"]);
    }

    #[test]
    fn grouped_does_not_assume_its_input_has_been_validated() {
        // Bypasses `validate()` entirely via direct struct construction —
        // an empty-string group could never reach `grouped()` through
        // `parse_toml`/`to_toml`, but `grouped()` takes a `&Config`
        // directly and must not panic or misbehave if called on one that
        // was never validated.
        let a = connection_with_group("a", Some(""));
        let b = connection_with_group("b", Some(""));
        let c = connection_with_group("c", None);

        let config = Config {
            connections: vec![a.clone(), b.clone(), c.clone()],
        };

        let groups = config.grouped();
        let labels: Vec<Option<&str>> = groups.iter().map(|g| g.label).collect();
        assert_eq!(labels, vec![Some(""), None]);
        assert_eq!(groups[0].connections, vec![&a, &b]);
        assert_eq!(groups[1].connections, vec![&c]);
    }

    #[test]
    fn connection_group_equality_is_sensitive_to_member_order() {
        let a = minimal_connection();
        let mut b = minimal_connection();
        b.name = "other".to_string();

        let forward = ConnectionGroup {
            label: Some("g"),
            connections: vec![&a, &b],
        };
        let reversed = ConnectionGroup {
            label: Some("g"),
            connections: vec![&b, &a],
        };
        let same_order = ConnectionGroup {
            label: Some("g"),
            connections: vec![&a, &b],
        };

        assert_ne!(
            forward, reversed,
            "PartialEq must be order-sensitive, not set-equality"
        );
        assert_eq!(forward, same_order);
    }

    #[test]
    fn empty_toml_parses_to_default_config() {
        let config = Config::parse_toml("").expect("empty document is valid");

        assert_eq!(config, Config::default());
        assert!(config.connections.is_empty());
    }

    #[test]
    fn omitted_port_defaults_to_postgres_port() {
        let text = r#"
            [[connections]]
            name = "local"
            host = "localhost"
            database = "app"
            user = "app_user"
        "#;

        let config = Config::parse_toml(text).expect("port is optional");

        assert_eq!(config.connections[0].port, DEFAULT_POSTGRES_PORT);
    }

    #[test]
    fn unknown_top_level_key_is_rejected() {
        let text = r#"
            unknown_key = "surprise"
        "#;

        let err = Config::parse_toml(text).expect_err("unknown key should be rejected");
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    #[test]
    fn unknown_connection_key_is_rejected() {
        let text = r#"
            [[connections]]
            name = "local"
            host = "localhost"
            database = "app"
            user = "app_user"
            surprise = "field"
        "#;

        let err = Config::parse_toml(text).expect_err("unknown key should be rejected");
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    #[test]
    fn bare_string_password_is_rejected_as_a_type_error() {
        let text = r#"
            [[connections]]
            name = "local"
            host = "localhost"
            database = "app"
            user = "app_user"
            password = "hunter2"
        "#;

        let err = Config::parse_toml(text).expect_err("password must be a table, not a string");
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    #[test]
    fn unknown_secret_source_variant_is_rejected() {
        let text = r#"
            [[connections]]
            name = "local"
            host = "localhost"
            database = "app"
            user = "app_user"

            [connections.password]
            source = "env"
        "#;

        let err = Config::parse_toml(text).expect_err("only `keyring` is a known source");
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    #[test]
    fn validate_rejects_duplicate_connection_names() {
        let mut a = minimal_connection();
        a.name = "dup".to_string();
        let mut b = full_connection();
        b.name = "dup".to_string();

        let config = Config {
            connections: vec![a, b],
        };

        let err = config.validate().expect_err("duplicate names are invalid");
        assert!(matches!(
            err,
            ConfigError::DuplicateConnectionName { name } if name == "dup"
        ));
    }

    #[test]
    fn validate_rejects_duplicate_names_across_different_groups() {
        let mut a = minimal_connection();
        a.name = "dup".to_string();
        a.group = Some("group1".to_string());
        let mut b = full_connection();
        b.name = "dup".to_string();
        b.group = Some("group2".to_string());

        let config = Config {
            connections: vec![a, b],
        };

        let err = config
            .validate()
            .expect_err("duplicate names across different groups are invalid");
        assert!(matches!(
            err,
            ConfigError::DuplicateConnectionName { name } if name == "dup"
        ));
    }

    #[test]
    fn validate_rejects_empty_name() {
        let mut connection = minimal_connection();
        connection.name = "  ".to_string();
        let config = Config {
            connections: vec![connection],
        };

        let err = config.validate().expect_err("empty name is invalid");
        assert!(matches!(err, ConfigError::EmptyField { field: "name", .. }));
    }

    #[test]
    fn validate_rejects_empty_group() {
        let mut connection = minimal_connection();
        connection.group = Some("".to_string());
        let config = Config {
            connections: vec![connection],
        };

        let err = config.validate().expect_err("empty group is invalid");
        assert!(matches!(
            err,
            ConfigError::EmptyField { field: "group", .. }
        ));
    }

    #[test]
    fn validate_rejects_whitespace_only_group() {
        let mut connection = minimal_connection();
        connection.group = Some("  ".to_string());
        let config = Config {
            connections: vec![connection],
        };

        let err = config
            .validate()
            .expect_err("whitespace-only group is invalid");
        assert!(matches!(
            err,
            ConfigError::EmptyField { field: "group", .. }
        ));
    }

    #[test]
    fn validate_rejects_empty_host() {
        let mut connection = minimal_connection();
        connection.host = "".to_string();
        let config = Config {
            connections: vec![connection],
        };

        let err = config.validate().expect_err("empty host is invalid");
        assert!(matches!(err, ConfigError::EmptyField { field: "host", .. }));
    }

    #[test]
    fn validate_rejects_empty_database() {
        let mut connection = minimal_connection();
        connection.database = "".to_string();
        let config = Config {
            connections: vec![connection],
        };

        let err = config.validate().expect_err("empty database is invalid");
        assert!(matches!(
            err,
            ConfigError::EmptyField {
                field: "database",
                ..
            }
        ));
    }

    #[test]
    fn validate_rejects_empty_user() {
        let mut connection = minimal_connection();
        connection.user = "".to_string();
        let config = Config {
            connections: vec![connection],
        };

        let err = config.validate().expect_err("empty user is invalid");
        assert!(matches!(err, ConfigError::EmptyField { field: "user", .. }));
    }

    #[test]
    fn validate_rejects_zero_port() {
        let mut connection = minimal_connection();
        connection.port = 0;
        let config = Config {
            connections: vec![connection],
        };

        let err = config.validate().expect_err("port 0 is invalid");
        assert!(matches!(err, ConfigError::ZeroPort { field: "port", .. }));
    }

    #[test]
    fn validate_rejects_tunnel_with_empty_host() {
        let mut connection = minimal_connection();
        connection.tunnel = Some(SshTunnel {
            host: "".to_string(),
            user: None,
            port: None,
        });
        let config = Config {
            connections: vec![connection],
        };

        let err = config.validate().expect_err("empty tunnel host is invalid");
        assert!(matches!(
            err,
            ConfigError::EmptyField {
                field: "tunnel.host",
                ..
            }
        ));
    }

    #[test]
    fn validate_rejects_tunnel_host_starting_with_dash() {
        let mut connection = minimal_connection();
        connection.tunnel = Some(SshTunnel {
            host: "-oProxyCommand=touch /tmp/pwned".to_string(),
            user: None,
            port: None,
        });
        let config = Config {
            connections: vec![connection],
        };

        let err = config
            .validate()
            .expect_err("tunnel host starting with '-' is invalid");
        assert!(matches!(
            err,
            ConfigError::InvalidField {
                field: "tunnel.host",
                ..
            }
        ));
    }

    #[test]
    fn validate_rejects_tunnel_host_containing_colon() {
        let mut connection = minimal_connection();
        connection.tunnel = Some(SshTunnel {
            host: "bastion:22".to_string(),
            user: None,
            port: None,
        });
        let config = Config {
            connections: vec![connection],
        };

        let err = config
            .validate()
            .expect_err("tunnel host containing ':' is invalid");
        assert!(matches!(
            err,
            ConfigError::InvalidField {
                field: "tunnel.host",
                ..
            }
        ));
    }

    #[test]
    fn validate_rejects_host_with_colon_when_tunnel_present_and_host_not_ipv6() {
        let mut connection = minimal_connection();
        connection.host = "db.internal:5432".to_string();
        connection.tunnel = Some(SshTunnel {
            host: "bastion".to_string(),
            user: None,
            port: None,
        });
        let config = Config {
            connections: vec![connection],
        };

        let err = config
            .validate()
            .expect_err("colon-containing non-IPv6 host with a tunnel is invalid");
        assert!(matches!(
            err,
            ConfigError::InvalidField { field: "host", .. }
        ));
    }

    #[test]
    fn validate_accepts_ipv6_literal_host_when_tunnel_present() {
        let mut connection = minimal_connection();
        connection.host = "::1".to_string();
        connection.tunnel = Some(SshTunnel {
            host: "bastion".to_string(),
            user: None,
            port: None,
        });
        let config = Config {
            connections: vec![connection],
        };

        config
            .validate()
            .expect("an IPv6 literal host with a tunnel is valid");
    }

    #[test]
    fn validate_accepts_host_with_colon_when_no_tunnel_present() {
        // Without a tunnel, `host` goes straight to tokio-postgres, which
        // accepts IPv6 literals directly — the colon check is tunnel-only.
        let mut connection = minimal_connection();
        connection.host = "not-ipv6:but-no-tunnel".to_string();
        connection.tunnel = None;
        let config = Config {
            connections: vec![connection],
        };

        config
            .validate()
            .expect("host colon check only applies when a tunnel is configured");
    }

    #[test]
    fn validate_rejects_tunnel_with_empty_user() {
        let mut connection = minimal_connection();
        connection.tunnel = Some(SshTunnel {
            host: "bastion".to_string(),
            user: Some("  ".to_string()),
            port: None,
        });
        let config = Config {
            connections: vec![connection],
        };

        let err = config.validate().expect_err("blank tunnel user is invalid");
        assert!(matches!(
            err,
            ConfigError::EmptyField {
                field: "tunnel.user",
                ..
            }
        ));
    }

    #[test]
    fn validate_rejects_tunnel_user_starting_with_dash() {
        let mut connection = minimal_connection();
        connection.tunnel = Some(SshTunnel {
            host: "bastion".to_string(),
            user: Some("-oProxyCommand=touch /tmp/pwned".to_string()),
            port: None,
        });
        let config = Config {
            connections: vec![connection],
        };

        let err = config
            .validate()
            .expect_err("tunnel user starting with '-' is invalid");
        assert!(matches!(
            err,
            ConfigError::InvalidField {
                field: "tunnel.user",
                ..
            }
        ));
    }

    #[test]
    fn validate_rejects_tunnel_with_zero_port() {
        let mut connection = minimal_connection();
        connection.tunnel = Some(SshTunnel {
            host: "bastion".to_string(),
            user: None,
            port: Some(0),
        });
        let config = Config {
            connections: vec![connection],
        };

        let err = config.validate().expect_err("tunnel port 0 is invalid");
        assert!(matches!(
            err,
            ConfigError::ZeroPort {
                field: "tunnel.port",
                ..
            }
        ));
    }

    #[test]
    fn validate_rejects_empty_keyring_account() {
        let mut connection = minimal_connection();
        connection.password = Some(SecretRef::Keyring {
            account: Some("  ".to_string()),
        });
        let config = Config {
            connections: vec![connection],
        };

        let err = config
            .validate()
            .expect_err("blank keyring account is invalid");
        assert!(matches!(
            err,
            ConfigError::EmptyField {
                field: "password.account",
                ..
            }
        ));
    }

    #[test]
    fn load_from_nonexistent_path_returns_default_config() {
        let dir = tempfile::tempdir().expect("tempdir creation");
        let path = dir.path().join("does-not-exist.toml");

        let config = Config::load_from(&path).expect("missing file is not an error");

        assert_eq!(config, Config::default());
    }

    #[test]
    fn save_to_then_load_from_round_trips_through_disk_creating_parent_dirs() {
        let dir = tempfile::tempdir().expect("tempdir creation");
        let path = dir.path().join("nested").join("subdir").join("config.toml");

        let config = Config {
            connections: vec![full_connection(), minimal_connection()],
        };

        config.save_to(&path).expect("save_to should succeed");
        let loaded = Config::load_from(&path).expect("load_from should succeed");

        assert_eq!(loaded, config);
    }

    #[test]
    #[cfg(unix)]
    fn save_to_writes_the_file_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir creation");
        let path = dir.path().join("config.toml");

        let config = Config {
            connections: vec![minimal_connection()],
        };
        config.save_to(&path).expect("save_to should succeed");

        let mode = fs::metadata(&path)
            .expect("saved file should exist")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn load_with_missing_explicit_env_override_path_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir creation");
        let path = dir.path().join("does-not-exist.toml");

        let result = Config::load_resolved(&path, true);

        let err = result.expect_err("missing explicit override path should be an error");
        assert!(matches!(err, ConfigError::Read { .. }));
    }

    #[test]
    fn keyring_account_is_none_without_a_password() {
        let connection = minimal_connection();
        assert_eq!(connection.keyring_account(), None);
    }

    #[test]
    fn keyring_account_uses_explicit_account_when_present() {
        let mut connection = minimal_connection();
        connection.password = Some(SecretRef::Keyring {
            account: Some("explicit-account".to_string()),
        });

        assert_eq!(
            connection.keyring_account(),
            Some("explicit-account".to_string())
        );
    }

    #[test]
    fn keyring_account_derives_default_when_account_is_none() {
        let mut connection = minimal_connection();
        connection.password = Some(SecretRef::Keyring { account: None });

        assert_eq!(
            connection.keyring_account(),
            Some(format!(
                "{}@{}:{}/{}",
                connection.user, connection.host, connection.port, connection.database
            ))
        );
    }

    #[test]
    fn keyring_service_is_the_shared_constant() {
        let connection = minimal_connection();
        assert_eq!(connection.keyring_service(), KEYRING_SERVICE);
    }
}
