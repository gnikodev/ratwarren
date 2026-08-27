mod command;
mod port;

use std::io::BufRead;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(15);
pub const DEFAULT_PROBE_INTERVAL: Duration = Duration::from_millis(50);
pub const DEFAULT_PROBE_CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
pub const DEFAULT_PORT_ATTEMPTS: u32 = 3;
pub const STDERR_CAPTURE_LIMIT: usize = 8 * 1024;
pub const LOCAL_BIND_ADDR: Ipv4Addr = Ipv4Addr::LOCALHOST;

// How long to wait for a lost-bind-race ssh to have exited before trusting a
// successful TCP probe (see wait_ready).
const READY_SETTLE_DELAY: Duration = Duration::from_millis(20);
// How long to wait, before ever spawning ssh, when checking whether a
// just-reserved local port is already held by something else (see
// open_with). Short because this only needs to catch an already-listening
// process, not a slow one.
const PRE_SPAWN_LIVENESS_PROBE_TIMEOUT: Duration = Duration::from_millis(75);
// How long to give the stderr reader thread to catch up before reading its
// captured text when the child has just been observed to exit.
const STDERR_DRAIN_BUDGET: Duration = Duration::from_millis(50);
const STDERR_DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelSpec {
    name: String,
    ssh_host: String,
    ssh_user: Option<String>,
    ssh_port: Option<u16>,
    remote_host: String,
    remote_port: u16,
}

impl TunnelSpec {
    pub fn from_connection(
        conn: &crate::config::Connection,
    ) -> Result<Option<TunnelSpec>, TunnelError> {
        match &conn.tunnel {
            None => Ok(None),
            Some(tunnel) => {
                TunnelSpec::from_parts(&conn.name, tunnel, &conn.host, conn.port).map(Some)
            }
        }
    }

