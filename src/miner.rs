use crate::api::{self, KickClient, Level, LiveInfo, User};
use crate::config::{Config, MAX_CONCURRENT, Priority};
use crate::drops::{self, DropPlan};
use crate::notify::{Notifier, NotifyEvent};
use crate::realtime::{self, Realtime};
use crate::watcher::{Connection, WatchStats, WatchTarget};
use anyhow::{Result, bail};
use serde_json::json;
use std::cmp::Ordering as CmpOrdering;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

const POPULAR_CACHE: Duration = Duration::from_secs(600);
const POLITE_DELAY: Duration = Duration::from_millis(350);
const DROP_RETRY: Duration = Duration::from_secs(3600);
const DROP_CAMPAIGNS_CACHE: Duration = Duration::from_secs(900);
const DROP_PROGRESS_CACHE: Duration = Duration::from_secs(300);
const CATEGORY_CACHE: Duration = Duration::from_secs(300);
const CONFIG_CHECK: Duration = Duration::from_secs(10);
/// The final progress check on shutdown must fit Docker's stop grace period (30s).
const SHUTDOWN_POLL: Duration = Duration::from_secs(8);
/// `analytics.jsonl` is rotated to `analytics.jsonl.1` past this size.
const ANALYTICS_MAX_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Debug, Default)]
struct Streamer {
    slug: String,
    /// position in config (`order` priority); fallback streams get usize::MAX
    index: usize,
    fallback: bool,
    channel_id: Option<u64>,
    live: Option<LiveInfo>,
    banned: bool,
    missing: bool,
    points: Option<i64>,
    start_points: Option<i64>,
    points_enabled: Option<bool>,
    watched: Duration,
    /// Set while this stream is picked for drop farming (reset every reconcile).
    drop_label: Option<String>,
    /// Was ever used for drops (for the summary).
    drop_farmed: bool,
    /// Listed in `streamers` in the config.
    configured: bool,
    /// Seen in the followed-channels list.
    followed: bool,
    /// When `/info` was last fetched (drop planning reuses recent results).
    checked_at: Option<Instant>,
}

impl Streamer {
    fn new(slug: &str, index: usize, fallback: bool) -> Self {
        Self {
            slug: slug.to_string(),
            index,
            fallback,
            ..Default::default()
        }
    }

    fn target(&self) -> Option<WatchTarget> {
        let live = self.live.as_ref()?;
        if !live.is_live || self.banned {
            return None;
        }
        Some(WatchTarget {
            slug: self.slug.clone(),
            channel_id: self.channel_id?,
            livestream_id: live.id,
            vod_id: live.vod_id.clone()?,
        })
    }

    fn viewers(&self) -> u64 {
        self.live.as_ref().map(|l| l.viewer_count).unwrap_or(0)
    }

    fn recently_checked(&self) -> bool {
        self.checked_at
            .is_some_and(|t| t.elapsed() < CATEGORY_CACHE)
    }
}

/// Slugs marked as followed that are missing from the latest followed list (unfollowed).
fn unfollowed(streamers: &HashMap<String, Streamer>, followed: &HashSet<String>) -> Vec<String> {
    streamers
        .values()
        .filter(|s| s.followed && !followed.contains(&s.slug))
        .map(|s| s.slug.clone())
        .collect()
}

/// A timer that waits a full period after a late tick, instead of tokio's default of firing
/// every missed tick back to back (which piled up requests when Kick was slow).
fn every(period: Duration) -> tokio::time::Interval {
    let mut i = tokio::time::interval(period);
    i.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    i
}

/// Keeps an append-only log bounded: past `max` bytes it's renamed to `<name>.1` (replacing
/// the previous one) and a fresh file is started.
fn rotate_if_larger(path: &Path, max: u64) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > max) {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        if let Err(e) = std::fs::rename(path, &old) {
            tracing::debug!("rotating {}: {e}", path.display());
        }
    }
}

struct Active {
    target: WatchTarget,
    since: Instant,
    conn: Option<Connection>,
}

pub struct Miner {
    cfg: Config,
    client: KickClient,
    user: User,
    interval: Duration,
    stats: Arc<WatchStats>,
    streamers: HashMap<String, Streamer>,
    active: HashMap<String, Active>,
    shared: Option<Connection>,
    popular: Vec<api::PopularStream>,
    popular_at: Option<Instant>,
    start_level: Option<Level>,
    last_level: Option<Level>,
    started: Instant,
    claimed: HashSet<String>,
    notifier: Notifier,
    realtime: Option<Realtime>,
    /// highest level we've already announced
    announced_level: u32,
    /// false until the first live refresh finished (avoids "went live" spam on startup)
    initial_refresh_done: bool,
    drops_claimed: HashSet<String>,
    drops_failed: HashMap<String, Instant>,
    drop_units_logged: HashMap<String, u64>,
    drop_campaigns: Vec<api::ActiveCampaign>,
    drop_campaigns_at: Option<Instant>,
    drop_progress: Vec<api::DropCampaign>,
    drop_progress_at: Option<Instant>,
    category_live: HashMap<u64, (Instant, Vec<String>)>,
    drop_skip_logged: HashSet<String>,
    /// (campaign id, slug) currently farmed, to log changes once
    drop_current: Vec<(String, String)>,
    /// Config file and its last-seen modification time (for live reload).
    cfg_path: PathBuf,
    cfg_mtime: Option<SystemTime>,
    /// Plain HTTP client for heartbeat pings (no Kick headers or token).
    health_http: reqwest::Client,
}

fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
}

impl Miner {
    pub async fn new(cfg: Config, client: KickClient, cfg_path: PathBuf) -> Result<Self> {
        // `context` keeps the ApiError inside, so callers can still detect "unauthorized".
        let user = client.me().await.map_err(|e| {
            if api::is_unauthorized(&e) {
                e.context("session token rejected — run `kick-level-farm login` again")
            } else {
                e
            }
        })?;
        let interval = match cfg.experiments.watch_event_interval_secs {
            Some(s) => {
                tracing::warn!("experiment: overriding watch event interval to {s}s");
                Duration::from_secs(s.max(10))
            }
            None => Duration::from_secs(client.watch_event_interval().await.unwrap_or(120)),
        };
        let notifier = Notifier::new(
            cfg.notifications.webhook(),
            &cfg.notifications.events,
            "kick-level-farm",
        );
        let mut streamers = HashMap::new();
        for (i, s) in cfg.streamers.iter().enumerate() {
            streamers
                .entry(s.clone())
                .or_insert_with(|| Streamer::new(s, i, false))
                .configured = true;
        }
        let cfg_mtime = std::fs::metadata(&cfg_path).and_then(|m| m.modified()).ok();
        Ok(Self {
            cfg,
            client,
            user,
            interval,
            stats: Arc::new(WatchStats::default()),
            streamers,
            active: HashMap::new(),
            shared: None,
            popular: Vec::new(),
            popular_at: None,
            start_level: None,
            last_level: None,
            started: Instant::now(),
            claimed: HashSet::new(),
            notifier,
            realtime: None,
            announced_level: 0,
            initial_refresh_done: false,
            drops_claimed: HashSet::new(),
            drops_failed: HashMap::new(),
            drop_units_logged: HashMap::new(),
            drop_campaigns: Vec::new(),
            drop_campaigns_at: None,
            drop_progress: Vec::new(),
            drop_progress_at: None,
            category_live: HashMap::new(),
            drop_skip_logged: HashSet::new(),
            drop_current: Vec::new(),
            cfg_path,
            cfg_mtime,
            health_http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
        })
    }

