//! Drover command-line entry point.

use std::future::IntoFuture;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use drover::api::{self, AppState, Phase};
use drover::app::{self, StartOptions};
use drover::bootstrap::{Bootstrap, LogConfig, LogFormat};
use drover::settings::StartupTestPolicy;
use drover::store::Stores;
use drover::testing::{TestSelection, TestStatus};

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const USAGE: &str = "\
Drover - evaluate CEL and Starlark rules against JSON records

USAGE:
    drover [COMMAND] [OPTIONS]

COMMANDS:
    serve           Run the HTTP server (default)
    check-config    Validate the configuration, data sources and rules, then exit
    test            Run the stored test suite and exit (status 1 if a test does not pass)

OPTIONS:
    -c, --config <PATH>   Configuration file [env: DROVER_CONFIG] [default: drover.yaml]
        --json            (test) Print the full JSON report
    -h, --help            Print this help
    -V, --version         Print the version
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    Serve,
    CheckConfig,
    Test,
}

struct Args {
    command: Command,
    config: PathBuf,
    json: bool,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut command = None;
    let mut config = None;
    let mut json = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("drover {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "-c" | "--config" => {
                config = Some(PathBuf::from(args.next().ok_or("--config requires a path")?));
            }
            "--json" => json = true,
            "serve" | "check-config" | "test" if command.is_none() => {
                command = Some(match arg.as_str() {
                    "serve" => Command::Serve,
                    "check-config" => Command::CheckConfig,
                    _ => Command::Test,
                });
            }
            other => return Err(format!("unexpected argument '{other}'\n\n{USAGE}")),
        }
    }
    let config = config
        .or_else(|| std::env::var_os("DROVER_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("drover.yaml"));
    Ok(Some(Args {
        command: command.unwrap_or(Command::Serve),
        config,
        json,
    }))
}

fn init_tracing(log: &LogConfig) {
    let filter = tracing_subscriber::EnvFilter::try_new(&log.level).unwrap_or_else(|e| {
        eprintln!("invalid log filter '{}' ({e}); using 'info'", log.level);
        tracing_subscriber::EnvFilter::new("info")
    });
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr);
    match log.format {
        LogFormat::Json => builder.json().init(),
        LogFormat::Text => builder.init(),
    }
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(args)) => args,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let bootstrap = match Bootstrap::load(&args.config) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("drover: {e}");
            return ExitCode::from(2);
        }
    };
    init_tracing(&bootstrap.log);
    let io_threads = bootstrap.server.io_threads.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get().min(4))
            .unwrap_or(2)
    });
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(io_threads.max(1))
        .thread_name("drover-io")
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("drover: cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(async move {
        match args.command {
            Command::Serve => serve(bootstrap).await,
            Command::CheckConfig => check_config(bootstrap).await,
            Command::Test => run_tests(bootstrap, args.json).await,
        }
    })
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

async fn serve(bootstrap: Bootstrap) -> ExitCode {
    for warning in bootstrap.warnings() {
        tracing::warn!("{warning}");
    }
    let state = AppState::new(bootstrap.auth.clone(), bootstrap.server.max_body_bytes);
    let listener = match tokio::net::TcpListener::bind(&bootstrap.server.bind).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("cannot listen on {}: {e}", bootstrap.server.bind);
            return ExitCode::FAILURE;
        }
    };
    if let Ok(addr) = listener.local_addr() {
        tracing::info!("listening on http://{addr} (initialising)");
    }
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(
        axum::serve(listener, api::router(state.clone()))
            .with_graceful_shutdown(async move {
                let _ = stop_rx.wait_for(|stop| *stop).await;
            })
            .into_future(),
    );

    let started = async {
        let stores = Stores::open(&bootstrap.datasources).await?;
        tracing::info!(data_sources = %stores.describe(), "data sources opened");
        app::start(stores, StartOptions::default()).await
    };
    let runtime = tokio::select! {
        result = started => match result {
            Ok(rt) => rt,
            Err(e) => {
                state.set_phase(Phase::Failed);
                tracing::error!("startup failed: {e}");
                return ExitCode::FAILURE;
            }
        },
        _ = shutdown_signal() => {
            tracing::info!("interrupted during startup");
            return ExitCode::FAILURE;
        }
    };
    state.set_ready(runtime.clone());
    tracing::info!(
        startup_ms = runtime.startup.duration_ms,
        worker_threads = runtime.pool.threads(),
        "Drover is ready"
    );

    shutdown_signal().await;
    tracing::info!("shutting down");
    state.set_phase(Phase::Stopping);
    let _ = stop_tx.send(true);
    let grace = Duration::from_secs(bootstrap.server.shutdown_grace_secs);
    if tokio::time::timeout(grace, server).await.is_err() {
        tracing::warn!("open HTTP connections did not finish within the grace period");
    }
    runtime.shutdown(grace).await;
    ExitCode::SUCCESS
}

async fn open_for_command(bootstrap: &Bootstrap) -> Result<std::sync::Arc<app::Runtime>, drover::Error> {
    let stores = Stores::open(&bootstrap.datasources).await?;
    app::start(
        stores,
        StartOptions {
            no_background: true,
            startup_tests: Some(StartupTestPolicy::Off),
        },
    )
    .await
}

async fn check_config(bootstrap: Bootstrap) -> ExitCode {
    for warning in bootstrap.warnings() {
        println!("warning: {warning}");
    }
    match open_for_command(&bootstrap).await {
        Ok(rt) => {
            let snapshot = rt.cache.snapshot();
            println!("data sources: {}", rt.stores.describe());
            println!(
                "rules: {} ({} enabled, {} invalid), groups: {}, rule configuration keys: {}",
                snapshot.rules.len(),
                snapshot.all.len(),
                snapshot.invalid_rules().count(),
                snapshot.groups.len(),
                snapshot.rule_config.len()
            );
            for warning in &rt.startup.warnings {
                println!("warning: {warning}");
            }
            println!("configuration OK");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("configuration invalid: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run_tests(bootstrap: Bootstrap, json: bool) -> ExitCode {
    let rt = match open_for_command(&bootstrap).await {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("startup failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let report = match rt.tests.run(&TestSelection::default()).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("test run failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
    } else {
        for r in &report.results {
            let mark = match r.status {
                TestStatus::Passed => "PASS ",
                TestStatus::Failed => "FAIL ",
                TestStatus::Error => "ERROR",
            };
            println!("{mark} {}", r.id);
            for d in &r.diagnostics {
                println!("      {d}");
            }
        }
        println!(
            "\n{} tests: {} passed, {} failed, {} errors ({:.1} ms)",
            report.total, report.passed, report.failed, report.errors, report.duration_ms
        );
    }
    if report.all_passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
