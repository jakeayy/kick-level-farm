# Research notes

Notes on how Kick tracks watching, what was measured, and the experiment knobs used along the
way. They were reverse-engineered from the live kick.com web app (Oct–Nov 2026) and can go stale
when Kick changes its client. Usage docs are in the [README](README.md).

## Table of contents

- [How Kick tracks watching](#how-kick-tracks-watching)
- [Measured results](#measured-results)
- [Experiments](#experiments)
- [Code layout](#code-layout)

## How Kick tracks watching

| What | How |
|---|---|
| Auth | `Authorization: Bearer <session_token cookie>` works on `kick.com/api/*`, `web.kick.com/api/*` and `websockets.kick.com`. An invalid token gets `200 {}` from `/api/v1/user`, not a 401. |
| Firewall | Kick's WAF answers reqwest's HTTP/1.1 requests with `403 "Request blocked by security policy"`. HTTP/2 works, and so does curl over HTTP/1.1. |
| Viewer token | `GET https://websockets.kick.com/viewer/v1/token` with header `X-CLIENT-TOKEN: e1393935…f823` (a public constant from the JS bundle) returns `{data:{token}}` |
| Presence socket | `wss://websockets.kick.com/viewer/v1/connect?token=<token>` |
| Watch event | `{"type":"user_event","data":{"message":{"name":"tracking.user.watch.livestream","channel_id",livestream_id,"vod_id"}}}`, sent when playback starts and then once per `interval` of playing time |
| Interval | `GET /api/internal/settings/global` → `events["tracking.user.watch.livestream"].interval` (120s). Per-channel: `/api/v2/channels/{slug}/channel-points/channel-settings` → `web_watch_event_interval_seconds` |
| Keep-alives | `channel_handshake {channelId}` every 15s, `{"type":"ping"}` every 30s (the server answers `pong`), `channel_disconnect` when leaving |
| Stream ids | `GET /api/v2/channels/{slug}/info` → `id`, `livestream.id`, `livestream.vod_id`, `livestream.categories` |
| Level / XP | `GET https://web.kick.com/api/v1/gamification/user/level` → `level, progress_xp, xp_to_next_level, total_xp` (shown in chat as the `level` badge) |
| Points | `GET /api/v2/channels/{slug}/points` |
| Daily rewards | `GET web.kick.com/api/v1/gamification/challenges` (`condition.type = watch_time_minutes`), claim with `POST …/challenges/{id}/claim` |
| Live by category | `GET web.kick.com/api/v1/livestreams?category_id={id}&limit=100&sort=viewer_count_desc` |
| Drops | `GET web.kick.com/api/v1/drops/progress` → campaigns with `progress_units` (minutes) and `rewards[{id, claimed, progress 0..1, required_units}]`; claim with `POST /v1/drops/claim {reward_id, campaign_id}`. All campaigns: `GET /v1/drops/campaigns`. Some campaigns need `connect_url` (a linked game account). The "Participate" button only links to `/category/{slug}/drops`. |
| Realtime | `POST web.kick.com/api/v1/realtime/connection` → Centrifugo websocket URL; `POST /v1/realtime/auth/connection {client_id}` → connection JWT (~45 min TTL); `POST /v1/realtime/auth/channel {channel}` → per-private-channel JWT. JSON protocol: `{"connect":{"token","name":"js"},"id":1}`, `{"subscribe":{"channel","token","flag":1},"id":n}`. The server pings with `{}` and expects `{}` back. |
| Realtime events | `private-livestream.{id}` (needs a token) → `App\Events\LivestreamUpdated` (title/category changes); `private-channelpoints-{uid}` → `PointsUpdated {channel_id, points, balance, reason}`; `private-{uid}` → `GamificationLevelUp {level}`, `GamificationWatchTimeGoalReached`; `channel.{channel_id}` → `App\Events\StreamerIsLive`, `App\Events\StopStreamBroadcast`; `drops_category_{id}` → `drops_campaign_started` |
| Other | `/v1/gamification/streak`, `/v1/gamification/collectibles`, `/v1/rewards` |

## Measured results

All runs used the bot only, with no browser open.

| Run | XP | Channel points | Watch-time minutes |
|---|---|---|---|
| 2 streams, 10 min | +16 (about 4 XP per 2 min) | **+20 on each channel** | +8 (counted once, not per stream) |
| 1 stream, 8 min | +12 (same rate), level 3 → 4 | +10 on the one channel | same |
| 2 streams, 13 min | +24 (about 110 XP/h) | +20 on each channel | Daily Reward **auto-claimed** at 60/60 |
| 2 streams, 10 min, **30s event interval** (4× the messages) | +16, no change | +20 on each channel, no change | no change |
| **5 streams**, 15 min | +32 (~120 XP/h), level 4 → 5 | only **2 channels earned at a time** | no change |
| Drops: MW4 stream + 1 other, 13 min | about 120 XP/h | — | MW4 drop +6 min (**half speed**) |
| Drops: MW4 stream alone, 13 min | about 110 XP/h | — | MW4 drop +12 min (full speed) |

What this means:

- **XP is earned per account, not per stream.** It doesn't go up with `max_concurrent`
  (about 110–120 XP/h).
- **Channel points are earned per channel** (about 10 every 5 minutes), but **at most 2 channels
  earn at once**, and the server picks which ones. Keep `max_concurrent = 2` so your priority
  order decides.
- **Drop watch time is split across concurrently watched streams.** That's why `[drops]
  exclusive = true` is the default.
- Watch-time challenges count real time once, however many streams you watch.
- **Sending watch events more often doesn't help.** The server credits by elapsed time and
  ignores the extra events. It didn't reject or disconnect them either.

## Experiments

These options are in `config.toml` under `[experiments]`. Each progress poll appends a JSON line
to `analytics.jsonl` (`total_xp`, `concurrent`, `points`, `watch_events_sent`, …), so settings
can be compared by XP per hour:

- **`shared_socket`:** send all channels over one presence socket instead of one socket per
  channel.
- **`watch_event_interval_secs`:** override the 120s watch-event interval. Tested: no gain.
- **`send_handshake = false`:** checks whether the 15s handshakes are needed at all.

The bot also logs any server message type on the presence socket that it hasn't seen before, to
help work out the rest of the protocol.

Quick XP/hour from the analytics file:

```bash
jq -s '(.[-1].total_xp - .[0].total_xp) / (((.[-1].ts|sub("\\..*";"")+"Z"|fromdate) - (.[0].ts|sub("\\..*";"")+"Z"|fromdate))/3600)' analytics.jsonl
```

## Code layout

```
src/api.rs       HTTP client and response types for the endpoints above
src/watcher.rs   viewer presence websocket (one task per connection, multiplexing supported)
src/realtime.rs  Centrifugo client for instant points / level-up / go-live / category events
src/drops.rs     drop campaign planner (reward type, priority, eligibility)
src/notify.rs    Discord webhook notifications
src/miner.rs     orchestrator: live refresh, priority selection, fallback, polling, claiming, analytics
src/config.rs    TOML config and token handling
Dockerfile, docker/entrypoint.sh, docker-compose.yml   container image + VPS setup
.github/workflows/docker.yml    multi-arch image → GHCR
.github/workflows/release.yml   stable releases + dev pre-release (Linux / Windows binaries)
```