    pub async fn run(mut self) -> Result<()> {
        tracing::info!(
            "logged in as {} (id {}), watch-event interval {}s, max {} concurrent stream(s), {} configured streamer(s)",
            self.user.username,
            self.user.id,
            self.interval.as_secs(),
            MAX_CONCURRENT,
            self.streamers.len()
        );
        if self.cfg.experiments.shared_socket {
            tracing::warn!("experiment: multiplexing all channels over one viewer socket");
            self.shared = Some(self.spawn_conn("shared".into()));
        }

        if self.notifier.enabled() {
            tracing::info!("discord notifications enabled");
        }
        let mut rt_rx = None;
        if self.cfg.farm.realtime {
            let (rt, rx) = Realtime::spawn(self.client.clone(), self.user.id);
            self.realtime = Some(rt);
            rt_rx = Some(rx);
        }

        let mut refresh = every(Duration::from_secs(self.cfg.farm.refresh_live_secs));
        let mut poll = every(Duration::from_secs(self.cfg.farm.poll_progress_secs));
        poll.reset_after(Duration::from_secs(5)); // let refresh run first
        // Set by realtime go-live/offline events: refresh shortly after.
        let mut refresh_soon: Option<tokio::time::Instant> = None;
        let mut config_check = every(CONFIG_CHECK);
        config_check.reset();
        let ctrl_c = shutdown_signal();
        tokio::pin!(ctrl_c);
        // Runs a long step (refresh, poll) but stops waiting for it on Ctrl-C / SIGTERM, so a
        // slow Kick can't hold up shutdown past Docker's grace period.
        macro_rules! or_shutdown {
            ($step:expr) => {
                tokio::select! {
                    r = $step => r,
                    _ = &mut ctrl_c => {
                        tracing::info!("shutdown signal received, stopping…");
                        break Ok(());
                    }
                }
            };
        }

        let mut started_sent = false;
        let result = loop {
            tokio::select! {
                _ = &mut ctrl_c => {
                    tracing::info!("shutdown signal received, stopping…");
                    break Ok(());
                }
                _ = refresh.tick() => {
                    match or_shutdown!(self.refresh_and_reconcile()) {
                        Ok(()) => self.write_heartbeat(),
                        Err(e) if api::is_unauthorized(&e) => break Err(e),
                        Err(e) => tracing::warn!("refresh failed: {e:#}"),
                    }
                    self.initial_refresh_done = true;
                }
                _ = config_check.tick() => {
                    if self.reload_config_if_changed().await {
                        refresh = every(Duration::from_secs(self.cfg.farm.refresh_live_secs));
                        refresh.reset();
                        poll = every(Duration::from_secs(self.cfg.farm.poll_progress_secs));
                        poll.reset();
                        refresh_soon = Some(tokio::time::Instant::now() + Duration::from_secs(1));
                    }
                }
                _ = async {
                    match refresh_soon {
                        Some(t) => tokio::time::sleep_until(t).await,
                        None => std::future::pending().await,
                    }
                } => {
                    refresh_soon = None;
                    refresh.reset();
                    if let Err(e) = or_shutdown!(self.refresh_and_reconcile()) {
                        if api::is_unauthorized(&e) { break Err(e); }
                        tracing::warn!("refresh failed: {e:#}");
                    }
                }
                _ = poll.tick() => {
                    match or_shutdown!(self.poll_progress()) {
                        Ok(()) => {
                            self.write_heartbeat();
                            self.ping_heartbeat();
                        }
                        Err(e) if api::is_unauthorized(&e) => break Err(e),
                        Err(e) => tracing::warn!("progress poll failed: {e:#}"),
                    }
                    if !started_sent {
                        started_sent = true;
                        let lvl = self.last_level.as_ref().map(|l| format!("level **{}** ({}/{} XP)", l.level, l.progress_xp, l.progress_xp + l.xp_to_next_level)).unwrap_or_default();
                        let watching: Vec<&str> = self.active.keys().map(String::as_str).collect();
                        self.notifier.send(
                            NotifyEvent::Started,
                            format!("Farming started as {}", self.user.username),
                            format!("{lvl}\nWatching: {}", if watching.is_empty() { "nothing live yet".to_string() } else { watching.join(", ") }),
                        );
                    }
                }
                ev = async {
                    match rt_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => match ev {
                    Some(ev) => match self.on_realtime(ev).await {
                        Ok(true) => {
                            refresh_soon.get_or_insert_with(|| tokio::time::Instant::now() + Duration::from_secs(3));
                        }
                        Ok(false) => {}
                        Err(e) if api::is_unauthorized(&e) => break Err(e),
                        Err(e) => tracing::warn!("realtime handling failed: {e:#}"),
                    },
                    None => rt_rx = None,
                },
            }
            if self.stats.unauthorized.load(Ordering::Relaxed) {
                break Err(anyhow::Error::from(api::ApiError::Unauthorized)
                    .context("viewer socket rejected the session token"));
            }
        };
        if let Some(rt) = self.realtime.take() {
            rt.shutdown();
        }

        // Best-effort final poll for the summary, bounded so the summary and the "stopped"
        // notification still go out before the container is killed.
        if tokio::time::timeout(SHUTDOWN_POLL, self.poll_progress())
            .await
            .is_err()
        {
            tracing::warn!("final progress check timed out; summary may be slightly behind");
        }
        let slugs: Vec<String> = self.active.keys().cloned().collect();
        for s in slugs {
            self.stop(&s, "shutdown").await;
        }
        if let Some(c) = self.shared.take() {
            c.shutdown().await;
        }
        let summary = self.summary();
        println!("{summary}");
        match &result {
            Ok(()) => self.notifier.send(
                NotifyEvent::Stopped,
                "Farming stopped",
                format!("```\n{summary}\n```"),
            ),
            Err(e) => self.notifier.send(
                NotifyEvent::Error,
                "Farming stopped with an error",
                format!("{e:#}\n```\n{summary}\n```"),
            ),
        }
        self.notifier.flush().await;
        result
    }

    /// Returns true when a live-status refresh should happen soon.
    async fn on_realtime(&mut self, ev: realtime::Event) -> Result<bool> {
        use realtime::Event as E;
        match ev {
            E::Connected => Ok(false),
            E::Unauthorized => Err(api::ApiError::Unauthorized.into()),
            E::PointsUpdated {
                channel_id,
                points,
                balance,
                reason,
            } => {
                // Only channels we farm points on (configured/followed), never the fallback.
                if let Some(s) = self
                    .streamers
                    .values_mut()
                    .find(|s| s.channel_id == Some(channel_id) && !s.fallback)
                {
                    if s.points == Some(balance) {
                        return Ok(false); // a poll already picked this balance up
                    }
                    s.start_points.get_or_insert(balance - points);
                    s.points = Some(balance);
                    let session = balance - s.start_points.unwrap_or(balance);
                    if reason.eq_ignore_ascii_case("redeemed") {
                        tracing::info!("{}: {points:+} points redeemed (now {balance})", s.slug);
                    } else {
                        tracing::info!(
                            "{}: {points:+} channel points (now {balance}, {session:+} this session) [live]",
                            s.slug
                        );
                    }
                }
                Ok(false)
            }
            E::LevelUp { level } => {
                self.announce_level(level, None);
                Ok(false)
            }
            E::WatchGoalReached => {
                tracing::info!("watch-time goal reached — checking rewards");
                if self.cfg.farm.claim_daily_rewards {
                    self.check_challenges().await?;
                }
                Ok(false)
            }
            E::LivestreamUpdated { livestream_id } => {
                let slug = self
                    .active
                    .iter()
                    .find(|(_, a)| a.target.livestream_id == livestream_id)
                    .map(|(s, _)| s.clone());
                match slug {
                    Some(slug) => {
                        tracing::debug!(
                            "realtime: {slug} updated its stream (title/category) — re-checking"
                        );
                        Ok(true)
                    }
                    None => Ok(false),
                }
            }
            E::StreamerLive { channel_id } | E::StreamerOffline { channel_id } => {
                let known = self
                    .streamers
                    .values()
                    .any(|s| s.channel_id == Some(channel_id) && !s.fallback);
                if known {
                    tracing::debug!("realtime: live status changed for channel {channel_id}");
                }
                Ok(known)
            }
        }
    }

    fn announce_level(&mut self, level: u32, lvl: Option<&Level>) {
        if level <= self.announced_level {
            return;
        }
        let first = self.announced_level == 0;
        self.announced_level = level;
        if first {
            return; // baseline, not a level-up
        }
        tracing::info!("★ LEVEL UP! now level {level}");
        let detail = lvl
            .map(|l| {
                format!(
                    "Total XP: {}\nNext level in {} XP",
                    l.total_xp, l.xp_to_next_level
                )
            })
            .unwrap_or_default();
        self.notifier.send(
            NotifyEvent::LevelUp,
            format!("Level up! {} is now level {level}", self.user.username),
            detail,
        );
    }

    /// Re-reads the config file when it changed and applies what can be applied live.
    /// Returns true when the new config was applied.
    async fn reload_config_if_changed(&mut self) -> bool {
        let mtime = std::fs::metadata(&self.cfg_path)
            .and_then(|m| m.modified())
            .ok();
        if mtime.is_none() || mtime == self.cfg_mtime {
            return false;
        }
        self.cfg_mtime = mtime;
        let mut new = match Config::load(&self.cfg_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("config change ignored, keeping the previous settings: {e:#}");
                return false;
            }
        };

        // These are wired up at startup; keep the running values and ask for a restart.
        let mut restart = Vec::new();
        if new.farm.realtime != self.cfg.farm.realtime {
            new.farm.realtime = self.cfg.farm.realtime;
            restart.push("farm.realtime");
        }
        if format!("{:?}", new.experiments) != format!("{:?}", self.cfg.experiments) {
            new.experiments = self.cfg.experiments.clone();
            restart.push("[experiments]");
        }
        if format!("{:?}", new.auth) != format!("{:?}", self.cfg.auth) {
            new.auth = self.cfg.auth.clone();
            restart.push("[auth]");
        }
        if !restart.is_empty() {
            tracing::warn!("restart to apply changes to: {}", restart.join(", "));
        }

        // Streamer list: add/reorder configured channels, drop ones that no longer apply.
        let order: HashMap<&String, usize> = new
            .streamers
            .iter()
            .enumerate()
            .map(|(i, s)| (s, i))
            .collect();
        for (slug, i) in &order {
            let s = self
                .streamers
                .entry((*slug).clone())
                .or_insert_with(|| Streamer::new(slug, *i, false));
            s.configured = true;
            s.fallback = false;
            s.missing = false;
            s.index = *i;
        }
        let mut removed = Vec::new();
        for s in self.streamers.values_mut() {
            if s.fallback || order.contains_key(&s.slug) {
                continue;
            }
            s.configured = false;
            if new.farm.include_followed && s.followed {
                // Placed after configured channels; the next refresh restores follow order.
                s.index += order.len();
            } else {
                removed.push(s.slug.clone());
            }
        }
        for slug in removed {
            self.stop(&slug, "removed from config").await;
            self.streamers.remove(&slug);
        }

        self.notifier = Notifier::new(
            new.notifications.webhook(),
            &new.notifications.events,
            "kick-level-farm",
        );
        self.cfg = new;
        tracing::info!(
            "config reloaded: {} configured streamer(s), followed channels {}",
            self.cfg.streamers.len(),
            if self.cfg.farm.include_followed {
                "on"
            } else {
                "off"
            }
        );
        true
    }

