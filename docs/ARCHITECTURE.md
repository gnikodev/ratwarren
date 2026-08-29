# Architecture

This document describes the technical design decisions for ratwarren. For
the full product rationale (problem analysis, competitor research, MVP
sequencing reasoning) see the planning documents at
`~/programming/.ai/projects/db-tui/` — this file focuses on *what to build*,
not the full *why*.

## Guiding principle

Don't over-engineer ahead of a real need. Concretely, on day one:

- One built-in driver (Postgres), not a driver-per-database matrix.
- No plugin runtime until a real plugin (sharded Postgres) needs one.
- No cloud sync, no telemetry, no multi-user concerns.

## Components (target shape, post-MVP2)

```
┌─────────────────────────────────────────────────────────────────┐
│                      ratwarren binary                           │
│                                                                 │
│  ┌────────────┐   ┌────────────────┐   ┌──────────────────────┐ │
│  │  ratatui   │   │  SQL editor    │   │  Object tree /       │ │
│  │  UI layer  │◄─►│  (sqlparser-rs │◄─►│  data grid           │ │
│  │            │   │  for splitting)│   │                      │ │
│  └────────────┘   └────────────────┘   └──────────┬───────────┘ │
│                                                   │             │
│                                          DataSource trait       │
│                                       (core's only abstraction) │
└───────────────────────────────┬────────────────────┬────────────┘
                                │                    │
                         built-in driver     subprocess plugin
                        (Postgres, MVP0)     (RPC over stdio)
                                │                    │
                    ┌───────────▼──────────┐ ┌───────▼────────────────┐
                    │  tokio-postgres +    │ │  e.g. sharded-pg-      │
                    │  system `ssh -L`     │ │  plugin: owns its own  │
                    │  (SSH tunnel)        │ │  N-shard routing logic │
                    └───────────┬──────────┘ └───────┬────────────────┘
                                │                    │
                        Postgres on a          Shards 1..N (routing
                        remote VPS (via         is entirely internal
                        tunnel)                 to the plugin)
```

## `DataSource` trait

The UI layer never talks to a database driver directly — it only calls a
single trait, implemented both by built-in drivers and by the subprocess
plugin bridge. Rough shape (finalize when implementing MVP0):

```rust
trait DataSource {
    fn list_schemas(&self) -> Result<Vec<Schema>>;
    fn list_tables(&self, schema: &str) -> Result<Vec<Table>>;
    fn list_columns(&self, schema: &str, table: &str) -> Result<Vec<Column>>;
    fn execute(&self, sql: &str) -> Result<RowStream>; // streams rows, doesn't buffer everything
    fn explain(&self, sql: &str) -> Result<String>;
    fn cancel(&self, query_id: QueryId) -> Result<()>;
}
```

## SQL editor: statement-aware execution

This is the direct fix for the pain point that motivated the project
(rainfrog/gobang force you to comment out everything except the one
statement you want to run). Use `sqlparser-rs` to tokenize the buffer and
split it into statements (dialect-aware — must handle Postgres
dollar-quoting, string literals, comments). Then:

- "Run statement under cursor" = find the statement whose span contains the
  cursor position.
- "Run selection" = if there's an explicit text selection, run every
  statement whose span *overlaps* that range, in full — not the selected
  substring itself. A sloppy line-wise selection starting mid-token must
  not ship syntactically broken SQL to the server; the tradeoff is that a
  selection touching a statement at all pulls in the whole statement. Phase
  6 implements this as `Split::statements_in`. Sub-expression-selection
  execution (DataGrip-style) is a deliberate MVP1+ deferral, not an
  oversight.
- "Run buffer" = run every statement in order.

## Plugin system

### Why not dynamic loading (`.so`/`.dylib`)

Rust has no stable ABI across compiler/dependency versions. A plugin built
with a different toolchain than the host can fail to load or hit UB. Fixable
with crates like `abi_stable`, but at a cost of fragility exactly where we
have the least control — the plugin author's build environment.

