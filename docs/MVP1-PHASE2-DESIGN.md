# MVP1 Phase 2 — Multi-session architecture + connection tabs (design gate)

Design output for the phase described in [MVP1-PLAN.md](MVP1-PLAN.md) §"Phase 2".
Read that section first; this file resolves it into concrete module/type
boundaries and settles the two risks the plan required settling here.

`DataSource` (`src/datasource/mod.rs`) is **unchanged by this phase**. No new
trait method, no `SessionId` in the trait, no pooling in
`PostgresDataSource`. Multiplicity lives entirely above the trait.

---

## 1. Empirical findings (these drive sections 2 and 6)

All measured on this machine: macOS 25.5.0 / arm64, OpenSSH_10.2p1,
keyring 4.1.6, ratatui 0.30.2 / ratatui-widgets 0.3.2, tokio 1.53.1.

**F1 — the local ephemeral port allocator here is strictly sequential.**
200 000 `bind(127.0.0.1:0)` → read port → close cycles: range 49152–65535,
16 378 distinct values, 99.963 % of consecutive draws are exactly +1, and
**zero** repeats within any sliding window up to 1000 draws. So on macOS two
in-process `reserve_local_port()` calls milliseconds apart cannot collide.
Linux is different: `bind(0)` there picks a randomised offset into
`net.ipv4.ip_local_port_range` (default 32768–60999, 28 232 ports), so a
same-window collision is roughly 1/28 000 per pair rather than 0. Not
verified on Linux — no Linux box available here.

**F2 — OpenSSH binds the `-L` listener strictly after authentication.**
Against a real local `sshd` with `-v`, the log order is
`Authenticated to 127.0.0.1 ...` (line 57) → `debug1: Local forwarding
listening on 127.0.0.1 port 65123.` (line 59). So the window between
spawning `ssh` and it owning the port is a full TCP + KEX + auth round trip:
~37 ms over loopback, and RTT-bound (hundreds of ms to seconds) to a real
VPS.

**F3 — `Tunnel::wait_ready` demonstrably accepts a foreign listener as ours.**
Reproduced end-to-end with the real `ssh` binary and ratwarren's exact argv:
an unrelated process listening on the chosen port, `ssh` pointed at a
never-responding fake sshd, then `wait_ready`'s logic replayed verbatim →
TCP probe succeeded at t=0.000 s, `READY_SETTLE_DELAY` (20 ms) elapsed, child
still alive (still handshaking) → **`wait_ready` returns `Ok(())` while our
`ssh` has bound nothing at all**, and `lsof` confirms the port belongs to the
foreign process. Our `ssh` was still running a second later. This is not a
"20 ms is too short" tuning problem — because of F2 the settle delay would
have to exceed an unbounded network RTT, so the current check is
*structurally* unable to tell our listener from someone else's.

**F4 — `ExitOnForwardFailure=yes` works and its stderr is already matched.**
With the port pre-occupied, `ssh` exited rc=255 in 0.037 s printing
`bind [127.0.0.1]:65132: Address already in use` /
`channel_setup_fwd_listener_tcpip: cannot listen to port: 65132` /
`Could not request local forwarding.` — all at *default* verbosity (no
`debug` prefix), and the first two are matched by the existing
`is_forward_bind_failure()`. Only the *success* line is `debug1:`-prefixed.

**F5 — keyring's Linux prompt never touches our tty.** keyring 4.1.6's
`default = ["v1"]` selects `apple-native-keyring-store` on macOS and
`zbus-secret-service-keyring-store` on other unix (confirmed in the crate's
`Cargo.toml` target table and `src/v1.rs::set_credential_store`). The Linux
path is a pure D-Bus client of `org.freedesktop.secrets`; unlocking a locked
collection returns an `org.freedesktop.Secret.Prompt` object that the
*service* performs in its own GUI prompter (gcr-prompter / kwalletd). On a
headless box with no session bus or no display that call **errors** — the
documented headless failure mode — it does not fall back to a terminal
prompt. macOS is a `securityd` GUI dialog; Windows is Credential Manager.
See §6 for the one unverified residual.

