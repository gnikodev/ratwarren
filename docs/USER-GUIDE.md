# User guide (MVP1)

What ratwarren can do right now, every config field it understands, and
every keybinding — as implemented, not as planned. See
[ROADMAP.md](ROADMAP.md) for what's coming next, [MVP1-PLAN.md](MVP1-PLAN.md)
for the phase-by-phase breakdown, and [ARCHITECTURE.md](ARCHITECTURE.md) for
the design rationale behind these choices.

## What works today

- Postgres only, via `tokio-postgres`. Query results stream row-by-row —
  a query is never fully buffered in memory before you see the first row.
- **Multiple connections open at once, as tabs.** Open a connection picker
  (`Ctrl+T`), grouped by an optional `group` label in the config, and switch
  between open sessions with `Ctrl+N`/`Ctrl+P`. Each tab has its own tree,
  editor, grid, and SSH tunnel — see "Multiple connections and tabs" below.
- SSH tunnel to the database via the system `ssh -L`, so it respects your
  existing `~/.ssh/config` (aliases, `ProxyJump`, keys, ssh-agent) exactly
  as your normal `ssh` command does. Opening several tunnelled tabs at once
  is safe — tunnel opens are serialized internally and each one is verified
  to own its own forwarded port before being trusted (see "SSH tunnels and
  concurrent tabs" below).
- Password from the OS keyring (Keychain / Credential Manager / Secret
  Service), from the `RATWARREN_PASSWORD` environment variable, or no
  password at all (peer/trust auth).
- Object tree: schemas → tables → columns, loaded lazily as you expand
  nodes.
- Data grid with pagination, 50 rows at a time, without an exact `COUNT(*)`
  over the table.
- A statement-aware SQL editor: run the statement under the cursor, run an
  explicit selection, or run the whole buffer — you never need to comment
  out the rest of the buffer to run just one query.
- Cancel a running query.
- **Saved SQL pages, as plain `.sql` files on disk** — several pages open
  at once per session tab, dirty tracking, and a save-or-discard prompt
  before a dirty page's edits would otherwise be lost. See "Saved SQL
  pages" below.

**Not yet implemented** (later MVP1 phases — see
[MVP1-PLAN.md](MVP1-PLAN.md)): query history, a confirmation gate before
destructive statements, and a session activity panel.

## Configuring a connection

Run `ratwarren` with no arguments. With no connections configured yet, it
prints the exact config file path for your platform and exits — you never
need to guess it:

- Linux: `~/.config/ratwarren/config.toml`
- macOS: `~/Library/Application Support/ratwarren/config.toml`
- Windows: `%APPDATA%\ratwarren\config.toml`

To use a different location entirely, set `RATWARREN_CONFIG` to a full
file path and ratwarren reads/writes there instead.

The file is plain TOML, hand-editable, one `[[connections]]` table per
connection:

```toml
[[connections]]
name = "prod"              # required, must be unique across ALL connections
                            # in the file, not just within a group
group = "personal-vps"     # optional — see "Multiple connections and tabs"
host = "127.0.0.1"         # required — see "host and tunnel" below
port = 5432                # optional, defaults to 5432
database = "app"           # required
user = "app"               # required

# Optional. Omit this whole table for passwordless/peer auth.
[connections.password]
source = "keyring"
account = "app@prod"       # optional; defaults to "user@host:port/database"

# Optional. Omit this whole table to connect directly, no tunnel.
[connections.tunnel]
host = "my-vps"            # required — an SSH destination, ideally a
                            # ~/.ssh/config Host alias (see below)
user = "deploy"            # optional — SSH login user
port = 22                  # optional — SSH port on the bastion, NOT the
                            # database port
```

`name`, `host`, `database`, `user` must be non-empty; `port` must not be
`0`. The config parser rejects unknown keys outright (a typo'd field name
fails to load rather than being silently ignored).

## Multiple connections and tabs

The config file is just a TOML array — add as many `[[connections]]` tables
as you have databases, each with its own `name`:

```toml
[[connections]]
name = "blog-prod"
group = "personal-vps"
host = "127.0.0.1"
database = "blog"
user = "app"

[connections.tunnel]
host = "blog-vps"          # a ~/.ssh/config Host alias

[[connections]]
name = "blog-staging"
group = "personal-vps"
host = "127.0.0.1"
database = "blog_staging"
user = "app"

[connections.tunnel]
host = "blog-vps"          # same box, different database

[[connections]]
name = "local-dev"
host = "localhost"          # no group — renders at the top level in the picker
database = "blog_dev"
user = "postgres"
```

`group` is a flat, free-form label purely for how the connection picker
displays entries — it has no other effect (names still have to be unique
across the whole file, groups don't nest). Connections with no `group` key
render ungrouped, in whatever position their first ungrouped entry appears
in the file.

Run `ratwarren` with no connection name (or with more than one connection
and no name at all) and it opens straight into the connection picker rather
than erroring out — an invalid *explicit* name (`ratwarren no-such-conn`)
still prints the list of configured names and exits. From inside the picker,
`Enter` opens the selected connection as a new tab; you can open several
connections at once this way, including the same box multiple times if you
want two tabs against the same database.

**Tab keybindings, from anywhere:**

| Key | Action |
|---|---|
| `Ctrl+T` | Open the connection picker |
| `Ctrl+W` | Close the active tab |
| `Ctrl+N` / `Ctrl+P` | Switch to the next/previous tab |

**Inside the picker overlay:**

| Key | Action |
|---|---|
| `↑`/`k`, `↓`/`j` | Move selection (skips group headers) |
| `Home`/`g`, `End`/`G` | Jump to first/last connection |
| `Enter` | Open the selected connection as a new tab |
| `Esc` or `Ctrl+T` | Close the picker (only if at least one tab is already open) |
| `q` / `Ctrl+C` | Quit ratwarren — only works here if there are no open tabs; otherwise these keys do nothing while the picker is open, so browsing connections can't accidentally quit a session you have open |

Closing the last remaining tab (`Ctrl+W`) automatically reopens the picker
instead of quitting.

## Saved SQL pages

Each session tab can hold several "pages" — independent editor buffers with
their own page-tab strip above the editor pane. A page is either a
never-saved scratch buffer or backed by a plain `.sql` file on disk, one
directory per connection under the platform data directory (next to the
config directory printed on first run) — nothing proprietary, nothing in a
database; every saved page is a normal file you can open, edit, or version
control outside ratwarren. A dirty page's tab shows a trailing `*`.

**Page-tab keybindings, from anywhere within a session:**

| Key | Action |
|---|---|
| `Ctrl+O` | Open a saved page (lists every `.sql` file for this connection) |
| `Ctrl+S` | Save the active page — prompts for a name the first time a scratch page is saved |
| `F2` | Rename the active page |
| `Ctrl+G` | Start a new scratch page |
| `Alt+W` or `Ctrl+F4` | Close the active page (prompts if it has unsaved edits) |
| `Alt+N` or `Ctrl+PageDown` | Switch to the next page |
| `Alt+P` or `Ctrl+PageUp` | Switch to the previous page |
| `F5` | Reload the active page from disk, discarding any in-memory edits |

`Alt+W`/`Alt+N`/`Alt+P` have non-Alt fallbacks because `Alt`+letter reporting
is unreliable in some terminals (notably macOS Terminal.app with default
settings) — if the `Alt` chord doesn't seem to do anything, use the
fallback instead.

**Inside the "open page" overlay (`Ctrl+O`):**

| Key | Action |
|---|---|
| `↑`/`k`, `↓`/`j` | Move selection |
| `Enter` | Open the selected page |
| `d` | Delete the selected page (asks for confirmation first) |
| `Esc` | Close the overlay |

**Inside the unsaved-changes and save-as/rename prompts:**

Closing a dirty page, closing a tab with dirty pages, or quitting with any
dirty page anywhere raises an unsaved-changes prompt listing what's dirty:

| Key | Action |
|---|---|
| `s` | Save every listed page and then proceed (a still-unnamed scratch page raises the save-as prompt below, one at a time, before continuing) |
| `y` / `Enter` | Discard the listed pages' unsaved edits and proceed |
| `n` / `Esc` | Cancel — nothing is closed, saved, or discarded |

A save-as or rename prompt (raised directly by `Ctrl+S` on a never-saved
page, `F2`, or by choosing `s` above for an unnamed page) is a plain text
box:

| Key | Action |
|---|---|
| Printable characters | Type the page name (`.sql` is appended automatically if omitted) |
| `Backspace` | Delete the last character |
| `Enter` | Confirm — refused if the name is already used by another saved page or another open tab |
| `Esc` | Cancel |

## SSH tunnels and concurrent tabs

Opening several tunnelled tabs at once — including in quick succession — is
safe. Two things make it so, worth knowing if you're specifically testing
this:

- Tunnel opens are serialized internally: only one `ssh -L` is ever being
  established at a time, process-wide, so two tabs opening together can
  never race onto the same local port. If you open a second tab while a
  first one's tunnel is still connecting, the second tab may briefly show
  "waiting for another tab's SSH tunnel to finish opening…" before its own
  `ssh` is spawned — this is expected, not a hang.
- Each tunnel confirms, via `ssh`'s own verbose output, that *it* — not some
  other process — actually owns the local port it's forwarding through. If
  that confirmation can't be obtained (rare — an unusual `ssh` build, or a
  race with a process outside ratwarren entirely), the tab's title gets a
  sticky `⚠` and the footer shows "tunnel readiness unconfirmed — could not
  verify this ssh owns port N; it may belong to another process" while that
  tab is active. The connection still works; this is a warning that the
  usual verification couldn't run, not a connection failure — treat it as a
  signal to double check you're talking to the box you think you are before
  running anything destructive.

### `host`/`port` vs. `tunnel` — the one non-obvious part

`host`/`port` always describe Postgres as seen by whoever opens the TCP
connection:

- **No `tunnel` table:** that's your own machine, so `host`/`port` is
  wherever Postgres is actually reachable from where you're running
  ratwarren.
- **With a `tunnel` table:** that's the bastion, so `host`/`port` is
  almost always `127.0.0.1`/`5432` — i.e. how Postgres looks *from the
  VPS itself*, since that's the end of the SSH forward.

The `[connections.tunnel]` table only carries the SSH destination. The
local end of the forward (which local port `ssh -L` binds to) is chosen
automatically at connect time — nothing to configure there.

### Setting up a tunnelled connection, concretely

Say Postgres runs on a VPS, bound to `localhost` only, reachable over SSH:

```toml
[[connections]]
name = "prod-vps"
host = "127.0.0.1"
port = 5432
database = "app"
user = "app"

[connections.password]
source = "keyring"

[connections.tunnel]
host = "my-vps"
```

Don't put a raw IP/hostname in `tunnel.host` — add an alias to
`~/.ssh/config` instead and reference it by name:

```
Host my-vps
    HostName 1.2.3.4
    User root
    IdentityFile ~/.ssh/id_ed25519
    ProxyJump bastion-if-any
```

`tunnel.host = "my-vps"` then inherits everything that alias already
does — keys, `ProxyJump` chains, everything your normal `ssh my-vps`
already works with. Only set `tunnel.user`/`tunnel.port` in ratwarren's
config if you need to override what `~/.ssh/config` would otherwise pick.

### Storing the password

If a connection has a `[connections.password]` table, store the actual
secret in the OS keyring:

```sh
ratwarren --set-password prod-vps
```

This prompts for the password without echoing it to the terminal, and
never opens a tunnel or connects to the database. The stored entry is
keyed by `account` (or the derived `user@host:port/database` default) —
editing any of those fields after storing a password orphans the stored
entry, and you'll need to run `--set-password` again.

For scripting, CI, or while iterating on the code, `RATWARREN_PASSWORD`
in the environment always wins over a keyring lookup — the keyring is
never even consulted if it's set.

**macOS quirk:** Keychain ties a stored credential to the exact binary
that created it. A `cargo run` rebuild produces a new binary and gets
re-prompted for Keychain authorization on every keyring read afterward.
Run `--set-password` against a binary built with `cargo build --release`
(or `cargo install --path .`), or just use `RATWARREN_PASSWORD` while
you're iterating on the code, to avoid the repeated prompt.

If the OS keyring is unavailable (e.g. a headless VPS with no keyring
daemon), ratwarren doesn't crash or hang — it prints a note and connects
with no password, letting Postgres's own authentication failure be the
visible error.

## Running it

```sh
ratwarren                     # one connection configured: opens it directly.
                               # zero or several configured: opens the picker.
ratwarren <connection-name>   # opens that connection directly as the first
                               # tab; open more from the picker (Ctrl+T)
ratwarren --set-password <connection-name>
ratwarren --help
```

## Keybindings

Three panes — Tree, Editor, Grid. `Tab` cycles focus between them
(Tree → Editor → Grid → Tree; Grid is skipped while no table/query result
is open).

**Global, regardless of focus:**

| Key | Action |
|---|---|
| `Ctrl+C` | Cancel the running query if one is in flight, otherwise quit |
| `Ctrl+R` | Run the statement under the cursor (or the selection, if one is active) |
| `Ctrl+E` | Run the whole editor buffer |
| `Tab` | Switch focus to the next pane |
| `Ctrl+T` | Open the connection picker (see "Multiple connections and tabs") |
| `Ctrl+W` | Close the active tab |
| `Ctrl+N` / `Ctrl+P` | Switch to the next/previous tab |

**Tree pane:**

| Key | Action |
|---|---|
| `↑`/`k`, `↓`/`j` | Move selection |
| `→`/`l` | Expand node |
| `←`/`h` | Collapse node |
| `Enter` | Expand/toggle a schema or table node; opens a table's data in the Grid |
| `PageUp`/`PageDown` | Page up/down |
| `Home`/`g`, `End`/`G` | Jump to first/last row |
| `.` | Show/hide system schemas (`pg_catalog`, `information_schema`) |
| `r` | Refresh the selected node |
| `q` | Quit |

**Grid pane:**

| Key | Action |
|---|---|
| `↑`/`k`, `↓`/`j` | Move row selection |
| `←`/`h`, `→`/`l` | Scroll columns left/right |
| `PageUp`/`PageDown` | Page up/down within the current page of rows |
| `Home`/`g`, `End`/`G` | Jump to first/last row on the current page |
| `n` / `p` | Next/previous page — table-browsing only; a no-op for an ad-hoc query result, which has no `OFFSET` to re-run with |
| `r` | Refresh |
| `Esc` | Return focus to the Tree |
| `q` | Quit |

**Editor pane:**

| Key | Action |
|---|---|
| Printable characters | Insert at cursor |
| `Enter` | New line |
| `Backspace` / `Delete` | Delete before/after cursor (deletes the active selection instead, if any) |
| `←`/`→`/`↑`/`↓` | Move cursor; hold `Shift` to extend the selection instead |
| `Home`/`End` | Start/end of the current line |
| `Ctrl+Home`/`Ctrl+End` | Start/end of the whole buffer |
| `Ctrl+A` | Select the entire buffer |
| `Esc` | Clear the current selection |

Note: `q` quits from the Tree/Grid panes, but inserts a literal `q`
character while the Editor is focused.

## Known limitations (MVP1, phases 1-3)

- Two sessions opened on the same connection each restore their saved pages
  independently — if both have the same page open and edited, whichever
  saves last wins (no cross-session file locking).
- No confirmation before `DELETE`/`UPDATE`/`DROP`/`TRUNCATE` — double-check
  before you run destructive SQL (a later MVP1 phase adds a confirmation
  gate).
- No query history.
- No connection picker search/filter yet — with a lot of connections
  configured, `↑`/`↓`/`Home`/`End` are the only ways to navigate the list.
- No reconnect/retry button on a tab that failed to connect — close it
  (`Ctrl+W`) and reopen from the picker instead.
- Paging an ad-hoc query result (not a table browse) is capped at the
  first 50 rows — there's no `OFFSET`-based next page for arbitrary SQL.
- Opening a table in the Grid while a multi-statement run is still going
  will show that table, but the run's next result will replace it when it
  arrives.
