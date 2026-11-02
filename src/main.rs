mod api;
mod config;
mod drops;
mod miner;
mod notify;
mod realtime;
mod watcher;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::{BufRead, Write};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "kick-level-farm", version, about = "Farm Kick.com account levels (XP) and channel points by emulating stream viewing")]
struct Cli {
    /// Path to the config file.
    #[arg(short, long, default_value = "config.toml", global = true)]
    config: PathBuf,
    /// More logging (-v debug, -vv trace).
    #[arg(short, action = clap::ArgAction::Count, global = true)]
    verbose: u8,
    /// When the session token is rejected, wait for a new one (`login`) instead of exiting.
    /// Meant for containers/services so they don't crash-loop.
    #[arg(long, global = true)]
    wait_for_login: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start farming (default).
    Run,
    /// Write an example config file.
    Init,
    /// Save your Kick session token (the `session_token` cookie).
    Login,
    /// Show level, XP, challenges, drops and points on configured streamers.
    Status,
    /// Send a test message to the configured Discord webhook.
    NotifyTest,
}

fn init_logging(verbose: u8) {
    let default = match verbose {
        0 => "kick_level_farm=info",
        1 => "kick_level_farm=debug",
        _ => "kick_level_farm=trace",
    };
    use tracing_subscriber::filter::Targets;
    use tracing_subscriber::prelude::*;
    // RUST_LOG keeps working with the usual `target=level,...` syntax.
    let filter: Targets = std::env::var("RUST_LOG")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| default.parse().expect("valid default filter"));
    tracing_subscriber::fmt()
        // The builder caps at INFO by default; let `Targets` decide instead.
        .with_max_level(tracing::Level::TRACE)
        // No colour codes in `docker logs` / journald.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_target(false)
        .with_timer(tracing_subscriber::fmt::time::ChronoLocal::new("%H:%M:%S".into()))
        .finish()
        .with(filter)
        .init();
}

// The bot is I/O-bound and light: one thread is plenty.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // rustls is built with the `ring` provider only (smaller than aws-lc).
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cli = Cli::parse();
    init_logging(cli.verbose);

    match cli.cmd.unwrap_or(Cmd::Run) {
        Cmd::Init => {
            if cli.config.exists() {
                anyhow::bail!("{} already exists", cli.config.display());
            }
            std::fs::write(&cli.config, config::EXAMPLE_CONFIG)?;
            println!("wrote {} — add some streamers, then run `kick-level-farm login`", cli.config.display());
            Ok(())
        }
        Cmd::Login => login(&cli.config).await,
        Cmd::Status => {
            let cfg = config::Config::load(&cli.config)?;
            let client = api::KickClient::new(cfg.resolve_token()?)?;
            miner::status(&cfg, &client).await
        }
        Cmd::NotifyTest => {
            let cfg = config::Config::load(&cli.config)?;
            let n = notify::Notifier::new(cfg.notifications.webhook(), &[notify::NotifyEvent::Started], "kick-level-farm");
            if !n.enabled() {
                anyhow::bail!("no Discord webhook configured ([notifications] discord_webhook_url or KICK_DISCORD_WEBHOOK)");
            }
            n.send(notify::NotifyEvent::Started, "Test notification", "kick-level-farm can post to this channel ✅");
            n.flush().await;
            println!("sent — check your Discord channel");
            Ok(())
        }
        Cmd::Run => loop {
            let cfg = config::Config::load(&cli.config)?;
            let token = match cfg.resolve_token() {
                Ok(t) => t,
                Err(e) if cli.wait_for_login => {
                    tracing::error!("{e:#}");
                    if !wait_for_token(&cli.config, None).await {
                        return Ok(());
                    }
                    continue;
                }
                Err(e) => return Err(e),
            };
            let client = api::KickClient::new(token.clone())?;
            let res = async { miner::Miner::new(cfg, client).await?.run().await }.await;
            match res {
                Err(e) if cli.wait_for_login && api::is_unauthorized(&e) => {
                    tracing::error!("{e:#}");
                    if !wait_for_token(&cli.config, Some(&token)).await {
                        return Ok(());
                    }
                }
                other => return other,
            }
        },
    }
}

/// Polls until a (different) token is available. Returns false if we were asked to shut down.
async fn wait_for_token(config_path: &PathBuf, old: Option<&str>) -> bool {
    tracing::warn!(
        "waiting for a new session token — run `kick-level-farm login` \
         (docker: `docker compose run --rm kick-level-farm login`)"
    );
    let shutdown = miner::shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => return false,
            _ = tokio::time::sleep(std::time::Duration::from_secs(20)) => {}
        }
        let Ok(cfg) = config::Config::load(config_path) else { continue };
        if let Ok(t) = cfg.resolve_token() {
            if Some(t.as_str()) != old {
                tracing::info!("new session token found, restarting");
                return true;
            }
        }
    }
}

async fn login(config_path: &PathBuf) -> Result<()> {
    let cfg = if config_path.exists() { config::Config::load(config_path)? } else { config::Config::default() };
    println!(
        "Kick login uses captcha + 2FA, so the bot reuses your browser session instead.\n\
         1. Log in on https://kick.com\n\
         2. Open DevTools (F12) -> Application/Storage -> Cookies -> https://kick.com\n\
         3. Copy the value of `session_token` (starts with `kat_`)\n"
    );
    print!("session_token: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let token = config::normalize_token(&line);
    if token.is_empty() {
        anyhow::bail!("no token entered");
    }
    let client = api::KickClient::new(token.clone())?;
    let me = client.me().await.context("token check failed")?;
    let path = &cfg.auth.token_file;
    std::fs::write(path, &token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    println!("✓ logged in as {} — token saved to {}", me.username, path.display());
    Ok(())
}
