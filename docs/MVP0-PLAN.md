# MVP0 implementation plan

Ordered, dependency-driven phase breakdown for MVP0 (see
[ROADMAP.md](ROADMAP.md) for the scope this refines). Each phase is scoped
to be independently reviewable and testable. Update the status marker as
work lands; this file is the shared checkpoint for both the author and any
subagent picking up work mid-MVP0.

Status legend: `[ ]` not started, `[~]` in progress, `[x]` done.

## Phase 0 — Project scaffold

- `cargo init` as a binary crate.
- Deps for this phase only: `ratatui`, `crossterm`. The rest of the "core
  deps" list (`tokio`, `tokio-postgres`, `sqlparser-rs`, `serde` + `toml`,
  `directories`, `keyring`) is deliberately deferred to the phase that
  actually needs it (see Phases 1, 3, 6 below) — adding them now would be
  exactly the over-engineering `ARCHITECTURE.md` warns against.
- Minimal ratatui event loop: renders a placeholder screen, quits on `q`.
- **Test:** `cargo build`, `cargo run` manually; one `TestBackend`-based
  unit test asserting the rendered title/body content, plus a
  too-small-terminal boundary test.

Status: `[x]`

## Phase 1 — Connection config

- TOML schema: host, port, database, user, secret reference, optional
  SSH-tunnel block (bastion host, remote port).
- Load/save via `directories` XDG config path.
- Pure logic, no UI dependency.
- **Test:** unit tests for parse/serialize round-trip and malformed-input
  handling.

Status: `[x]`

## Phase 2 — SSH tunnel manager

- Build/spawn `ssh -L <local_port>:<remote_host>:<remote_port>
  <user>@<bastion> -N` as a child process.
- Allocate a free local port; track process lifecycle (kill on drop/app
  exit).
- Must respect the user's existing `~/.ssh/config`, `ProxyJump`, ssh-agent
  — do not pass credentials on the command line.
- **Test:** unit tests on command construction (no real network needed);
  manual test against a real personal VPS for the actual tunnel.

Status: `[x]` (automated criteria only — manual VPS smoke test against a
real bastion/ProxyJump setup is still outstanding, see conversation)

## Phase 3 — `DataSource` trait + Postgres driver

- Finalize the `DataSource` trait per `ARCHITECTURE.md`
  (`list_schemas`, `list_tables`, `list_columns`, `execute`, `explain`,
  `cancel`).
- `tokio-postgres`-backed implementation. `execute` must stream rows, never
  buffer a full result set.
- **Test:** integration tests against a local/docker Postgres instance.

Status: `[x]`

## Phase 4 — Object tree UI

- ratatui widget: schema → table → columns navigation, wired to
  `DataSource`.
- **Test:** manual UI walkthrough; light unit coverage on tree
  state/selection logic where it's pure enough to isolate.

Status: `[x]`

## Phase 5 — Data grid + pagination

- Table data view widget using `LIMIT 51 OFFSET N*50`.
- Render 50 rows; use the 51st only to decide whether "next page" is shown.
  Never render row 51, never run an exact `COUNT(*)`.
- **Test:** unit tests on the has-next-page boundary logic (0, 50, 51 rows
  returned); manual UI test for the widget itself.

Status: `[x]` (automated criteria only — manual UI walkthrough of the grid
paging/scroll/NULL-rendering is still outstanding, see conversation)

## Phase 6 — SQL editor buffer + statement splitting

- Text buffer: cursor position, explicit selection.
- `sqlparser-rs`-based splitting into: statement under cursor,
  statement(s) in selection, whole buffer.
- This is the core UX bet (see `ARCHITECTURE.md`) — gets the heaviest test
  coverage of any MVP0 phase.
- **Test:** unit tests covering dollar-quoting, string literals containing
  `;`, comments, multi-line statements, cursor sitting exactly on a
  statement boundary.

Status: `[x]`

## Phase 7 — Execution wiring

- Run the split statement(s) from Phase 6 against the `DataSource` from
  Phase 3; route results into the Phase 5 grid; surface errors; wire
  cancel.
- Carried over from Phase 5's review: `worker::fetch_page`/`RowStream::finish()`
  are only safe because every Phase 5 caller passes a `LIMIT`-bounded query.
  Once arbitrary editor SQL (no `LIMIT`) reaches this path, calling
  `finish()` after `take(51)` would drain the *entire* remaining result set
  synchronously before returning — freezing the UI on a query that "looked
  instant". Phase 7 must NOT call `finish()`/drain-to-`Ended` when the
  stream wasn't already at/near completion; the existing drop-and-abandon
  path (`RowStream::drop` -> background drain + `cancel_query`) is the
  correct mechanism for an unbounded stream the grid only partially
  consumed, same as Phase 3's `execute_streams_rows_without_buffering_...`
  test already proves for the raw `DataSource::execute` path.