    /// Records that the bot is alive; read by `kick-level-farm health` (Docker HEALTHCHECK).
    fn write_heartbeat(&self) {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Healthy while updated within 3 cycles of the slower loop (+1 min slack).
        let max_age = 3 * self
            .cfg
            .farm
            .refresh_live_secs
            .max(self.cfg.farm.poll_progress_secs)
            + 60;
        let path = self.cfg.heartbeat_path();
        let tmp = path.with_extension("tmp");
        let res = std::fs::write(&tmp, format!("{now} {max_age}\n"))
            .and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(e) = res {
            tracing::debug!("heartbeat file {}: {e}", path.display());
        }
    }

    /// Pings the configured heartbeat URL (healthchecks.io, Uptime Kuma, …) in the background.
    fn ping_heartbeat(&self) {
        let Some(url) = self.cfg.health.url() else {
            return;
        };
        let http = self.health_http.clone();
        tokio::spawn(async move {
            match http.get(&url).send().await {
                Ok(r) if r.status().is_success() => {}
                Ok(r) => tracing::warn!("heartbeat ping returned HTTP {}", r.status()),
                Err(e) => tracing::warn!("heartbeat ping failed: {e}"),
            }
        });
    }

    /// Opens rewards that are still "boxed" so they can be used right away.
    async fn unbox_rewards(&mut self) -> Result<()> {
        let boxed = match self.client.boxed_rewards().await {
            Ok(b) => b,
            Err(e) if api::is_unauthorized(&e) => return Err(e),
            Err(e) => {
                tracing::debug!("boxed rewards: {e:#}");
                return Ok(());
            }
        };
        if boxed.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = boxed.iter().map(|r| r.id.clone()).collect();
        match self.client.mark_unboxed(&ids).await {
            Ok(_) => {
                let names: Vec<String> = boxed
                    .iter()
                    .map(|r| {
                        let what = [r.rarity.as_str(), r.kind.as_str()]
                            .iter()
                            .filter(|x| !x.is_empty())
                            .copied()
                            .collect::<Vec<_>>()
                            .join(" ");
                        let mut name = if what.is_empty() {
                            r.display_label.clone()
                        } else {
                            format!("{} ({what})", r.display_label)
                        };
                        if !r.source_label.is_empty() {
                            name.push_str(&format!(" from {}", r.source_label));
                        }
                        name
                    })
                    .collect();
                tracing::info!("📦 unboxed {}", names.join(", "));
                self.notifier.send(
                    NotifyEvent::RewardClaimed,
                    format!("Unboxed {} reward(s)", boxed.len()),
                    names.join("\n"),
                );
            }
            Err(e) => tracing::warn!("unboxing rewards failed: {e:#}"),
        }
        Ok(())
    }