**F6 — pinned crate APIs (verified against vendored sources, not memory).**
- `ratatui::widgets::Tabs` exists (re-exported from `ratatui-widgets` 0.3.2).
  `Tabs::new(impl IntoIterator<Item: Into<Line>>)`, `.select(impl Into<Option<usize>>)`,
  `.block()`, `.style()`, `.highlight_style()`, `.divider()`, `.padding()`.
  It implements plain `Widget` (and `Widget for &Tabs`), **not**
  `StatefulWidget` — use `frame.render_widget`. It does not scroll or elide
  when the titles overflow the area; long tab strips are simply clipped.
- `ratatui::widgets::{Clear, List, ListItem, ListState, Paragraph, Wrap}` all
  present.
- `tokio::sync::Mutex::const_new` is `pub const fn` in tokio 1.53.1 — a
  `static` mutex needs no `OnceLock`.

---

## 2. Tunnel bind race — resolution

The plan asked for the mutex to be confirmed, not assumed. Result: **the
mutex is necessary and it is sufficient for the hazard this phase
introduces, but F3 shows `wait_ready` has a second, pre-existing hole the
mutex does not close.** Two changes, T1 required, T2 recommended.

### T1 (required) — serialize tunnel opens process-wide

```rust
// src/tunnel/mod.rs
/// Serializes `ssh -L` spawns process-wide. Rationale: `reserve_local_port`
/// frees its port before ssh binds it, and per F2 ssh only binds after
/// authenticating, so with two opens in flight both can legitimately be
/// handed the same free port and the loser then observes the winner's
/// listener (F3) and reports ready against the wrong VPS. Holding this
/// across a whole open means the previous tunnel's listener is already live
/// when the next `bind(0)` runs, and `bind(0)` never returns a port with a
/// live listener — which reduces N concurrent tunnels to MVP0's
/// already-handled single-tunnel case.
pub static OPEN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
```

Acquired in `PostgresDataSource::connect_with`, held **only** around the
`spawn_blocking(Tunnel::open_with)` call and released before
`cfg.connect(...)`:

```rust
let (host, port, tunnel) = match spec {
    Some(spec) => {
        let _open_guard = crate::tunnel::OPEN_LOCK.lock().await;
        let join = tokio::task::spawn_blocking(move || Tunnel::open_with(&spec, &tunnel_options));
        // ... unchanged ...
    }
    None => (conn.host.clone(), conn.port, None),
};
```

Notes for the implementer:
- It must be a `tokio::sync::Mutex`, not `std::sync::Mutex`: the guard is
  held across an `.await` that can last seconds.
- Connections with no `tunnel` never take the lock, so local/no-tunnel
  sessions never queue.
- Only one lock exists and it is acquired and released inside one function,
  so there is no ordering/deadlock question. The guard is a local, so every
  early return (`TunnelTaskPanicked`, `Tunnel`) releases it.
- Worst-case latency: a queued open waits up to `ready_timeout` (15 s) per
  tunnel ahead of it. This never blocks the event loop (§4 runs opens on
  spawned tasks) and the tab shows a `Connecting` state throughout. Do not
  "fix" it by shortening the hold.

### T2 (recommended) — confirm *our* ssh owns the port

F3 is a demonstrated wrong-target failure, not a timing-precision one, and
T1 does not close it against a **second ratwarren process** (a second
terminal connecting to a different box), which is an ordinary thing to do.
Post-T1 probability is low — per F1 effectively zero on macOS, ~1/28 000 per
concurrent cross-process pair on Linux — but the outcome is "silently
reading and writing a different VPS's database", which is exactly what makes
the Phase 5 destructive gate matter.

Fix: use the authoritative in-process signal ssh already emits (F2), rather
than inferring ownership from a third party's TCP accept.

1. `src/tunnel/command.rs` — add `-v` to the argv. Update the module comment:
   it currently claims `-o ExitOnForwardFailure` and `-o BatchMode` are the
   only forced flags. Update `exactly_two_dash_o_flags_and_no_others` and the
   four exact-argv tests.
