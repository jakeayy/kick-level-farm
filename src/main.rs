mod api;
mod config;
mod drops;
mod miner;
mod notify;
mod realtime;
mod selftest;
mod watcher;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "kick-level-farm",
    version,
    about = "Farm Kick.com account levels (XP) and channel points by emulating stream viewing"
)]
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
    /// Exit 0 if the running bot wrote a recent heartbeat, 1 otherwise (Docker HEALTHCHECK).
    Health,
    /// Check Kick's public API still matches what the bot expects (no login needed).
    Selftest,
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
        .with_timer(tracing_subscriber::fmt::time::ChronoLocal::new(
            "%H:%M:%S".into(),
        ))
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
            println!(
                "wrote {} — add some streamers, then run `kick-level-farm login`",
                cli.config.display()
            );
            Ok(())
        }
        Cmd::Login => login(&cli.config).await,
        Cmd::Status => {
            let cfg = config::Config::load(&cli.config)?;
            let client = api::KickClient::new(cfg.resolve_token()?)?;
            miner::status(&cfg, &client).await
        }
        Cmd::Health => health(&cli.config),
        Cmd::Selftest => selftest::run().await,
        Cmd::NotifyTest => {
            let cfg = config::Config::load(&cli.config)?;
            let n = notify::Notifier::new(
                cfg.notifications.webhook(),
                &[notify::NotifyEvent::Started],
                "kick-level-farm",
            );
            if !n.enabled() {
                anyhow::bail!(
                    "no Discord webhook configured ([notifications] discord_webhook_url or KICK_DISCORD_WEBHOOK)"
                );
            }
            n.send(
                notify::NotifyEvent::Started,
                "Test notification",
                "kick-level-farm can post to this channel ✅",
            );
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
            let res = async {
                miner::Miner::new(cfg, client, cli.config.clone())
                    .await?
                    .run()
                    .await
            }
            .await;
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

/// Reads the heartbeat file next to the config. Doesn't parse the config, so it also works
/// (and reports unhealthy) when the bot can't start.
fn health(config_path: &Path) -> Result<()> {
    let path = config_path
        .parent()
        .unwrap_or(Path::new("."))
        .join(config::HEARTBEAT_FILE);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("unhealthy: no heartbeat at {}", path.display()))?;
    let mut parts = raw.split_whitespace().filter_map(|x| x.parse::<u64>().ok());
    let (Some(at), Some(max_age)) = (parts.next(), parts.next()) else {
        anyhow::bail!("unhealthy: malformed heartbeat file {}", path.display());
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let age = now.saturating_sub(at);
    if age > max_age {
        anyhow::bail!("unhealthy: last heartbeat {age}s ago (limit {max_age}s)");
    }
    println!("healthy: last heartbeat {age}s ago");
    Ok(())
}

/// Polls until a (different) token is available. Returns false if we were asked to shut down.
async fn wait_for_token(config_path: &Path, old: Option<&str>) -> bool {
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
        let Ok(cfg) = config::Config::load(config_path) else {
            continue;
        };
        if let Ok(t) = cfg.resolve_token()
            && Some(t.as_str()) != old
        {
            tracing::info!("new session token found, restarting");
            return true;
        }
    }
}

async fn login(config_path: &Path) -> Result<()> {
    let cfg = if config_path.exists() {
        config::Config::load(config_path)?
    } else {
        config::Config::default()
    };
    println!(
        "Kick login uses captcha + 2FA, so the bot reuses your browser session instead.\n\
         1. Log in on https://kick.com\n\
         2. Open DevTools (F12) -> Application/Storage -> Cookies -> https://kick.com\n\
         3. Copy the whole value of `session_token` (e.g. `kat_...` or `12345678%7C...`)\n"
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
    write_secret(path, &token)?;
    println!(
        "✓ logged in as {} — token saved to {}",
        me.username,
        path.display()
    );
    Ok(())
}

/// Writes a secret so that it's never readable by others, not even briefly: a new file is
/// created as 0600, and an existing one is emptied and locked down before the secret goes in.
fn write_secret(path: &Path, secret: &str) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(secret.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_written_owner_only() {
        let dir = std::env::temp_dir().join(format!("klf-secret-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".kick_token");
        // An existing, world-readable file gets locked down too.
        std::fs::write(&path, "old").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        write_secret(&path, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
