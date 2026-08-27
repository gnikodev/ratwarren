use std::process::ExitCode;
use std::sync::Arc;

use ratwarren::app::{self, App};
use ratwarren::config::Config;
use ratwarren::datasource::{DataSource, PostgresDataSource};

#[tokio::main]
async fn main() -> ExitCode {
    let config = match Config::load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("failed to load config: {}", ratwarren::ui::error_chain(&e));
            return ExitCode::FAILURE;
        }
    };

    let name = match pick_connection(&config) {
        Ok(name) => name,
        Err(code) => return code,
    };
    let conn = config
        .connection(&name)
        .expect("pick_connection only returns names present in config");

    let password = std::env::var("RATWARREN_PASSWORD").ok();
    if conn.password.is_some() && password.is_none() {
        eprintln!(
            "note: connection {name:?} has a keyring password configured, but keyring-based \
             secret resolution isn't wired up until Phase 8 — set RATWARREN_PASSWORD in the \
             environment to supply the password for now."
        );
    }

    eprintln!("connecting to {name}…");
    let source = match PostgresDataSource::connect(conn, password.as_deref()).await {
        Ok(source) => source,
        Err(e) => {
            eprintln!("{}", ratwarren::ui::error_chain(&e));
            return ExitCode::FAILURE;
        }
    };

    let pg = Arc::new(source);
    let worker_source: Arc<dyn DataSource> = pg.clone();
    let canceller_source: Arc<dyn DataSource> = pg.clone();

    let (request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
    let (response_tx, response_rx) = tokio::sync::mpsc::unbounded_channel();
    let (cancel_tx, cancel_rx) = tokio::sync::mpsc::unbounded_channel();
    let worker_handle = app::worker::spawn(worker_source, request_rx, response_tx.clone());
    let canceller_handle = app::worker::spawn_canceller(canceller_source, cancel_rx, response_tx);

    let mut terminal = ratatui::init();
    let mut app = App::new(name, request_tx, response_rx, cancel_tx);
    let result = app::run(&mut terminal, &mut app).await;
    ratatui::restore();

    // `app` owns the last clone of `request_tx`/`cancel_tx` outside the
    // worker/canceller tasks themselves; drop it explicitly so their
    // `recv().await` loops observe the channel close.
    drop(app);

    // The worker only notices the channel closing *after* its current
    // `handle(...).await` call returns, and nothing times out a request
    // against an unresponsive connection — so a plain `.await` here can hang
    // forever with the terminal already restored. Abort it instead: for a
    // quit path in a single-user tool, not waiting for an in-flight
    // DataSource call to finish is the right tradeoff, since nothing
    // consumes its response after quit anyway. `worker_handle.await`
    // resolves promptly once the task notices the abort at its next await
    // point, which drops its `Arc<dyn DataSource>` clone — a precondition
    // for `Arc::into_inner` below to succeed. The canceller task holds its
    // own separate `Arc<dyn DataSource>` clone and must be aborted/awaited
    // the same way before that precondition holds.
    worker_handle.abort();
    let _ = worker_handle.await;
    canceller_handle.abort();
    let _ = canceller_handle.await;

    // `None` only if something above still holds a clone; the tunnel's own
    // Drop impl reaps the ssh child regardless, so that case is safe to skip.
    if let Some(pg) = Arc::into_inner(pg) {
        pg.close().await;
    }

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn pick_connection(config: &Config) -> Result<String, ExitCode> {
    if let Some(name) = std::env::args().nth(1) {
        if config.connection(&name).is_some() {
            return Ok(name);
        }
        print_available(config);
        return Err(ExitCode::from(2));
    }
    if config.connections.len() == 1 {
        return Ok(config.connections[0].name.clone());
    }
    print_available(config);
    Err(ExitCode::from(2))
}

fn print_available(config: &Config) {
    if config.connections.is_empty() {
        eprintln!("no connections configured");
        return;
    }
    eprintln!("usage: ratwarren <connection-name>");
    eprintln!("available connections:");
    for conn in &config.connections {
        eprintln!("  {}", conn.name);
    }
}
