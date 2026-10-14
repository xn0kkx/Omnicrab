mod config;
mod crypto;
mod model;
mod orchestrator;
mod project;
mod providers;
mod store;
mod tui;
mod usage;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "omnicrab",
    version,
    about = "Orchestrate AI agents across many accounts and local models"
)]
struct Cli {
    /// Project folder agents work in (default: the current directory).
    #[arg(long, global = true, value_name = "DIR")]
    workspace: Option<std::path::PathBuf>,
    /// Agents may read the project but not change it.
    #[arg(long, global = true)]
    read_only: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run migrations against the configured database.
    InitDb,
    /// Check database, Ollama and installed CLIs.
    Doctor,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = config::Config::load()?;

    // Logs go to a file: stdout belongs to the TUI.
    let dir = config::log_dir();
    let _ = std::fs::create_dir_all(&dir);
    let (nb, _guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily(&dir, "omnicrab.log"));
    tracing_subscriber::fmt()
        .with_writer(nb)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "omnicrab=info".into()),
        )
        .init();

    match cli.cmd {
        Some(Cmd::Doctor) => doctor(&cfg).await,
        Some(Cmd::InitDb) => {
            store::Store::connect(&cfg.database_url()?).await?;
            println!("database ready");
            Ok(())
        }
        None => {
            let store = store::Store::connect(&cfg.database_url()?).await?;
            let cwd = std::env::current_dir().unwrap_or_default();
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            let workspace = project::resolve_root(cli.workspace, &cwd, home.as_deref());
            tui::run(store, &cfg, workspace, cli.read_only).await
        }
    }
}

async fn doctor(cfg: &config::Config) -> Result<()> {
    println!(
        "config file : {}",
        config::config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    );
    match cfg.database_url() {
        Ok(url) => match store::Store::connect(&url).await {
            Ok(s) => println!(
                "postgres    : ok (vault {})",
                if s.vault_header().await?.is_some() {
                    "initialized"
                } else {
                    "not created yet"
                }
            ),
            Err(e) => println!("postgres    : FAILED - {e:#}"),
        },
        Err(e) => println!(
            "postgres    : not configured - {}",
            e.to_string().lines().next().unwrap_or("")
        ),
    }
    match providers::ollama_discovery::list_models("http://localhost:11434").await {
        Ok(m) => println!(
            "ollama      : ok ({} models: {})",
            m.len(),
            m.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
        ),
        Err(_) => println!("ollama      : not reachable at localhost:11434"),
    }
    for bin in ["claude", "agy", "codex", "gemini"] {
        match providers::which(bin) {
            Some(p) => println!("{bin:<12}: {}", p.display()),
            None => println!("{bin:<12}: not installed"),
        }
    }
    Ok(())
}