    fn spawn_conn(&self, label: String) -> Connection {
        Connection::spawn(
            self.client.clone(),
            self.interval,
            self.cfg.experiments.send_handshake,
            self.stats.clone(),
            label,
        )
    }

    // ---------------- live status ----------------

    async fn refresh_and_reconcile(&mut self) -> Result<()> {
        if self.cfg.farm.include_followed {
            match self.client.followed().await {
                Ok(list) => {
                    let base = self.cfg.streamers.len();
                    for (i, f) in list.iter().enumerate() {
                        let slug = f.channel_slug.to_lowercase();
                        let s = self
                            .streamers
                            .entry(slug.clone())
                            .or_insert_with(|| Streamer::new(&slug, base + i, false));
                        s.followed = true;
                        // A followed channel that was a fallback becomes a regular one.
                        s.fallback = false;
                        if !s.configured {
                            s.index = base + i;
                        }
                        if !f.is_live {
                            s.live = None;
                        }
                    }
                    // Unfollowed channels aren't refreshed any more, so their last live state
                    // would stick (and keep a watch slot on a dead stream). Drop them.
                    let followed: HashSet<String> =
                        list.iter().map(|f| f.channel_slug.to_lowercase()).collect();
                    for slug in unfollowed(&self.streamers, &followed) {
                        let configured = {
                            let s = self.streamers.get_mut(&slug).expect("listed above");
                            s.followed = false;
                            s.configured
                        };
                        if !configured {
                            tracing::info!("{slug} is no longer followed");
                            self.stop(&slug, "unfollowed").await;
                            self.streamers.remove(&slug);
                        }
                    }
                    // Only hit /info for followed channels that are actually live.
                    let live: HashSet<String> = list
                        .iter()
                        .filter(|f| f.is_live)
                        .map(|f| f.channel_slug.to_lowercase())
                        .collect();
                    self.refresh_slugs(|s| !s.fallback && (s.configured || live.contains(&s.slug)))
                        .await?;
                }
                Err(e) if api::is_unauthorized(&e) => return Err(e),
                Err(e) => {
                    tracing::warn!("could not fetch followed channels: {e:#}");
                    self.refresh_slugs(|s| !s.fallback).await?;
                }
            }
        } else {
            self.refresh_slugs(|s| !s.fallback).await?;
        }
        // Re-check fallback streams we are currently watching.
        let watched_fallbacks: HashSet<String> = self
            .active
            .keys()
            .filter(|k| self.streamers.get(*k).is_some_and(|s| s.fallback))
            .cloned()
            .collect();
        self.refresh_slugs(|s| watched_fallbacks.contains(&s.slug))
            .await?;

        self.reconcile().await
    }

    async fn refresh_slugs(&mut self, filter: impl Fn(&Streamer) -> bool) -> Result<()> {
        let slugs: Vec<String> = self
            .streamers
            .values()
            .filter(|s| !s.missing && filter(s))
            .map(|s| s.slug.clone())
            .collect();
        for slug in slugs {
            self.refresh_one(&slug).await?;
            tokio::time::sleep(POLITE_DELAY).await;
        }
        Ok(())
    }

    async fn refresh_one(&mut self, slug: &str) -> Result<()> {
        let info = match self.client.channel_info(slug).await {
            Ok(i) => i,
            Err(e) if api::is_not_found(&e) => {
                tracing::warn!("channel `{slug}` does not exist — ignoring it");
                if let Some(s) = self.streamers.get_mut(slug) {
                    s.missing = true;
                    s.live = None;
                    s.checked_at = Some(Instant::now());
                }
                return Ok(());
            }
            Err(e) if api::is_unauthorized(&e) => return Err(e),
            Err(e) => {
                tracing::debug!("info for {slug} failed: {e:#}");
                return Ok(());
            }
        };
        let need_settings;
        let need_points;
        {
            let s = self.streamers.get_mut(slug).expect("streamer exists");
            let was_live = s.live.as_ref().is_some_and(|l| l.is_live);
            s.checked_at = Some(Instant::now());
            s.channel_id = Some(info.id);
            s.banned = info.is_banned;
            s.live = info.livestream.filter(|l| l.is_live);
            let is_live = s.live.is_some();
            if is_live != was_live && !s.fallback {
                tracing::info!(
                    "{} is now {}",
                    slug,
                    if is_live { "LIVE" } else { "offline" }
                );
                if is_live && self.initial_refresh_done {
                    let viewers = s.viewers();
                    self.notifier.send(
                        NotifyEvent::StreamerLive,
                        format!("{slug} is live"),
                        format!("https://kick.com/{slug} — {viewers} viewers"),
                    );
                }
            }
            if !s.fallback
                && let Some(rt) = &self.realtime
            {
                rt.watch_channel(info.id);
            }
            need_settings = is_live && s.points_enabled.is_none();
            // Points are only needed once a channel can be watched (priority, summary), so
            // they're fetched here instead of for every followed channel on the first poll.
            need_points = is_live && !s.fallback && s.points.is_none();
        }
        if need_settings && let Ok(ps) = self.client.points_settings(slug).await {
            let s = self.streamers.get_mut(slug).unwrap();
            s.points_enabled = Some(ps.channel_points_enabled);
            if ps.web_watch_event_interval_seconds != self.interval.as_secs()
                && self.cfg.experiments.watch_event_interval_secs.is_none()
            {
                tracing::debug!(
                    "{slug}: channel watch interval {}s differs from global {}s",
                    ps.web_watch_event_interval_seconds,
                    self.interval.as_secs()
                );
            }
        }
        if need_points {
            match self.client.points(slug).await {
                Ok(p) => {
                    let s = self.streamers.get_mut(slug).expect("streamer exists");
                    s.start_points.get_or_insert(p);
                    s.points = Some(p);
                }
                Err(e) if api::is_unauthorized(&e) => return Err(e),
                Err(e) => tracing::debug!("points for {slug}: {e:#}"),
            }
        }
        Ok(())
    }

