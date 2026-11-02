# kick-level-farm

[![Release](https://img.shields.io/github/v/release/jakeayy/kick-level-farm?label=release&sort=semver)](https://github.com/jakeayy/kick-level-farm/releases/latest)
[![Pre-release](https://img.shields.io/github/v/release/jakeayy/kick-level-farm?include_prereleases&label=pre-release)](https://github.com/jakeayy/kick-level-farm/releases)
[![Docker](https://img.shields.io/badge/docker-ghcr.io-2496ED?logo=docker&logoColor=white)](https://github.com/jakeayy/kick-level-farm/pkgs/container/kick-level-farm)
[![License](https://img.shields.io/github/license/jakeayy/kick-level-farm)](LICENSE)

A lightweight Rust bot that farms **Kick.com account levels (XP)**, **channel points**,
**Daily Reward chests** and **drops** by emulating the kick.com web player's viewer presence. It
doesn't download any video. Each watched stream costs one small websocket, which comes to a few KB
per minute.

The idea comes from [Twitch-Channel-Points-Miner-v2](https://github.com/rdavydov/Twitch-Channel-Points-Miner-v2).

> ⚠️ This automates an undocumented API and probably goes against Kick's ToS. Accounts can be
> flagged. Try it on an account you can afford to lose.

## Table of contents

- [Features](#features)
- [Installation](#installation)
  - [Docker (VPS)](#docker-vps)
  - [Prebuilt binaries (Linux / Windows)](#prebuilt-binaries-linux--windows)
  - [From source](#from-source)
- [Getting your session token](#getting-your-session-token)
- [Configuration](#configuration)
  - [Discord notifications](#discord-notifications)
- [Commands](#commands)
- [License](#license)
- [Star history](#star-history)

## Features

- **Level XP:** watches live streams to earn account XP, about 110–120 XP per hour. XP is per
  account, so when none of your channels are live the bot watches **one** popular stream to keep
  earning it. No channel points are farmed on that stream.
- **Channel points:** farmed on every channel you follow (and any extra channels you list),
  picked by a configurable priority. Kick pays points on at most 2 channels at once, so
  `max_concurrent = 2` is the default.
- **Drops:** watches streams that take part in drop campaigns and claims rewards when they're
  done.
  - Priority is **badges → emotes → items**. Items are only farmed if you turn them on.
  - Campaigns that need a linked game account you haven't linked are skipped.
  - Kick splits drop time across all streams you watch, so by default only the drops stream is
    watched while a drop is in progress.
  - If a drops stream switches to another category, the bot moves to another qualifying stream
    within seconds.
- **Daily Reward and event chests** are claimed automatically.
- **Realtime updates:** point gains, level-ups and channels going live or offline arrive
  instantly through Kick's push service.
- **Discord notifications** for start/stop, level-ups, claimed rewards and drops, streamers
  going live, and errors.
- **Runs anywhere:** Docker image (amd64/arm64, about 24 MB), static Linux binaries and a
  Windows `.exe`.

How Kick tracks watching, the measurements behind these defaults, and the code layout are in
[RESEARCH.md](RESEARCH.md).

## Installation

### Docker (VPS)

```bash
mkdir kick-farm && cd kick-farm
curl -O https://raw.githubusercontent.com/jakeayy/kick-level-farm/main/docker-compose.yml
docker compose up -d                                  # first start writes data/config.toml
docker compose run --rm kick-level-farm login         # paste your session_token
docker compose logs -f                                # the bot picks the token up within ~20s
```

- `./data` holds `config.toml`, `.kick_token` and `analytics.jsonl`. Edit the config there,
  then run `docker compose restart`.
- To update, run `docker compose pull && docker compose up -d`.
- The container runs as `PUID`/`PGID` (default 1000) and fixes the ownership of `./data` on start.
- You can set `KICK_TOKEN` and `KICK_DISCORD_WEBHOOK` in `docker-compose.yml` instead of
  using the files.
- **Expired token:** the bot doesn't crash-loop. It waits until you run `login` again, then
  resumes on its own.
- To build the image yourself, uncomment `build: .` in `docker-compose.yml` and run
  `docker compose up -d --build`.

### Prebuilt binaries (Linux / Windows)

Download from [Releases](https://github.com/jakeayy/kick-level-farm/releases):

| File | For |
|---|---|
| `kick-level-farm-<tag>-x86_64-unknown-linux-musl.tar.gz` | Linux x86_64 (static, any distro) |
| `kick-level-farm-<tag>-aarch64-unknown-linux-musl.tar.gz` | Linux ARM64 (Raspberry Pi 4/5, ARM VPS) |
| `kick-level-farm-<tag>-x86_64-pc-windows-msvc.zip` | Windows 10/11 x64 |
| `SHA256SUMS.txt` | checksums |

- **Stable releases** (`vX.Y.Z`) come out when the version changes.
- **The pre-release** (`dev-<commit>`) is the latest build from `main`.

**Windows:** extract the zip, open PowerShell or Terminal in that folder, then run:

```powershell
.\kick-level-farm.exe init      # writes config.toml next to the exe
.\kick-level-farm.exe login     # paste your session_token
.\kick-level-farm.exe           # start farming, Ctrl+C to stop
```

**Linux:** `tar xzf kick-level-farm-*.tar.gz`, then use `./kick-level-farm` the same way.

### From source

```bash
cargo build --release
./target/release/kick-level-farm init
./target/release/kick-level-farm login
./target/release/kick-level-farm
```

## Getting your session token

Kick's login has a captcha and 2FA, so the bot reuses your browser session instead of logging in
itself:

1. Log in on [kick.com](https://kick.com).
2. Open DevTools (F12) and go to Application (Storage in Firefox) → Cookies → `https://kick.com`.
3. Copy the value of `session_token` (it starts with `kat_`).
4. Run `kick-level-farm login` and paste it, or set it in the `KICK_TOKEN` env var.

## Configuration

`kick-level-farm init` writes a commented `config.toml`. The defaults work out of the box.
The options you're most likely to change:

| Option | Default | What it does |
|---|---|---|
| `streamers` | `[]` | Extra channels to farm, in order of preference (followed channels are added automatically) |
| `[farm] max_concurrent` | `2` | Streams watched at once |
| `[farm] priority` | `["order"]` | `order`, `points_ascending`, `points_descending`, `viewers_ascending`, `viewers_descending`, `points_enabled` |
| `[farm] include_followed` | `true` | Farm every channel you follow |
| `[farm] fallback_to_popular` | `true` | Watch one popular stream for XP when nothing else is live |
| `[farm] fallback_language` | `"en"` | Language of that popular stream |
| `[drops] farm` / `claim` | `true` | Watch drop streams / claim finished drops |
| `[drops] badges` / `emotes` / `items` | `true` / `true` / `false` | Which reward types to farm |
| `[drops] exclusive` | `true` | Watch only the drops stream while a drop is in progress |

### Discord notifications

Create a webhook in Discord (Server Settings → Integrations → Webhooks → New Webhook), then add
it to `config.toml`:

```toml
[notifications]
discord_webhook_url = "https://discord.com/api/webhooks/..."
events = ["started", "stopped", "level_up", "reward_claimed", "drop_claimed", "streamer_live", "error"]
```

The `KICK_DISCORD_WEBHOOK` env var works too. Run `kick-level-farm notify-test` to check it.

## Commands

| Command | What it does |
|---|---|
| `kick-level-farm` / `run` | Start farming. Add `-v` for debug logs. |
| `init` | Write the default `config.toml` |
| `login` | Save your session token |
| `status` | Show level, XP, challenges, the drop plan and points per streamer |
| `notify-test` | Send a test message to your Discord webhook |

Global options: `-c <path>` sets the config file, and `--wait-for-login` waits for a new token
instead of exiting when the current one expires (Docker uses this).

## License

[GPL-3.0](LICENSE). You can use, modify and share it, but distributed modifications must stay
open source under the same license.

## Star history

<a href="https://www.star-history.com/#jakeayy/kick-level-farm&Date">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/svg?repos=jakeayy/kick-level-farm&type=Date&theme=dark" />
    <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/svg?repos=jakeayy/kick-level-farm&type=Date" />
    <img alt="Star history chart" src="https://api.star-history.com/svg?repos=jakeayy/kick-level-farm&type=Date" />
  </picture>
</a>
