use crate::notify::NotifyEvent;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Streams watched at once. Kick awards channel points on at most 2 channels at a time (and
/// splits drop time across everything watched), so more would only cause surprises.
pub const MAX_CONCURRENT: usize = 2;

/// Name of the heartbeat file written next to the config (used by `kick-level-farm health`).
pub const HEARTBEAT_FILE: &str = ".heartbeat";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct Config {
    pub auth: AuthConfig,
    pub farm: FarmConfig,
    /// Channel slugs to farm, in order of preference (used by the `order` priority).
    pub streamers: Vec<String>,
    pub experiments: Experiments,
    pub notifications: Notifications,
    pub drops: DropsConfig,
    pub health: HealthConfig,
    /// Folder the config was loaded from (token, analytics and heartbeat live here).
    #[serde(skip)]
    pub base_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct HealthConfig {
    /// URL pinged (GET) after every successful progress check, e.g. a healthchecks.io or
    /// Uptime Kuma push URL. Missing pings tell you the bot stopped working.
    /// The KICK_HEARTBEAT_URL env var overrides it.
    pub heartbeat_url: Option<String>,
}

impl HealthConfig {
    pub fn url(&self) -> Option<String> {
        std::env::var("KICK_HEARTBEAT_URL")
            .ok()
            .or_else(|| self.heartbeat_url.clone())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DropsConfig {
    /// Actively watch streams that take part in drop campaigns.
    pub farm: bool,
    /// Claim drop rewards once they reach 100%.
    pub claim: bool,
    /// Which reward types to farm. Priority: badges > emotes > items.
    pub badges: bool,
    pub emotes: bool,
    /// In-game items (usually need a linked game account). Off unless you turn it on.
    pub items: bool,
    /// How many of the 2 watch slots drop farming may use (1 or 2).
    pub slots: usize,
    /// Kick splits drop watch time across concurrently watched streams, so by default
    /// nothing else is watched while a drop is being farmed (full drop speed).
    pub exclusive: bool,
}

impl Default for DropsConfig {
    fn default() -> Self {
        Self {
            farm: true,
            claim: true,
            badges: true,
            emotes: true,
            items: false,
            slots: 1,
            exclusive: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Notifications {
    /// Discord webhook URL (Server Settings -> Integrations -> Webhooks). Empty = disabled.
    /// The KICK_DISCORD_WEBHOOK env var overrides it.
    pub discord_webhook_url: Option<String>,
    /// Which events to send.
    pub events: Vec<NotifyEvent>,
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            discord_webhook_url: None,
            events: NotifyEvent::all(),
        }
    }
}

impl Notifications {
    pub fn webhook(&self) -> Option<String> {
        std::env::var("KICK_DISCORD_WEBHOOK")
            .ok()
            .or_else(|| self.discord_webhook_url.clone())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// The `session_token` cookie value (`kat_...` or `<id>%7C<random>`). Prefer `token_file` or KICK_TOKEN.
    pub token: Option<String>,
    /// File holding the token (written by `kick-level-farm login`).
    pub token_file: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Order the streamers appear in the config.
    Order,
    /// Channels where you have the fewest points first.
    PointsAscending,
    /// Channels where you have the most points first.
    PointsDescending,
    /// Smallest live streams first.
    ViewersAscending,
    /// Biggest live streams first.
    ViewersDescending,
    /// Channels with channel points enabled first.
    PointsEnabled,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FarmConfig {
    /// Deprecated and ignored: always 2 (see `MAX_CONCURRENT`). Still accepted so older
    /// configs keep loading.
    #[serde(skip_serializing)]
    pub max_concurrent: Option<usize>,
    /// Tie-breakers applied in order when choosing which live streams to watch.
    pub priority: Vec<Priority>,
    /// Farm every channel you follow (after `streamers` in `order` priority). On by default.
    pub include_followed: bool,
    /// Account level XP is global, so when none of your configured/followed channels are
    /// live the bot watches ONE popular stream for level XP only (no points tracking).
    pub fallback_to_popular: bool,
    /// Language code for the popular-streams fallback (e.g. "en", "de", "tr").
    pub fallback_language: String,
    /// Seconds between live-status refreshes.
    pub refresh_live_secs: u64,
    /// Seconds between XP / points / daily-reward polls.
    pub poll_progress_secs: u64,
    /// Automatically claim gamification challenges (Daily Reward, events).
    pub claim_daily_rewards: bool,
    /// Open ("unbox") won rewards automatically so they're ready to use.
    pub unbox_rewards: bool,
    /// Use Kick's realtime push service for instant points / level-up / go-live events.
    /// Polling still runs as a fallback.
    pub realtime: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Experiments {
    /// Multiplex all watched channels over ONE viewer websocket instead of one per channel.
    pub shared_socket: bool,
    /// Override the server-provided `tracking.user.watch.livestream` interval (seconds).
    /// The web client uses 120. Lower values may be ignored or flagged by Kick.
    pub watch_event_interval_secs: Option<u64>,
    /// Send the `channel_handshake` keep-alive every 15s like the web client.
    pub send_handshake: bool,
    /// Append a JSON line per progress poll (XP, level, points, watched set) for analysis.
    pub analytics_file: Option<PathBuf>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            token: None,
            token_file: PathBuf::from(".kick_token"),
        }
    }
}

impl Default for FarmConfig {
    fn default() -> Self {
        Self {
            max_concurrent: None,
            priority: vec![Priority::Order],
            include_followed: true,
            fallback_to_popular: true,
            fallback_language: "en".into(),
            refresh_live_secs: 90,
            poll_progress_secs: 300,
            claim_daily_rewards: true,
            unbox_rewards: true,
            realtime: true,
        }
    }
}

impl Default for Experiments {
    fn default() -> Self {
        Self {
            shared_socket: false,
            watch_event_interval_secs: None,
            send_handshake: true,
            analytics_file: Some(PathBuf::from("analytics.jsonl")),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            anyhow::bail!(
                "config file {} not found — run `kick-level-farm init` to create one",
                path.display()
            );
        }
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        cfg.streamers = cfg
            .streamers
            .iter()
            .map(|s| normalize_slug(s))
            .filter(|s| !s.is_empty())
            .collect();
        if let Some(n) = cfg.farm.max_concurrent.take()
            && n != MAX_CONCURRENT
        {
            tracing::warn!(
                "`max_concurrent = {n}` is ignored: the bot always watches up to {MAX_CONCURRENT} streams \
                 (Kick pays channel points on at most {MAX_CONCURRENT}). Remove it from {}",
                path.display()
            );
        }
        cfg.drops.slots = cfg.drops.slots.clamp(1, MAX_CONCURRENT);
        cfg.farm.refresh_live_secs = cfg.farm.refresh_live_secs.max(30);
        cfg.farm.poll_progress_secs = cfg.farm.poll_progress_secs.max(60);
        if cfg.farm.priority.is_empty() {
            cfg.farm.priority.push(Priority::Order);
        }
        // Relative token/analytics paths are resolved next to the config file.
        let base = path.parent().unwrap_or(Path::new("."));
        cfg.base_dir = base.to_path_buf();
        if cfg.auth.token_file.is_relative() {
            cfg.auth.token_file = base.join(&cfg.auth.token_file);
        }
        if let Some(a) = &cfg.experiments.analytics_file
            && a.is_relative()
        {
            cfg.experiments.analytics_file = Some(base.join(a));
        }
        Ok(cfg)
    }

    pub fn heartbeat_path(&self) -> PathBuf {
        self.base_dir.join(HEARTBEAT_FILE)
    }

    /// Token resolution order: KICK_TOKEN env var, `auth.token`, `auth.token_file`.
    pub fn resolve_token(&self) -> Result<String> {
        if let Ok(t) = std::env::var("KICK_TOKEN")
            && !t.trim().is_empty()
        {
            return Ok(normalize_token(&t));
        }
        if let Some(t) = &self.auth.token
            && !t.trim().is_empty()
        {
            return Ok(normalize_token(t));
        }
        let t = std::fs::read_to_string(&self.auth.token_file).with_context(|| {
            format!(
                "no token found (KICK_TOKEN, auth.token or {}) — run `kick-level-farm login`",
                self.auth.token_file.display()
            )
        })?;
        Ok(normalize_token(&t))
    }
}

/// Turns what people paste into `streamers` into a bare channel slug: `xqc`, `XQC`,
/// `https://kick.com/xqc/`, `kick.com/xqc`, `www.kick.com/xqc?ref=x` and `@xqc` all give `xqc`.
pub fn normalize_slug(s: &str) -> String {
    let s = s.trim().to_lowercase();
    let s = s.split_once("://").map_or(s.as_str(), |(_, rest)| rest);
    let s = s.strip_prefix("www.").unwrap_or(s);
    let s = s.strip_prefix("kick.com/").unwrap_or(s);
    let s = s.trim_start_matches(['/', '@']);
    s.split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Accepts the raw cookie value, a URL-encoded one, or a `Bearer ...` string.
pub fn normalize_token(t: &str) -> String {
    let t = t.trim().trim_start_matches("Bearer ").trim();
    let t = t.strip_prefix("session_token=").unwrap_or(t);
    t.replace("%7C", "|").replace("%7c", "|")
}

pub const EXAMPLE_CONFIG: &str = r#"# kick-level-farm configuration

# Extra channels to farm on top of the ones you follow, in order of preference
# (the slug after kick.com/, or the whole channel link). Listed channels come before followed ones.
streamers = [
    # "xqc",
    # "trainwreckstv",
]

[auth]
# Get it from your browser: DevTools -> Application -> Cookies -> kick.com -> session_token
# or just run `kick-level-farm login`. KICK_TOKEN env var also works.
# token = "kat_..."   (or "12345678%7C...", depending on the account)
token_file = ".kick_token"

[farm]
# The bot watches up to 2 streams at once (Kick pays channel points on at most 2).
# Tie-breakers, applied in order: order | points_ascending | points_descending |
# viewers_ascending | viewers_descending | points_enabled
priority = ["order"]
# Farm every channel you follow (on by default).
include_followed = true
# Level XP is per account: when none of your channels are live, watch ONE popular stream
# for level XP only. Channel points are only farmed on `streamers` and followed channels.
fallback_to_popular = true
fallback_language = "en"
refresh_live_secs = 90
poll_progress_secs = 300
# Auto-claim Daily Reward / event chests once their watch-time goal is reached.
claim_daily_rewards = true
# Open won rewards (emotes, badges, …) automatically so they're ready to use.
unbox_rewards = true
# Instant points / level-up / go-live events via Kick's realtime service (polling stays as backup).
realtime = true

[experiments]
# Multiplex every watched channel over a single viewer websocket.
shared_socket = false
# Override the watch-event interval (web client = 120s). Tested: 30s gave NO extra XP or
# points (the server credits by elapsed time). It only makes the account look less normal.
# watch_event_interval_secs = 60
send_handshake = true
# JSON-lines log of every progress poll, for measuring XP/hour. Rotated at 10 MB
# (the previous file is kept as analytics.jsonl.1).
analytics_file = "analytics.jsonl"

[drops]
# Actively watch streams that take part in drop campaigns (uses one of the 2 watch slots).
farm = true
# Claim drop rewards once they reach 100%.
claim = true
# Reward types to farm. Priority: badges > emotes > items.
badges = true
emotes = true
# In-game items usually need a linked game account. Only farmed when switched on.
items = false
slots = 1
# Kick splits drop watch time across all streams watched at once (tested: 2 streams = half
# speed). With exclusive = true only the drops stream is watched while a drop is farmed.
exclusive = true

[notifications]
# Discord webhook (Server Settings -> Integrations -> Webhooks). Leave empty to disable.
# The KICK_DISCORD_WEBHOOK env var works too.
discord_webhook_url = ""
# started | stopped | level_up | reward_claimed | drop_claimed | streamer_live | error
events = ["started", "stopped", "level_up", "reward_claimed", "drop_claimed", "streamer_live", "error"]

[health]
# Pinged after every successful progress check (healthchecks.io, Uptime Kuma push URL, …).
# Missing pings mean the bot stopped working. The KICK_HEARTBEAT_URL env var works too.
heartbeat_url = ""
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `body` to a fresh temp dir and loads it as a config.
    fn load(name: &str, body: &str) -> Result<Config> {
        let dir = std::env::temp_dir().join(format!("klf-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, body).unwrap();
        Config::load(&path)
    }

    #[test]
    fn example_config_loads_with_defaults() {
        let c = load("example", EXAMPLE_CONFIG).unwrap();
        assert!(c.farm.include_followed && c.farm.fallback_to_popular && c.farm.unbox_rewards);
        assert!(c.drops.exclusive && !c.drops.items);
        assert_eq!(c.health.heartbeat_url.as_deref(), Some(""));
        assert_eq!(c.heartbeat_path(), c.base_dir.join(HEARTBEAT_FILE));
        assert_eq!(
            c.auth.token_file,
            c.base_dir.join(".kick_token"),
            "relative to the config"
        );
    }

    #[test]
    fn deprecated_max_concurrent_is_accepted_and_ignored() {
        let c = load("maxc", "[farm]\nmax_concurrent = 5\n").unwrap();
        assert_eq!(c.farm.max_concurrent, None);
        assert_eq!(MAX_CONCURRENT, 2);
    }

    #[test]
    fn values_are_clamped_and_normalised() {
        let c = load(
            "clamp",
            "streamers = [\" https://kick.com/XQC \", \"\"]\n[farm]\nrefresh_live_secs = 1\npoll_progress_secs = 1\npriority = []\n[drops]\nslots = 9\n",
        )
        .unwrap();
        assert_eq!(c.streamers, ["xqc"]);
        assert_eq!(
            (c.farm.refresh_live_secs, c.farm.poll_progress_secs),
            (30, 60)
        );
        assert_eq!(c.farm.priority, [Priority::Order]);
        assert_eq!(c.drops.slots, MAX_CONCURRENT);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = load("unknown", "[farm]\nmax_concurent = 2\n").unwrap_err();
        assert!(format!("{err:#}").contains("max_concurent"));
    }

    #[test]
    fn token_normalisation() {
        assert_eq!(normalize_token("  kat_abc \n"), "kat_abc");
        assert_eq!(normalize_token("Bearer kat_abc"), "kat_abc");
        assert_eq!(normalize_token("session_token=kat_abc"), "kat_abc");
        assert_eq!(normalize_token("123%7Cabc"), "123|abc");
    }

    #[test]
    fn slug_normalisation() {
        for raw in [
            "xqc",
            " XQC ",
            "https://kick.com/xqc",
            "https://kick.com/xqc/",
            "http://www.kick.com/xqc?ref=home",
            "kick.com/xqc",
            "www.kick.com/xqc#chat",
            "@xqc",
            "https://kick.com/xqc/videos",
        ] {
            assert_eq!(normalize_slug(raw), "xqc", "{raw:?}");
        }
        assert_eq!(normalize_slug("https://kick.com/"), "");
    }
}