    fn cmp_priority(&self, a: &Streamer, b: &Streamer) -> CmpOrdering {
        let opt = |x: Option<i64>| x.unwrap_or(i64::MAX);
        for p in &self.cfg.farm.priority {
            let o = match p {
                Priority::Order => a.index.cmp(&b.index),
                Priority::PointsAscending => opt(a.points).cmp(&opt(b.points)),
                Priority::PointsDescending => b.points.unwrap_or(-1).cmp(&a.points.unwrap_or(-1)),
                Priority::ViewersAscending => a.viewers().cmp(&b.viewers()),
                Priority::ViewersDescending => b.viewers().cmp(&a.viewers()),
                Priority::PointsEnabled => b
                    .points_enabled
                    .unwrap_or(false)
                    .cmp(&a.points_enabled.unwrap_or(false)),
            };
            if o != CmpOrdering::Equal {
                return o;
            }
        }
        a.index.cmp(&b.index)
    }

    async fn reconcile(&mut self) -> Result<()> {
        let max = MAX_CONCURRENT;
        for s in self.streamers.values_mut() {
            s.drop_label = None;
        }

        // 1) Drop campaigns get the first slot(s): they're time-limited, points aren't.
        let mut desired: Vec<String> = Vec::new();
        if self.cfg.drops.farm && self.cfg.drops.slots > 0 {
            match self.pick_drop_streams(self.cfg.drops.slots.min(max)).await {
                Ok(d) => desired = d,
                Err(e) if api::is_unauthorized(&e) => return Err(e),
                Err(e) => tracing::warn!("drop planning failed: {e:#}"),
            }
        }

        // 2) Channel points on configured/followed channels.
        let mut cands: Vec<&Streamer> = self
            .streamers
            .values()
            .filter(|s| !s.fallback && s.target().is_some() && !desired.contains(&s.slug))
            .collect();
        cands.sort_by(|a, b| self.cmp_priority(a, b));
        // Drop progress is split across concurrent streams, so keep the drop stream alone.
        let room = if self.cfg.drops.exclusive && !desired.is_empty() {
            0
        } else {
            max.saturating_sub(desired.len())
        };
        let more: Vec<String> = cands.iter().take(room).map(|s| s.slug.clone()).collect();
        desired.extend(more);

        // 3) Popular fallback is for level XP only. XP is per account, so a single stream is
        // enough, and it's only used when none of the configured/followed channels are live.
        if desired.is_empty() && self.cfg.farm.fallback_to_popular {
            self.fill_with_popular(&mut desired, 1).await?;
        }

        let desired_targets: HashMap<String, WatchTarget> = desired
            .iter()
            .filter_map(|s| Some((s.clone(), self.streamers.get(s)?.target()?)))
            .collect();

        // Stop streams that are no longer wanted or whose livestream changed.
        let to_stop: Vec<(String, &'static str)> = self
            .active
            .iter()
            .filter_map(|(slug, a)| match desired_targets.get(slug) {
                None if self.streamers.get(slug).and_then(|s| s.target()).is_none() => {
                    Some((slug.clone(), "went offline"))
                }
                None => Some((slug.clone(), "higher-priority stream available")),
                Some(t) if *t != a.target => Some((slug.clone(), "new livestream session")),
                Some(_) => None,
            })
            .collect();
        for (slug, why) in to_stop {
            self.stop(&slug, why).await;
        }
        for (slug, t) in desired_targets {
            if !self.active.contains_key(&slug) {
                self.start(t);
            }
        }
        if self.active.is_empty() {
            tracing::info!(
                "nothing live to watch right now; checking again in {}s",
                self.cfg.farm.refresh_live_secs
            );
        }
        Ok(())
    }

    async fn refresh_drop_data(&mut self) -> Result<()> {
        if self
            .drop_campaigns_at
            .is_none_or(|t| t.elapsed() > DROP_CAMPAIGNS_CACHE)
        {
            match self.client.drop_campaigns().await {
                Ok(c) => {
                    self.drop_campaigns = c;
                    self.drop_campaigns_at = Some(Instant::now());
                }
                Err(e) if api::is_unauthorized(&e) => return Err(e),
                Err(e) => tracing::warn!("could not fetch drop campaigns: {e:#}"),
            }
        }
        if self
            .drop_progress_at
            .is_none_or(|t| t.elapsed() > DROP_PROGRESS_CACHE)
        {
            match self.client.drops_progress().await {
                Ok(p) => {
                    self.drop_progress = p;
                    self.drop_progress_at = Some(Instant::now());
                }
                Err(e) if api::is_unauthorized(&e) => return Err(e),
                Err(e) => tracing::debug!("drops progress: {e:#}"),
            }
        }
        Ok(())
    }

    async fn pick_drop_streams(&mut self, slots: usize) -> Result<Vec<String>> {
        self.refresh_drop_data().await?;
        let mut skipped = HashMap::new();
        let plans = drops::plan(
            &self.drop_campaigns,
            &self.drop_progress,
            &self.cfg.drops,
            &mut skipped,
        );
        for (id, msg) in skipped {
            if self.drop_skip_logged.insert(id) {
                tracing::info!("{msg}");
            }
        }
        // Streams we were farming drops on that left the campaign's category.
        for (cid, slug) in &self.drop_current {
            let (Some(plan), Some(s)) = (
                plans.iter().find(|p| &p.campaign_id == cid),
                self.streamers.get(slug),
            ) else {
                continue;
            };
            if let Some(live) = s.live.as_ref().filter(|l| !l.in_category(plan.category_id)) {
                tracing::warn!(
                    "drops: {slug} switched category to {} — it no longer counts for `{}` ({}), switching stream",
                    live.category_names(),
                    plan.campaign_name,
                    plan.category_name
                );
            }
        }
        let mut out: Vec<String> = Vec::new();
        let mut current = Vec::new();
        for plan in &plans {
            if out.len() >= slots {
                break;
            }
            match self.find_drop_stream(plan, &out).await? {
                Some(slug) => {
                    if let Some(s) = self.streamers.get_mut(&slug) {
                        s.drop_label =
                            Some(format!("{} `{}`", plan.kind.label(), plan.reward_name));
                        s.drop_farmed = true;
                    }
                    current.push((plan.campaign_id.clone(), slug.clone()));
                    if !self
                        .drop_current
                        .contains(&(plan.campaign_id.clone(), slug.clone()))
                    {
                        tracing::info!(
                            "drops: farming {} `{}` from `{}` ({}) on {slug} — {} min to go",
                            plan.kind.label(),
                            plan.reward_name,
                            plan.campaign_name,
                            plan.category_name,
                            plan.remaining_minutes
                        );
                    }
                    out.push(slug);
                }
                None => tracing::debug!(
                    "drops: no live stream for `{}` ({})",
                    plan.campaign_name,
                    plan.category_name
                ),
            }
        }
        if current.is_empty() && !self.drop_current.is_empty() {
            tracing::info!("drops: nothing farmable live right now");
        }
        self.drop_current = current;
        Ok(out)
    }

    async fn category_live(&mut self, category_id: u64) -> Result<Vec<String>> {
        if let Some((t, v)) = self.category_live.get(&category_id)
            && t.elapsed() < CATEGORY_CACHE
        {
            return Ok(v.clone());
        }
        let v = match self.client.live_in_category(category_id, 100).await {
            Ok(v) => v,
            Err(e) if api::is_unauthorized(&e) => return Err(e),
            Err(e) => {
                tracing::debug!("live streams in category {category_id}: {e:#}");
                Vec::new()
            }
        };
        self.category_live
            .insert(category_id, (Instant::now(), v.clone()));
        Ok(v)
    }

    async fn find_drop_stream(
        &mut self,
        plan: &DropPlan,
        exclude: &[String],
    ) -> Result<Option<String>> {
        let ok = |s: &Streamer| {
            !exclude.contains(&s.slug)
                && s.target().is_some()
                && s.live
                    .as_ref()
                    .is_some_and(|l| l.in_category(plan.category_id))
                && plan.accepts(&s.slug)
        };
        // 1) A configured/followed channel that qualifies also keeps earning points.
        let mut regs: Vec<&Streamer> = self
            .streamers
            .values()
            .filter(|s| !s.fallback && ok(s))
            .collect();
        regs.sort_by(|a, b| self.cmp_priority(a, b));
        if let Some(s) = regs.first() {
            return Ok(Some(s.slug.clone()));
        }
        // 2) Stay on the stream we're already farming this campaign on.
        if let Some(slug) = self
            .active
            .keys()
            .find(|k| self.streamers.get(*k).is_some_and(ok))
        {
            return Ok(Some(slug.clone()));
        }
        // 3) Any live participating stream in the campaign's category.
        let live = self.category_live(plan.category_id).await?;
        let mut pool: Vec<String> = live
            .into_iter()
            .filter(|s| plan.accepts(s) && !exclude.contains(s))
            .take(6)
            .collect();
        if pool.is_empty() && !plan.channels.is_empty() {
            // Participating channels may be beyond the first page of the category.
            pool = plan
                .channels
                .iter()
                .filter(|s| !exclude.contains(s))
                .take(8)
                .cloned()
                .collect();
        }
        for slug in pool {
            // Reuse a recent check: otherwise offline participants were fetched again on
            // every refresh, for every campaign without a live stream.
            let fresh = self
                .streamers
                .entry(slug.clone())
                .or_insert_with(|| Streamer::new(&slug, usize::MAX, true))
                .recently_checked();
            if !fresh {
                self.refresh_one(&slug).await?;
            }
            if self.streamers.get(&slug).is_some_and(ok) {
                return Ok(Some(slug));
            }
            if !fresh {
                tokio::time::sleep(POLITE_DELAY).await;
            }
        }
        Ok(None)
    }

    async fn fill_with_popular(&mut self, desired: &mut Vec<String>, max: usize) -> Result<()> {
        // Keep fallback streams we're already on (sticky) as long as they are live.
        let mut current: Vec<String> = self
            .active
            .keys()
            .filter(|k| {
                self.streamers
                    .get(*k)
                    .is_some_and(|s| s.fallback && !s.drop_farmed && s.target().is_some())
            })
            .cloned()
            .collect();
        current.sort();
        for slug in current {
            if desired.len() >= max {
                return Ok(());
            }
            if !desired.contains(&slug) {
                desired.push(slug);
            }
        }
        if desired.len() >= max {
            return Ok(());
        }
        if self.popular_at.is_none_or(|t| t.elapsed() > POPULAR_CACHE) {
            match self
                .client
                .popular_streams(&self.cfg.farm.fallback_language, 24)
                .await
            {
                Ok(p) => {
                    self.popular = p;
                    self.popular_at = Some(Instant::now());
                }
                Err(e) if api::is_unauthorized(&e) => return Err(e),
                Err(e) => tracing::warn!("could not fetch popular streams: {e:#}"),
            }
        }
        // Random pick for variety (XP is the same on any stream, big or small).
        let mut pool: Vec<String> = self
            .popular
            .iter()
            .map(|p| p.slug.to_lowercase())
            .filter(|s| !desired.contains(s) && !self.streamers.get(s).is_some_and(|x| !x.fallback))
            .collect();
        fastrand::shuffle(&mut pool);
        for slug in pool {
            if desired.len() >= max {
                break;
            }
            self.streamers
                .entry(slug.clone())
                .or_insert_with(|| Streamer::new(&slug, usize::MAX, true));
            self.refresh_one(&slug).await?;
            if self.streamers.get(&slug).and_then(|s| s.target()).is_some() {
                desired.push(slug);
            }
            tokio::time::sleep(POLITE_DELAY).await;
        }
        Ok(())
    }

    fn start(&mut self, t: WatchTarget) {
        let s = &self.streamers[&t.slug];
        tracing::info!(
            "▶ watching {}{} (channel {}, livestream {}{})",
            t.slug,
            match (&s.drop_label, s.fallback) {
                (Some(d), true) => format!(" [drops: {d}]"),
                (Some(d), false) => format!(" [drops: {d} + points]"),
                (None, true) => " [fallback: level XP only]".to_string(),
                (None, false) => String::new(),
            },
            t.channel_id,
            t.livestream_id,
            match s.points_enabled {
                _ if s.fallback => "",
                Some(true) => ", channel points on",
                Some(false) => ", channel points off",
                None => "",
            }
        );
        let conn = match &self.shared {
            Some(shared) => {
                shared.add(t.clone());
                None
            }
            None => {
                let c = self.spawn_conn(t.slug.clone());
                c.add(t.clone());
                Some(c)
            }
        };
        if let Some(rt) = &self.realtime {
            rt.watch_livestream(t.livestream_id);
        }
        self.active.insert(
            t.slug.clone(),
            Active {
                target: t,
                since: Instant::now(),
                conn,
            },
        );
    }

    async fn stop(&mut self, slug: &str, why: &str) {
        let Some(a) = self.active.remove(slug) else {
            return;
        };
        if let Some(rt) = &self.realtime {
            rt.unwatch_livestream(a.target.livestream_id);
        }
        let elapsed = a.since.elapsed();
        if let Some(s) = self.streamers.get_mut(slug) {
            s.watched += elapsed;
        }
        match a.conn {
            Some(c) => c.shutdown().await,
            None => {
                if let Some(shared) = &self.shared {
                    shared.remove(a.target.channel_id);
                }
            }
        }
        tracing::info!("■ stopped {slug} after {} ({why})", fmt_dur(elapsed));
    }

    // ---------------- progress ----------------

    async fn poll_progress(&mut self) -> Result<()> {
        // Account level / XP
        let level = self.client.level().await?;
        let start = self
            .start_level
            .get_or_insert_with(|| level.clone())
            .clone();
        let gained = level.total_xp.saturating_sub(start.total_xp);
        let since_last = self
            .last_level
            .as_ref()
            .map(|l| level.total_xp.saturating_sub(l.total_xp))
            .unwrap_or(0);
        let hours = self.started.elapsed().as_secs_f64() / 3600.0;
        self.announce_level(level.level, Some(&level));
        tracing::info!(
            "level {} — {}/{} XP to next (total {}, +{} since last poll, +{} this session{})",
            level.level,
            level.progress_xp,
            level.progress_xp + level.xp_to_next_level,
            level.total_xp,
            since_last,
            gained,
            if hours > 0.05 {
                format!(", ~{:.0} XP/h", gained as f64 / hours)
            } else {
                String::new()
            }
        );
        self.last_level = Some(level.clone());

        // Channel points on what we're watching. Live channels get their first balance in
        // `refresh_one`; offline ones aren't needed (this used to fetch every followed channel,
        // and retry the failures on every poll).
        let slugs: Vec<String> = self
            .active
            .keys()
            // Channel points are only farmed/tracked on configured or followed channels.
            .filter(|k| self.streamers.get(*k).is_some_and(|s| !s.fallback))
            .cloned()
            .collect();
        for slug in slugs {
            match self.client.points(&slug).await {
                Ok(p) => {
                    let s = self.streamers.get_mut(&slug).unwrap();
                    let prev = s.points;
                    s.start_points.get_or_insert(p);
                    s.points = Some(p);
                    if let Some(prev) = prev
                        && p != prev
                    {
                        tracing::info!(
                            "{slug}: {:+} channel points (now {p}, +{} this session)",
                            p - prev,
                            p - s.start_points.unwrap_or(p)
                        );
                    }
                }
                Err(e) if api::is_unauthorized(&e) => return Err(e),
                Err(e) => tracing::debug!("points for {slug}: {e:#}"),
            }
            tokio::time::sleep(POLITE_DELAY).await;
        }

        if self.cfg.farm.claim_daily_rewards {
            self.check_challenges().await?;
        }
        if self.cfg.drops.claim || self.cfg.drops.farm {
            self.check_drops().await?;
        }
        if self.cfg.farm.unbox_rewards {
            self.unbox_rewards().await?;
        }
        self.write_analytics(&level);
        Ok(())
    }

    async fn check_challenges(&mut self) -> Result<()> {
        let list = match self.client.challenges().await {
            Ok(l) => l,
            Err(e) if api::is_unauthorized(&e) => return Err(e),
            Err(e) => {
                tracing::debug!("challenges: {e:#}");
                return Ok(());
            }
        };
        for c in list {
            let key = format!("{}:{}", c.id, c.status);
            if c.is_claimable() && !self.claimed.contains(&c.id) {
                match self.client.claim_challenge(&c.id).await {
                    Ok(v) => {
                        self.claimed.insert(c.id.clone());
                        tracing::info!(
                            "🎁 claimed `{}`: {}",
                            c.name,
                            v.to_string().chars().take(400).collect::<String>()
                        );
                        self.notifier.send(
                            NotifyEvent::RewardClaimed,
                            format!("Claimed {}", c.name),
                            format!(
                                "{}/{} {} reached — open kick.com to unbox it.",
                                c.condition.progress,
                                c.condition.threshold,
                                c.condition.kind.replace('_', " ")
                            ),
                        );
                    }
                    Err(e) => tracing::warn!("claiming `{}` failed: {e:#}", c.name),
                }
            } else if c.condition.kind == "watch_time_minutes" && c.status == "in_progress" {
                tracing::info!(
                    "challenge `{}`: {}/{} min watched",
                    c.name,
                    c.condition.progress,
                    c.condition.threshold
                );
            } else {
                tracing::debug!("challenge `{}` [{key}]", c.name);
            }
        }
        Ok(())
    }

    async fn check_drops(&mut self) -> Result<()> {
        let campaigns = match self.client.drops_progress().await {
            Ok(c) => c,
            Err(e) if api::is_unauthorized(&e) => return Err(e),
            Err(e) => {
                tracing::debug!("drops progress: {e:#}");
                return Ok(());
            }
        };
        self.drop_progress = campaigns.clone();
        self.drop_progress_at = Some(Instant::now());
        for c in campaigns {
            let needs_link = !c.connect_url.is_empty() && !c.user_app_connected;
            let category = c.category.as_ref().map(|x| x.name.as_str()).unwrap_or("?");
            for r in &c.rewards {
                if !self.cfg.drops.claim {
                    break;
                }
                if !r.is_claimable(c.progress_units) || self.drops_claimed.contains(&r.id) {
                    continue;
                }
                if self
                    .drops_failed
                    .get(&r.id)
                    .is_some_and(|t| t.elapsed() < DROP_RETRY)
                {
                    continue;
                }
                match self.client.claim_drop(&r.id, &c.id).await {
                    Ok(_) => {
                        self.drops_claimed.insert(r.id.clone());
                        self.drop_progress_at = None; // re-plan with fresh progress
                        tracing::info!("🎁 claimed drop `{}` ({} / {category})", r.name, c.name);
                        self.notifier.send(
                            NotifyEvent::DropClaimed,
                            format!("Drop claimed: {}", r.name),
                            format!("Campaign **{}** ({category})", c.name),
                        );
                    }
                    Err(e) => {
                        self.drops_failed.insert(r.id.clone(), Instant::now());
                        let hint = if needs_link {
                            format!(
                                " — this campaign needs a linked account: {}",
                                c.connect_url.split('?').next().unwrap_or("")
                            )
                        } else {
                            String::new()
                        };
                        tracing::warn!("claiming drop `{}` failed: {e:#}{hint}", r.name);
                    }
                }
                tokio::time::sleep(POLITE_DELAY).await;
            }
            // Progress towards the next unclaimed reward (logged only when it changes).
            if let Some(next) = c
                .rewards
                .iter()
                .find(|r| !r.claimed && !self.drops_claimed.contains(&r.id))
                && self
                    .drop_units_logged
                    .insert(c.id.clone(), c.progress_units)
                    != Some(c.progress_units)
            {
                tracing::info!(
                    "drop `{}` ({category}): {}/{} min{}",
                    next.name,
                    c.progress_units.min(next.required_units),
                    next.required_units,
                    if needs_link {
                        " (needs linked account to claim)"
                    } else {
                        ""
                    }
                );
            }
        }
        Ok(())
    }

    fn write_analytics(&self, level: &Level) {
        let Some(path) = &self.cfg.experiments.analytics_file else {
            return;
        };
        let points: HashMap<&str, i64> = self
            .streamers
            .values()
            .filter(|s| !s.fallback)
            .filter_map(|s| Some((s.slug.as_str(), s.points?)))
            .collect();
        let watching: Vec<&str> = self.active.keys().map(String::as_str).collect();
        let events: u64 = self.stats.events.lock().unwrap().values().sum();
        let line = json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "level": level.level,
            "total_xp": level.total_xp,
            "progress_xp": level.progress_xp,
            "watching": watching,
            "concurrent": watching.len(),
            "shared_socket": self.cfg.experiments.shared_socket,
            "interval_secs": self.interval.as_secs(),
            "watch_events_sent": events,
            "points": points,
        });
        rotate_if_larger(path, ANALYTICS_MAX_BYTES);
        let res = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = res {
            tracing::debug!("analytics write failed: {e}");
        }
    }

    fn summary(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let elapsed = self.started.elapsed();
        let _ = writeln!(
            out,
            "──────── session summary ({}) ────────",
            fmt_dur(elapsed)
        );
        if let (Some(a), Some(b)) = (&self.start_level, &self.last_level) {
            let _ = writeln!(
                out,
                "level {} -> {}   XP {} -> {} (+{})",
                a.level,
                b.level,
                a.total_xp,
                b.total_xp,
                b.total_xp.saturating_sub(a.total_xp)
            );
        }
        let mut rows: Vec<&Streamer> = self
            .streamers
            .values()
            .filter(|s| s.watched > Duration::ZERO)
            .collect();
        rows.sort_by_key(|s| std::cmp::Reverse(s.watched));
        for s in rows {
            if s.fallback {
                let tag = if s.drop_farmed {
                    "[drops only]"
                } else {
                    "[fallback: level XP only]"
                };
                let _ = writeln!(
                    out,
                    "  {:<24} watched {:>7}  {tag}",
                    s.slug,
                    fmt_dur(s.watched)
                );
                continue;
            }
            let gained = match (s.start_points, s.points) {
                (Some(a), Some(b)) => format!("{:+}", b - a),
                _ => "-".into(),
            };
            let events = s
                .channel_id
                .map(|id| self.stats.events_for(id))
                .unwrap_or(0);
            let _ = writeln!(
                out,
                "  {:<24} watched {:>7}  events {:>4}  points {:>8} ({gained})",
                s.slug,
                fmt_dur(s.watched),
                events,
                s.points.map(|p| p.to_string()).unwrap_or("-".into()),
            );
        }
        out.trim_end().to_string()
    }
}