    pub fn from_parts(
        name: &str,
        tunnel: &crate::config::SshTunnel,
        remote_host: &str,
        remote_port: u16,
    ) -> Result<TunnelSpec, TunnelError> {
        let invalid = |field: &'static str, reason: &'static str| TunnelError::InvalidField {
            name: name.to_string(),
            field,
            reason,
        };

        if tunnel.host.trim().is_empty() {
            return Err(invalid("tunnel.host", "must not be empty"));
        }
        if tunnel.host.starts_with('-') {
            return Err(invalid("tunnel.host", "must not start with '-'"));
        }
        if tunnel.host.contains(':') {
            return Err(invalid("tunnel.host", "must not contain ':'"));
        }
        if tunnel.host.contains('@') {
            return Err(invalid(
                "tunnel.host",
                "must not contain '@' — put the login name in `user` instead",
            ));
        }

        if let Some(user) = &tunnel.user {
            if user.trim().is_empty() {
                return Err(invalid("tunnel.user", "must not be empty"));
            }
            if user.starts_with('-') {
                return Err(invalid("tunnel.user", "must not start with '-'"));
            }
        }

        if tunnel.port == Some(0) {
            return Err(invalid("tunnel.port", "must not be 0"));
        }

        if remote_host.trim().is_empty() {
            return Err(invalid("host", "must not be empty"));
        }
        if remote_host.contains(':') && remote_host.parse::<Ipv6Addr>().is_err() {
            return Err(invalid(
                "host",
                "must not contain ':' unless it is an IPv6 literal",
            ));
        }
        if remote_host.contains('[') || remote_host.contains(']') {
            return Err(invalid("host", "must not contain '[' or ']'"));
        }
        if remote_port == 0 {
            return Err(invalid("port", "must not be 0"));
        }

        Ok(TunnelSpec {
            name: name.to_string(),
            ssh_host: tunnel.host.clone(),
            ssh_user: tunnel.user.clone(),
            ssh_port: tunnel.port,
            remote_host: remote_host.to_string(),
            remote_port,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn ssh_host(&self) -> &str {
        &self.ssh_host
    }

    pub fn ssh_user(&self) -> Option<&str> {
        self.ssh_user.as_deref()
    }

    pub fn ssh_port(&self) -> Option<u16> {
        self.ssh_port
    }

    pub fn remote_host(&self) -> &str {
        &self.remote_host
    }

    pub fn remote_port(&self) -> u16 {
        self.remote_port
    }

    pub fn ssh_argv(&self, local_port: u16) -> Vec<String> {
        command::ssh_argv(self, local_port)
    }
}

#[derive(Debug, Clone)]
pub struct TunnelOptions {
    pub ssh_program: PathBuf,
    pub ready_timeout: Duration,
    pub probe_interval: Duration,
    pub probe_connect_timeout: Duration,
    pub port_attempts: u32,
}

impl Default for TunnelOptions {
    fn default() -> Self {
        Self {
            ssh_program: PathBuf::from("ssh"),
            ready_timeout: DEFAULT_READY_TIMEOUT,
            probe_interval: DEFAULT_PROBE_INTERVAL,
            probe_connect_timeout: DEFAULT_PROBE_CONNECT_TIMEOUT,
            port_attempts: DEFAULT_PORT_ATTEMPTS,
        }
    }
}

#[derive(Debug, Default)]
struct StderrCapture {
    text: String,
    truncated: bool,
}

impl StderrCapture {
    fn push_line(&mut self, line: &str) {
        if self.truncated {
            return;
        }

        let remaining = STDERR_CAPTURE_LIMIT.saturating_sub(self.text.len());
        if remaining == 0 {
            self.truncated = true;
            return;
        }

        if line.len() < remaining {
            self.text.push_str(line);
            self.text.push('\n');
        } else {
            let mut cut = remaining;
            while cut > 0 && !line.is_char_boundary(cut) {
                cut -= 1;
            }
            self.text.push_str(&line[..cut]);
            self.truncated = true;
        }
    }

    // Keeps the *head* of the stream once STDERR_CAPTURE_LIMIT is hit, not the
    // tail — for a long-lived tunnel that's the opposite of the most useful
    // diagnostic (what ssh said right before it died). Acceptable for MVP0
    // since ssh's own bind/auth failures show up immediately at the start of
    // its stderr, but a ring-buffer-of-recent-lines would be a better fix if
    // this ever needs to cover long-running noisy tunnels.
    fn captured_text(&self) -> String {
        if self.truncated {
            format!("{} (truncated)", self.text.trim_end())
        } else {
            self.text.trim_end().to_string()
        }
    }
}

#[derive(Debug)]
pub struct Tunnel {
    child: Child,
    local_port: u16,
    stderr: Arc<Mutex<StderrCapture>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Tunnel {
    pub fn open(spec: &TunnelSpec) -> Result<Tunnel, TunnelError> {
        Tunnel::open_with(spec, &TunnelOptions::default())
    }

    pub fn open_with(spec: &TunnelSpec, options: &TunnelOptions) -> Result<Tunnel, TunnelError> {
        Tunnel::open_with_port_source(spec, options, port::reserve_local_port)
    }

    // Test-only seam: `port::reserve_local_port()` binds to port 0, so a
    // test can't predict (and therefore can't pre-occupy) the port it will
    // return. `open_with` always calls this with `port::reserve_local_port`
    // itself, so production behavior is unchanged; tests that need to force
    // a `PortOccupied` retry pass a closure that hands back an already-bound
    // port on the first call instead. Not `pub`: the tests that need this
    // seam live in this module's own `#[cfg(test)] mod tests` below.
    fn open_with_port_source(
        spec: &TunnelSpec,
        options: &TunnelOptions,
        mut reserve_port: impl FnMut() -> std::io::Result<u16>,
    ) -> Result<Tunnel, TunnelError> {
        let mut last_err = None;

        for _ in 0..options.port_attempts.max(1) {
            let local_port =
                reserve_port().map_err(|source| TunnelError::PortReservation { source })?;

            // reserve_local_port() frees the port immediately after picking it, so
            // this doesn't make the reserve-then-bind race impossible (something
            // can still grab the port between this check and ssh's own bind
            // below) — but it closes the much more common case of a stale/stuck
            // process (e.g. a leftover tunnel from a crashed run) already
            // listening on the port before we ever spawn ssh, which the
            // settle-delay recheck in wait_ready is too short a window to catch.
            if is_port_occupied(local_port, PRE_SPAWN_LIVENESS_PROBE_TIMEOUT) {
                last_err = Some(TunnelError::PortOccupied { local_port });
                continue;
            }

            let mut tunnel = Tunnel::spawn_at(spec, options, local_port)?;

            match tunnel.wait_ready(options) {
                Ok(()) => return Ok(tunnel),
                Err(TunnelError::SshExited { status, stderr })
                    if is_forward_bind_failure(&stderr) =>
                {
                    last_err = Some(TunnelError::SshExited { status, stderr });
                }
                Err(err) => return Err(err),
            }
        }

        Err(last_err.expect("loop runs at least once, so an attempt was made"))
    }

    pub fn spawn_at(
        spec: &TunnelSpec,
        options: &TunnelOptions,
        local_port: u16,
    ) -> Result<Tunnel, TunnelError> {
        let argv = command::ssh_argv(spec, local_port);

        let mut child = std::process::Command::new(&options.ssh_program)
            .args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            // why: the ssh child (and anything its ~/.ssh/config tells it to
            // exec, e.g. a ProxyCommand/Match-exec/LocalCommand) must never
            // see the DB password `secret::resolve` already put in this
            // process's environment for connecting to Postgres -- it has no
            // legitimate use for it, and ratwarren can't vet what a
            // ProxyCommand does with its environment (log it, crash-dump it,
            // etc).
            .env_remove(crate::secret::PASSWORD_ENV_VAR)
            .spawn()
            .map_err(|source| TunnelError::Spawn {
                program: options.ssh_program.clone(),
                source,
            })?;

        let stderr = Arc::new(Mutex::new(StderrCapture::default()));
        let child_stderr = child.stderr.take().expect("stderr was configured as piped");

        let reader_stderr = Arc::clone(&stderr);
        let reader = std::thread::spawn(move || {
            // Raw bytes + from_utf8_lossy instead of BufRead::lines(): ssh's
            // stderr can contain non-UTF-8 bytes (a Latin-1 byte in a MOTD/banner,
            // for example). lines() would yield Err(InvalidData) on that and this
            // thread would exit, closing the pipe's read end and SIGPIPE/EPIPE-ing
            // ssh on its next stderr write — killing an otherwise healthy tunnel.
            let mut reader = std::io::BufReader::new(child_stderr);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&buf);
                        let line = line.trim_end_matches(['\n', '\r']);
                        reader_stderr.lock().unwrap().push_line(line);
                    }
                }
            }
        });

        Ok(Tunnel {
            child,
            local_port,
            stderr,
            reader: Some(reader),
        })
    }