- **Test:** integration test against local Postgres for the full
  run-statement path (cursor statement, selection, whole buffer).

Status: `[x]` — full architect → code-writer → test-writer → code-reviewer
pipeline complete across three review rounds (see conversation); the
finish()/drop() branch, retry_on_busy, and the fixes below are all verified
against real Postgres, including independent re-verification by the review
round itself, not just by the agent that made the fix.

Review round 2 found and fixed: a panic in `jump_to_error_position` on a
concurrent buffer edit (extracted the pure `error_offset`/`ByteSpan::try_slice`,
non-panicking instead of relying on `slice`'s invariant, which a query in
flight across an await point can no longer guarantee); a cancel race where a
fast statement finishing `Ok()` after `request_cancel()` would advance to the
next statement instead of stopping the run (destructive-statement hazard,
Postgres's cancel being unacknowledged and out-of-band) — independently
reproduced by round 3's reviewer against real Postgres (a `DELETE` that would
previously have run was confirmed to no longer execute); and a flaky
regression test (`AbandonStats::last_cancel_delay`, overwritten by every
escalation attempt, ~4/10 spurious failures), replaced with
`first_cancel_delay`/`first_cancels`/`multi_attempt_abandons`, pinning only
the first cancel attempt — the number the original coop-starvation defect
actually corrupted. Also reworded `BusyTimedOut` and corrected two
`src/ui/grid/state.rs` comments that wrongly called the Table-vs-Query
origin-kind staleness check "defensive"/"unreachable" (it's routinely hit:
`DataGridState`'s and `RunState`'s `RequestId` counters are independent and
both start at 0).

Verification: `full_run_path_abandon_and_retry_is_reliable_across_repeated_back_to_back_runs`
run 3x by the fixing pass (30/30 iterations) plus 8 more times independently
by round 3's reviewer (80 more iterations) against real Postgres
(postgres:17, Docker) — 110/110 clean. Observed `first_cancel_delay()` was
~100-101ms (≈ `abandon_grace`) across all measured iterations via a
throwaway (non-retained) probe, with `multi_attempt_abandons()` at 0
throughout — comfortably inside the committed 300ms bound. Round 3 flagged
that the test's own comment initially over-attributed the original 4/10
failures to "legitimate attempt-2 escalation being common for this
scenario" — that explanation isn't supported by any measurement taken
(`multi_attempt_abandons` was 0 in every run checked), so the comment was
softened to describe only what's actually established: mixing attempt-1 and
attempt-2+ delays into one metric made a real regression indistinguishable
from normal operation, which is why only the first attempt is pinned now.

Deferred to Phase 8 as status-bar polish (round 3, non-blocking): a cancel
that lands after the in-flight statement already committed reports
identically ("cancelled at statement N of M") to one that actually
interrupted the statement — worth distinguishing so a user doesn't assume a
completed DELETE was rolled back; a stale `CancelFailed` status from a
previous run's cancel attempt can outlive the keypress-clears-status rule
if it arrives after a new run has already started (low severity, needs a
real `cancel_query` transport failure to trigger); opening a table mid-run
silently discards the run's still-pending result when it lands (behavior is
correct — no wrong data shown — just potentially confusing).

## Phase 8 — Integration & dogfood

- Compose tree/editor/grid into the full layout; keybindings; status/error
  bar. The basic version of all three already shipped in Phase 7 (three-pane
  layout, per-focus keymaps, `Focus::Editor`, status bar with run
  progress/summaries) — what's left here is polish (see Phase 8's other
  bullets), not building this from scratch.
- Secret resolution via `keyring`: Phase 1 built `Connection::keyring_account()`
  and Phase 3's `PostgresDataSource::connect` deliberately takes
  `password: Option<&str>` as an explicit parameter rather than reaching into
  `keyring` itself — nothing in Phases 0-7 has a home for the actual lookup.
  Wire it here, since this is the first phase that needs a real end-to-end
  connect with a password-protected DB.
- **Test:** the actual MVP0 "done when" criterion from `ROADMAP.md` — use
  it instead of rainfrog/gobang/psql against a real personal VPS box.

Status: `[x]` (automated criteria only — real personal-VPS dogfood session
is the actual "done when" criterion for all of MVP0 and is still
outstanding; keyring resolution, --set-password, and the three Phase 7
status-bar carry-overs are implemented and verified, including a security
fix confirmed by an independent reviewer: RATWARREN_PASSWORD no longer
leaks into the spawned ssh tunnel child's environment)
