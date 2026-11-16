use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use smmm_core::{Config, Host, Module, ModuleFactory, Registry, StateOptions, StateStore};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "smartmmm", version, about = "Modular smart mirror core")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the core and its HTTP/WS server (default).
    Run {
        /// Path to config.toml. If omitted, the default locations are searched.
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Override the listen address, e.g. `0.0.0.0:8080`.
        #[arg(short, long)]
        listen: Option<String>,
        /// Where runtime state — visibility, placement, and config edits made
        /// from the phone — is kept between restarts.
        #[arg(long, value_name = "PATH")]
        state: Option<PathBuf>,
        /// Do not persist runtime state: every restart returns to config.toml.
        #[arg(long)]
        no_state: bool,
    },
    /// List the modules compiled into this build.
    ModuleList,
    /// Print host capability and health information.
    Doctor {
        #[arg(short, long)]
        config: Option<PathBuf>,
        #[arg(long, value_name = "PATH")]
        state: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Command::Run {
        config: None,
        listen: None,
        state: None,
        no_state: false,
    }) {
        Command::Run {
            config,
            listen,
            state,
            no_state,
        } => run(config, listen, state, no_state).await,
        Command::ModuleList => {
            list_modules();
            Ok(())
        }
        Command::Doctor { config, state } => doctor(config, state),
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

fn registry() -> Registry {
    let mut registry = Registry::new();
    registry.register(ModuleFactory::new(build_clock));
    registry.register(ModuleFactory::new(build_weather));
    registry.register(ModuleFactory::new(build_news));
    registry.register(ModuleFactory::new(build_presence));
    registry
}

fn build_clock() -> Box<dyn Module> {
    Box::new(smmm_mod_clock::ClockModule::new())
}

fn build_weather() -> Box<dyn Module> {
    Box::new(smmm_mod_weather::WeatherModule::new())
}

fn build_news() -> Box<dyn Module> {
    Box::new(smmm_mod_news::NewsModule::new())
}

fn build_presence() -> Box<dyn Module> {
    Box::new(smmm_mod_presence::PresenceModule::new())
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

async fn run(
    config_path: Option<PathBuf>,
    listen_override: Option<String>,
    state_path: Option<PathBuf>,
    no_state: bool,
) -> Result<()> {
    let (mut config, source) = Config::load(config_path.as_deref())?;
    if let Some(listen) = listen_override {
        config.core.listen = listen;
    }

    init_tracing(&config.core.log_level);

    match &source {
        Some(path) => tracing::info!("loaded config from {}", path.display()),
        None => tracing::info!("no config file found; using built-in defaults"),
    }

    if config.core.auth_token.is_none() {
        tracing::warn!(
            "no auth token configured — the API is open on the network (fine for local dev)"
        );
    }

    // `config.toml` is declared intent and is never written; the phone's changes
    // accumulate in a separate state file so both survive a restart.
    let state = if no_state {
        tracing::info!("runtime state: disabled (--no-state)");
        StateOptions::default()
    } else {
        let path = state_path.unwrap_or_else(smmm_core::persist::default_path);
        StateOptions::load(Some(path))?
    };

    let store = StateStore::new();
    let host = Host::start(&config, &registry(), store, state).await?;
    tracing::info!("started {} module(s)", host.module_ids().len());

    let app = smmm_server::router(host.clone(), config.clone());

    let listener = tokio::net::TcpListener::bind(&config.core.listen)
        .await
        .with_context(|| format!("binding {}", config.core.listen))?;
    let local = listener.local_addr()?;

    print_urls(&config, local.port());

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("running HTTP server")?;

    // The last change may still be inside the write debounce.
    if let Err(e) = host.flush_state() {
        tracing::warn!("could not save runtime state: {e:#}");
    }
    Ok(())
}

/// Resolve on Ctrl-C or SIGTERM.
///
/// A service manager stops the unit with SIGTERM, and the shutdown is what lets
/// the final state write happen rather than being skipped by a hard exit.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
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

fn print_urls(config: &Config, port: u16) {
    tracing::info!("mirror:  http://localhost:{port}/");
    tracing::info!("phone:   http://localhost:{port}/phone");
    if let Some(ip) = local_ip() {
        tracing::info!("on your network: http://{ip}:{port}/phone");
    }
    tracing::info!("serving \"{}\"", config.core.name);
}

fn init_tracing(level: &str) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("{level},hyper=warn,reqwest=warn")));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

// ---------------------------------------------------------------------------
// module list
// ---------------------------------------------------------------------------

fn list_modules() {
    println!("{:<10} {:<12} {:<10} NAME", "ID", "VERSION", "REGION");
    for factory in registry().factories() {
        let module = factory.build();
        let manifest = module.manifest();
        println!(
            "{:<10} {:<12} {:<10} {}",
            manifest.id,
            manifest.version,
            manifest.default_region.as_str(),
            manifest.name,
        );
    }
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

fn doctor(config_path: Option<PathBuf>, state_path: Option<PathBuf>) -> Result<()> {
    let (config, source) = Config::load(config_path.as_deref())?;
    let state = state_path.unwrap_or_else(smmm_core::persist::default_path);

    println!("SmartMMM doctor");
    println!("  version   {}", env!("CARGO_PKG_VERSION"));
    println!(
        "  platform  {}/{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("  config    {}", describe_source(&source));
    println!(
        "  state     {} ({})",
        state.display(),
        if state.is_file() {
            "runtime changes kept here"
        } else {
            "not written yet"
        }
    );
    println!("  listen    {}", config.core.listen);
    println!(
        "  auth      {}",
        match &config.core.auth_token {
            Some(_) => "token required",
            None => "open (no token)",
        }
    );

    println!("  modules");
    for factory in registry().factories() {
        let manifest = factory.build().manifest();
        println!("    {:<10} {}", manifest.id, manifest.name);
    }

    println!("  display");
    for binary in ["vcgencmd", "cec-ctl", "ddcutil"] {
        println!(
            "    {:<10} {}",
            binary,
            if which(binary) { "found" } else { "not found" }
        );
    }

    if which("vcgencmd") {
        if let Some(output) = command_output("vcgencmd", &["measure_temp"]) {
            println!("    temp       {}", output);
        }
        if let Some(output) = command_output("vcgencmd", &["get_throttled"]) {
            let healthy = output.trim().ends_with("0x0");
            println!(
                "    throttled  {} ({})",
                output,
                if healthy {
                    "healthy"
                } else {
                    "undervoltage/thermal!"
                }
            );
        }
    }

    Ok(())
}

fn describe_source(source: &Option<PathBuf>) -> String {
    source
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<defaults>".to_string())
}

fn which(binary: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(binary).is_file()))
        .unwrap_or(false)
}

fn command_output(binary: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(binary)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Best-effort LAN address, without sending any packets.
fn local_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|addr| addr.ip())
}