    pub fn wait_ready(&mut self, options: &TunnelOptions) -> Result<(), TunnelError> {
        let deadline = Instant::now() + options.ready_timeout;

        loop {
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|source| TunnelError::Wait { source })?
            {
                self.wait_for_stderr_drain(STDERR_DRAIN_BUDGET);
                return Err(TunnelError::SshExited {
                    status,
                    stderr: self.stderr_tail(),
                });
            }

            if let Ok(stream) =
                TcpStream::connect_timeout(&self.local_addr(), options.probe_connect_timeout)
            {
                let _ = stream.shutdown(std::net::Shutdown::Both);

                // This is best-effort, not a guarantee: it only catches a
                // bind-race loser that exits within READY_SETTLE_DELAY of the
                // probe succeeding. The pre-spawn liveness check in open_with
                // closes the much more common case (a stale/stuck process
                // already holding the port before we ever spawn); a fully
                // authoritative fix would parse ssh's own "Local forwarding
                // listening on ..." stderr line instead of inferring readiness
                // from a third-party TCP connect. That line is only emitted by
                // OpenSSH at `-v` (verbose) level, though, so this would also
                // require adding `-v` to the argv and handling a much noisier
                // stderr stream against STDERR_CAPTURE_LIMIT — a bigger change
                // than "read a different line", and out of scope for MVP0.
                std::thread::sleep(READY_SETTLE_DELAY);
                return match self.child.try_wait() {
                    Ok(None) => Ok(()),
                    Ok(Some(status)) => {
                        self.wait_for_stderr_drain(STDERR_DRAIN_BUDGET);
                        Err(TunnelError::SshExited {
                            status,
                            stderr: self.stderr_tail(),
                        })
                    }
                    Err(source) => Err(TunnelError::Wait { source }),
                };
            }

            if Instant::now() >= deadline {
                // Drain stderr before terminate(): terminate() sets self.reader
                // to None, which would make any subsequent drain wait a
                // permanent no-op and silently drop whatever ssh printed right
                // before the timeout fired.
                self.wait_for_stderr_drain(STDERR_DRAIN_BUDGET);
                self.terminate();
                return Err(TunnelError::ReadyTimeout {
                    local_port: self.local_port,
                    timeout: options.ready_timeout,
                    stderr: self.stderr_tail(),
                });
            }

            std::thread::sleep(options.probe_interval);
        }
    }

    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    pub fn local_addr(&self) -> SocketAddr {
        SocketAddr::from((LOCAL_BIND_ADDR, self.local_port))
    }

    pub fn check_alive(&mut self) -> Result<(), TunnelError> {
        match self
            .child
            .try_wait()
            .map_err(|source| TunnelError::Wait { source })?
        {
            Some(status) => {
                self.wait_for_stderr_drain(STDERR_DRAIN_BUDGET);
                Err(TunnelError::SshExited {
                    status,
                    stderr: self.stderr_tail(),
                })
            }
            None => Ok(()),
        }
    }

    pub fn stderr_tail(&self) -> String {
        self.stderr.lock().unwrap().captured_text()
    }

    pub fn shutdown(mut self) {
        self.terminate();
    }

    // Gives the stderr reader thread a short, bounded chance to catch up before
    // we read its captured text. try_wait() observing child exit races the
    // reader thread's next scheduling slot with no synchronization between
    // them, so a real bind failure can otherwise be reported with empty
    // stderr (see is_forward_bind_failure callers). Not a guarantee — just
    // shrinks the window — because terminate() (see its comment on the
    // ProxyCommand-hang issue) deliberately no longer unconditionally join()s
    // the reader thread.
    fn wait_for_stderr_drain(&self, budget: Duration) {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            match &self.reader {
                Some(handle) if !handle.is_finished() => {
                    std::thread::sleep(STDERR_DRAIN_POLL_INTERVAL);
                }
                _ => return,
            }
        }
    }

    // Deliberately does not join the stderr reader thread: with a
    // ProxyJump/ProxyCommand in ~/.ssh/config, OpenSSH forks a hop process
    // that inherits the stderr pipe, so killing/reaping the direct `ssh`
    // child does not close it. The reader thread would then block until that
    // orphaned hop process eventually exits (e.g. stuck in a TCP connect to
    // an unresponsive bastion) — tens of seconds or more — hanging shutdown,
    // Drop, and the ReadyTimeout path that also calls this. The Arc<Mutex<_>>
    // keeps whatever text was captured before drop readable via
    // stderr_tail(); the thread exits on its own whenever its read end
    // eventually reaches EOF and is not otherwise waited on.
    fn terminate(&mut self) {
        let still_running = matches!(self.child.try_wait(), Ok(None));
        if still_running {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();

        self.reader = None;
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn is_port_occupied(local_port: u16, timeout: Duration) -> bool {
    let addr = SocketAddr::from((LOCAL_BIND_ADDR, local_port));
    match TcpStream::connect_timeout(&addr, timeout) {
        Ok(stream) => {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            true
        }
        Err(_) => false,
    }
}

fn is_forward_bind_failure(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    lower.contains("address already in use") || lower.contains("cannot listen to port")
}

#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    #[error("connection {name:?}: `{field}` {reason}")]
    InvalidField {
        name: String,
        field: &'static str,
        reason: &'static str,
    },

    #[error("failed to reserve a local port on 127.0.0.1")]
    PortReservation {
        #[source]
        source: std::io::Error,
    },

    #[error("reserved local port {local_port} on 127.0.0.1 is already in use by another process")]
    PortOccupied { local_port: u16 },

    #[error("failed to spawn {}: is OpenSSH installed and on PATH?", program.display())]
    Spawn {
        program: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("ssh exited ({status}){}", stderr_suffix(stderr))]
    SshExited { status: ExitStatus, stderr: String },

    #[error(
        "timed out after {:.1}s waiting for 127.0.0.1:{local_port} to accept connections{}",
        timeout.as_secs_f32(),
        stderr_suffix(stderr)
    )]
    ReadyTimeout {
        local_port: u16,
        timeout: Duration,
        stderr: String,
    },

    #[error("failed to wait on the ssh process")]
    Wait {
        #[source]
        source: std::io::Error,
    },
}