### Why not WASM (at least not for the first plugin)

WASM (`wasmtime`, component model via WIT) gives sandboxing and a stable
interface, and is a reasonable longer-term option. But the first plugin
(sharded Postgres) needs to open real TCP/TLS connections to arbitrary
hosts, and WASI socket support (preview2) isn't mature/ubiquitous enough
in 2026 to be the only path for a plugin whose entire job is opening
network connections.

### Decision: subprocess + JSON-RPC over stdio

Precedent: Terraform providers and TFLint plugins — each plugin is a
separate executable, process-isolated (a crashing plugin can't take down the
host), version-independent from the host, and can be written in any
language. Terraform uses gRPC; ratwarren uses **JSON-RPC over
stdin/stdout** instead (same family of idea as the Language Server
Protocol) to keep the barrier to entry low for plugin authors who aren't
using Rust — no protobuf toolchain, no codegen required.

- A plugin is any executable that speaks JSON-RPC over stdio.
- RPC methods mirror the `DataSource` trait: `list_schemas`, `list_tables`,
  `list_columns`, `execute` (must stream rows, not buffer the whole result),
  `explain`, `cancel`.
- If JSON-over-stdio becomes a bottleneck for large binary results, switch
  the framing to length-prefixed msgpack without changing the
  "subprocess + RPC" model itself.
- The sharded-Postgres plugin holds its own list of physical shard
  connections and routing rules internally. The core app only ever sees one
  logical `DataSource` — sharding complexity never leaks into the core.

Not building this until MVP2 (see [ROADMAP.md](ROADMAP.md)) — the built-in
Postgres driver ships first as a normal Rust module, and only gets moved
behind the same RPC protocol once there's a second real consumer of it
(dog-fooding, same pattern Terraform used for in-tree providers).

## Built-in drivers

- **Postgres** — `tokio-postgres`. Chosen over `sqlx` for MVP0 because full
  control over the wire protocol is needed for proper row streaming on
  paginated queries.
- **SSH tunneling (MVP0–MVP2)** — shell out to the system `ssh -L` binary
  rather than reimplementing the SSH protocol (`russh`/`thrussh`). This
  automatically respects whatever the user already has configured:
  `~/.ssh/config`, `ProxyJump`/bastion chains, ssh-agent, keys. Revisit with
  an embedded `russh` client later only if the system-`ssh` dependency
  becomes a real problem (most likely on Windows).
- A second built-in driver (MySQL or SQLite) is added in MVP2 specifically
  to validate that `DataSource` isn't implicitly Postgres-shaped.

## Data pagination

**No exact `COUNT(*)`.** On large tables, counting rows exactly is itself an
expensive full scan (a well-known Postgres pain point). Instead: request
`LIMIT 51 OFFSET N*50`. If the 51st row comes back, there's a next page —
show a "next" affordance, don't render row 51. Don't show a total row count
at all in MVP0; if it's needed later, use an approximate estimate
(`pg_class.reltuples` / `EXPLAIN`), never an exact count.

## State storage

- **Connection config** — TOML file in the XDG config dir (`directories`
  crate for cross-platform paths), with folder/project grouping.
- **Secrets** (passwords, private keys not already handled by ssh-agent) —
  never stored in plaintext in the config. Use the `keyring` crate (macOS
  Keychain / Linux Secret Service / Windows Credential Manager).
- **Saved SQL pages** — plain `.sql` files on disk (XDG data dir, one
  subfolder per connection profile), not a proprietary blob format — so
  they're git-friendly and editable outside the tool. Tab
  order/cursor-position metadata goes in a separate lightweight sidecar
  file, not mixed into the SQL file content.

## Explicitly not building yet

- A full WASM/dynamic plugin runtime — until a plugin actually needs it.
- A custom SSH protocol implementation — system `ssh` covers MVP0–MVP2.
- Auto-update, telemetry, usage analytics.
