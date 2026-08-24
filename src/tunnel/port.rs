use std::net::{Ipv4Addr, TcpListener};

// Binding to port 0 asks the OS to pick a free ephemeral port; dropping the
// listener immediately after reading it back frees it for `ssh -L` to bind.
// This is inherently racy (TOCTOU) but is the same approach every other tool
// in this space uses — a real bind failure is handled by the retry loop in
// `Tunnel::open_with`.
pub(crate) fn reserve_local_port() -> std::io::Result<u16> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_a_nonzero_port() {
        let port = reserve_local_port().expect("reserving a local port should succeed");
        assert_ne!(port, 0);
    }

    #[test]
    fn returned_port_can_be_immediately_rebound() {
        let port = reserve_local_port().expect("reserving a local port should succeed");

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).expect(
            "the reserved port should be immediately rebindable with no lingering TIME_WAIT lock",
        );
        drop(listener);
    }
}
