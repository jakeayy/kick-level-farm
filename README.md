# kick-level-farm

A lightweight Rust bot that farms **Kick.com account levels (XP)**, **channel points** and
**Daily Reward chests** by emulating the kick.com web player's viewer presence. It doesn't
download any video. Each watched stream costs one small websocket, which comes to a few KB per
minute.

The idea comes from [Twitch-Channel-Points-Miner-v2](https://github.com/rdavydov/Twitch-Channel-Points-Miner-v2).
The protocol was reverse-engineered from the live kick.com web app (Oct 2026).

> ⚠️ This automates an undocumented API and probably goes against Kick's ToS. Accounts can be
> flagged. Try it on an account you can afford to lose.

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
- Reconnects with backoff, follows livestream restarts (a new `livestream_id`), and shuts down
  cleanly on Ctrl-C with a session summary.
- Built for experiments. The `[experiments]` section and the `analytics.jsonl` log let you measure
  what actually earns XP.

## Usage

```bash
cargo build --release
./target/release/kick-level-farm init      # writes config.toml
./target/release/kick-level-farm login     # paste your session_token cookie
./target/release/kick-level-farm status    # level, XP, challenges, points per streamer
./target/release/kick-level-farm           # start farming (-v for debug logs)
```

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
| Other | `/v1/gamification/streak`, `/v1/gamification/collectibles`, `/v1/rewards`, `/v1/drops/claim`, realtime (Centrifugo) channel `private-channelpoints-{userId}` |

### Measured results (bot only, no browser open, Oct 31 2026)

| Run | XP | Channel points | Daily-reward watch minutes |
|---|---|---|---|
| 2 streams, 10 min | +16 (about 4 XP per 2 min) | **+20 on each channel** | +8 (counted once, not per stream) |
| 1 stream, 8 min | +12 (same rate), level 3 → 4 | +10 on the one channel | same |
| 2 streams, 13 min | +24 (about 110 XP/h) | +20 on each channel | Daily Reward **auto-claimed** at 60/60 |
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
src/miner.rs    orchestrator: live refresh, priority selection, fallback, polling, claiming, analytics
src/config.rs   TOML config and token handling
```

## Ideas not done yet

- Subscribe to the realtime `private-channelpoints-{userId}` channel for instant point updates
  instead of polling.
- Auto-claim drops (`/v1/drops/claim`) and spend points on rewards (`/rewards/{id}/redeem`).
- Run several accounts from one process.
- Notifications through Discord/Telegram webhooks.