fn stderr_suffix(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!(": {trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Connection, SshTunnel};

    fn valid_tunnel() -> SshTunnel {
        SshTunnel {
            host: "bastion.example.com".to_string(),
            user: Some("alice".to_string()),
            port: Some(2222),
        }
    }

    fn assert_invalid_field(result: Result<TunnelSpec, TunnelError>, expected_field: &str) {
        match result.expect_err("expected validation to reject this input") {
            TunnelError::InvalidField { field, .. } => {
                assert_eq!(field, expected_field);
            }
            other => panic!("expected InvalidField, got {other:?}"),
        }
    }

    // --- tunnel.host ---

    #[test]
    fn rejects_empty_tunnel_host() {
        let mut tunnel = valid_tunnel();
        tunnel.host = "".to_string();
        let result = TunnelSpec::from_parts("conn", &tunnel, "dbhost", 5432);
        assert_invalid_field(result, "tunnel.host");
    }

    #[test]
    fn rejects_whitespace_only_tunnel_host() {
        let mut tunnel = valid_tunnel();
        tunnel.host = "   ".to_string();
        let result = TunnelSpec::from_parts("conn", &tunnel, "dbhost", 5432);
        assert_invalid_field(result, "tunnel.host");
    }

    #[test]
    fn rejects_tunnel_host_starting_with_dash() {
        let mut tunnel = valid_tunnel();
        tunnel.host = "-oProxyCommand=touch /tmp/pwned".to_string();
        let result = TunnelSpec::from_parts("conn", &tunnel, "dbhost", 5432);
        assert_invalid_field(result, "tunnel.host");
    }

    #[test]
    fn rejects_tunnel_host_containing_colon() {
        let mut tunnel = valid_tunnel();
        tunnel.host = "bastion:22".to_string();
        let result = TunnelSpec::from_parts("conn", &tunnel, "dbhost", 5432);
        assert_invalid_field(result, "tunnel.host");
    }

    #[test]
    fn rejects_tunnel_host_containing_at() {
        let mut tunnel = valid_tunnel();
        tunnel.host = "user@bastion".to_string();
        let result = TunnelSpec::from_parts("conn", &tunnel, "dbhost", 5432);
        assert_invalid_field(result, "tunnel.host");
    }

    // --- tunnel.user ---

    #[test]
    fn rejects_empty_tunnel_user() {
        let mut tunnel = valid_tunnel();
        tunnel.user = Some("".to_string());
        let result = TunnelSpec::from_parts("conn", &tunnel, "dbhost", 5432);
        assert_invalid_field(result, "tunnel.user");
    }

    #[test]
    fn rejects_whitespace_only_tunnel_user() {
        let mut tunnel = valid_tunnel();
        tunnel.user = Some("   ".to_string());
        let result = TunnelSpec::from_parts("conn", &tunnel, "dbhost", 5432);
        assert_invalid_field(result, "tunnel.user");
    }

    #[test]
    fn rejects_tunnel_user_starting_with_dash() {
        let mut tunnel = valid_tunnel();
        tunnel.user = Some("-oProxyCommand=touch /tmp/pwned".to_string());
        let result = TunnelSpec::from_parts("conn", &tunnel, "dbhost", 5432);
        assert_invalid_field(result, "tunnel.user");
    }

    #[test]
    fn accepts_email_style_tunnel_user() {
        let mut tunnel = valid_tunnel();
        tunnel.user = Some("alice@example.com".to_string());
        let spec = TunnelSpec::from_parts("conn", &tunnel, "dbhost", 5432)
            .expect("email-style login names are a legitimate SSO/LDAP username");
        assert_eq!(spec.ssh_user(), Some("alice@example.com"));
    }

    // --- tunnel.port ---

    #[test]
    fn rejects_zero_tunnel_port() {
        let mut tunnel = valid_tunnel();
        tunnel.port = Some(0);
        let result = TunnelSpec::from_parts("conn", &tunnel, "dbhost", 5432);
        assert_invalid_field(result, "tunnel.port");
    }

    #[test]
    fn accepts_nonzero_tunnel_port() {
        let mut tunnel = valid_tunnel();
        tunnel.port = Some(22);
        let spec = TunnelSpec::from_parts("conn", &tunnel, "dbhost", 5432)
            .expect("nonzero tunnel port is valid");
        assert_eq!(spec.ssh_port(), Some(22));
    }

    #[test]
    fn accepts_none_tunnel_port() {
        let mut tunnel = valid_tunnel();
        tunnel.port = None;
        let spec =
            TunnelSpec::from_parts("conn", &tunnel, "dbhost", 5432).expect("None port is valid");
        assert_eq!(spec.ssh_port(), None);
    }

    // --- remote_host ---

    #[test]
    fn rejects_empty_remote_host() {
        let tunnel = valid_tunnel();
        let result = TunnelSpec::from_parts("conn", &tunnel, "", 5432);
        assert_invalid_field(result, "host");
    }

    #[test]
    fn rejects_remote_host_with_colon_that_is_not_valid_ipv6() {
        let tunnel = valid_tunnel();
        let result = TunnelSpec::from_parts("conn", &tunnel, "db:5432", 5432);
        assert_invalid_field(result, "host");
    }

    #[test]
    fn accepts_remote_host_that_is_a_valid_ipv6_literal() {
        let tunnel = valid_tunnel();
        let spec = TunnelSpec::from_parts("conn", &tunnel, "::1", 5432)
            .expect("bare IPv6 literal is valid");
        assert_eq!(spec.remote_host(), "::1");

        let spec = TunnelSpec::from_parts("conn", &tunnel, "2001:db8::1", 5432)
            .expect("full IPv6 literal is valid");
        assert_eq!(spec.remote_host(), "2001:db8::1");
    }

    #[test]
    fn rejects_remote_host_containing_brackets() {
        let tunnel = valid_tunnel();
        let result = TunnelSpec::from_parts("conn", &tunnel, "[::1]", 5432);
        assert_invalid_field(result, "host");
    }

    // --- remote_port ---

    #[test]
    fn rejects_zero_remote_port() {
        let tunnel = valid_tunnel();
        let result = TunnelSpec::from_parts("conn", &tunnel, "dbhost", 0);
        assert_invalid_field(result, "port");
    }

    #[test]
    fn accepts_nonzero_remote_port() {
        let tunnel = valid_tunnel();
        let spec = TunnelSpec::from_parts("conn", &tunnel, "dbhost", 5432)
            .expect("nonzero remote port is valid");
        assert_eq!(spec.remote_port(), 5432);
    }

    // --- from_connection ---

    fn connection_without_tunnel() -> Connection {
        Connection {
            name: "local".to_string(),
            host: "localhost".to_string(),
            port: 5432,
            database: "app".to_string(),
            user: "app_user".to_string(),
            password: None,
            tunnel: None,
        }
    }

    #[test]
    fn from_connection_returns_none_when_tunnel_is_none() {
        let conn = connection_without_tunnel();
        let result = TunnelSpec::from_connection(&conn).expect("no tunnel is not an error");
        assert!(result.is_none());
    }

    #[test]
    fn from_connection_maps_fields_from_connection_and_tunnel() {
        let mut conn = connection_without_tunnel();
        conn.name = "prod".to_string();
        conn.host = "dbhost".to_string();
        conn.port = 6543;
        conn.tunnel = Some(valid_tunnel());

        let spec = TunnelSpec::from_connection(&conn)
            .expect("valid tunnel should not error")
            .expect("tunnel is Some");

        assert_eq!(spec.name(), "prod");
        assert_eq!(spec.remote_host(), "dbhost");
        assert_eq!(spec.remote_port(), 6543);
        assert_eq!(spec.ssh_host(), "bastion.example.com");
        assert_eq!(spec.ssh_user(), Some("alice"));
        assert_eq!(spec.ssh_port(), Some(2222));
    }

    // --- is_forward_bind_failure ---

    #[test]
    fn detects_address_already_in_use_case_insensitively() {
        assert!(is_forward_bind_failure(
            "bind: Address already in use\r\nchannel_setup_fwd_listener_tcpip"
        ));
        assert!(is_forward_bind_failure("ADDRESS ALREADY IN USE"));
    }

    #[test]
    fn detects_cannot_listen_to_port_case_insensitively() {
        assert!(is_forward_bind_failure(
            "Warning: cannot listen to port: 40000"
        ));
        assert!(is_forward_bind_failure("CANNOT LISTEN TO PORT"));
    }

    #[test]
    fn does_not_match_unrelated_stderr() {
        assert!(!is_forward_bind_failure(
            "Permission denied (publickey).\r\n"
        ));
        assert!(!is_forward_bind_failure(""));
    }

    // --- is_port_occupied ---

    #[test]
    fn is_port_occupied_detects_a_live_listener() {
        let listener = std::net::TcpListener::bind((LOCAL_BIND_ADDR, 0))
            .expect("binding an ephemeral port for setup");
        let port = listener
            .local_addr()
            .expect("bound listener should have a local addr")
            .port();

        assert!(
            is_port_occupied(port, Duration::from_millis(50)),
            "a port with a live listener should be reported as occupied"
        );

        drop(listener);
    }

    #[test]
    fn is_port_occupied_reports_free_port_as_unoccupied() {
        let listener = std::net::TcpListener::bind((LOCAL_BIND_ADDR, 0))
            .expect("binding an ephemeral port for setup");
        let port = listener
            .local_addr()
            .expect("bound listener should have a local addr")
            .port();
        drop(listener);

        assert!(
            !is_port_occupied(port, Duration::from_millis(50)),
            "a port with nothing listening should not be reported as occupied"
        );
    }

    // --- open_with_port_source ---
    //
    // Unix-only: these stand in a fake `ssh` binary (a small shell script) to
    // exercise the real spawn/monitor path without a real sshd, which needs
    // `chmod` and `#!/bin/sh` to work as expected.

    #[cfg(unix)]
    fn write_stub(dir: &std::path::Path, name: &str, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join(name);
        std::fs::write(&path, script).expect("writing stub script should succeed");
        let mut perms = std::fs::metadata(&path)
            .expect("stub script should exist after writing")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod on stub script should succeed");
        path
    }

    #[cfg(unix)]
    fn port_source_test_spec() -> TunnelSpec {
        TunnelSpec::from_parts(
            "conn",
            &SshTunnel {
                host: "bastion.example.com".to_string(),
                user: None,
                port: None,
            },
            "dbhost",
            5432,
        )
        .expect("test spec should be valid")
    }

    #[cfg(unix)]
    fn port_source_test_options(ssh_program: PathBuf) -> TunnelOptions {
        TunnelOptions {
            ssh_program,
            ready_timeout: Duration::from_secs(2),
            probe_interval: Duration::from_millis(25),
            probe_connect_timeout: Duration::from_millis(300),
            port_attempts: 3,
        }
    }

    #[cfg(unix)]
    #[test]
    fn open_with_port_source_exhausts_retries_when_reserved_port_stays_occupied() {
        // Simulates the code-review finding this test closes: something else
        // is already listening on the port `reserve_local_port()` handed
        // back *before* ssh is ever spawned. Since `reserve_local_port()`
        // itself binds to port 0 and can't be steered to a specific port,
        // this uses the `open_with_port_source` test seam to force every
        // attempt to reserve the same still-occupied port.
        let dir = tempfile::tempdir().expect("tempdir creation");
        let counter_path = dir.path().join("invocations");
        let stub = write_stub(
            dir.path(),
            "ssh-sleep.sh",
            &format!(
                "#!/bin/sh\necho x >> {}\nexec sleep 30\n",
                counter_path.display()
            ),
        );

        // Held open for the whole test: this is the "squatter" that occupies
        // the port `reserve_local_port()` would otherwise have handed to ssh.
        let occupied =
            std::net::TcpListener::bind((LOCAL_BIND_ADDR, 0)).expect("binding for setup");
        let occupied_port = occupied
            .local_addr()
            .expect("bound listener should have a local addr")
            .port();

        let spec = port_source_test_spec();
        let options = TunnelOptions {
            port_attempts: 3,
            ..port_source_test_options(stub)
        };

        let result = Tunnel::open_with_port_source(&spec, &options, || Ok(occupied_port));

        match result {
            Err(TunnelError::PortOccupied { local_port }) => {
                assert_eq!(local_port, occupied_port);
            }
            other => panic!("expected PortOccupied, got {other:?}"),
        }

        let invocations = std::fs::read_to_string(&counter_path).unwrap_or_default();
        assert_eq!(
            invocations.lines().count(),
            0,
            "ssh must never be spawned against a port the pre-spawn liveness probe found occupied"
        );

        drop(occupied);
    }

    #[cfg(unix)]
    #[test]
    fn open_with_port_source_retries_past_an_occupied_first_attempt() {
        // Complements the exhaustion test above by proving the loop actually
        // moves on to a fresh `reserve_port()` call (and spawns ssh) once a
        // later attempt gets a genuinely free port, rather than getting
        // stuck.
        let dir = tempfile::tempdir().expect("tempdir creation");
        let counter_path = dir.path().join("invocations");
        let stub = write_stub(
            dir.path(),
            "ssh-sleep.sh",
            &format!(
                "#!/bin/sh\necho x >> {}\nexec sleep 30\n",
                counter_path.display()
            ),
        );

        let occupied =
            std::net::TcpListener::bind((LOCAL_BIND_ADDR, 0)).expect("binding for setup");
        let occupied_port = occupied
            .local_addr()
            .expect("bound listener should have a local addr")
            .port();

        let spec = port_source_test_spec();
        let options = TunnelOptions {
            port_attempts: 2,
            ..port_source_test_options(stub)
        };

        let mut attempt = 0u32;
        let result = Tunnel::open_with_port_source(&spec, &options, move || {
            attempt += 1;
            if attempt == 1 {
                Ok(occupied_port)
            } else {
                let listener = std::net::TcpListener::bind((LOCAL_BIND_ADDR, 0))
                    .expect("binding an ephemeral port for setup");
                let port = listener
                    .local_addr()
                    .expect("bound listener should have a local addr")
                    .port();
                drop(listener);
                Ok(port)
            }
        });

        // The second attempt's port is genuinely free, so the pre-spawn
        // check passes and ssh actually gets spawned; the stub never binds
        // anything, so wait_ready() eventually times out — that's expected
        // and besides the point here, which is just: did it retry past the
        // occupied port and reach a real spawn?
        assert!(
            matches!(result, Err(TunnelError::ReadyTimeout { .. })),
            "expected the loop to move past the occupied first attempt to a real spawn attempt \
             that then times out waiting for the (non-binding) stub, got {result:?}"
        );

        let invocations = std::fs::read_to_string(&counter_path).unwrap_or_default();
        assert_eq!(
            invocations.lines().count(),
            1,
            "ssh should be spawned exactly once, on the attempt after the occupied one"
        );

        drop(occupied);
    }
}
