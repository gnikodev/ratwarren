# ratwarren

A terminal UI (TUI) client for SQL databases, built for a workflow where your
databases live on remote VPS boxes and their ports are never exposed to the
public internet.

Written in Rust with [ratatui](https://ratatui.rs). Inspired by the
`lazy*` family of tools (lazygit, lazydocker), but for SQL.

> **Status: MVP0 done, MVP1 in progress (phases 1-2 done).** Connection
> grouping and multiple simultaneous connections/tabs are implemented; a
> real multi-tab session against personal VPS boxes is the next outstanding
> step. See [docs/ROADMAP.md](docs/ROADMAP.md) and
> [docs/MVP1-PLAN.md](docs/MVP1-PLAN.md) for what's done and what's next.

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

## Getting started

For the full picture — every config field, SSH tunnel setup, every
keybinding, and current limitations — see
[docs/USER-GUIDE.md](docs/USER-GUIDE.md). The short version:

1. Build it: `cargo build --release`. Use `target/release/ratwarren` from
   here on, not `cargo run` — see the Keychain note below for why.
2. Run it once with no config to find out where it expects one:
   ```sh
   ratwarren
   ```
   With nothing configured yet, it prints the exact config file path for
   your platform (see below) and exits — it never guesses or creates the
   file for you.
3. Create that file with at least one connection:
   ```toml
   [[connections]]
   name = "prod"
   host = "127.0.0.1"    # or a bastion-local address if using an SSH tunnel below
   database = "app"
   user = "app"

   [connections.password]
   source = "keyring"    # omit this whole table entirely for passwordless/peer auth
   ```
   Add an `[connections.tunnel]` table (`host`, optionally `user`/`port`) if
   the database is only reachable through an SSH bastion — see
   [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the tunnel model.
4. If you added a `[connections.password]` table, store the actual secret
   (see "Configuration and secrets" below) before connecting — otherwise
   the database will simply reject the connection with a
   "password missing"/"password authentication failed" error.
5. Connect:
   ```sh
   ratwarren prod          # explicit connection name
   ratwarren                # works too if you only have one connection configured
   ```
   With zero or several connections configured and no name given, ratwarren
   opens straight into a connection picker instead of guessing — open more
   connections as additional tabs from there with `Ctrl+T`. Add an optional
   `group = "..."` field to a connection to organize the picker when you
   have several boxes configured; see
   [docs/USER-GUIDE.md](docs/USER-GUIDE.md#multiple-connections-and-tabs)
   for the full multi-connection and tunnel setup, including what a tab
   looks like while it's still connecting or if it fails.

## Configuration and secrets

Connections live in a plain TOML config file at your platform's standard
config directory — not your home directory directly, but the OS-standard
per-app location, exactly like most native macOS/Linux/Windows apps use
(`~/.config/ratwarren/config.toml` on Linux,
`~/Library/Application Support/ratwarren/config.toml` on macOS,
`%APPDATA%\ratwarren\config.toml` on Windows). ratwarren prints the exact
path itself if you run it before adding any connections — you never need to
guess it or hardcode it. If you'd rather keep the file somewhere else
entirely, set `RATWARREN_CONFIG=/path/to/config.toml` to override the
location outright.

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
