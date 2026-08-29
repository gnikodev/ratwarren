# MVP1 implementation plan

Ordered, dependency-driven phase breakdown for MVP1 (see
[ROADMAP.md](ROADMAP.md) for the scope this refines, and
[MVP0-PLAN.md](MVP0-PLAN.md) for the finished foundation it builds on). Each
phase is scoped to be independently reviewable and testable. Update the
status marker as work lands; this file is the shared checkpoint for both the
author and any subagent picking up work mid-MVP1.

Unlike MVP0, this is incremental work on a running app. Exactly one phase
(Phase 2) is a real architecture change; the rest are additive on top of it.
Phase 2 is sequenced early on purpose — every later phase adds per-connection
state, and building that state against a single-connection `App` first would
mean moving all of it into a per-session struct afterwards.

Phases 3–4 (saved pages, history) and Phase 5 (destructive-statement
classification) have no code dependency on each other and can have their
code-writer/test-writer calls run in parallel once Phase 2 has landed.

Status legend: `[ ]` not started, `[~]` in progress, `[x]` done.

## Phase 1 — Connection grouping in config

- Optional flat `group: Option<String>` field on `Connection`. Connections
  with no group render at the top level. Nested/path-style groups (`"a/b"`)
  are deliberately out of scope — a single flat label satisfies
  `ARCHITECTURE.md`'s "folder/project grouping" without a tree model in the
  config parser.
- `Config` gains an ordered grouping accessor (group order = first appearance
  in the file, member order = file order) for the picker in Phase 2 to
  render. `Config::connection(name)` and name uniqueness are unchanged —
  names stay globally unique, not unique-per-group, so nothing that resolves
  a connection by name (CLI arg, `--set-password`, saved-page and history
  directory names in Phases 3–4) has to become group-qualified.
- `#[serde(deny_unknown_fields)]` stays; the new field is `#[serde(default,
  skip_serializing_if = "Option::is_none")]` so a pre-grouping config still
  parses and still re-serializes byte-identically.
- No UI in this phase — pure config logic, same shape as MVP0's Phase 1.
- **Test:** round-trip unit tests for a grouped config and for a
  pre-grouping config (must parse, and re-serialize without introducing the
  new key); grouping-accessor ordering test; validation test that two
  connections in *different* groups still cannot share a name; malformed
  input (`group = 3`, `group = ""`) handling.

Status: `[x]`

## Phase 2 — Multi-session architecture + connection tabs

The one genuine architecture change in MVP1. Everything downstream depends
on it.

- Extract `main.rs`'s inline connect-and-spawn wiring
  (`PostgresDataSource::connect` → `worker::spawn` → `worker::spawn_canceller`
  → the three channels → the shutdown/abort/`Arc::into_inner` teardown) into
  a reusable `app::session::SourceHandle`. One handle per session in this
  phase. It must be constructible more than once per process — not a
  singleton and not tied to `main` — because Phase 7 adds a *second* handle
  per session for the activity monitor, and Phase 6 may need one too.
- `App` becomes `Vec<Session>` + an active index. `Session` owns what is
  today `App`'s per-connection state: `tree`, `grid`, `editor`, `run`,
  `status`, `focus`, `connection_name`, and its `SourceHandle`. `App` keeps
  only the tab list, the active index, and genuinely global state.
- Responses must be routed to the session that owns them — either one merged
  response channel carrying a `SessionId`, or per-session receivers selected
  over. Note the hazard the MVP0 Phase 7 review already documented:
  `DataGridState`'s and `RunState`'s `RequestId` counters are independent and
  both start at 0, so a `RequestId` is only meaningful within one session's
  one state machine and must never be compared across sessions.
