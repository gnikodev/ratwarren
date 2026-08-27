use std::process::ExitCode;
use std::sync::Arc;

use ratwarren::app::{self, App};
use ratwarren::cli::{self, Invocation};
use ratwarren::config::Config;
use ratwarren::datasource::{DataSource, PostgresDataSource};

#[tokio::main]
async fn main() -> ExitCode {
    match cli::parse_args(std::env::args()) {
        Invocation::Help => {
            println!("{}", cli::USAGE);
            ExitCode::SUCCESS
        }
        Invocation::BadUsage(msg) => {
            eprintln!("{msg}");
            eprintln!("{}", cli::USAGE);
            ExitCode::from(2)
        }
        Invocation::SetPassword { name } => set_password(&name),
        Invocation::Run { name } => run(name).await,
    }
}

async fn run(name: Option<String>) -> ExitCode {
    let config = match Config::load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("failed to load config: {}", ratwarren::ui::error_chain(&e));
            return ExitCode::FAILURE;
        }
    };

    let name = match pick_connection(&config, name) {
        Ok(name) => name,
        Err(code) => return code,
    };
    let conn = config
        .connection(&name)
        .expect("pick_connection only returns names present in config");

    // Must happen before ratatui::init(): a blocking keyring call and any
    // stderr note it prints need the primary screen -- doing this inside the
    // tokio event loop (after the terminal is in alternate-screen/raw mode)
    // would freeze the UI instead of showing the user anything.
    let secret = ratwarren::secret::resolve(conn);
    if let Some(note) = secret.note() {
        eprintln!("note: {note}");
    }
    eprintln!("connecting to {name}…");
    let source = PostgresDataSource::connect(conn, secret.password()).await;
    drop(secret);
    let source = match source {
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

fn set_password(name: &str) -> ExitCode {
    let config = match Config::load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("failed to load config: {}", ratwarren::ui::error_chain(&e));
            return ExitCode::FAILURE;
        }
    };
    let Some(conn) = config.connection(name) else {
        print_available(&config);
        return ExitCode::from(2);
    };
    let Some(account) = conn.keyring_account() else {
        eprintln!(
            "connection {name:?} has no `password` entry in the config; add \
             [connections.password] / source = \"keyring\" first"
        );
        return ExitCode::from(2);
    };

    let password =
        match ratwarren::secret::read_password_from_stdin(&format!("password for {name}: ")) {
            Ok(Some(p)) => p,
            Ok(None) => {
                eprintln!("no password entered, nothing stored");
                return ExitCode::from(1);
            }
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::from(1);
            }
        };

    let service = conn.keyring_service();
    let result = keyring::Entry::new(service, &account).and_then(|e| e.set_password(&password));
    if let Err(e) = result {
        eprintln!("{}", ratwarren::ui::error_chain(&e));
        return ExitCode::from(1);
    }

    println!("stored password for connection {name:?} (service {service:?}, account {account:?})");
    ExitCode::SUCCESS
}

fn pick_connection(config: &Config, name: Option<String>) -> Result<String, ExitCode> {
    if let Some(name) = name {
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
        if let Ok(path) = ratwarren::config::paths::config_file_path() {
            eprintln!("add one at {}, for example:", path.display());
        } else {
            eprintln!("add one to your config file, for example:");
        }
        eprintln!();
        eprintln!("  [[connections]]");
        eprintln!("  name = \"prod\"");
        eprintln!("  host = \"localhost\"");
        eprintln!("  database = \"app\"");
        eprintln!("  user = \"app_user\"");
        return;
    }
    eprintln!("usage: ratwarren <connection-name>");
    eprintln!("available connections:");
    for conn in &config.connections {
        eprintln!("  {}", conn.name);
    }
}