2. `src/tunnel/mod.rs` — `Tunnel` gains `forward_confirmed: Arc<AtomicBool>`.
   The existing stderr reader thread, for each line, first checks for the
   confirmation before deciding whether to capture it:

   ```rust
   // Exact wording verified against OpenSSH_10.2p1:
   //   "debug1: Local forwarding listening on 127.0.0.1 port 65123."
   fn is_forward_listening(line: &str, local_port: u16) -> bool {
       let lower = line.to_lowercase();
       lower.contains("local forwarding listening on")
           && lower.contains(&format!("port {local_port}"))
   }
   ```
3. `StderrCapture::push_line` must **skip lines whose trimmed text starts
   with `debug`** (`debug1:`/`debug2:`/`debug3:`). Per F4 every diagnostic
   the existing error paths depend on — the bind failure lines,
   `Permission denied`, `Could not request local forwarding` — is printed
   without a debug prefix, so this preserves `is_forward_bind_failure()` and
   the user-facing `stderr_suffix` text while keeping `-v` noise out of the
   8 KB head-retained budget.
4. `TunnelOptions` gains `forward_confirm_grace: Duration` (default 2 s).
   `wait_ready`'s probe-success branch becomes:
   - flag set → `Ok(())` (confirmed).
   - flag unset → record the first-probe-success instant and **keep
     looping** rather than accepting. Per F2 the flag is written by ssh
     immediately after its `listen()` succeeds, so in normal operation the
     flag is already set the first time the probe can succeed; 2 s is
     enormously generous.
   - flag still unset `forward_confirm_grace` after the first probe success,
     child alive → accept, but leave `forward_confirmed == false`.
5. `Tunnel::forward_confirmed() -> bool`, mirrored by
   `PostgresDataSource::tunnel_forward_confirmed() -> Option<bool>`. A
   session whose tunnel is unconfirmed shows a sticky `⚠` in its tab title
   and a warning in its status line: *"tunnel readiness unconfirmed — could
   not verify this ssh owns port N; it may belong to another process"*.

This deliberately fails **open + loud**, not closed: an OpenSSH whose wording
changes must not brick the tool, and because the warning is invisible in
normal operation, its appearance is itself the signal that the check broke.
Flipping it to fail-closed is a one-line change if the author prefers that.

If the author declines T2, T1 alone still ships — but record in
`MVP1-PLAN.md` that F3 is a **known, reproduced** wrong-target hole against
other processes, not an accepted imprecision.

### T3 (optional, partial) — post-connect target assertion

One extra round trip at the end of `connect_with`:
`SELECT current_database(), inet_server_addr()::text, inet_server_port()`,
failing the connect if `current_database() != conn.database`. Cheap and
inside `PostgresDataSource` (no trait change). **Be explicit that it is a
partial net:** it catches nothing when two boxes run the same-named database,
which is the likely shape of "several personal VPS boxes running the same
app". Redundant if T2 lands. Include only if T2 is declined.

### Manual verification still required

The plan's manual test stands, with one correction: `current_database()` is
not a sufficient discriminator if the boxes share a dbname. Open 3+ tunnelled
sessions to different real boxes back to back and verify each against
something genuinely box-unique — `SELECT current_setting('cluster_name')`, a
seeded marker table, or `inet_server_addr()` where the boxes actually differ.
Repeat 5–10 times. Also verify T2's confirmation line still appears through a
**`ProxyJump`** chain (the outer `ssh` owns the `-L` listener, so it should,
but this is untested here).

---

## 3. Secret resolution with the TUI running — resolution

Per F5: **do not suspend the terminal.** No backend on any supported
platform draws a prompt on our tty. Instead fix the two things that *are*
certain problems.

### S1 — `secret::resolve` must not occupy the blocking pool

`resolve()` blocks the calling thread up to `KEYRING_GIVE_UP_AFTER` (60 s).
Wrapping it in `spawn_blocking` would reintroduce exactly the quit-hang the
existing comment in `keyring_lookup` warns about, since `#[tokio::main]`'s
runtime drop waits on the blocking pool. Add an async sibling:

```rust
// src/secret.rs
pub async fn resolve_async(conn: &crate::config::Connection, notes: &NoteSink) -> Resolved;

pub(crate) async fn resolve_with_async<F>(
    conn: &crate::config::Connection,
    env_password: Option<String>,
    lookup: F,
    notes: &NoteSink,
) -> Resolved
where
    F: FnOnce(&str, &str) -> Result<String, String> + Send + 'static;
```

Implementation: keep the existing **detached `std::thread`** — its
justification gets stronger here, not weaker — but replace
`std::sync::mpsc` + `recv_timeout` with `tokio::sync::oneshot` +
`tokio::time::timeout`. Nothing occupies a blocking-pool thread; quit is
never delayed by a hung keyring. Reuse the pure `resolve_with` decision logic
so its existing unit tests keep covering it.

### S2 — the `eprintln!` slow-path notice must go

`keyring_lookup`'s 3-second `eprintln!("note: still waiting for the OS
keyring …")` would scribble directly onto the alternate screen. Replace it
with a note routed to the session's `Connecting` label (see `NoteSink` /
`OpenEvent::Progress` in §4). Same for `Resolved::note()`, which `main.rs`
currently prints to stderr.

### S3 — remove the pre-`ratatui::init()` special case

`main.rs`'s "resolve the secret before `ratatui::init()`" block goes away
entirely. The first session opens through the **same** `spawn_open` path as
every session opened later from the picker — one code path, no special case.
`--set-password` and the usage/`print_available` output stay on the pre-TUI
path, unchanged.

After this, blocking `resolve()` / `keyring_lookup` have no production
caller. Delete them (or `#[cfg(test)]`-gate) rather than leaving dead code.

---

## 4. Module and type layout

### New file: `src/app/session.rs` (`app::session`)

```rust
// ---- identity ----------------------------------------------------------
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(pub u64);

// ---- the extracted connect-and-spawn wiring ----------------------------
pub struct SourceHandle {
    source: Arc<PostgresDataSource>,        // concrete: close()/tunnel_local_port() are inherent
    requests: UnboundedSender<WorkerRequest>,
    cancels: UnboundedSender<CancelRequest>,
    worker: JoinHandle<()>,
    canceller: JoinHandle<()>,
}

impl SourceHandle {
    /// Wires an already-connected source into a worker + canceller pair.
    /// Deliberately split from the connect step so Phase 7's monitor handle —
    /// which dials the session's existing tunnel rather than opening a new
    /// one — reuses exactly this half without a second `ssh -L`.
    pub fn attach(
        source: PostgresDataSource,
        session: SessionId,
        responses: UnboundedSender<SessionResponse>,
    ) -> SourceHandle;

    pub fn send(&self, req: WorkerRequest);       // ignores send errors, as App does today
    pub fn cancel(&self, req: CancelRequest);
    pub fn source(&self) -> &Arc<PostgresDataSource>;
    pub fn tunnel_local_port(&self) -> Option<u16>;   // Phase 7
    pub fn tunnel_forward_confirmed(&self) -> Option<bool>;  // T2

    /// Verbatim port of main.rs's teardown block (lines 77–103), comments
    /// preserved: drop the senders, abort+await both tasks so their
    /// `Arc<dyn DataSource>` clones drop, then `Arc::into_inner` + `close()`.
    pub async fn shutdown(self);
}

// ---- per-session state -------------------------------------------------
pub enum SessionState {
    Connecting { message: String },     // e.g. "reading the OS keyring…"
    Ready(SourceHandle),
    Failed { message: String },         // already run through ui::error_chain
}

pub struct Session {
    pub id: SessionId,
    pub connection_name: String,
    pub group: Option<String>,
    tree: ObjectTreeState,
    grid: DataGridState,
    editor: EditorState,
    run: RunState,
    status: Option<Status>,
    focus: Focus,
    state: SessionState,
}

pub enum SessionAction { Quit }   // the only thing on_key still bubbles up in Phase 2

impl Session {
    pub fn new(id: SessionId, connection_name: String, group: Option<String>) -> Session;
    pub fn on_connected(&mut self, handle: SourceHandle);   // sets Ready, then sends the root tree refresh
    pub fn on_failed(&mut self, message: String);
    pub fn is_ready(&self) -> bool;
    fn send(&self, req: WorkerRequest);                      // no-op unless Ready
    pub fn apply(&mut self, r: WorkerResponse);
    pub fn on_key(&mut self, key: KeyEvent) -> Option<SessionAction>;
    pub fn start_run(&mut self, target: RunTarget);
    pub fn render(&mut self, frame: &mut Frame, area: Rect);
    pub fn tab_title(&self) -> Line<'_>;
}

// ---- opening a session without blocking the event loop -----------------
pub enum OpenEvent {
    Progress { session: SessionId, message: String },
    Done { session: SessionId, result: Result<SourceHandle, String> },
}

pub fn spawn_open(
    conn: crate::config::Connection,          // owned clone — no &Config lifetime into a task
    session: SessionId,
    responses: UnboundedSender<SessionResponse>,
    events: UnboundedSender<OpenEvent>,
) -> JoinHandle<()>;
```

