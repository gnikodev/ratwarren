use std::time::Duration;

pub const PASSWORD_ENV_VAR: &str = "RATWARREN_PASSWORD";

const KEYRING_NOTICE_AFTER: Duration = Duration::from_secs(3);
const KEYRING_GIVE_UP_AFTER: Duration = Duration::from_secs(60);

/// Where the async keyring path (`resolve_with_async`) sends progress notes
/// that would otherwise have gone to stderr -- routed instead to the
/// session's `Connecting` label so they never land on the alternate screen.
/// See `app::session::OpenEvent::Progress`.
pub type NoteSink = dyn Fn(String) + Send + Sync;

pub enum Resolved {
    FromEnv(String),
    FromKeyring(String),
    NotConfigured,
    Unavailable { reason: String },
}

// Hand-written, NOT derived: a derived Debug would print the password
// itself, leaking it into any future dbg!/format!("{:?}") call site.
impl std::fmt::Debug for Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Resolved::FromEnv(_) => "FromEnv",
            Resolved::FromKeyring(_) => "FromKeyring",
            Resolved::NotConfigured => "NotConfigured",
            Resolved::Unavailable { .. } => "Unavailable",
        };
        write!(f, "Resolved::{name}(..)")
    }
}

impl Resolved {
    pub fn password(&self) -> Option<&str> {
        match self {
            Resolved::FromEnv(p) | Resolved::FromKeyring(p) => Some(p),
            Resolved::NotConfigured | Resolved::Unavailable { .. } => None,
        }
    }

    pub fn note(&self) -> Option<String> {
        match self {
            Resolved::FromEnv(_) => Some("using the password from RATWARREN_PASSWORD".to_string()),
            Resolved::FromKeyring(_) | Resolved::NotConfigured => None,
            Resolved::Unavailable { reason } => Some(format!(
                "could not read the password from the OS keyring ({reason}); connecting \
                 without one — set RATWARREN_PASSWORD to supply it directly"
            )),
        }
    }
}

