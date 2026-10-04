use std::fs::{self, OpenOptions};
use std::os::fd::AsRawFd;
use std::process::ExitCode;
use std::sync::Arc;

use slopwatch_daemon::auth::GhToken;
use slopwatch_daemon::clis::Clis;
use slopwatch_daemon::drafts::{Drafts, PipelineSource, StepNeeds};
use slopwatch_daemon::github::GitHub;
use slopwatch_daemon::github::api::Api;
use slopwatch_daemon::notifications::{self, LaunchPace, OpenApp};
use slopwatch_daemon::plugins::{self, Plugins};
use slopwatch_daemon::secrets::SecurityCli;
use slopwatch_daemon::shell_env;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::transport::unix::Listener;
use slopwatch_daemon::{Daemon, DataDir, Library, Retention, Runs, RunsConfig, Watching};
use slopwatch_protocol::Flavor;

/// A log past this size starts over when the daemon starts.
const LOG_LIMIT_BYTES: u64 = 8 * 1024 * 1024;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Built-in Plugins run from this executable (the `plugins` module).
    if args.first().map(String::as_str) == Some("plugin") {
        return plugins::main(&args[1..]);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("start the tokio runtime")
        .block_on(serve())
}

async fn serve() -> ExitCode {
    let path = Flavor::CURRENT.data_dir();
    let data_dir = match DataDir::lock(&path) {
        Ok(data_dir) => data_dir,
        Err(error) => {
            eprintln!("slopwatchd: can't take {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    log_to_data_dir(&data_dir);

    let listener = match Listener::bind(&data_dir) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!(
                "slopwatchd: can't listen on {}: {error}",
                data_dir.socket_path().display()
            );
            return ExitCode::FAILURE;
        }
    };
    let db = data_dir.path().join("state.db");
    let store = match Store::open(&db) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("slopwatchd: can't open {}: {error}", db.display());
            return ExitCode::FAILURE;
        }
    };
    // launchd's PATH lacks what the developer's shell finds, so Steps get
    // the login shell's merged in. Asking it takes a moment, and the
    // first Step can't start without it.
    let login_path = tokio::task::spawn_blocking(shell_env::login_shell_path)
        .await
        .unwrap_or_default();
    // Third-party Plugins are found before any Run loads its Pipeline,
    // and their `describe` gets the PATH Steps get. The CLIs the daemon
    // runs itself, `gh` and `git`, are looked up on it too.
    let step_path = shell_env::merge(std::env::var("PATH").ok().as_deref(), login_path.as_deref());
    let clis = match store.cli_settings() {
        Ok(settings) => Arc::new(Clis::new(settings)),
        Err(error) => {
            eprintln!("slopwatchd: can't load {}: {error}", db.display());
            return ExitCode::FAILURE;
        }
    };
    let github: Arc<dyn GitHub> = Arc::new(Api::new(Arc::new(GhToken::new(
        Arc::clone(&clis),
        step_path.clone(),
    ))));
    let watching = match Watching::new(store.clone(), Arc::clone(&github)) {
        Ok(watching) => Arc::new(watching),
        Err(error) => {
            eprintln!("slopwatchd: can't load {}: {error}", db.display());
            return ExitCode::FAILURE;
        }
    };
    let steps = Library::default_dir(Flavor::CURRENT);
    let library = match Library::open(&steps) {
        Ok(library) => Arc::new(library),
        Err(error) => {
            eprintln!(
                "slopwatchd: can't open the Library at {}: {error}",
                steps.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("slopwatchd: can't find its own executable: {error}");
            return ExitCode::FAILURE;
        }
    };
    let plugins = Plugins::new(exe, Arc::clone(&library))
        .in_folder(plugins::default_dir(Flavor::CURRENT), step_path)
        .await;
    let config = RunsConfig {
        data_dir: data_dir.path().to_owned(),
        plugins,
        login_path,
        retention: Retention::default(),
        keychain: Arc::new(SecurityCli::new(SecurityCli::default_service(
            Flavor::CURRENT,
        ))),
        clis,
    };
    let runs = match Runs::start(store.clone(), github, Arc::clone(&watching), config) {
        Ok(runs) => runs,
        Err(error) => {
            eprintln!("slopwatchd: can't load Runs from {}: {error}", db.display());
            return ExitCode::FAILURE;
        }
    };
    tokio::spawn(Arc::clone(&runs).prune_forever());
    tokio::spawn(notifications::launch_gui_when_unheard(
        Arc::clone(runs.notifications()),
        Arc::new(OpenApp),
        LaunchPace::DAEMON,
    ));
    // Drafts resolve their Steps the way a Run does, third-party Plugins
    // included.
    let drafts = Drafts::new(
        store,
        Arc::clone(runs.plugins()) as _,
        Arc::clone(&library),
        Arc::clone(&runs) as Arc<dyn PipelineSource>,
    )
    .with_needs(Arc::clone(&runs) as Arc<dyn StepNeeds>);
    let daemon = Arc::new(
        Daemon::new(watching, library)
            .with_runs(runs)
            .with_drafts(Arc::new(drafts)),
    );
    eprintln!(
        "slopwatchd: build {} listening on {}",
        daemon.build_id(),
        listener.path().display()
    );

    tokio::spawn(Arc::clone(&daemon).poll_forever());
    // Crash-only (ADR 0009): no shutdown path. A signal kills the process,
    // and the next start replaces the stale socket. `restart` exits without
    // draining, and launchd starts whatever binary the bundle holds now.
    tokio::select! {
        () = listener.run(Arc::clone(&daemon)) => {}
        () = daemon.restart_requested() => {
            eprintln!("slopwatchd: restarting on a client's request");
            daemon.kill_steps().await;
        }
    }
    ExitCode::SUCCESS
}

/// Under launchd, stderr goes nowhere, so the daemon appends it to
/// `daemon.log` in its data dir instead. Run from a terminal, it leaves
/// stderr alone.
fn log_to_data_dir(data_dir: &DataDir) {
    // SAFETY: isatty only inspects the descriptor.
    if unsafe { libc::isatty(libc::STDERR_FILENO) } == 1 {
        return;
    }
    let path = data_dir.path().join("daemon.log");
    if fs::metadata(&path).is_ok_and(|meta| meta.len() > LOG_LIMIT_BYTES) {
        let _ = fs::remove_file(&path);
    }
    let Ok(log) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    // SAFETY: both descriptors are open, and dup2 replaces stderr
    // atomically. `log` closing afterwards leaves the duplicate open.
    unsafe { libc::dup2(log.as_raw_fd(), libc::STDERR_FILENO) };
}