`spawn_open`'s body: `Progress("reading the OS keyring…")` →
`secret::resolve_async` → `Progress("connecting to {name}…")` →
`PostgresDataSource::connect_with(&conn, secret.password(), &ConnectOptions::default())`
→ `drop(secret)` → `SourceHandle::attach(...)` → `Done`. On error,
`Done { result: Err(ui::error_chain(&e)) }`. `Resolved::note()` and the
keyring slow-path notice become `Progress` messages.

Everything in `Session` (`apply`, `on_key`, `start_run`,
`apply_query_response`, `jump_to_error_position`, `activate`, the render
body) is today's `App` code moved as-is, with `self.requests.send(x)`
becoming `self.send(x)`. The free functions `error_offset`, `title_of`,
`running_status`, `summary_status`, `pane_border_style` move with it; their
existing unit tests move with them.

### Changed: `src/app/message.rs`

```rust
pub struct SessionResponse {
    pub session: SessionId,
    pub response: WorkerResponse,
}
```

### Changed: `src/app/worker.rs`

`spawn` and `spawn_canceller` each take a `session: SessionId` and a
`UnboundedSender<SessionResponse>`, and wrap every send in
`SessionResponse { session, response }`. That is the whole diff — no extra
forwarder task, no extra hop, and the tag is unforgeable by construction
because a worker can only stamp its own id. `handle`/`handle_tree`/
`handle_grid`/`handle_query`/`fetch_page`/`retry_on_busy` are untouched
except for `handle_query`'s intermediate `Started` send, which also needs the
wrapper.

### New: `src/ui/picker/{mod.rs,state.rs,widget.rs}`

Follows the existing `ui::tree` / `ui::grid` / `ui::editor` split.

```rust
// state.rs
pub enum PickerRow {
    GroupHeader { label: Option<String> },   // not selectable
    Connection { name: String },
}

pub struct PickerState { rows: Vec<PickerRow>, selected: usize, list: ListState }

pub enum PickerCommand { MoveUp, MoveDown, First, Last }

impl PickerState {
    /// Flattens `Config::grouped()` into owned rows. Owned, not borrowed:
    /// `ConnectionGroup<'a>` borrows the Config and cannot be held across
    /// frames without a self-referential struct.
    pub fn from_config(config: &Config) -> PickerState;
    pub fn command(&mut self, cmd: PickerCommand);
    pub fn selected_connection(&self) -> Option<&str>;
    pub fn is_empty(&self) -> bool;
}
```

`from_config` lands the initial selection on the first `Connection` row.
`command` skips `GroupHeader` rows in both directions and must not move off
either end. A config with zero connections yields `rows` containing no
`Connection` and `selected_connection() == None`.

`widget.rs`: a `PickerWidget` rendering a `List` of rows (headers styled dim
+ non-highlightable, connections plain) inside a `Block::bordered()`,
preceded by `Clear` over its area. The overlay `Rect` is centred at ~60 % ×
60 % of `frame.area()`, floored at 40 × 10 and clamped to the frame — must
not panic on a tiny terminal.

No filter/search box in Phase 2. Phase 4's history overlay introduces that
widget shape; the picker can adopt it in Phase 8.

### Changed: `src/app/mod.rs`

```rust
pub struct App {
    sessions: Vec<Session>,
    active: usize,                  // invariant: sessions.is_empty() || active < sessions.len()
    next_session_id: u64,
    config: Config,                 // owned; the picker reads grouped() from it on open
    picker: Option<PickerState>,
    responses_tx: UnboundedSender<SessionResponse>,  // kept so `recv()` never yields None
    responses: UnboundedReceiver<SessionResponse>,
    open_tx: UnboundedSender<OpenEvent>,             // ditto
    opens: UnboundedReceiver<OpenEvent>,
    should_quit: bool,
}

