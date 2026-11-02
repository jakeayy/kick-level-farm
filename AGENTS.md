# AGENTS.md

Instructions for AI coding agents (Claude Code, Codex, Cursor, Copilot, Gemini CLI, …) working
with **kick-level-farm**. It's a Rust bot that farms Kick.com account levels (XP), channel points,
drops and daily rewards by emulating the web player's viewer presence. The user docs are in
[README.md](README.md), and the protocol and measurement notes are in [RESEARCH.md](RESEARCH.md).

There are two parts:

- [Installing it for a user](#installing-it-for-a-user): the user wants to *run* the bot.
- [Contributing](#contributing): the user wants to *change* the code.

---

## Installing it for a user

### Ground rules

1. **Tell the user about the risk before installing.** The bot automates an undocumented
   Kick API and probably goes against Kick's ToS, so the account can be flagged. Continue only
   once they confirm.
2. **Never handle the session token yourself.** Don't ask the user to paste their
   `session_token` (`kat_…`) into the chat. Don't print it, log it, put it in a commit, or send
   it anywhere. The user runs the interactive `login` command themselves (see
   [Step 3](#step-3-log-in-the-user-does-this)).
3. **Never run anything that needs root** (`sudo`, package managers, installing Docker)
   yourself. Give the user the exact commands and let them run them.
4. **Explain before installing anything without root.** That covers downloading the binary or
   installing Rust with `rustup` into the home directory. Tell the user what will be installed
   and where, then go ahead unless they object.
5. **Check every step's result** before moving on. If something fails, show the user the
   error rather than guessing your way past it.

### Step 1: Choose the install method

Check for a working Docker, which needs the daemon running and permission to use it:

```bash
docker info >/dev/null 2>&1 && docker compose version
```

| Result | Use |
|---|---|
| Both succeed | **[Docker](#method-a-docker-preferred)** (preferred: auto-restart, easy updates) |
| `docker` is installed but `docker info` fails with "permission denied" | The user isn't in the `docker` group. Tell them to run `sudo usermod -aG docker $USER` and log out and back in, **or** use the binary method |
| `docker` isn't installed | **[Prebuilt binary](#method-b-prebuilt-binary-no-root-needed)**. Also mention that Docker is the recommended option on servers, with install docs at <https://docs.docker.com/engine/install/> (they need root, so the user runs them) |
| macOS (no prebuilt binary) | **[From source](#method-c-from-source)**, or Docker Desktop if they have it |

Supported prebuilt platforms are Linux x86_64, Linux aarch64 (static musl, any distro) and
Windows x64. The Docker images are `linux/amd64` and `linux/arm64`.

### Step 2: Install

#### Method A: Docker (preferred)

No root needed if the user can already run `docker`. Pick a folder (for example
`~/kick-level-farm`) and run:

```bash
mkdir -p ~/kick-level-farm && cd ~/kick-level-farm
curl -fsSLO https://raw.githubusercontent.com/jakeayy/kick-level-farm/main/docker-compose.yml
# Optional: set TZ in docker-compose.yml to the user's timezone (log timestamps).
docker compose up -d
```

The first start writes `./data/config.toml` and then waits for a token. That's expected.

#### Method B: Prebuilt binary (no root needed)

Tell the user first, for example: *"I'll download the latest kick-level-farm release from
GitHub into `~/.local/share/kick-level-farm`, verify its SHA-256 checksum and link the binary
into `~/.local/bin`. Nothing outside your home directory is touched."*

**Linux:**

```bash
bash -s <<'EOF'
set -eu
REPO=jakeayy/kick-level-farm
DIR="$HOME/.local/share/kick-level-farm"
case "$(uname -m)" in
  x86_64|amd64)  ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac
TARGET="$ARCH-unknown-linux-musl"

mkdir -p "$DIR" "$HOME/.local/bin" && cd "$DIR"
API=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest")
URL=$(printf '%s' "$API" | grep -o "https://[^\"]*-$TARGET\.tar\.gz" | head -n1)
SUMS=$(printf '%s' "$API" | grep -o 'https://[^"]*/SHA256SUMS\.txt' | head -n1)
curl -fsSLO "$URL" && curl -fsSLO "$SUMS"
grep " $(basename "$URL")\$" SHA256SUMS.txt | sha256sum -c -
tar xzf "$(basename "$URL")" --strip-components=1
rm -f "$(basename "$URL")" SHA256SUMS.txt
ln -sf "$DIR/kick-level-farm" "$HOME/.local/bin/kick-level-farm"
"$HOME/.local/bin/kick-level-farm" --version
EOF
```

If `kick-level-farm` isn't found afterwards, `~/.local/bin` isn't on the user's `PATH`. Run it
as `~/.local/bin/kick-level-farm`, or tell them how to add it to `PATH`.

**Windows (PowerShell):**

```powershell
$ErrorActionPreference = "Stop"
$Repo = "jakeayy/kick-level-farm"
$Dir  = Join-Path $env:LOCALAPPDATA "kick-level-farm"
New-Item -ItemType Directory -Force -Path $Dir | Out-Null

$rel  = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest"
$zip  = $rel.assets | Where-Object { $_.name -like "*-x86_64-pc-windows-msvc.zip" } | Select-Object -First 1
$sums = $rel.assets | Where-Object { $_.name -eq "SHA256SUMS.txt" } | Select-Object -First 1
$zipPath  = Join-Path $Dir $zip.name
$sumsPath = Join-Path $Dir "SHA256SUMS.txt"
Invoke-WebRequest $zip.browser_download_url  -OutFile $zipPath
Invoke-WebRequest $sums.browser_download_url -OutFile $sumsPath

$line     = Select-String -Path $sumsPath -Pattern ([regex]::Escape($zip.name)) | Select-Object -First 1
$expected = ($line.Line -split '\s+')[0]
$actual   = (Get-FileHash $zipPath -Algorithm SHA256).Hash.ToLower()
if ($actual -ne $expected) { throw "Checksum mismatch for $($zip.name)" }

Expand-Archive $zipPath -DestinationPath $Dir -Force
$sub = Join-Path $Dir ([IO.Path]::GetFileNameWithoutExtension($zip.name))
Get-ChildItem $sub | Move-Item -Destination $Dir -Force
Remove-Item $sub -Recurse
Remove-Item $zipPath, $sumsPath
& (Join-Path $Dir "kick-level-farm.exe") --version
```

The exe is now in `%LOCALAPPDATA%\kick-level-farm`. Run all later commands from that folder,
since the config is created next to it.

**Create the config** in the install folder. The bot reads `config.toml` from the current
directory, and the systemd unit below expects it there:

```bash
cd ~/.local/share/kick-level-farm && kick-level-farm init           # Linux
```

```powershell
cd "$env:LOCALAPPDATA\kick-level-farm"; .\kick-level-farm.exe init   # Windows
```

#### Method C: From source

This needs a Rust toolchain. If `cargo` is missing, tell the user that you'll install Rust with
`rustup` into `~/.rustup` and `~/.cargo` (no root), then run:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. "$HOME/.cargo/env"
```

A C compiler is also required for the `ring` crypto crate:

- **Linux:** `gcc`/`cc` from the distro's build tools. That needs root, so the user installs it,
  e.g. `sudo apt install build-essential` or `sudo dnf install gcc`.
- **macOS:** the Command Line Tools. The user runs `xcode-select --install`, which opens a
  system dialog.

Then:

```bash
git clone https://github.com/jakeayy/kick-level-farm.git && cd kick-level-farm
cargo build --release
./target/release/kick-level-farm init
```

### Step 3: Log in (the user does this)

Tell the user how to get the token:

1. Log in on <https://kick.com>.
2. Open DevTools (F12) and go to Application (Storage in Firefox) → Cookies → `https://kick.com`.
3. Copy the value of `session_token` (it starts with `kat_`).

Then have the **user** run the interactive login in their own terminal. It prompts for the token
and checks it:

| Method | Command |
|---|---|
| Docker | `docker compose run --rm kick-level-farm login` (in the compose folder) |
| Linux binary | `cd ~/.local/share/kick-level-farm && kick-level-farm login` |
| Windows | `.\kick-level-farm.exe login` (in `%LOCALAPPDATA%\kick-level-farm`) |
| Source | `./target/release/kick-level-farm login` (in the repo folder) |

Don't pipe or echo the token into the command for them. If your harness can hand an
interactive command to the user (e.g. a `!` prefix), use that. Otherwise ask them to run it in
their own terminal.

### Step 4: Verify

| Method | Check |
|---|---|
| Docker | `docker compose logs --tail 50`: expect `new session token found`, `logged in as …`, `▶ watching …` and a `level …` line within about 30s of the login |
| Binary / source | `kick-level-farm status` shows the account, level and challenges. Then run `kick-level-farm` and expect `logged in as …` and `▶ watching …` (Ctrl+C stops it and prints a summary) |

A `session token rejected` message means the token was wrong or expired, so redo Step 3.

### Optional: Keep it running

- **Docker:** already handled (`restart: unless-stopped`). Updates:
  `docker compose pull && docker compose up -d`.
- **Linux binary:** a systemd **user** service needs no root:

  ```ini
  # ~/.config/systemd/user/kick-level-farm.service
  [Unit]
  Description=kick-level-farm
  After=network-online.target

  [Service]
  WorkingDirectory=%h/.local/share/kick-level-farm
  ExecStart=%h/.local/bin/kick-level-farm --wait-for-login
  Restart=on-failure

  [Install]
  WantedBy=default.target
  ```

  Then `systemctl --user daemon-reload && systemctl --user enable --now kick-level-farm`, and
  read the logs with `journalctl --user -u kick-level-farm -f`. To keep it running after the
  user logs out, they need `sudo loginctl enable-linger $USER`. That needs root, so the user
  runs it.
- **Windows:** leave it running in a terminal, or let the user set up a Task Scheduler task
  that runs `kick-level-farm.exe --wait-for-login` at logon with the exe's folder as "Start in".

### Optional: Configuration

The defaults work. Common changes go in `config.toml` (Docker: `./data/config.toml`, then
`docker compose restart`). The option table is in [README.md](README.md#configuration). To set
up Discord notifications, ask the user for their webhook URL. It's a secret too, so only write
it into `config.toml`, never into anything that gets committed. Then run
`kick-level-farm notify-test`.

---

## Contributing

### ⚠️ Human review is required

**The user must ALWAYS review the final result of any code an agent writes for this project**:
read the diff, build it, and check that the bot still farms correctly against live Kick before
merging, releasing or deploying. Agents can produce code that compiles but quietly breaks
farming, leaks secrets or trips Kick's anti-bot checks. Always tell the user which parts they
should look at most carefully.

### Project map

The code layout is in [RESEARCH.md](RESEARCH.md#code-layout). In short:

- `src/api.rs`: HTTP client for Kick's endpoints.
- `src/watcher.rs`: viewer presence websocket.
- `src/realtime.rs`: push events.
- `src/drops.rs`: drop planner.
- `src/miner.rs`: orchestrator.
- `src/config.rs`: config.
- `src/notify.rs`: Discord.

### Build and check

Run all of these before saying a change is done:

```bash
cargo fmt --check
cargo clippy --release --all-targets -- -D warnings
cargo test
cargo build --release
```

CI runs the same checks on Linux and Windows for every pull request, and they're required to
merge. Add or update unit tests for any logic you change. Response samples live in
`tests/fixtures/`; keep them trimmed and free of personal data.

Optional checks when relevant:

- **Windows-specific code:** with the MinGW toolchain installed,
  `cargo clippy --release --target x86_64-pc-windows-gnu -- -D warnings`.
- **Dockerfile / entrypoint changes:** `docker build -t kick-level-farm:dev .`
- **Workflow changes:** `docker run --rm -v "$PWD":/repo -w /repo rhysd/actionlint:latest`
- **Dependency changes:** `cargo deny check` (cargo-deny, configured in `deny.toml`). New
  dependencies must have a GPL-3.0-compatible license from the allowlist there.

CI also runs CodeQL (Rust and workflows) and cargo-deny, and Dependabot opens update PRs. Fix
or explain any alerts they raise instead of suppressing them.

### Verify against live Kick

There's no test suite. Kick's undocumented API is the real test, so verify behaviour by running
the bot with a session token the **user** provides (via `login`, `KICK_TOKEN` in their own
shell, or their existing `config.toml`):

```bash
kick-level-farm status                      # API access, level, drop plan
timeout --signal=INT 180 kick-level-farm -v # one run: expect watch events, XP, clean shutdown
```

XP arrives as about +4 every 2 minutes, and channel points as about +10 per channel every ~5
minutes. If those stop, something is broken.

### Rules learned the hard way

- **Keep HTTP/2 in `reqwest`.** Kick's WAF answers reqwest's HTTP/1.1 requests with
  `403 "Request blocked by security policy"`.
- **Keep `panic = "unwind"`** (the default). One panicking task must not kill the whole bot.
- **An invalid token gets `200 {}`, not 401,** from `/api/v1/user`. `KickClient::me()` handles
  this, so keep it that way.
- **Don't speed up watch events.** Faster intervals earn nothing and look less human (see
  RESEARCH.md).
- **When Kick changes something,** check the live web app first: use browser DevTools to see
  what the real player sends, then update the code **and** `RESEARCH.md`.
- **Never commit secrets.** `config.toml`, `.kick_token` and `*.jsonl` are gitignored, so keep
  it that way and don't paste tokens or webhook URLs into code, docs, issues or commit messages.

### Releases

Every push to `main` that touches `src/**`, `Cargo.toml` or `Cargo.lock` publishes a `dev-<sha>`
pre-release and a new Docker image. A **stable release** is published when the `version` in
`Cargo.toml` changes. Bump it only when the user asks for a release.

Record every user-facing change under `## [Unreleased]` in [CHANGELOG.md](CHANGELOG.md).
When releasing, rename that section to the new version and date. The release workflow uses it
as the release notes.

A nightly `kick-canary` workflow runs `kick-level-farm selftest` and checks the web player's
protocol markers. When Kick changes something, it opens a `kick-api-change` issue. Fix that
first, and update `RESEARCH.md` along with the code.

### Automated triage (CI)

`.github/workflows/claude-triage.yml` runs Claude on every new issue and PR. Keep its security
design intact when editing it:

- The `analyze` job uses a read-only token and read-only tools only.
- The PR code is never checked out or run (`pull_request_target` + diff as plain text).
- Untrusted text reaches scripts only through `env:`.
- Only the separate `act` job (no AI) has write access.

The `main` branch is protected by a ruleset (no deletion or force-push, changes via PR). Only
repository admins bypass it.

### Pull requests

Fill in [the PR template](.github/pull_request_template.md). It repeats the checks above.
Include what you verified against live Kick and what the user should review.
