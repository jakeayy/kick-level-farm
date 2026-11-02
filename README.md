# kick-level-farm

A lightweight Rust bot that farms **Kick.com account levels (XP)**, **channel points**,
**Daily Reward chests** and **drops** by emulating the kick.com web player's viewer presence. It doesn't
download any video. Each watched stream costs one small websocket, which comes to a few KB per
minute.

The idea comes from [Twitch-Channel-Points-Miner-v2](https://github.com/rdavydov/Twitch-Channel-Points-Miner-v2).
The protocol was reverse-engineered from the live kick.com web app (Oct 2026).

> ⚠️ This automates an undocumented API and probably goes against Kick's ToS. Accounts can be
> flagged. Try it on an account you can afford to lose.

## Table of contents

- [Features](#features)
- [Usage](#usage)
  - [Discord notifications](#discord-notifications)
  - [Docker (VPS)](#docker-vps)
- [How Kick tracks watching (findings)](#how-kick-tracks-watching-findings)
  - [Measured results](#measured-results-bot-only-no-browser-open-oct-31-2026)
- [Experiments](#experiments)
- [Layout](#layout)
- [Ideas not done yet](#ideas-not-done-yet)

## Features

- Watches up to `max_concurrent` live streams at once and picks them by a chain of priorities
  (`order`, `points_ascending`, `points_descending`, `viewers_ascending`, `viewers_descending`,
  `points_enabled`).
- By default it farms every channel you follow (`include_followed = true`). You can add more
  channels in `streamers`.
- **Popular-stream fallback (level XP only):** XP is per account, so when none of your
  configured or followed channels are live, the bot watches **one** popular stream in your
  language. It doesn't farm or track channel points there. Points are only farmed on channels
  in `streamers` and, if `include_followed` is on, channels you follow.
- Polls level/XP and channel points, logs the deltas and estimates XP per hour.
- Claims Daily Reward and event challenges (e.g. *Trick or Treat*) automatically.
- **Drops:** the bot actively watches streams that take part in drop campaigns, then claims each
  reward once it reaches 100%.
  - Priority is **badges → emotes → items**. Ties go to the campaign ending soonest, then the
    reward closest to done.
  - **Items are only farmed when you turn them on** (`[drops] items = true`).
  - Campaigns that need a linked game account are skipped once Kick reports the account isn't
    linked, since their rewards couldn't be claimed anyway.
  - **Kick splits drop watch time across all streams you watch at once.** In a test, 2 streams
    gave half the drop speed (+6 min of progress in 13 min, against +12 with the drops stream
    alone). So by default (`[drops] exclusive = true`) the bot watches **only** the drops
    stream while a drop is being farmed. Set it to `false` to keep farming points on other
    channels at half drop speed.
  - Channels you follow or list are preferred when they qualify, so they keep earning points
    too. A drops-only stream earns no tracked points, the same as the popular fallback.
  - **Category changes:** the bot subscribes to each watched livestream's realtime updates. If
    a drops stream switches to another category, it logs the change and moves to another
    qualifying stream within seconds. Every refresh also re-checks the category as a backup.
  - Kick has no reward-type field, so the type comes from the name ("…Badge", "…Emote").
    Rewards from publishers that need a linked account count as items, and other Kick-native
    rewards count as emotes.
- **Realtime updates:** subscribes to Kick's push service, the same one the website uses. Point
  gains, level-ups and channels going live or offline show up instantly instead of waiting for
  the next poll. Polling stays on as a backup.
- **Discord notifications** through a webhook. They cover start/stop (with the session
  summary), level-ups, claimed rewards and drops, streamers going live, and errors such as an
  expired token. You choose which events are sent.
- Reconnects with backoff, follows livestream restarts (a new `livestream_id`), and shuts down
  cleanly on Ctrl-C with a session summary.
- Built for experiments. The `[experiments]` section and the `analytics.jsonl` log let you measure
  what actually earns XP.

## Usage

```bash
cargo build --release
./target/release/kick-level-farm init      # writes config.toml
./target/release/kick-level-farm login     # paste your session_token cookie
./target/release/kick-level-farm status    # level, XP, challenges, drop plan + progress, points
./target/release/kick-level-farm notify-test   # check the Discord webhook
./target/release/kick-level-farm           # start farming (-v for debug logs)
```

### Discord notifications

In Discord, go to Server Settings → Integrations → Webhooks → New Webhook and copy the URL. Then
add it to `config.toml`:

```toml
[notifications]
discord_webhook_url = "https://discord.com/api/webhooks/..."
events = ["started", "stopped", "level_up", "reward_claimed", "drop_claimed", "streamer_live", "error"]
```

You can also pass the URL in the `KICK_DISCORD_WEBHOOK` env var. Run `kick-level-farm notify-test`
to check that it works.

### Docker (VPS)

Images for `linux/amd64` and `linux/arm64` are published to
`ghcr.io/jakeayy/kick-points-farm` on every push to `main`. Each is about 24 MB: a static musl
binary of about 4 MB on Alpine. Each architecture is built natively on GitHub's runners.

```bash
mkdir kick-farm && cd kick-farm
curl -O https://raw.githubusercontent.com/jakeayy/kick-points-farm/main/docker-compose.yml
docker compose up -d                                  # first start writes data/config.toml
docker compose run --rm kick-level-farm login         # paste your session_token
docker compose logs -f                                # the bot picks the token up within ~20s
```

- `./data` holds `config.toml`, `.kick_token` and `analytics.jsonl`. Edit the config there,
  then run `docker compose restart`.
- The container runs as `PUID`/`PGID` (default 1000). It fixes the ownership of `./data` on
  start, so a root-owned folder on the VPS is fine.
- You can set `KICK_TOKEN` and `KICK_DISCORD_WEBHOOK` in `docker-compose.yml` instead of
  using the files.
- **Expired token:** the bot doesn't crash-loop (and spam Discord). It logs the problem and waits
  until you run `login` again, then resumes on its own (`--wait-for-login`).
- `docker stop` / `docker compose down` shut down cleanly with the session summary and the
  Discord "stopped" message.
- Other commands: `docker compose run --rm kick-level-farm status` and
  `docker compose run --rm kick-level-farm notify-test`.
- To build from source instead of pulling, uncomment `build: .` in `docker-compose.yml` and
  run `docker compose up -d --build`.

GHCR packages are private by default. Either make the package public (GitHub → Packages →
kick-points-farm → Package settings), or run `docker login ghcr.io` on the VPS with a token
that has the `read:packages` scope.

Kick's login has a captcha and 2FA, so the bot reuses a browser session instead of logging in
itself. To get the token: DevTools → Application → Cookies → `https://kick.com` → copy the
`session_token` value (it starts with `kat_`). You can also pass it in the `KICK_TOKEN` env var.

## How Kick tracks watching (findings)

| What | How |
|---|---|
| Auth | `Authorization: Bearer <session_token cookie>` works on `kick.com/api/*`, `web.kick.com/api/*` and `websockets.kick.com`. Plain HTTP clients aren't blocked by Cloudflare. |
| Viewer token | `GET https://websockets.kick.com/viewer/v1/token` with header `X-CLIENT-TOKEN: e1393935…f823` (a public constant from the JS bundle) returns `{data:{token}}` |
| Presence socket | `wss://websockets.kick.com/viewer/v1/connect?token=<token>` |
| Watch event | `{"type":"user_event","data":{"message":{"name":"tracking.user.watch.livestream","channel_id",livestream_id,"vod_id"}}}`, sent when playback starts and then once per `interval` of playing time |
| Interval | `GET /api/internal/settings/global` → `events["tracking.user.watch.livestream"].interval` (120s). Per-channel: `/api/v2/channels/{slug}/channel-points/channel-settings` → `web_watch_event_interval_seconds` |
| Keep-alives | `channel_handshake {channelId}` every 15s, `{"type":"ping"}` every 30s (the server answers `pong`), `channel_disconnect` when leaving |
| Stream ids | `GET /api/v2/channels/{slug}/info` → `id`, `livestream.id`, `livestream.vod_id` |
| Level / XP | `GET https://web.kick.com/api/v1/gamification/user/level` → `level, progress_xp, xp_to_next_level, total_xp` (shown in chat as the `level` badge) |
| Points | `GET /api/v2/channels/{slug}/points` |
| Daily rewards | `GET web.kick.com/api/v1/gamification/challenges` (`condition.type = watch_time_minutes`), claim with `POST …/challenges/{id}/claim` |
| Live by category | `GET web.kick.com/api/v1/livestreams?category_id={id}&limit=100&sort=viewer_count_desc` |
| Drops | `GET web.kick.com/api/v1/drops/progress` → campaigns with `progress_units` (minutes) and `rewards[{id, claimed, progress 0..1, required_units}]`; claim with `POST /v1/drops/claim {reward_id, campaign_id}`. All campaigns: `GET /v1/drops/campaigns`. Some campaigns need `connect_url` (a linked game account). |
| Realtime | `POST web.kick.com/api/v1/realtime/connection` → Centrifugo websocket URL; `POST /v1/realtime/auth/connection {client_id}` → connection JWT (~45 min TTL); `POST /v1/realtime/auth/channel {channel}` → per-private-channel JWT. JSON protocol: `{"connect":{"token","name":"js"},"id":1}`, `{"subscribe":{"channel","token","flag":1},"id":n}`. The server pings with `{}` and expects `{}` back. |
| Realtime events | `private-livestream.{id}` (needs a token) → `App\Events\LivestreamUpdated` (title/category changes); `private-channelpoints-{uid}` → `PointsUpdated {channel_id, points, balance, reason}`; `private-{uid}` → `GamificationLevelUp {level}`, `GamificationWatchTimeGoalReached`; `channel.{channel_id}` → `App\Events\StreamerIsLive`, `App\Events\StopStreamBroadcast`; `drops_category_{id}` → `drops_campaign_started` |
| Other | `/v1/gamification/streak`, `/v1/gamification/collectibles`, `/v1/rewards` |

### Measured results (bot only, no browser open, Oct 31 2026)

| Run | XP | Channel points | Daily-reward watch minutes |
|---|---|---|---|
| 2 streams, 10 min | +16 (about 4 XP per 2 min) | **+20 on each channel** | +8 (counted once, not per stream) |
| 1 stream, 8 min | +12 (same rate), level 3 → 4 | +10 on the one channel | same |
| 2 streams, 13 min | +24 (about 110 XP/h) | +20 on each channel | Daily Reward **auto-claimed** at 60/60 |
| Drops: MW4 stream + 1 other, 13 min | about 120 XP/h | — | MW4 drop +6 min (**half speed**) |
| Drops: MW4 stream alone, 13 min | about 110 XP/h | — | MW4 drop +12 min (full speed) |
| 2 streams, 10 min, **30s event interval** (4× the messages) | +16, no change | +20 on each channel, no change | no change |
| **5 streams**, 15 min (3 followed + 2 explicit, 1 with points off) | +32 (~120 XP/h), level 4 → 5 | only **2 channels earned at a time**: buddha +20, noodlewolf +20, niter +10 (first 2 min only), rraenee +0 | no change |

What this means so far:

- **XP is earned per account, not per stream.** It doesn't go up with `max_concurrent`.
- **Channel points are earned per channel** (about 10 every 5 minutes), so watching several
  streams at once multiplies points.
- Watch-time challenges count real time once, however many streams you watch.
- **Channel points seem to be capped at 2 channels at once** (like Twitch). With 5 watched,
  every point drop went to at most 2 channels, and the server, not the bot, picked which ones.
  Keep `max_concurrent = 2` so your priority order decides which channels earn.
- **Sending watch events more often doesn't help.** The server credits by elapsed time (4 XP
  every 2 minutes) and ignores the extra events. It didn't reject or disconnect them either.

## Experiments

These options are in `config.toml` under `[experiments]`. Each progress poll appends a JSON line
to `analytics.jsonl` (`total_xp`, `concurrent`, `points`, `watch_events_sent`, …). That lets you
compare settings by XP per hour:

- **Concurrency:** does XP scale with `max_concurrent`, or is it capped per account?
- **`shared_socket`:** send all channels over one presence socket instead of one socket per
  channel.
- **`watch_event_interval_secs`:** send watch events more often than 120s. Does the server
  count them, ignore them, or flag the account?
- **`send_handshake = false`:** are the 15s handshakes needed at all?

The bot also logs any server message type on the presence socket that it hasn't seen before, to
help work out the rest of the protocol.

Quick XP/hour from the analytics file:

```bash
jq -s '(.[-1].total_xp - .[0].total_xp) / (((.[-1].ts|sub("\\..*";"")+"Z"|fromdate) - (.[0].ts|sub("\\..*";"")+"Z"|fromdate))/3600)' analytics.jsonl
```

## Layout

```
src/api.rs      HTTP client and response types for the endpoints above
src/watcher.rs  viewer presence websocket (one task per connection, multiplexing supported)
src/realtime.rs Centrifugo client for instant points / level-up / go-live events
src/drops.rs    drop campaign planner (reward type, priority, eligibility)
src/notify.rs   Discord webhook notifications
src/miner.rs    orchestrator: live refresh, priority selection, fallback, polling, claiming, analytics
src/config.rs   TOML config and token handling
Dockerfile, docker/entrypoint.sh, docker-compose.yml   container image + VPS setup
.github/workflows/docker.yml   builds and publishes the multi-arch image to GHCR
```

## Ideas not done yet

- Spend points on rewards (`/api/v2/channels/{slug}/rewards/{id}/redeem`).
- Run several accounts from one process.
- Telegram notifications.