impl App {
    pub fn new(config: Config) -> App;
    pub fn open_connection(&mut self, name: &str);   // mint id, push Connecting session, spawn_open
    pub fn apply(&mut self, msg: SessionResponse);
    pub fn apply_open_event(&mut self, ev: OpenEvent);
    pub fn on_key(&mut self, key: KeyEvent);
    pub fn render(&mut self, frame: &mut Frame);
    pub fn should_quit(&self) -> bool;
    pub async fn shutdown(self);      // awaits SourceHandle::shutdown for every Ready session
    fn active_mut(&mut self) -> Option<&mut Session>;
    fn close_active(&mut self);
    fn next_tab(&mut self); fn prev_tab(&mut self);
}
```

### Changed: `src/main.rs`

Shrinks to: parse args → `Config::load()` → resolve the starting connection
name (or none) → `ratatui::init()` → `App::new(config)` (+ `open_connection`
if a name was given, else the picker opens) → `app::run` → `ratatui::restore()`
→ `app.shutdown().await` → exit code. All the connect/spawn/teardown wiring
moves into `SourceHandle`. `set_password` and `print_available` are unchanged.

---

## 5. SessionId routing

**One merged response channel**, not per-session receivers selected over —
`tokio::select!` cannot select over a `Vec` of receivers without
`select_all`/`StreamMap`, and the merged channel is what makes the routing
invariant checkable in one place.

The invariant, to be stated as a `why` comment on `App::apply` and asserted
by tests:

> `RequestId` is scoped to *(SessionId, state machine)*. `RunState`'s and
> `DataGridState`'s counters are independent and both start at 0, so
> `RequestId(0)` in session A and `RequestId(0)` in session B are unrelated
> values. Routing is therefore by `SessionId` **first and unconditionally**;
> a `RequestId` may only be compared after a session has been located.
> `App::apply` must **never** fall back to `active_mut()` when the lookup
> fails — a response for a closed or unknown session is silently dropped.

```rust
pub fn apply(&mut self, msg: SessionResponse) {
    let Some(session) = self.sessions.iter_mut().find(|s| s.id == msg.session) else {
        return;   // closed tab; must NOT fall through to the active session
    };
    session.apply(msg.response);
}
```

`CancelRequest` travels the other direction on a per-session channel owned by
`SourceHandle`, so it needs no tag.

### `app::run` — third select arm

```rust
tokio::select! {
    event = events.next() => ...,                        // unchanged
    msg = app.responses.recv() => match msg {
        Some(m) => app.apply(m),
        None => return Err(io::Error::other("response channel closed unexpectedly")),
    },
    ev = app.opens.recv() => match ev {
        Some(e) => app.apply_open_event(e),
        None => return Err(io::Error::other("open channel closed unexpectedly")),
    },
}
```

Because `App` holds a sender clone of both channels, both `None` branches are
unreachable; keep them as defensive asserts. Note this **replaces** today's
`None => Err("datasource worker stopped")` semantics, which is correct now:
with N sessions, one session's worker ending must not kill the app.
`app.start()` is gone — the root tree refresh moves into
`Session::on_connected`.

---

## 6. Keymap (minimal; Phase 8 owns the full reconciliation)

New global bindings, added to `map_key`'s pre-focus block. All four are
currently unbound in every `Focus`, including `Focus::Editor` where bare
printable characters insert:

| key | `AppCommand` |
|---|---|
| `Ctrl+T` | `OpenPicker` |
| `Ctrl+W` | `CloseTab` |
| `Ctrl+N` | `NextTab` |
| `Ctrl+P` | `PrevTab` |

`q` keeps meaning **quit the app**, not close-tab. Do not repurpose it: that
is a Phase 8 decision and it becomes a data-loss hazard as soon as Phase 3
adds dirty pages. `Ctrl+C` keeps its cancel-or-quit meaning, scoped to the
active session's run.

Modal handling, deliberately the minimum viable version of Phase 8's
"overlay layer above `Focus`":

```rust
pub fn on_key(&mut self, key: KeyEvent) {
    if self.picker.is_some() {
        self.picker_key(key);     // ↑/↓/j/k, Enter = open, Esc/Ctrl+T = close
        return;                   // map_key is NOT consulted while the picker is open
    }
    ...
}
```

`Session::on_key` returns `Option<SessionAction>`; only `SessionAction::Quit`
bubbles to `App`.

---

## 7. Tab lifecycle semantics

**Open** — mint `SessionId`, push a `Connecting` session, `spawn_open`,
switch `active` to it.

**Close (`close_active`)** — remove the session from the vec, then by state:
- `Ready(handle)` → `tokio::spawn(async move { handle.shutdown().await })`.
  Detached, because `on_key` is sync. Safe at process exit: dropping the task
  at runtime shutdown drops the `SourceHandle` → `PostgresDataSource` →
  `Option<Mutex<Tunnel>>` → `Tunnel::drop` → `terminate()` reaps the ssh
  child. No orphaned `ssh`.
- `Connecting` → the in-flight open task will still deliver an `OpenEvent`
  for a session id that no longer exists. `apply_open_event` must handle
  `Done` for an unknown session by **spawning `handle.shutdown()`** on the
  handle it just received, not by dropping it: a plain drop skips
  `close().await`'s `conn_task.abort()`.
- `Failed` → nothing to tear down.

Then `if self.active >= self.sessions.len() { self.active = self.sessions.len().saturating_sub(1) }`.

**Closing the last tab** leaves `sessions.is_empty()` and **auto-opens the
picker** rather than quitting. With zero sessions there is no `Focus` to feed
`map_key`, so `on_key` in that state handles only picker keys plus `q` /
`Ctrl+C` → quit. Spec this as an explicit state and test it.

**Switch** — `next_tab`/`prev_tab` wrap; both are no-ops when `sessions` is
empty.

---

## 8. Rendering

`Layout::vertical([Length(1), Min(0), Length(1)])` → tab bar / body / footer.

- **Tab bar**: `Tabs::new(titles).select(Some(active)).highlight_style(REVERSED)`,
  `frame.render_widget` (plain `Widget`, per F6). Titles are `Line`s carrying
  per-tab style: `Ready` → `" name "`, `Connecting` → `" … name "`,
  `Failed` → `" ! name "` in red, plus a `⚠` marker when T2 reports an
  unconfirmed tunnel. Overflow is clipped by the widget; acceptable at MVP1
  tab counts.
- **Body**, by active session state:
  - `Ready` → today's `App::render` body, moved to `Session::render`.
  - `Connecting { message }` → centred `Paragraph` in a bordered block.
  - `Failed { message }` → red wrapped `Paragraph` + "Ctrl+W to close". No
    retry key in Phase 2.
  - no sessions → the picker fills the body.
- **Footer**: today's per-`Focus` footer for the active session, plus a
  short tab hint. The full help overlay is Phase 8's job.
- **Picker overlay**: `Clear` then the bordered `List` over the centred rect.

---

## 9. Tests

Beyond the plan's list, add the ones the findings above make necessary.

**Routing (the core safety property)**
1. Two sessions whose `RunState` *and* `DataGridState` counters have both
   independently reached the same `RequestId`; a `SessionResponse` tagged for
   B must leave A's `run`, `grid`, `editor` and `status` **byte-identical** —
   assert A is untouched, not merely that B changed.
2. A response for an unknown/closed `SessionId` mutates nothing and does not
   fall through to the active session.

**Tabs**
3. Open / switch / close: closing a middle tab, closing the tab at the last
   index, closing the only tab (→ picker opens, `sessions` empty), `q` from
   the empty state quits.
4. Closing a `Connecting` tab, then delivering its late `OpenEvent::Done`,
   must not resurrect a session.

**Picker**
5. `PickerState`: headers never selectable; `MoveUp`/`MoveDown` skip headers
   and stop at the ends; empty config; single-connection config;
   `selected_connection()` correct after every navigation.

**Keymap**
6. `Ctrl+T`/`Ctrl+W`/`Ctrl+N`/`Ctrl+P` map to the tab commands in **all
   three** `Focus` values — in particular they must not insert a character in
   `Focus::Editor`.
7. With the picker open, a key that would otherwise be
   `AppCommand::Editor(Insert(_))` never reaches the session.

**Tunnel (T1)**
8. Automatable with the existing stub-`ssh` seam: `TunnelOptions.ssh_program`
   is public and reachable via `ConnectOptions`, so point two concurrent
   `connect_with` calls at a stub script that appends `start $$` / `end $$`
   with timestamps, and assert the two intervals never overlap.

**Tunnel (T2)**
9. Stub script that prints the exact confirmation line → `forward_confirmed()`
   is true. Stub that binds the port but never prints it → open still
   succeeds after `forward_confirm_grace` with `forward_confirmed() == false`.
   Stub that prints a bind failure → still classified by
   `is_forward_bind_failure` with `debug`-prefixed lines filtered out of the
   capture.

**Secret**
10. `resolve_with_async` covers the same matrix as the existing
    `resolve_with` tests, plus: a lookup that never returns must not prevent
    the future from being dropped, and must emit the slow-path note as a
    `Progress` message rather than to stderr.

**Integration** (`tests/postgres.rs`)
11. Two sessions against local Postgres, a statement run on each with
    interleaved responses, each session's grid holding its own result.

**Manual** — see §2. The plan's `current_database()` criterion needs
strengthening to something box-unique.

---

## 10. Out of scope for Phase 2

- **A second `SourceHandle` per session** (activity monitor / preview). Phase
  2 only makes it *possible* by splitting `SourceHandle::attach` from the
  connect step. Phase 7 builds it, reusing the session's existing tunnel.
- **Ad-hoc result pagination for editor queries** — unblocked by this phase
  but explicitly not on MVP1's list (MVP1-PLAN §"Out of scope"). Do not let
  it drift in.
- **A connection pool inside `PostgresDataSource`.** N independent
  connections above the trait is not a pool; the one-permit semaphore keeps
  its exact current meaning.
- **Any `DataSource` trait change** — no `SessionId` parameter, no `preview`,
  no `session_id()` accessor, nothing "so the plugin system will fit later".
- **The full modal/overlay keymap layer, per-tab status line, help overlay,
  startup-opens-the-picker behavior** — Phase 8. Phase 2 ships only the
  minimum `picker.is_some()` short-circuit.
- **Saved pages, dirty tracking, page tabs** — Phase 3. `Session` owns one
  `EditorState` in Phase 2, exactly as `App` does today.
- **Connection picker search/filter, `Alt+1..9` tab jumps, reconnect/retry on
  a `Failed` tab, drag/reorder tabs, per-tab config editing.**
- **Nested connection groups** — Phase 1 shipped a flat label; the picker
  renders one header level and nothing more.

---

## 11. Process note

Per `CLAUDE.md`'s named triggers, this phase touches **both** the SSH tunnel
spawn path (T1/T2) and the ratatui redraw/event loop (third `select!` arm,
tab bar, overlay). **perf-analyzer must be invoked for this phase**, not
skipped — it is not part of the default per-phase loop but these are exactly
its documented triggers.
