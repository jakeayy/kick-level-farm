use crate::notify::NotifyEvent;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
    /// How many of the `max_concurrent` slots drop farming may use.
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
    /// The `session_token` cookie value (starts with `kat_`). Prefer `token_file` or KICK_TOKEN.
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
    /// How many streams to "watch" at the same time.
    pub max_concurrent: usize,
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
            max_concurrent: 2,
            priority: vec![Priority::Order],
            include_followed: true,
            fallback_to_popular: true,
            fallback_language: "en".into(),
            refresh_live_secs: 90,
            poll_progress_secs: 300,
            claim_daily_rewards: true,
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
            .map(|s| {
                s.trim()
                    .trim_start_matches("https://kick.com/")
                    .to_lowercase()
            })
            .filter(|s| !s.is_empty())
            .collect();
        cfg.farm.max_concurrent = cfg.farm.max_concurrent.max(1);
        cfg.farm.refresh_live_secs = cfg.farm.refresh_live_secs.max(30);
        cfg.farm.poll_progress_secs = cfg.farm.poll_progress_secs.max(60);
        if cfg.farm.priority.is_empty() {
            cfg.farm.priority.push(Priority::Order);
        }
        // Relative token/analytics paths are resolved next to the config file.
        let base = path.parent().unwrap_or(Path::new("."));
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

/// Accepts the raw cookie value, a URL-encoded one, or a `Bearer ...` string.
pub fn normalize_token(t: &str) -> String {
    let t = t.trim().trim_start_matches("Bearer ").trim();
    let t = t.strip_prefix("session_token=").unwrap_or(t);
    t.replace("%7C", "|").replace("%7c", "|")
}

pub const EXAMPLE_CONFIG: &str = r#"# kick-level-farm configuration

# Extra channels to farm on top of the ones you follow, in order of preference
# (slug = the part after kick.com/). Listed channels come before followed ones.
streamers = [
    # "xqc",
    # "trainwreckstv",
]

[auth]
# Get it from your browser: DevTools -> Application -> Cookies -> kick.com -> session_token
# or just run `kick-level-farm login`. KICK_TOKEN env var also works.
# token = "kat_..."
token_file = ".kick_token"

[farm]
# How many streams to watch simultaneously. Tested: Kick seems to award channel points on at
# most 2 channels at once (it picks which), so values above 2 add no points.
max_concurrent = 2
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
# Instant points / level-up / go-live events via Kick's realtime service (polling stays as backup).
realtime = true

[experiments]
# Multiplex every watched channel over a single viewer websocket.
shared_socket = false
# Override the watch-event interval (web client = 120s). Tested: 30s gave NO extra XP or
# points (the server credits by elapsed time). It only makes the account look less normal.
# watch_event_interval_secs = 60
send_handshake = true
# JSON-lines log of every progress poll, for measuring XP/hour.
analytics_file = "analytics.jsonl"

[drops]
# Actively watch streams that take part in drop campaigns (uses one of the max_concurrent slots).
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
"#;
