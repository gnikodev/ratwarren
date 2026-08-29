// Process-level tests for the SSH tunnel manager (Phase 2 of MVP0). These
// stand in a fake `ssh` binary (a small shell script) so they exercise the
// real `std::process::Command` spawn/monitor/kill path without needing a
// real sshd or network target. Gated `#[cfg(unix)]` throughout because the
// stubs are shell scripts and one test shells out to `kill -0`.
#![cfg(unix)]

use std::fs;
use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ratwarren::app::session::{Session, SessionId, SourceHandle};
use ratwarren::config::{Connection, SshTunnel};
use ratwarren::datasource::{ConnectOptions, DataSourceError, PostgresDataSource};
use ratwarren::secret::PASSWORD_ENV_VAR;
use ratwarren::tunnel::{Tunnel, TunnelError, TunnelOptions, TunnelSpec};

// Every test in this file binds/holds ephemeral 127.0.0.1 ports and/or
// spawns ssh stubs that themselves reserve one via the real
// `reserve_local_port()`. cargo test runs tests within a binary on multiple
// threads by default, and this whole suite shares one OS-wide ephemeral
// port namespace: a test holding a listener open (to simulate an occupied
// port, or just to fake ssh's own bind) can otherwise collide with a
// concurrently-running sibling test's own port reservation. Serializing
// with this lock trades a bit of wall-clock time for determinism, which
// matters more for a suite that's specifically about port-collision
// behavior.
static PORT_TEST_LOCK: Mutex<()> = Mutex::new(());

fn serialize_port_test() -> MutexGuard<'static, ()> {
    PORT_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_stub(dir: &Path, name: &str, script: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, script).expect("writing stub script should succeed");
    let mut perms = fs::metadata(&path)
        .expect("stub script should exist after writing")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).expect("chmod on stub script should succeed");
    path
}

