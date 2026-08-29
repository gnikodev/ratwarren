use std::process::ExitCode;

use ratwarren::app::{self, App};
use ratwarren::cli::{self, Invocation};
use ratwarren::config::Config;

fn main() -> ExitCode {
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
        Invocation::Run { name } => {
            // Deliberately not `#[tokio::main]`: since Phase 2, connecting a
            // session (`PostgresDataSource::connect_with`) runs
            // `Tunnel::open_with` on `spawn_blocking` *from inside the running
            // event loop*, not before it. `#[tokio::main]`'s expansion drops
            // the `Runtime` at the end of `main`, and `Runtime`'s `Drop` waits
            // for the blocking pool to drain -- it cannot cancel an in-flight
            // `spawn_blocking` closure. So quitting while a tunnel open is
            // still stuck (e.g. an unreachable host) would hang the process
            // for up to `ready_timeout` with the terminal already restored,
            // and `Ctrl+C` can't help once raw mode has turned it into a
            // swallowed key event. Building the runtime by hand and calling
            // `shutdown_background()` instead of letting it drop returns
            // immediately without waiting for that blocking task -- verified
            // with a stub `ssh` that never binds: quitting mid-connect exits
            // the process in well under a second instead of hanging for
            // `ready_timeout`.
            //
            // `shutdown_background()` returning immediately means it does NOT
            // wait for that blocking task either -- it abandons it, so a
            // `Tunnel` value still live on that task's stack never reaches
            // `Drop`/`terminate()` (the only thing that kills the `ssh`
            // child) through the ownership chain alone. `kill_all_registered()`
            // below is the unconditional safety net for exactly that case --
            // see `tunnel::LIVE_CHILDREN`'s doc comment.
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => {
                    eprintln!("failed to start the async runtime: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let code = runtime.block_on(run(name));
            runtime.shutdown_background();
            ratwarren::tunnel::kill_all_registered();
            code
        }
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

    // A resolvable starting connection name is optional here (unlike MVP0):
    // with none, `App` starts with zero sessions and the picker auto-opens.
    // Every session -- including this first one -- opens through the same
    // `spawn_open` path, so there is no special pre-`ratatui::init()`
    // secret-resolution step anymore.
    let name = match resolve_starting_connection(&config, name) {
        Ok(name) => name,
        Err(code) => return code,
    };

    let mut app = App::new(config);
    if let Some(name) = &name {
        app.open_connection(name);
    }

    let mut terminal = ratatui::init();
    let result = app::run(&mut terminal, &mut app).await;
    ratatui::restore();

    app.shutdown().await;

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

/// `Ok(None)` means "no starting connection resolved" -- `App` then starts
/// with zero sessions and the picker auto-opens, rather than this being an
/// error. An explicit but unknown `name`, unlike the no-name case, is still
/// a hard error (unchanged from MVP0's `pick_connection`): the user asked
/// for a specific connection that doesn't exist, which is a typo to report,
/// not a reason to silently fall back to the picker.
fn resolve_starting_connection(
    config: &Config,
    name: Option<String>,
) -> Result<Option<String>, ExitCode> {
    if let Some(name) = name {
        if config.connection(&name).is_some() {
            return Ok(Some(name));
        }
        print_available(config);
        return Err(ExitCode::from(2));
    }
    if config.connections.len() == 1 {
        return Ok(Some(config.connections[0].name.clone()));
    }
    Ok(None)
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