/// Shared by `resolve_with` and `resolve_with_async`: the env-var and
/// not-configured branches never touch the keyring and so never need to be
/// async, no matter which caller is asking.
enum PreLookup {
    Resolved(Resolved),
    NeedsLookup { service: &'static str, account: String },
}

fn pre_lookup(conn: &crate::config::Connection, env_password: Option<String>) -> PreLookup {
    if let Some(p) = env_password
        && !p.is_empty()
    {
        return PreLookup::Resolved(Resolved::FromEnv(p));
    }
    let Some(account) = conn.keyring_account() else {
        return PreLookup::Resolved(Resolved::NotConfigured);
    };
    PreLookup::NeedsLookup {
        service: conn.keyring_service(),
        account,
    }
}

// No production caller since S1/S3 (resolve_async/resolve_with_async replace
// it end to end); kept #[cfg(test)] rather than deleted purely so its own
// unit tests below keep pinning the FromEnv/NotConfigured/FromKeyring/
// Unavailable decision matrix independently of the async plumbing.
#[cfg(test)]
pub(crate) fn resolve_with<F>(
    conn: &crate::config::Connection,
    env_password: Option<String>,
    lookup: F,
) -> Resolved
where
    F: FnOnce(&str, &str) -> Result<String, String>,
{
    match pre_lookup(conn, env_password) {
        PreLookup::Resolved(r) => r,
        PreLookup::NeedsLookup { service, account } => match lookup(service, &account) {
            Ok(p) => Resolved::FromKeyring(p),
            Err(reason) => Resolved::Unavailable { reason },
        },
    }
}

/// Async sibling of `resolve_with`. `resolve()`/`keyring_lookup` (the old
/// blocking path, pre-S1) are gone: wrapping the blocking keyring call in
/// `spawn_blocking` would reintroduce exactly the quit-hang their removed
/// doc comment warned about, since `#[tokio::main]`'s runtime drop waits on
/// the blocking-task pool. This keeps the same detached `std::thread` --
/// its justification gets stronger here, not weaker -- but replaces
/// `std::sync::mpsc` + `recv_timeout` with `tokio::sync::oneshot` +
/// `tokio::time::timeout`, so nothing occupies a blocking-pool thread and
/// quit is never delayed by a hung keyring.
pub async fn resolve_async(conn: &crate::config::Connection, notes: &NoteSink) -> Resolved {
    let env_password = std::env::var(PASSWORD_ENV_VAR).ok();
    resolve_with_async(conn, env_password, keyring_lookup_blocking, notes).await
}

pub(crate) async fn resolve_with_async<F>(
    conn: &crate::config::Connection,
    env_password: Option<String>,
    lookup: F,
    notes: &NoteSink,
) -> Resolved
where
    F: FnOnce(&str, &str) -> Result<String, String> + Send + 'static,
{
    let (service, account) = match pre_lookup(conn, env_password) {
        PreLookup::Resolved(r) => return r,
        PreLookup::NeedsLookup { service, account } => (service, account),
    };

    let (tx, mut rx) = tokio::sync::oneshot::channel();
    // Detached, not spawn_blocking: see this function's doc comment above.
    // If the OS keyring is showing an auth prompt nobody answers, the
    // process still exits once main() returns -- only this one lookup is
    // stuck, not shutdown; `tx.send` on a dropped `rx` is simply ignored.
    std::thread::spawn(move || {
        let result = lookup(service, &account);
        let _ = tx.send(result);
    });

    // `&mut rx` (not `rx` by value): passing the receiver into `timeout` by
    // value would drop it -- and with it, any result the thread sends after
    // the deadline -- the instant the first wait elapses, making the second
    // wait below unable to ever receive anything.
    match tokio::time::timeout(KEYRING_NOTICE_AFTER, &mut rx).await {
        Ok(lookup_result) => resolved_from_lookup(lookup_result),
        Err(_elapsed) => {
            notes(
                "still waiting for the OS keyring — if your OS is showing an authorization \
                 dialog, approve it (or press Ctrl-C and set RATWARREN_PASSWORD)"
                    .to_string(),
            );
            match tokio::time::timeout(
                KEYRING_GIVE_UP_AFTER.saturating_sub(KEYRING_NOTICE_AFTER),
                &mut rx,
            )
            .await
            {
                Ok(lookup_result) => resolved_from_lookup(lookup_result),
                Err(_elapsed) => Resolved::Unavailable {
                    reason: "timed out waiting for the OS keyring".to_string(),
                },
            }
        }
    }
}

fn resolved_from_lookup(
    lookup_result: Result<Result<String, String>, tokio::sync::oneshot::error::RecvError>,
) -> Resolved {
    match lookup_result {
        Ok(Ok(p)) => Resolved::FromKeyring(p),
        Ok(Err(reason)) => Resolved::Unavailable { reason },
        // The sender was dropped without sending -- only possible if the
        // lookup thread panicked.
        Err(_) => Resolved::Unavailable {
            reason: "the OS keyring lookup thread ended unexpectedly".to_string(),
        },
    }
}

fn keyring_lookup_blocking(service: &str, account: &str) -> Result<String, String> {
    let result = keyring::Entry::new(service, account).and_then(|e| e.get_password());
    classify(result)
}

fn classify(result: keyring::Result<String>) -> Result<String, String> {
    match result {
        Ok(p) => Ok(p),
        Err(keyring::Error::NoEntry) => Err("no entry in the OS keyring".to_string()),
        Err(keyring::Error::NoDefaultStore) => {
            Err("this machine has no usable OS keyring".to_string())
        }
        Err(e) => Err(crate::ui::error_chain(&e)),
    }
}

pub fn read_password_from_stdin(prompt: &str) -> std::io::Result<Option<String>> {
    use std::io::{IsTerminal, Write};
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    if std::io::stdin().is_terminal() {
        read_password_no_echo()
    } else {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let trimmed = line.trim_end_matches(['\r', '\n']);
        Ok(if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        })
    }
}

