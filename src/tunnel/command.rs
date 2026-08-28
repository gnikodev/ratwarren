use std::net::Ipv6Addr;

use super::{LOCAL_BIND_ADDR, TunnelSpec};

pub(crate) fn ssh_argv(spec: &TunnelSpec, local_port: u16) -> Vec<String> {
    // `-v`, the two `-o` overrides, and `-N` are the only flags this tool
    // ever forces onto the argv — everything else (ciphers, keepalives,
    // ProxyJump/ProxyCommand chains, identity files, ...) is left to the
    // user's ~/.ssh/config, per the project's ssh-tunnel non-negotiable.
    // ExitOnForwardFailure=yes makes ssh exit immediately (instead of running
    // degraded) if the -L forward can't be established, which is what lets
    // the bind-failure retry loop above detect a lost port race quickly.
    // BatchMode=yes disables passphrase and host-key prompts: this is a
    // fullscreen TUI with stdin set to /dev/null and no way to relay an
    // interactive prompt to the user. This is a deliberate MVP0 tradeoff, not
    // an oversight — a user whose key has a passphrase and isn't loaded in
    // ssh-agent will see a fast "Permission denied" failure instead of a hang
    // waiting on a prompt nobody can answer. `-v` (verbose) is what makes
    // OpenSSH emit `debug1: Local forwarding listening on ... port N.` once
    // its `-L` listener is actually bound — the only authoritative,
    // in-process signal that this ssh (and not some other process reusing
    // the same just-freed ephemeral port) owns the forward; see
    // `Tunnel::forward_confirmed`.
    let mut argv = vec![
        "-v".to_string(),
        "-N".to_string(),
        "-o".to_string(),
        "ExitOnForwardFailure=yes".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-L".to_string(),
        forward_spec(spec, local_port),
    ];

    if let Some(port) = spec.ssh_port() {
        argv.push("-p".to_string());
        argv.push(port.to_string());
    }

    argv.push(destination(spec));

    argv
}

pub(crate) fn forward_spec(spec: &TunnelSpec, local_port: u16) -> String {
    format!(
        "{LOCAL_BIND_ADDR}:{local_port}:{}:{}",
        forward_host(spec.remote_host()),
        spec.remote_port()
    )
}

pub(crate) fn destination(spec: &TunnelSpec) -> String {
    match spec.ssh_user() {
        Some(user) => format!("{user}@{}", spec.ssh_host()),
        None => spec.ssh_host().to_string(),
    }
}

// ssh's `-L` forward spec uses ':' as its own field separator, so an IPv6
// literal host segment must be bracketed to disambiguate it from that syntax.
fn forward_host(remote_host: &str) -> String {
    if remote_host.parse::<Ipv6Addr>().is_ok() {
        format!("[{remote_host}]")
    } else {
        remote_host.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SshTunnel;

    fn spec(user: Option<&str>, ssh_port: Option<u16>, remote_host: &str) -> TunnelSpec {
        TunnelSpec::from_parts(
            "conn",
            &SshTunnel {
                host: "bastion.example.com".to_string(),
                user: user.map(str::to_string),
                port: ssh_port,
            },
            remote_host,
            5432,
        )
        .expect("spec should be valid")
    }

    #[test]
    fn full_spec_produces_expected_argv() {
        let spec = spec(Some("alice"), Some(2222), "dbhost");

        assert_eq!(
            spec.ssh_argv(40000),
            vec![
                "-v".to_string(),
                "-N".to_string(),
                "-o".to_string(),
                "ExitOnForwardFailure=yes".to_string(),
                "-o".to_string(),
                "BatchMode=yes".to_string(),
                "-L".to_string(),
                "127.0.0.1:40000:dbhost:5432".to_string(),
                "-p".to_string(),
                "2222".to_string(),
                "alice@bastion.example.com".to_string(),
            ]
        );
    }

    #[test]
    fn minimal_spec_omits_user_prefix_and_port_flag() {
        let spec = spec(None, None, "dbhost");

        assert_eq!(
            spec.ssh_argv(40000),
            vec![
                "-v".to_string(),
                "-N".to_string(),
                "-o".to_string(),
                "ExitOnForwardFailure=yes".to_string(),
                "-o".to_string(),
                "BatchMode=yes".to_string(),
                "-L".to_string(),
                "127.0.0.1:40000:dbhost:5432".to_string(),
                "bastion.example.com".to_string(),
            ]
        );
    }

    #[test]
    fn only_ssh_user_set_adds_user_prefix_but_no_port_flag() {
        let spec = spec(Some("alice"), None, "dbhost");
        let argv = spec.ssh_argv(40000);

        assert!(argv.contains(&"alice@bastion.example.com".to_string()));
        assert!(!argv.contains(&"-p".to_string()));
        assert_eq!(argv.last(), Some(&"alice@bastion.example.com".to_string()));
    }

    #[test]
    fn only_ssh_port_set_adds_port_flag_with_bare_destination() {
        let spec = spec(None, Some(2222), "dbhost");
        let argv = spec.ssh_argv(40000);

        assert_eq!(
            argv,
            vec![
                "-v".to_string(),
                "-N".to_string(),
                "-o".to_string(),
                "ExitOnForwardFailure=yes".to_string(),
                "-o".to_string(),
                "BatchMode=yes".to_string(),
                "-L".to_string(),
                "127.0.0.1:40000:dbhost:5432".to_string(),
                "-p".to_string(),
                "2222".to_string(),
                "bastion.example.com".to_string(),
            ]
        );
    }

    #[test]
    fn ipv6_remote_host_is_bracketed_in_forward_spec() {
        let spec = spec(None, None, "::1");
        let argv = spec.ssh_argv(40000);

        let l_index = argv.iter().position(|s| s == "-L").expect("-L present");
        assert_eq!(argv[l_index + 1], "127.0.0.1:40000:[::1]:5432");
    }

    #[test]
    fn destination_is_always_the_last_argv_element() {
        let specs = vec![
            spec(Some("alice"), Some(2222), "dbhost"),
            spec(None, None, "dbhost"),
            spec(Some("alice"), None, "dbhost"),
            spec(None, Some(2222), "dbhost"),
            spec(None, None, "::1"),
        ];

        for spec in specs {
            let argv = spec.ssh_argv(40000);
            assert_eq!(argv.last(), Some(&destination(&spec)));
        }
    }

    #[test]
    fn exactly_two_dash_o_flags_and_no_others() {
        let spec = spec(Some("alice"), Some(2222), "dbhost");
        let argv = spec.ssh_argv(40000);

        let o_count = argv.iter().filter(|s| s.as_str() == "-o").count();
        assert_eq!(o_count, 2);

        let known_flags = ["-v", "-N", "-o", "-L", "-p"];
        let known_flag_values = [
            "ExitOnForwardFailure=yes",
            "BatchMode=yes",
            "127.0.0.1:40000:dbhost:5432",
            "2222",
        ];
        for arg in &argv {
            let is_flag = known_flags.contains(&arg.as_str());
            let is_value = known_flag_values.contains(&arg.as_str());
            let is_destination = arg == &destination(&spec);
            assert!(
                is_flag || is_value || is_destination,
                "unexpected argv element: {arg:?}"
            );
        }
    }
}