/// Resolves on Ctrl-C (SIGINT) or SIGTERM (`docker stop`, systemd).
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

pub async fn status(cfg: &Config, client: &KickClient) -> Result<()> {
    let me = match client.me().await {
        Ok(u) => u,
        Err(e) if api::is_unauthorized(&e) => {
            bail!("session token rejected — run `kick-level-farm login`")
        }
        Err(e) => return Err(e),
    };
    println!("account: {} (id {})", me.username, me.id);
    let l = client.level().await?;
    println!(
        "level:   {}  ({}/{} XP to next, total {})",
        l.level,
        l.progress_xp,
        l.progress_xp + l.xp_to_next_level,
        l.total_xp
    );
    if let Ok(s) = client.streak().await {
        println!(
            "streak:  {}",
            s.get("current_streak").cloned().unwrap_or_default()
        );
    }
    if let Ok(list) = client.challenges().await {
        for c in list {
            println!(
                "challenge: {:<20} {:>4}/{:<4} {:<20} [{}]{}",
                c.name,
                c.condition.progress,
                c.condition.threshold,
                c.condition.kind,
                c.status,
                if c.is_claimable() {
                    "  <- claimable"
                } else {
                    ""
                }
            );
        }
    }
    let progress = client.drops_progress().await;
    if let (Ok(campaigns), Ok(progress)) = (client.drop_campaigns().await, &progress) {
        let mut skipped = HashMap::new();
        let plans = drops::plan(&campaigns, progress, &cfg.drops, &mut skipped);
        for p in &plans {
            println!(
                "drop plan: {:<6} {:<28} {:>5} min left  `{}` ({}){}",
                p.kind.label(),
                p.reward_name,
                p.remaining_minutes,
                p.campaign_name,
                p.category_name,
                if p.channels.is_empty() {
                    String::new()
                } else {
                    format!(", {} channel(s)", p.channels.len())
                }
            );
        }
        for msg in skipped.values() {
            println!("drop plan: {msg}");
        }
    }
    if let Ok(drops) = progress {
        for c in drops {
            for r in c.rewards.iter().filter(|r| !r.claimed) {
                println!(
                    "drop:    {:<30} {:>4}/{:<4} min  ({}){}{}",
                    r.name,
                    c.progress_units.min(r.required_units),
                    r.required_units,
                    c.name,
                    if r.is_claimable(c.progress_units) {
                        "  <- claimable"
                    } else {
                        ""
                    },
                    if !c.connect_url.is_empty() && !c.user_app_connected {
                        "  [needs linked account]"
                    } else {
                        ""
                    }
                );
            }
        }
    }
    if !cfg.streamers.is_empty() {
        println!(
            "\n{:<24} {:<8} {:>10}  points",
            "streamer", "live", "viewers"
        );
    }
    for slug in &cfg.streamers {
        let info = client.channel_info(slug).await;
        let pts = client.points(slug).await.ok();
        let enabled = client
            .points_settings(slug)
            .await
            .ok()
            .map(|p| p.channel_points_enabled);
        match info {
            Ok(i) => {
                let live = i.livestream.as_ref().filter(|l| l.is_live);
                println!(
                    "{:<24} {:<8} {:>10}  {}{}",
                    slug,
                    if live.is_some() { "LIVE" } else { "offline" },
                    live.map(|l| l.viewer_count.to_string()).unwrap_or_default(),
                    pts.map(|p| p.to_string()).unwrap_or("-".into()),
                    if enabled == Some(false) {
                        " (points disabled)"
                    } else {
                        ""
                    }
                );
            }
            Err(e) => println!("{slug:<24} error: {e:#}"),
        }
        tokio::time::sleep(POLITE_DELAY).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfollowed_channels_are_detected() {
        let mut streamers = HashMap::new();
        for (slug, followed) in [("still", true), ("gone", true), ("configured", false)] {
            let mut s = Streamer::new(slug, 0, false);
            s.followed = followed;
            streamers.insert(slug.to_string(), s);
        }
        let list: HashSet<String> = ["still".to_string()].into();
        assert_eq!(unfollowed(&streamers, &list), ["gone"]);
        assert!(unfollowed(&streamers, &["still".into(), "gone".into()].into()).is_empty());
    }

    #[test]
    fn recent_checks_are_reused() {
        let mut s = Streamer::new("x", 0, true);
        assert!(!s.recently_checked());
        s.checked_at = Some(Instant::now());
        assert!(s.recently_checked());
    }

    #[tokio::test]
    async fn timers_do_not_burst_after_a_slow_step() {
        let t = every(Duration::from_secs(90));
        assert_eq!(
            t.missed_tick_behavior(),
            tokio::time::MissedTickBehavior::Delay
        );
    }

    #[test]
    fn analytics_log_is_rotated() {
        let dir = std::env::temp_dir().join(format!("klf-rotate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("analytics.jsonl");
        let old = dir.join("analytics.jsonl.1");
        std::fs::write(&path, "0123456789").unwrap();
        rotate_if_larger(&path, 10);
        assert!(path.exists() && !old.exists(), "not over the limit yet");
        std::fs::write(&path, "0123456789A").unwrap();
        rotate_if_larger(&path, 10);
        assert!(!path.exists());
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "0123456789A");
        // The next rotation replaces the old file.
        std::fs::write(&path, "second rotation").unwrap();
        rotate_if_larger(&path, 10);
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "second rotation");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
