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

use ratwarren::config::SshTunnel;
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