fn test_spec() -> TunnelSpec {
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

fn reserve_port() -> u16 {
    let listener =
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("binding an ephemeral port for setup");
    let port = listener
        .local_addr()
        .expect("bound listener should have a local addr")
        .port();
    drop(listener);
    port
}

// Timeouts here are deliberately more generous than the bare minimum needed
// on a fast, idle machine: process spawn/exec scheduling latency in shared
// CI/sandboxed environments can occasionally exceed a couple hundred
// milliseconds, and this suite intentionally exercises real subprocess
// spawn/kill, not just in-memory logic. Still orders of magnitude below the
// production default (15s) so the suite stays fast.
fn fast_options(ssh_program: PathBuf) -> TunnelOptions {
    TunnelOptions {
        ssh_program,
        ready_timeout: Duration::from_secs(2),
        probe_interval: Duration::from_millis(25),
        probe_connect_timeout: Duration::from_millis(300),
        port_attempts: 3,
        // Production default is 2s; scaled down like the other timeouts here
        // so tests that deliberately never confirm stay fast.
        forward_confirm_grace: Duration::from_millis(200),
    }
}

#[test]
fn spawn_delivers_argv_to_the_child_process_unmangled() {
    let _guard = serialize_port_test();
    // Guards against a regression to shell-joining the argv (e.g. spawning
    // via `sh -c "ssh ..."` instead of Command::args(...)), which would
    // silently reinterpret spaces/metacharacters inside individual argv
    // elements (e.g. the "-L 127.0.0.1:PORT:host:port" forward spec).
    let dir = tempfile::tempdir().expect("tempdir creation");
    let out_path = dir.path().join("argv.out");
    let stub = write_stub(
        dir.path(),
        "ssh-dump-argv.sh",
        &format!(
            "#!/bin/sh\nfor a in \"$@\"; do echo \"$a\" >> {}; done\nexec sleep 30\n",
            out_path.display()
        ),
    );

    let spec = test_spec();
    let options = fast_options(stub);
    let local_port = reserve_port();

    let tunnel =
        Tunnel::spawn_at(&spec, &options, local_port).expect("spawning the stub should succeed");

    let expected = spec.ssh_argv(local_port);

    // Poll for the expected *number of lines*, not mere file existence: the
    // stub writes one argv element per line, so the file exists (with
    // partial contents) after the very first append, and a scheduling stall
    // mid-loop could otherwise cause a read of a truncated dump.
    let deadline = Instant::now() + Duration::from_secs(2);
    let received: Vec<String> = loop {
        let received: Vec<String> = fs::read_to_string(&out_path)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        if received.len() >= expected.len() || Instant::now() >= deadline {
            break received;
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    assert_eq!(
        received, expected,
        "argv received by the spawned process must exactly match ssh_argv(), unmangled"
    );

    tunnel.shutdown();
}

#[test]
fn spawned_ssh_child_never_sees_ratwarren_password() {
    let _guard = serialize_port_test();
    // Every other test in this file is serialized behind the same lock, so
    // this is the only test able to observe/mutate the process-wide
    // environment while it does so -- required since std::env::set_var
    // affects the whole process, not just this thread.
    // Safety: no other thread reads/writes process env concurrently here,
    // guaranteed by `serialize_port_test`'s exclusive lock over this whole
    // file's test suite.
    unsafe {
        std::env::set_var(PASSWORD_ENV_VAR, "super-secret-db-password");
    }

    let dir = tempfile::tempdir().expect("tempdir creation");
    let out_path = dir.path().join("env.out");
    let stub = write_stub(
        dir.path(),
        "ssh-dump-env.sh",
        &format!("#!/bin/sh\nenv > {}\nexec sleep 30\n", out_path.display()),
    );

    let spec = test_spec();
    let options = fast_options(stub);
    let local_port = reserve_port();

    let tunnel =
        Tunnel::spawn_at(&spec, &options, local_port).expect("spawning the stub should succeed");

    let deadline = Instant::now() + Duration::from_secs(2);
    let observed = loop {
        let observed = fs::read_to_string(&out_path).unwrap_or_default();
        if !observed.is_empty() || Instant::now() >= deadline {
            break observed;
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    tunnel.shutdown();
    // Safety: same single-threaded-w.r.t.-env guarantee as the set_var above.
    unsafe {
        std::env::remove_var(PASSWORD_ENV_VAR);
    }

    assert!(
        !observed.contains(PASSWORD_ENV_VAR),
        "the spawned ssh child's environment must not contain {PASSWORD_ENV_VAR}, got:\n{observed}"
    );
}

#[test]
fn open_with_returns_immediately_on_non_bind_failure_with_no_retry() {
    let _guard = serialize_port_test();
    let dir = tempfile::tempdir().expect("tempdir creation");
    let counter_path = dir.path().join("invocations");
    let stub = write_stub(
        dir.path(),
        "ssh-permission-denied.sh",
        &format!(
            "#!/bin/sh\necho x >> {}\necho 'Permission denied (publickey).' 1>&2\nexit 255\n",
            counter_path.display()
        ),
    );

    let spec = test_spec();
    // A high port_attempts makes this a meaningful test: if the code
    // incorrectly retried on non-bind-failure errors, we'd see >1 invocation.
    let options = TunnelOptions {
        port_attempts: 5,
        ..fast_options(stub)
    };

    let result = Tunnel::open_with(&spec, &options);

    assert!(
        matches!(result, Err(TunnelError::SshExited { .. })),
        "expected SshExited, got {result:?}"
    );

    let invocations = fs::read_to_string(&counter_path).unwrap_or_default();
    assert_eq!(
        invocations.lines().count(),
        1,
        "a non-bind-failure ssh exit must not be retried"
    );
}

#[test]
fn open_with_retries_on_bind_failure_until_attempts_exhausted() {
    let _guard = serialize_port_test();
    let dir = tempfile::tempdir().expect("tempdir creation");
    let counter_path = dir.path().join("invocations");
    let stub = write_stub(
        dir.path(),
        "ssh-bind-failure.sh",
        &format!(
            "#!/bin/sh\necho x >> {}\necho 'bind: Address already in use' 1>&2\nexit 255\n",
            counter_path.display()
        ),
    );

    let spec = test_spec();
    let options = TunnelOptions {
        port_attempts: 2,
        ..fast_options(stub)
    };

    let result = Tunnel::open_with(&spec, &options);

    assert!(
        matches!(result, Err(TunnelError::SshExited { .. })),
        "expected SshExited after exhausting retries, got {result:?}"
    );

    let invocations = fs::read_to_string(&counter_path).unwrap_or_default();
    assert_eq!(
        invocations.lines().count(),
        2,
        "a bind-failure ssh exit should be retried exactly port_attempts times"
    );
}

#[test]
fn wait_ready_times_out_when_stub_never_binds() {
    let _guard = serialize_port_test();
    let dir = tempfile::tempdir().expect("tempdir creation");
    let stub = write_stub(dir.path(), "ssh-sleep.sh", "#!/bin/sh\nexec sleep 30\n");

    let spec = test_spec();
    let options = fast_options(stub);
    let local_port = reserve_port();

    let mut tunnel =
        Tunnel::spawn_at(&spec, &options, local_port).expect("spawning the stub should succeed");

    let start = Instant::now();
    let result = tunnel.wait_ready(&options);
    let elapsed = start.elapsed();

    assert!(
        matches!(result, Err(TunnelError::ReadyTimeout { .. })),
        "expected ReadyTimeout, got {result:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "wait_ready should time out in bounded time, took {elapsed:?}"
    );
}

#[test]
fn wait_ready_succeeds_via_raw_tcp_probe_independent_of_stub() {
    let _guard = serialize_port_test();
    let dir = tempfile::tempdir().expect("tempdir creation");
    let stub = write_stub(dir.path(), "ssh-sleep.sh", "#!/bin/sh\nexec sleep 30\n");

    let spec = test_spec();
    let options = fast_options(stub);

    // Bind the local_port ourselves *before* spawning the stub, which never
    // itself binds anything. If wait_ready() still succeeds, it proves the
    // TCP-connect readiness probe is independent of what the ssh process does.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("binding for setup");
    let local_port = listener
        .local_addr()
        .expect("bound listener should have a local addr")
        .port();

    let mut tunnel =
        Tunnel::spawn_at(&spec, &options, local_port).expect("spawning the stub should succeed");

    let result = tunnel.wait_ready(&options);
    assert!(result.is_ok(), "expected Ok(()), got {result:?}");

    drop(listener);
    tunnel.shutdown();
}

#[test]
fn check_alive_reports_alive_then_exited() {
    let _guard = serialize_port_test();
    let dir = tempfile::tempdir().expect("tempdir creation");
    // `exec` avoids a fork for `sleep`, so killing/waiting on the tracked
    // child pid directly reflects the sleep's own lifetime (see the
    // sleep-30 stubs elsewhere in this file for what goes wrong otherwise).
    // A whole-second duration is used because fractional seconds are a
    // GNU/BSD `sleep` extension, not POSIX; the poll loop below (rather than
    // a fixed sleep-then-assert) is what actually waits for the exit, so the
    // 1s duration only needs to be "not instant", not tuned to any margin.
    let stub = write_stub(
        dir.path(),
        "ssh-short-lived.sh",
        "#!/bin/sh\nexec sleep 1\n",
    );

    let spec = test_spec();
    let options = fast_options(stub);
    let local_port = reserve_port();

    let mut tunnel =
        Tunnel::spawn_at(&spec, &options, local_port).expect("spawning the stub should succeed");

    assert!(
        tunnel.check_alive().is_ok(),
        "should report alive immediately after spawn"
    );

    // Poll instead of a single fixed sleep so this isn't sensitive to
    // scheduling jitter from other tests/processes running concurrently.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match tunnel.check_alive() {
            Err(TunnelError::SshExited { .. }) => break,
            Ok(()) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            other => panic!(
                "expected check_alive to eventually report SshExited within 5s, last saw {other:?}"
            ),
        }
    }
}

#[test]
fn dropping_a_tunnel_kills_the_ssh_child_process() {
    let _guard = serialize_port_test();
    let dir = tempfile::tempdir().expect("tempdir creation");
    let pid_path = dir.path().join("pid");
    let stub = write_stub(
        dir.path(),
        "ssh-sleep-pid.sh",
        &format!(
            "#!/bin/sh\necho $$ > {}\nexec sleep 30\n",
            pid_path.display()
        ),
    );

    let spec = test_spec();
    let options = fast_options(stub);
    let local_port = reserve_port();

    {
        let _tunnel = Tunnel::spawn_at(&spec, &options, local_port)
            .expect("spawning the stub should succeed");

        // Poll for non-empty *contents*, not mere existence: `echo $$ > file`
        // makes the file exist (0 bytes) before the write itself completes,
        // so an existence-only check can observe an empty file and race the
        // drop below against the stub still writing to it.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if fs::read_to_string(&pid_path)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
            {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // _tunnel dropped at end of this block.
    }

    let pid = fs::read_to_string(&pid_path)
        .expect("stub should have written its pid before the tunnel was dropped")
        .trim()
        .to_string();
    assert!(!pid.is_empty(), "pid file should contain a pid");

    let mut still_alive = true;
    for _ in 0..40 {
        let status = std::process::Command::new("kill")
            .arg("-0")
            .arg(&pid)
            .stderr(std::process::Stdio::null())
            .status()
            .expect("running `kill -0` should succeed");
        if !status.success() {
            still_alive = false;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    assert!(
        !still_alive,
        "the ssh child process (pid {pid}) should be killed when Tunnel is dropped"
    );
}

// --- T1: OPEN_LOCK serializes concurrent tunnel opens (docs/MVP1-PHASE2-DESIGN.md §2, §9 item 8) ---

fn test_connection(name: &str) -> Connection {
    Connection {
        name: name.to_string(),
        group: None,
        host: "dbhost".to_string(),
        port: 5432,
        database: "postgres".to_string(),
        user: "postgres".to_string(),
        password: None,
        tunnel: Some(SshTunnel {
            host: "bastion.example.com".to_string(),
            user: None,
            port: None,
        }),
    }
}

fn connect_options(ssh_program: PathBuf) -> ConnectOptions {
    ConnectOptions {
        // Nothing is really listening on the tunnel's local port, so the
        // Postgres dial after the tunnel opens is expected to fail fast
        // (connection refused) -- this only needs to be short enough not to
        // slow the test down if that assumption is ever wrong.
        connect_timeout: Duration::from_millis(500),
        tunnel: TunnelOptions {
            ready_timeout: Duration::from_secs(5),
            ..fast_options(ssh_program)
        },
        ..ConnectOptions::default()
    }
}

// The stub records its own pid's start/end into a shared log file: "start"
// as soon as it's spawned, "end" only once it has finished everything that
// must happen before `wait_ready` can possibly return -- then holds the
// tunnel open with `exec sleep 30`. The 0.2s delay between the two makes the
// test meaningful: without it, two racing (unserialized) spawns could still
// happen to log start/end without overlapping purely by scheduling luck.
//
// Critically, "end" is logged BEFORE the readiness confirmation line is
// echoed to stderr, not after: `OPEN_LOCK` is released the instant the
// parent's stderr-reader thread *observes* that line (setting
// `forward_confirmed`), which races independently of when this script's own
// "end" write actually lands on disk -- logging "end" first, synchronously,
// guarantees it is visible before the confirmation line can possibly unblock
// a queued second `connect_with`.
fn open_lock_stub_script(log_path: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         echo \"start $$\" >> {log}\n\
         sleep 0.2\n\
         prev=\"\"\n\
         lport=\"\"\n\
         for a in \"$@\"; do\n\
         \tif [ \"$prev\" = \"-L\" ]; then\n\
         \t\tlport=$(printf '%s' \"$a\" | cut -d: -f2)\n\
         \tfi\n\
         \tprev=\"$a\"\n\
         done\n\
         echo \"end $$\" >> {log}\n\
         echo \"debug1: Local forwarding listening on 127.0.0.1 port ${{lport}}.\" 1>&2\n\
         exec sleep 30\n",
        log = log_path.display()
    )
}

// Plain `#[test]` with its own hand-built runtime, not `#[tokio::test]`:
// `serialize_port_test()`'s guard is a `std::sync::MutexGuard` and needs to
// stay held for the whole test (including the concurrent opens below), which
// would otherwise mean holding it across an `.await` inside an async test fn
// -- `rt.block_on(..)` is an ordinary synchronous call from this sync
// function's point of view, so the guard being live across it is fine.
#[test]
fn open_lock_serializes_concurrent_tunnel_opens_so_their_windows_never_overlap() {
    let _guard = serialize_port_test();
    let dir = tempfile::tempdir().expect("tempdir creation");
    let log_path = dir.path().join("open-lock.log");
    let stub = write_stub(
        dir.path(),
        "ssh-open-lock.sh",
        &open_lock_stub_script(&log_path),
    );

    let conn1 = test_connection("open-lock-1");
    let conn2 = test_connection("open-lock-2");
    let opts1 = connect_options(stub.clone());
    let opts2 = connect_options(stub);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("building a test-local tokio runtime should succeed");
    let (r1, r2) = rt.block_on(async {
        tokio::join!(
            PostgresDataSource::connect_with(&conn1, None, &opts1),
            PostgresDataSource::connect_with(&conn2, None, &opts2),
        )
    });

    // Both calls are expected to fail once they reach the actual Postgres
    // dial (nothing is really listening on the tunnel's local port) -- only
    // the tunnel-open timing is under test here. `PostgresDataSource` has no
    // `Debug` impl, so match on the error directly rather than formatting
    // the whole `Result`.
    match &r1 {
        Err(DataSourceError::Connect { .. }) => {}
        Err(other) => {
            panic!("expected a Connect error for conn1, got a different error: {other:?}")
        }
        Ok(_) => panic!("expected a Connect error for conn1, got Ok(_)"),
    }
    match &r2 {
        Err(DataSourceError::Connect { .. }) => {}
        Err(other) => {
            panic!("expected a Connect error for conn2, got a different error: {other:?}")
        }
        Ok(_) => panic!("expected a Connect error for conn2, got Ok(_)"),
    }

    let log = fs::read_to_string(&log_path).unwrap_or_default();
    let mut open_pid: Option<&str> = None;
    for line in log.lines() {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some("start"), Some(pid)) => {
                assert!(
                    open_pid.is_none(),
                    "OPEN_LOCK should prevent a second ssh spawn from starting while another is \
                     still mid-open, but saw \"start {pid}\" while pid {open_pid:?} was still \
                     open -- full log:\n{log}"
                );
                open_pid = Some(pid);
            }
            (Some("end"), Some(pid)) => {
                assert_eq!(
                    open_pid,
                    Some(pid),
                    "\"end {pid}\" did not match the currently-open pid {open_pid:?} -- full \
                     log:\n{log}"
                );
                open_pid = None;
            }
            _ => panic!("unexpected log line {line:?} -- full log:\n{log}"),
        }
    }
    assert_eq!(
        log.lines().count(),
        4,
        "expected exactly one start/end pair per connect_with call, got:\n{log}"
    );
    assert!(
        open_pid.is_none(),
        "an interval was left open at EOF:\n{log}"
    );
}

// --- T2: forward-confirmation state machine (docs/MVP1-PHASE2-DESIGN.md §2, §9 item 9) ---

fn t2_test_options(ssh_program: PathBuf, forward_confirm_grace: Duration) -> TunnelOptions {
    TunnelOptions {
        forward_confirm_grace,
        ..fast_options(ssh_program)
    }
}

#[test]
fn forward_confirmed_is_true_when_the_stub_prints_the_exact_confirmation_line() {
    let _guard = serialize_port_test();
    let dir = tempfile::tempdir().expect("tempdir creation");
    let stub = write_stub(
        dir.path(),
        "ssh-confirm.sh",
        "#!/bin/sh\n\
         prev=\"\"\n\
         lport=\"\"\n\
         for a in \"$@\"; do\n\
         \tif [ \"$prev\" = \"-L\" ]; then\n\
         \t\tlport=$(printf '%s' \"$a\" | cut -d: -f2)\n\
         \tfi\n\
         \tprev=\"$a\"\n\
         done\n\
         echo \"debug1: Local forwarding listening on 127.0.0.1 port ${lport}.\" 1>&2\n\
         exec sleep 30\n",
    );

    let spec = test_spec();
    let options = t2_test_options(stub, Duration::from_secs(2));
    let local_port = reserve_port();

    let mut tunnel =
        Tunnel::spawn_at(&spec, &options, local_port).expect("spawning the stub should succeed");

    let result = tunnel.wait_ready(&options);
    assert!(result.is_ok(), "expected Ok(()), got {result:?}");
    assert!(
        tunnel.forward_confirmed(),
        "wait_ready succeeded via the confirmation line, so forward_confirmed() must be true"
    );

    tunnel.shutdown();
}

// Regression coverage for the lock-free `PostgresDataSource` read path
// (docs/MVP1-PHASE2-DESIGN.md item 4 of the Phase 2 fix round):
// `PostgresDataSource` stores `Tunnel::forward_confirmed_handle()`'s result
// directly, rather than re-deriving it later, specifically so it observes
// the SAME atomic the stderr-reader thread flips -- not a snapshot taken at
// construction time. This proves that property at the `Tunnel` level (the
// layer `PostgresDataSource` delegates to): grab the handle immediately
// after spawn, before the stub has had any chance to confirm, then poll the
// handle -- never `tunnel.forward_confirmed()` -- until it flips.
#[test]
fn forward_confirmed_handle_observes_the_readers_later_flip_not_a_stale_snapshot() {
    let _guard = serialize_port_test();
    let dir = tempfile::tempdir().expect("tempdir creation");
    let stub = write_stub(
        dir.path(),
        "ssh-delayed-confirm.sh",
        "#!/bin/sh\n\
         prev=\"\"\n\
         lport=\"\"\n\
         for a in \"$@\"; do\n\
         \tif [ \"$prev\" = \"-L\" ]; then\n\
         \t\tlport=$(printf '%s' \"$a\" | cut -d: -f2)\n\
         \tfi\n\
         \tprev=\"$a\"\n\
         done\n\
         sleep 0.3\n\
         echo \"debug1: Local forwarding listening on 127.0.0.1 port ${lport}.\" 1>&2\n\
         exec sleep 30\n",
    );

    let spec = test_spec();
    let options = t2_test_options(stub, Duration::from_secs(2));
    let local_port = reserve_port();

    let tunnel =
        Tunnel::spawn_at(&spec, &options, local_port).expect("spawning the stub should succeed");

    // Taken right after spawn, well before the stub's 0.3s delay elapses --
    // this is the exact moment `PostgresDataSource::connect_with` takes its
    // own copy.
    let handle = tunnel.forward_confirmed_handle();
    assert!(
        !handle.load(std::sync::atomic::Ordering::Acquire),
        "the stub sleeps 0.3s before confirming, so the handle must read false immediately \
         after spawn"
    );

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if handle.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "forward_confirmed_handle()'s Arc never observed the reader thread's flip within 2s \
             -- if this fires, PostgresDataSource::tunnel_forward_confirmed() would be reading a \
             decoupled snapshot instead of live state"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // Cross-check against the tunnel's own accessor: confirms the flip
    // observed through the handle is the real one, not a coincidental
    // separate `true`.
    assert!(
        tunnel.forward_confirmed(),
        "tunnel.forward_confirmed() should agree with the handle once it has flipped"
    );

    tunnel.shutdown();
}

#[test]
fn open_succeeds_after_forward_confirm_grace_when_the_stub_never_confirms() {
    let _guard = serialize_port_test();
    let dir = tempfile::tempdir().expect("tempdir creation");
    // Never prints anything -- binds nothing itself either; readiness comes
    // purely from the raw TCP probe against a listener we bind ourselves
    // below (same technique as `wait_ready_succeeds_via_raw_tcp_probe_independent_of_stub`).
    let stub = write_stub(dir.path(), "ssh-sleep.sh", "#!/bin/sh\nexec sleep 30\n");

    let spec = test_spec();
    let grace = Duration::from_millis(150);
    let options = t2_test_options(stub, grace);

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("binding for setup");
    let local_port = listener
        .local_addr()
        .expect("bound listener should have a local addr")
        .port();

    let mut tunnel =
        Tunnel::spawn_at(&spec, &options, local_port).expect("spawning the stub should succeed");

    let start = Instant::now();
    let result = tunnel.wait_ready(&options);
    let elapsed = start.elapsed();

    assert!(result.is_ok(), "expected Ok(()), got {result:?}");
    assert!(
        !tunnel.forward_confirmed(),
        "the stub never printed a confirmation line, so forward_confirmed() must stay false \
         even though open succeeded"
    );
    assert!(
        elapsed >= grace,
        "open must wait out the full forward_confirm_grace before accepting an unconfirmed \
         probe success, took {elapsed:?}, grace was {grace:?}"
    );

    drop(listener);
    tunnel.shutdown();
}

#[test]
fn bind_failure_is_still_classified_with_debug_prefixed_noise_filtered_from_the_capture() {
    let _guard = serialize_port_test();
    let dir = tempfile::tempdir().expect("tempdir creation");
    // A realistic `-v` transcript: several debug-prefixed lines (which T2
    // must filter out of the captured/reported stderr) surrounding the
    // undebugged bind-failure lines `is_forward_bind_failure` depends on.
    let stub = write_stub(
        dir.path(),
        "ssh-bind-failure-verbose.sh",
        "#!/bin/sh\n\
         echo 'debug1: Reading configuration data /etc/ssh/ssh_config' 1>&2\n\
         echo 'debug1: Connecting to bastion.example.com port 22.' 1>&2\n\
         echo 'debug1: Authentication succeeded (publickey).' 1>&2\n\
         echo 'bind [127.0.0.1]:REDACTED: Address already in use' 1>&2\n\
         echo 'channel_setup_fwd_listener_tcpip: cannot listen to port: REDACTED' 1>&2\n\
         echo 'Could not request local forwarding.' 1>&2\n\
         exit 255\n",
    );

    let spec = test_spec();
    let options = TunnelOptions {
        port_attempts: 1,
        ..t2_test_options(stub, Duration::from_millis(100))
    };

    let result = Tunnel::open_with(&spec, &options);
    let Err(TunnelError::SshExited { stderr, .. }) = result else {
        panic!("expected TunnelError::SshExited, got {result:?}");
    };

    assert!(
        !stderr.contains("debug1"),
        "debug-prefixed lines must be filtered out of the captured stderr, got:\n{stderr}"
    );
    assert!(
        stderr.contains("Address already in use"),
        "the undebugged bind-failure line must still be captured, got:\n{stderr}"
    );
    assert!(
        stderr.contains("Could not request local forwarding"),
        "the undebugged 'could not request' line must still be captured, got:\n{stderr}"
    );
}

// --- T2 addendum: Session::tunnel_warning() end-to-end (docs/MVP1-PHASE2-DESIGN.md §2 T2 item 5) ---
//
// `tunnel_warning()`'s `Some(true)`/`Some(false)` branches only exist once a
// session is genuinely `Ready`, which requires `PostgresDataSource::connect_with`
// to complete a real ssh tunnel open *and* a real Postgres wire-protocol
// handshake -- `SessionState::TestReady` deliberately bypasses both (see its
// doc comment on that variant), so it can't stand in here, and there is no
// fake/mock `DataSource` to substitute. These two tests therefore layer a
// real Postgres instance (the same opt-in `RATWARREN_TEST_PG=1` gate
// tests/postgres.rs uses) on top of this file's stub-`ssh` infrastructure:
// since the stub's own `-L` target is never real (see `test_connection`'s
// "dbhost" placeholder), the stub relays raw bytes from the tunnel's local
// port to the real Postgres instance via `nc -k` (BSD/GNU nc with `-k` to
// survive `wait_ready`'s own raw-TCP-probe connects, which each open and
// immediately close a connection through the same listener), so
// `cfg.connect(...)` inside `connect_with` really does complete a live
// handshake. Skipped like tests/postgres.rs when `RATWARREN_TEST_PG` isn't
// set to `1`; also requires `nc` on PATH.

fn pg_relay_test_enabled() -> bool {
    std::env::var("RATWARREN_TEST_PG").as_deref() == Ok("1")
}

fn relay_env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn real_pg_addr() -> (String, u16) {
    let host = relay_env_or("RATWARREN_TEST_PG_HOST", "127.0.0.1");
    let port = relay_env_or("RATWARREN_TEST_PG_PORT", "5432")
        .parse()
        .expect("RATWARREN_TEST_PG_PORT must be a valid u16");
    (host, port)
}

fn real_pg_password() -> String {
    relay_env_or("RATWARREN_TEST_PG_PASSWORD", "postgres")
}

fn real_pg_connection(name: &str) -> Connection {
    Connection {
        name: name.to_string(),
        group: None,
        // Never dialed directly -- when `conn.tunnel` is `Some`, `connect_with`
        // always dials "127.0.0.1":<tunnel local port> instead. Kept as an
        // obviously-fake placeholder, same as `test_connection` above.
        host: "dbhost".to_string(),
        port: 5432,
        database: relay_env_or("RATWARREN_TEST_PG_DB", "postgres"),
        user: relay_env_or("RATWARREN_TEST_PG_USER", "postgres"),
        password: None,
        tunnel: Some(SshTunnel {
            host: "bastion.example.com".to_string(),
            user: None,
            port: None,
        }),
    }
}

fn real_connect_options(ssh_program: PathBuf, forward_confirm_grace: Duration) -> ConnectOptions {
    ConnectOptions {
        connect_timeout: Duration::from_secs(5),
        tunnel: TunnelOptions {
            forward_confirm_grace,
            ready_timeout: Duration::from_secs(5),
            ..fast_options(ssh_program)
        },
        ..ConnectOptions::default()
    }
}

// `confirm`: whether the stub also prints the real confirmation stderr line
// (`forward_confirmed() == true`) or never does, relying purely on
// `wait_ready`'s raw-TCP-probe-plus-grace fallback (`forward_confirmed() ==
// false`). The relay is started, in both cases, before that decision is
// made, so it's always up in time for the real Postgres dial that follows
// `wait_ready`'s return either way.
//
// A single backgrounded `nc -k -l ... | nc upstream ...` pipeline (unchanged
// from before) implements the relay; both `nc` pids are then recorded to
// `pid_log` via `pgrep -P $$` (both are direct children of this script, so
// this reliably finds both -- unlike `$!`, which after `cmd1 | cmd2 &` only
// gives cmd2's pid). This is NOT cleaned up via a `trap ... EXIT` in this
// script: `Tunnel::terminate()` always reaps the tracked `ssh` stub pid with
// `Child::kill()`, which is SIGKILL on Unix -- uncatchable, so no trap or
// other in-process handler in a process being killed that way ever runs.
// The `pid_log` this writes is instead read and explicitly killed by the
// *test*, from outside the process being torn down, once it's done with the
// relay -- see `kill_relay_pids` below.
fn relay_stub_script(
    upstream_host: &str,
    upstream_port: u16,
    confirm: bool,
    pid_log: &Path,
) -> String {
    let confirm_line = if confirm {
        "echo \"debug1: Local forwarding listening on 127.0.0.1 port ${lport}.\" 1>&2\n"
    } else {
        ""
    };
    format!(
        "#!/bin/sh\n\
         prev=\"\"\n\
         lport=\"\"\n\
         for a in \"$@\"; do\n\
         \tif [ \"$prev\" = \"-L\" ]; then\n\
         \t\tlport=$(printf '%s' \"$a\" | cut -d: -f2)\n\
         \tfi\n\
         \tprev=\"$a\"\n\
         done\n\
         fifo=$(mktemp -u)\n\
         mkfifo \"$fifo\"\n\
         nc -k -l 127.0.0.1 \"$lport\" < \"$fifo\" | nc {upstream_host} {upstream_port} > \"$fifo\" &\n\
         pgrep -P $$ >> {pid_log}\n\
         sleep 0.15\n\
         {confirm_line}\
         exec sleep 30\n",
        pid_log = pid_log.display(),
    )
}

// Kills every pid `relay_stub_script` recorded in `pid_log`, ignoring
// entries for pids that are already gone. Must be called by every test that
// uses `relay_stub_script`, after it's done with the relay -- see that
// function's doc comment for why this can't be done from inside the stub
// script itself.
fn kill_relay_pids(pid_log: &Path) {
    let contents = fs::read_to_string(pid_log).unwrap_or_default();
    for pid in contents.split_whitespace() {
        let _ = std::process::Command::new("kill")
            .arg("-KILL")
            .arg(pid)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

// Calls `kill_relay_pids` on drop, so a panicking assertion between a relay
// test's stub spawn and its normal cleanup call still reaps the backgrounded
// `nc` pair instead of leaking it -- `cargo test` unwinds panics by default,
// so a guard constructed before the assertions still drops on that path.
struct RelayGuard(PathBuf);

impl Drop for RelayGuard {
    fn drop(&mut self) {
        kill_relay_pids(&self.0);
    }
}

#[test]
fn tunnel_warning_is_none_once_the_forward_is_confirmed_end_to_end_through_a_real_postgres() {
    if !pg_relay_test_enabled() {
        eprintln!(
            "skipping: set RATWARREN_TEST_PG=1 (with a real Postgres reachable per the \
             RATWARREN_TEST_PG_* env vars, and `nc` on PATH) to run this test -- see the doc \
             comment at the top of tests/postgres.rs for a throwaway docker one-liner."
        );
        return;
    }
    let _guard = serialize_port_test();
    let dir = tempfile::tempdir().expect("tempdir creation");
    let (host, port) = real_pg_addr();
    let pid_log = dir.path().join("nc-pids");
    let _relay_guard = RelayGuard(pid_log.clone());
    let stub = write_stub(
        dir.path(),
        "ssh-relay-confirmed.sh",
        &relay_stub_script(&host, port, true, &pid_log),
    );

    let conn = real_pg_connection("relay-confirmed");
    let options = real_connect_options(stub, Duration::from_secs(2));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("building a test-local tokio runtime should succeed");

    // `SourceHandle::attach` (`tokio::spawn`s the worker/canceller tasks)
    // must run inside the runtime's context, not just via `block_on` on a
    // plain future -- so the whole chain from connect through
    // `tunnel_warning()` runs inside one `block_on` async block rather than
    // being split across several top-level `rt.block_on(...)` calls.
    let (local_port, confirmed, warning) = rt.block_on(async {
        let source = PostgresDataSource::connect_with(&conn, Some(&real_pg_password()), &options)
            .await
            .expect(
                "connect_with should complete a real handshake through the nc relay -- if this \
                 fails, check that `nc -k` is available and that RATWARREN_TEST_PG_* points at a \
                 reachable Postgres",
            );

        let (responses_tx, _responses_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = SourceHandle::attach(source, SessionId(0), responses_tx);
        let local_port = handle
            .tunnel_local_port()
            .expect("test setup: a tunnel must be attached");
        let confirmed = handle.tunnel_forward_confirmed();

        let mut session = Session::new(SessionId(0), "relay-confirmed".to_string(), None);
        session.on_connected(handle);
        (local_port, confirmed, session.tunnel_warning())
    });

    assert_eq!(
        confirmed,
        Some(true),
        "test setup: the stub printed the confirmation line, so the tunnel must report confirmed"
    );
    assert_eq!(
        warning, None,
        "a Ready session with a confirmed tunnel must not show the T2 warning, port was \
         {local_port}"
    );
}

#[test]
fn tunnel_warning_reports_the_exact_t2_message_when_the_forward_is_unconfirmed_end_to_end() {
    if !pg_relay_test_enabled() {
        eprintln!(
            "skipping: set RATWARREN_TEST_PG=1 (with a real Postgres reachable per the \
             RATWARREN_TEST_PG_* env vars, and `nc` on PATH) to run this test -- see the doc \
             comment at the top of tests/postgres.rs for a throwaway docker one-liner."
        );
        return;
    }
    let _guard = serialize_port_test();
    let dir = tempfile::tempdir().expect("tempdir creation");
    let (host, port) = real_pg_addr();
    let pid_log = dir.path().join("nc-pids");
    let _relay_guard = RelayGuard(pid_log.clone());
    let stub = write_stub(
        dir.path(),
        "ssh-relay-unconfirmed.sh",
        &relay_stub_script(&host, port, false, &pid_log),
    );

    let conn = real_pg_connection("relay-unconfirmed");
    // Short grace so `wait_ready`'s raw-TCP-probe fallback (the only path
    // available, since this stub never prints a confirmation line) resolves
    // quickly.
    let options = real_connect_options(stub, Duration::from_millis(150));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("building a test-local tokio runtime should succeed");

    let (local_port, confirmed, warning) = rt.block_on(async {
        let source = PostgresDataSource::connect_with(&conn, Some(&real_pg_password()), &options)
            .await
            .expect(
                "connect_with should complete a real handshake through the nc relay -- if this \
                 fails, check that `nc -k` is available and that RATWARREN_TEST_PG_* points at a \
                 reachable Postgres",
            );

        let (responses_tx, _responses_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = SourceHandle::attach(source, SessionId(0), responses_tx);
        let local_port = handle
            .tunnel_local_port()
            .expect("test setup: a tunnel must be attached");
        let confirmed = handle.tunnel_forward_confirmed();

        let mut session = Session::new(SessionId(0), "relay-unconfirmed".to_string(), None);
        session.on_connected(handle);
        (local_port, confirmed, session.tunnel_warning())
    });

    assert_eq!(
        confirmed,
        Some(false),
        "test setup: the stub never printed a confirmation line, so the tunnel must report \
         unconfirmed even though the connect itself succeeded"
    );
    let expected = format!(
        "tunnel readiness unconfirmed — could not verify this ssh owns port {local_port}; it \
         may belong to another process"
    );
    assert_eq!(
        warning,
        Some(expected),
        "a Ready session with an unconfirmed tunnel must show the exact T2 warning text"
    );
}