- `DataSource` is **unchanged**. Multiplicity lives entirely above the trait:
  N `Arc<dyn DataSource>`, no pooling inside `PostgresDataSource`, and its
  one-permit semaphore keeps meaning exactly what it means today ("this
  physical connection, one thing at a time").
- Connection picker overlay listing configured connections by Phase 1's
  groups; opens a new session. Tab bar via `ratatui::widgets::Tabs` (present
  in the pinned ratatui 0.30 / ratatui-widgets 0.3.2). Keys for
  next/previous/close tab.
- Opening a session from inside the running TUI must not block the event
  loop. `PostgresDataSource::connect_with` is already fully async and already
  runs `Tunnel::open_with` on `spawn_blocking`, so drive it from a spawned
  task and give each tab a `Connecting` / `Failed` render state rather than
  awaiting it in `app::run`.
- **Tunnel bind race — resolve at this phase's design gate, do not defer.**
  `Tunnel::wait_ready` infers readiness from a TCP connect to
  `127.0.0.1:<local_port>` plus a `READY_SETTLE_DELAY` liveness recheck, and
  its own comment states this is "best-effort, not a guarantee: it only
  catches a bind-race loser that exits within READY_SETTLE_DELAY". With one
  tunnel that was benign. With two `ssh -L` spawns in flight at once, the
  loser of a bind race can observe the *winner's* listener on its port and
  report ready — a session silently talking to a different VPS's database.
  That is a wrong-target risk, not a timing-precision risk, and it is exactly
  what makes destructive statements dangerous. Baseline mitigation: a
  process-wide async mutex serializing tunnel opens so no two `ssh -L` spawns
  are ever concurrent, which reduces this to MVP0's already-handled
  single-tunnel case (pre-spawn liveness probe + `is_forward_bind_failure`
  retry). This depends on the real behavior of `ssh` and the OS port
  allocator, not on pure logic — it needs empirical confirmation (repeatedly
  open 3+ tunnelled sessions back to back and assert each session's
  `current_database()` / `inet_server_addr()` matches its own profile), not a
  reasoned-about "accepted tradeoff".
- **Secret resolution with the TUI already up — verify, don't assume.** MVP0
  deliberately calls the blocking `secret::resolve` keyring lookup *before*
  `ratatui::init()`, precisely so a prompt has the primary screen. Opening a
  session from inside the running TUI has no such window. On macOS the
  Keychain prompt is a separate GUI dialog (harmless); a Linux Secret Service
  backend falling back to a terminal `pinentry` would fight the alternate
  screen for the same tty. Unverified on Linux. Confirm what the backend
  actually does before choosing between "resolve on `spawn_blocking` and hope"
  and "suspend the terminal around the call".
- **Test:** unit tests that a response tagged for session B never mutates
  session A's state, including the case where both sessions' `RequestId`
  counters have independently reached the same value; tab open/close/switch
  state tests covering closing the active tab and closing the last tab;
  integration test against local Postgres opening two sessions and running a
  statement on each with interleaved responses; manual test with three
  tunnelled sessions to different real boxes open simultaneously, each
  verified to be on the database its profile names.

All automatable coverage above is done — routing invariant, tab lifecycle,
picker, keymap, T1/T2 tunnel-safety mechanisms (including the process-wide
tunnel-child registry closing the abandoned-blocking-task leak found in
review), and the two-session integration test all land and pass, including
under real Postgres/SSH. The **manual** portion — three tunnelled sessions to
different real VPS boxes open simultaneously, each verified against
something box-unique, plus confirming T2's confirmation line survives a
`ProxyJump` chain — is still outstanding. Per the same precedent as MVP0's
"done when" dogfooding criterion (see `CLAUDE.md`/`ROADMAP.md`): this does
not block marking the phase done; treat what dogfooding turns up as normal
follow-up work, not grounds to reopen this phase.

Status: `[x]`

## Phase 3 — Saved SQL pages

Additive: no change to the connection/worker architecture from Phase 2.

- `config::paths` gains a data-directory resolver (`ProjectDirs::data_dir()`,
  plus a `RATWARREN_DATA_DIR` override mirroring the existing
  `RATWARREN_CONFIG` handling) and a per-connection pages directory,
  `<data>/pages/<connection>/`. Connection names are free-form config strings
  and can contain `/`, `..`, or other path-hostile characters — sanitize
  before they become a directory name.
- A `pages` module: list `.sql` files in a connection's directory, load into
  a `TextBuffer`, save, save-as, rename, delete. Plain UTF-8 `.sql` content
  only — no header line, no embedded metadata, git-friendly and editable
  outside the tool, per `ARCHITECTURE.md`.
- Per-session: several pages open at once with a page tab strip; the active
  page's buffer is what the session's `EditorState` wraps. Open-page order
  and per-page cursor position go in a **separate sidecar file**, never mixed
  into the `.sql` content. A missing, truncated or unparseable sidecar must
  degrade to "no pages open" — it is a convenience cache, never an error that
  blocks opening a session.
- Dirty tracking and a save prompt on page close / tab close / quit. External
  edits are not watched; reloading a file changed outside the tool is a
  manual action.
- **Test:** unit tests for connection-name sanitization (`..`, `/`, leading
  `.`, empty, very long) and for absent/corrupt sidecar degradation;
  load → edit → save → reload preserving exact bytes including trailing
  newline and CRLF handling; page-tab state tests (open, switch, close clean,
  close dirty); test that saving to a path outside the connection's pages
  directory is refused.

Status: `[x]`

## Phase 4 — Query history with re-run

Depends on Phase 3 only for the shared data-directory helper and the overlay
list widget shape; the storage format is its own.

- Append each executed `RunUnit` to a per-connection history file under
  Phase 3's data directory, created with `0o600` at open time the same way
  `Config::save_to` does it. Load the most recent N entries into memory when
  a session opens; do not read the whole file.
- Recording hooks into the existing run pipeline (`App::start_run` /
  `RunState::start` / the `RunOutcome::Next` advance) so each statement of a
  multi-statement run is recorded individually, each with its timestamp and
  final outcome (ok / failed / cancelled). A cancelled statement that
  `CancelOutcome::CompletedFirst` shows actually committed must not be
  recorded as cancelled.
- History overlay: newest-first list, substring filter, `Enter` loads the
  entry into the active editor page. It must **never** auto-run — whether
  loading inserts at the cursor or opens a scratch page is a design-gate
  decision, but re-run is always a second, explicit keypress.
- Consecutive duplicate entries collapse; the file is capped (entry count or
  bytes) and trimmed so it cannot grow without bound.
- Note: history is plaintext SQL on disk and will capture whatever literals
  were typed into the editor, including a `CREATE ROLE ... PASSWORD '...'`.
  `0o600` plus a documented caveat is the MVP1 answer; automatic redaction is
  out of scope.
- **Test:** unit tests for append / trim / consecutive-dedupe; a truncated or
  partially-written history file must load the entries it can and drop the
  rest without erroring; a failed statement and a cancelled statement are
  each recorded with the correct outcome, and a `CompletedFirst` cancel is
  recorded as having run; loading an entry populates the buffer and dispatches
  no `WorkerRequest`.

Status: `[ ]`

## Phase 5 — Destructive-statement classification + confirmation gate

Independent of Phases 3–4. Splits off from Phase 6 because it is shippable
safety on its own, while Phase 6's mechanism is still an open product
decision.

- New `editor::classify`, producing a `StatementKind` per `RunUnit`.
- **The gate itself stays tokenizer-only.** The blocking decision is made
  from the leading significant token on the existing `sqlparser::tokenizer`
  path: `DELETE` / `UPDATE` / `DROP` / `TRUNCATE`. That check cannot fail,
  which preserves MVP0's deliberate "tokenizer only, never refuse to run
  valid SQL the parser doesn't understand" property for the one code path
  that can stop a run.
- `sqlparser::Parser::parse_sql` is pulled in for the first time, but only to
  *enrich*: target table(s), and whether an `UPDATE`/`DELETE` carries a
  `WHERE` (`Update::selection` and `Delete::selection` are both
  `Option<Expr>`). Verified available in the pinned `sqlparser =0.62.0` with
  the current `default-features = false, features = ["std"]` set — no
  Cargo.toml change needed. A parse failure must never block a run;
  sqlparser's Postgres dialect does not parse all valid Postgres, and it
  degrades to "destructive, target unknown".
- One deliberate fail-*closed* case: a statement whose leading keyword is
  `WITH` can still be data-modifying (`WITH x AS (...) DELETE FROM t ...`),
  and the tokenizer alone cannot tell. If the AST parse succeeds, classify
  from the AST; if it fails on a `WITH`-led statement, treat it as
  potentially destructive and confirm.
- One confirmation modal listing **every** destructive statement in the run,
  shown before the first statement is dispatched — not one prompt per
  statement mid-run. A missing `WHERE` is called out distinctly from a
  qualified statement. Esc / `n` aborts the whole run and dispatches nothing.
- Per-connection opt-out in config (a local dev box should not carry the same
  friction as prod). Exact key name and default settle at the design gate;
  the default must be "confirm".
- **Test:** classification unit tests over `DELETE`/`UPDATE` with and without
  `WHERE`, `DROP TABLE`, `TRUNCATE`, `WITH ... DELETE`, a comment-prefixed
  `DELETE`, a `SELECT` whose *string literal* contains the word `delete`, and
  a statement sqlparser cannot parse but whose first token is `DELETE` (must
  still gate); a run of only `SELECT`s never opens the modal; aborting the
  modal sends zero `WorkerRequest`s; the config opt-out suppresses the modal
  for that connection only.

Status: `[ ]`

## Phase 6 — Affected-row preview (the "Terraform-plan" part)

- **Scope question for the author, to be settled at this phase's design gate
  before any code is written.** ROADMAP.md names sabiql's "Terraform-plan
  style" preview without specifying a mechanism, and the candidates are three
  different features with very different cost and blast radius:
  - **(a) `EXPLAIN` only** — estimated affected row count, no data, no side
    effects, already covered by `DataSource::explain`. Cheapest; also the
    weakest, since a planner estimate is not "here are the rows".
  - **(b) derived `SELECT`** — reuse Phase 5's AST to rewrite the statement's
    target and `WHERE` into `SELECT * ... LIMIT 51` and show the rows that
    *would* be touched, in the existing grid with the existing pagination
    rule. Only works for statements the AST parse actually understood.
  - **(c) `BEGIN; <stmt> RETURNING ...; ROLLBACK;`** — exact before/after
    rows, but it takes real write locks on a production table for the
    duration of the preview and can deadlock against the primary connection.
  Pick one. Do not build a framework that could later do all three.
- Whatever is picked must go through `DataSource` as it stands
  (`execute` / `explain`). Do **not** add a `preview` method to the trait —
  the trait is the core's one abstraction boundary and it is not a place to
  park Postgres-shaped conveniences.
- (b) and (c) both want a second physical connection per session so a preview
  cannot evict the primary connection's in-flight state — that is the same
  second `SourceHandle` Phase 7 needs. If (c) is chosen, sequence this phase
  after Phase 7, or move the second-handle work forward into this one.
- **Test:** the specific criteria depend on the mechanism chosen and are
  written at the design gate. Non-negotiable minimum regardless of mechanism:
  an integration test against local Postgres proving the target table is
  byte-identical (row count *and* contents) after previewing a `DELETE` and
  an `UPDATE`, and that a preview of a statement against a table another
  connection holds a lock on cannot wedge the primary connection.

Status: `[ ]`

## Phase 7 — Session activity panel with cancel/kill

- The read path is plain SQL through the existing `DataSource::execute` —
  `SELECT pid, backend_start, state, wait_event_type, wait_event, xact_start,
  query_start, usename, application_name, client_addr, query FROM
  pg_stat_activity WHERE datname = current_database()`. **No new `DataSource`
  trait surface.**
- Needs a **second `SourceHandle` per session**, built on Phase 2's
  extraction. The primary connection's one-permit semaphore means a poll
  issued while a long query is running would sit in `worker::retry_on_busy`
  until `BUSY_RETRY_BUDGET` (10s) expires — the panel would be blind exactly
  when it is most useful. Open the monitor connection lazily on first opening
  the panel, and reuse the session's existing tunnel by dialing
  `127.0.0.1:<existing local_port>` — do not spawn a second `ssh -L`, which
  would re-open the Phase 2 bind-race question for no benefit.
- Polling adds a third arm (a `tokio::time::interval`) to `app::run`'s
  `tokio::select!`. The timer must only tick while the panel is visible, and
  a tick arriving while the previous poll is still in flight must be dropped,
  not queued.
- **Cancel/kill must not signal a recycled pid.** A pid from a snapshot can
  belong to a different backend by the time the user presses the key. Signal
  through a re-verifying predicate rather than a bare pid:
  `SELECT pg_cancel_backend(pid) FROM pg_stat_activity WHERE pid = $1 AND
  backend_start = $2`. `backend_start` is fixed for the life of a backend and
  is microsecond-resolution, so it pins the identity of the session the user
  actually selected. A zero-row result means "that backend is already gone"
  and must be reported as such, not as success.
- **Privileges are a real, silent failure mode — verify against the author's
  actual role, don't assume superuser.** `pg_cancel_backend` /
  `pg_terminate_backend` require superuser, membership in the target's role,
  or the `pg_signal_backend` predefined role (which cannot signal a
  superuser's backend); a non-privileged user sees `NULL` in `query` and
  several other columns for other roles' backends. The panel must render "not
  permitted to see this query" distinctly from "idle backend with no query",
  and a permission error on cancel must surface, never be swallowed. Report
  the returned boolean too: `pg_terminate_backend(pid, timeout)` can return
  false with a warning rather than raising.
- `pg_terminate_backend` (kill) gets its own confirmation prompt, separate
  from cancel. Killing one of ratwarren's own backends — the session's
  primary connection or its monitor connection — must be refused up front,
  not attempted and reported after the fact.
- **Test:** unit tests on snapshot → rendered row, including the NULL-`query`
  case and the refuse-to-kill-our-own-backend case; integration test against
  local Postgres — open a second connection, start `pg_sleep(30)` on it, see
  it in the panel, cancel it, assert it disappears; integration test that the
  re-verify predicate matches zero rows when `backend_start` differs from the
  snapshot; manual test against a real VPS with the author's actual (likely
  non-superuser) role to establish what is genuinely visible and permitted
  there.

Status: `[ ]`

## Phase 8 — Integration & dogfood

- Reconcile the keymap across everything MVP1 added: connection picker, tab
  switch/close, page tabs, history overlay, destructive-confirm modal,
  activity panel. `map_key` currently dispatches on `Focus` alone; with
  several modal overlays all wanting `Esc`, `Tab`, `Enter` and `q`, it needs
  a modal/overlay layer above `Focus` rather than more special cases inside
  it.
- Per-tab status line and a footer that reflects the active session and
  active overlay. The single-line footer can no longer list every binding —
  add a help overlay.
- **Test:** the actual MVP1 "done when" criterion from `ROADMAP.md` — used
  instead of rainfrog/gobang/psql across *every* personal box, with several
  open at once, not just one. Automatable portion: a keymap test asserting no
  overlay leaves a key ambiguously bound, and a full-app smoke test that
  opens two sessions, saves and reloads a page, re-runs from history, and
  confirms a destructive statement.

Status: `[ ]`

## Out of scope for MVP1

Named here because each is a plausible-looking extension of a phase above,
and each belongs to a later stage or to no stage at all:

- **Plugin runtime, JSON-RPC/subprocess protocol, a second DB driver** —
  MVP2, per ROADMAP.md. Nothing in MVP1 may add abstraction "so the plugin
  system will fit later"; `DataSource` stays a plain in-process trait.
- **Ad-hoc result pagination for editor queries** (deferred to MVP1 by the
  MVP0 Phase 7 review because it "needs a second connection"). Phase 2's
  `SourceHandle` extraction and Phase 7's second-handle work *unblock* it,
  but it is not on ROADMAP.md's MVP1 list and is not implemented by any phase
  here. Add it deliberately if wanted, don't let it drift in via Phase 7.
- **Sub-expression-selection execution** (DataGrip-style running of a
  selected fragment rather than the whole overlapping statement) — an
  explicit MVP1+ deferral in `ARCHITECTURE.md`, and not required by any MVP1
  bullet.
- **Nested connection folders**, config editing from inside the TUI, and
  connection profiles created at runtime — Phase 1 is a flat label read from
  a hand-edited TOML file.
- **Schema-aware autocomplete, ER diagrams, data export, global object
  search** — MVP4+.
- **Watching saved `.sql` files for external changes**, multi-cursor editing,
  syntax highlighting, undo/redo — none are MVP1 bullets; the editor stays
  what MVP0 built plus load/save.
- **Redacting secrets out of query history**, encrypting history or saved
  pages — `0o600` and a documented caveat is the MVP1 position.
- **A connection pool inside `PostgresDataSource`.** Phase 2 gives N
  independent connections above the trait; Phase 7 adds one named second
  connection with a specific job. Neither is a pool, and neither should grow
  into one.