fn read_password_no_echo() -> std::io::Result<Option<String>> {
    use crossterm::event::{Event, KeyCode, KeyEventKind};

    struct RawModeGuard;
    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }

    crossterm::terminal::enable_raw_mode()?;
    let _guard = RawModeGuard;
    let mut buf = String::new();
    loop {
        if let Event::Key(key) = crossterm::event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Enter => break,
                // `\r\n`, not `eprintln!`/a bare `\n`: raw mode (still active
                // here, on every exit path) has OPOST/ONLCR cleared, so a
                // bare `\n` doesn't return the cursor to column 0 and
                // whatever's printed next ends up visually indented.
                KeyCode::Esc => {
                    eprint!("\r\n");
                    return Ok(None);
                }
                KeyCode::Char('c')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    eprint!("\r\n");
                    return Ok(None);
                }
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            }
        }
    }
    eprint!("\r\n");
    Ok(if buf.is_empty() { None } else { Some(buf) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Connection, SecretRef};

    fn connection_without_password() -> Connection {
        Connection {
            name: "test".to_string(),
            group: None,
            host: "localhost".to_string(),
            port: 5432,
            database: "app".to_string(),
            user: "app_user".to_string(),
            password: None,
            tunnel: None,
        }
    }

    fn connection_with_keyring_password() -> Connection {
        let mut conn = connection_without_password();
        conn.password = Some(SecretRef::Keyring {
            account: Some("test-account".to_string()),
        });
        conn
    }

    #[test]
    fn env_var_takes_precedence_over_keyring() {
        let conn = connection_with_keyring_password();
        let resolved = resolve_with(&conn, Some("from-env".to_string()), |_, _| {
            panic!("keyring lookup must not be attempted when the env var is set")
        });
        assert_eq!(resolved.password(), Some("from-env"));
    }

    #[test]
    fn empty_env_var_falls_through_to_keyring() {
        let conn = connection_with_keyring_password();
        let resolved = resolve_with(&conn, Some(String::new()), |_, _| {
            Ok("from-keyring".to_string())
        });
        assert_eq!(resolved.password(), Some("from-keyring"));
    }

    #[test]
    fn no_password_configured_and_no_env_var_is_not_configured() {
        let conn = connection_without_password();
        let resolved = resolve_with(&conn, None, |_, _| {
            panic!("keyring lookup must not be attempted without a configured password")
        });
        assert!(matches!(resolved, Resolved::NotConfigured));
        assert_eq!(resolved.password(), None);
        assert_eq!(resolved.note(), None);
    }

    #[test]
    fn keyring_lookup_success_is_used_when_no_env_var() {
        let conn = connection_with_keyring_password();
        let resolved = resolve_with(&conn, None, |service, account| {
            assert_eq!(service, conn.keyring_service());
            assert_eq!(account, "test-account");
            Ok("secret".to_string())
        });
        assert_eq!(resolved.password(), Some("secret"));
        assert_eq!(resolved.note(), None);
    }

    #[test]
    fn keyring_lookup_failure_is_unavailable_with_a_note() {
        let conn = connection_with_keyring_password();
        let resolved = resolve_with(&conn, None, |_, _| Err("no entry".to_string()));
        assert!(matches!(resolved, Resolved::Unavailable { .. }));
        assert_eq!(resolved.password(), None);
        let note = resolved.note().expect("Unavailable must carry a note");
        assert!(
            note.contains("no entry"),
            "the note must surface the actual lookup failure reason, got {note:?}"
        );
    }

    #[test]
    fn lookup_is_called_with_exactly_the_connections_keyring_service_and_account() {
        let conn = connection_with_keyring_password();
        let expected_service = conn.keyring_service();
        let expected_account = conn.keyring_account().expect("test setup: account is Some");
        let resolved = resolve_with(&conn, None, |service, account| {
            assert_eq!(service, expected_service);
            assert_eq!(account, expected_account);
            Ok("secret".to_string())
        });
        assert_eq!(resolved.password(), Some("secret"));
    }

    #[test]
    fn debug_never_prints_the_password_from_env() {
        let resolved = Resolved::FromEnv("hunter2".to_string());
        let debug = format!("{resolved:?}");
        assert!(!debug.contains("hunter2"));
    }

    #[test]
    fn debug_never_prints_the_password_from_keyring() {
        let resolved = Resolved::FromKeyring("supersecret".to_string());
        let debug = format!("{resolved:?}");
        assert!(
            !debug.contains("supersecret"),
            "Resolved's hand-written Debug impl must never leak the password, got {debug:?}"
        );
    }
}
