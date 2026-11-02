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
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_timer(tracing_subscriber::fmt::time::ChronoLocal::new("%H:%M:%S".into()))
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
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
        Cmd::Run => {
            let cfg = config::Config::load(&cli.config)?;
            let client = api::KickClient::new(cfg.resolve_token()?)?;
            miner::Miner::new(cfg, client).await?.run().await
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
