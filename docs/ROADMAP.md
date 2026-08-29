# Roadmap

Deliberately sequenced to defer the riskiest part (the plugin system) until
the basic single-driver loop is polished and actually in daily use. See
[ARCHITECTURE.md](ARCHITECTURE.md) for the "why" behind each decision below.

## MVP0 — personal replacement for rainfrog/gobang/psql, one Postgres box

- One built-in driver: Postgres (`tokio-postgres`).
- SSH tunnel via system `ssh -L` (uses the existing `~/.ssh/config`).
- Connection config: flat TOML file, no folders/groups yet.
- Object tree: schema → table → columns.
- Table data view, paginated 50 rows at a time, no exact `COUNT(*)`.
- SQL editor: run statement under cursor, run explicit selection, run whole
  buffer. Buffer lives in memory only — no saved pages yet.

**Done when:** actually used instead of rainfrog/gobang for "SSH into a VPS,
look at and edit some data."

## MVP1 — full daily driver for all personal VPS boxes

- Saved SQL pages (`.sql` files on disk, tied to a connection profile).
- Multiple simultaneous connections/tabs.
- Connection grouping (folders/projects) in config.
- Query history with re-run.
- Safety preview before `UPDATE`/`DELETE`/`DROP`/`TRUNCATE` (Terraform-plan
  style, borrowed from sabiql).
- Session activity panel (`pg_stat_activity`-style) with cancel/kill.

**Done when:** fully replaces rainfrog/gobang/psql across every personal use
case, not just one box.

## MVP2 — plugin architecture, validated on itself

- Implement the "subprocess + JSON-RPC over stdio" protocol.
- Move the existing Postgres driver behind that same protocol as a
  "built-in plugin" — dog-fooding the architecture before any external
  plugin exists.
- A second driver (SQLite or MySQL) as a second example, proving
  `DataSource` isn't implicitly Postgres-shaped.

**Done when:** a third-party plugin can be written and wired in without
touching core code, and it works on par with built-ins.

## MVP3 — sharded Postgres plugin (real work use case)

- `sharded-postgres` plugin: a shard list + routing rule, presented to the
  core as a single logical `DataSource`.
- Validated against a real work environment — the only convincing test that
  the plugin architecture handles something non-trivial, not just "one more
  database driver."

**Done when:** actually used at work instead of the current way of dealing
with the sharded database.

## MVP4+ (not a priority, doesn't block anything above)

- Schema-aware autocomplete.
- ASCII/box-drawing ER diagram (à la sabiql).
- Data export (CSV/JSON, later SQL `INSERT` dumps).
- Global object search across the whole tree.
- Publish the plugin protocol and `sharded-postgres` as a reference example
  — only once there's an audience beyond the author.
