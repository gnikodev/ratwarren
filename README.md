# ratwarren

A terminal UI (TUI) client for SQL databases, built for a workflow where your
databases live on remote VPS boxes and their ports are never exposed to the
public internet.

Written in Rust with [ratatui](https://ratatui.rs). Inspired by the
`lazy*` family of tools (lazygit, lazydocker), but for SQL.

> **Status: early development, not usable yet.** This repository currently
> contains only planning documentation. Implementation follows the roadmap
> in [docs/ROADMAP.md](docs/ROADMAP.md), starting with a single-driver
> Postgres MVP.

## Why

Existing terminal DB clients (rainfrog, gobang, lazysql) are close but fall
short in everyday use — most notably, none of them make it easy to run a
single statement out of several written in the editor without commenting the
rest out. Existing GUI clients (DBeaver, DataGrip) support SSH tunnels but
are heavy and not terminal-native.

ratwarren is built around three things at once:

- A fast, self-contained binary you can `scp` straight onto a bare VPS and
  run over SSH, no exposed DB port, no local GUI required.
- A statement-aware SQL editor: run the statement under the cursor, run a
  selection, or run the whole buffer — never "comment out everything else."
- A plugin system that extends not just the list of supported databases, but
  the connection logic itself. The first plugin target is sharded
  PostgreSQL — routing one logical connection across N physical shards.

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the technical design
and [docs/ROADMAP.md](docs/ROADMAP.md) for the MVP plan.

## Configuration and secrets

Connections live in a plain TOML config file at your platform's standard
config directory (e.g. `~/.config/ratwarren/config.toml` on Linux,
`~/Library/Application Support/ratwarren/config.toml` on macOS). ratwarren
prints the exact path itself if you run it before adding any connections.

Passwords are never stored in that file. A connection opts into a password
by adding a `[connections.password]` table with `source = "keyring"`, and
the actual secret is stored in your OS's native credential store (Keychain
on macOS, Credential Manager on Windows, Secret Service on Linux) via the
`keyring` crate. To store one:

```sh
ratwarren --set-password <connection>
```

This prompts for the password (without echoing it to the terminal) and
writes it to the OS keyring; it never opens a tunnel or connects to the
database.

For scripting, CI, or while iterating on the code, you can bypass the
keyring entirely by setting `RATWARREN_PASSWORD` in the environment — it
always takes precedence over a keyring-configured password. This is also
the easiest workaround for a macOS quirk: Keychain ties a stored item to
the exact binary that created it, so a `cargo run` rebuild produces a new
binary and gets re-prompted for authorization on every keyring read.
Running `--set-password` against a binary built with `cargo install` (or
just using `RATWARREN_PASSWORD` while developing) avoids that.

## License

MIT — see [LICENSE](LICENSE).
