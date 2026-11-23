# Install

Linux, macOS, and Windows (Git Bash / MSYS2). clauth needs `claude` on `PATH` for `clauth start`, `resume`, the MCP `delegate` tool, and the Claude Code plugin's install and self-heal, both of which drive the `claude plugin` CLI. Everything else works without it.

## Cargo

```bash
cargo install clauth
```

Upgrade the same way. A cargo-installed binary is never self-replaced: it reports that a newer release exists and leaves the swap to you.

## Install script

```bash
curl -fsSL https://raw.githubusercontent.com/uwuclxdy/clauth/mommy/install.sh | bash
```

The script uses `cargo` when it finds it. Pass `--nocargo` to force a prebuilt binary instead:

```bash
curl -fsSL https://raw.githubusercontent.com/uwuclxdy/clauth/mommy/install.sh | bash -s -- --nocargo
```

The binary lands in `~/.local/bin` (or `/usr/local/bin` when that is writable). The script prints a `PATH` hint when the install dir is not on your `PATH`, and it never edits your shell profile. Uninstall by deleting the binary it names.

## From source

```bash
git clone https://github.com/uwuclxdy/clauth
cd clauth
cargo build --release
# binary at ./target/release/clauth
```

## Updates

The running `clauth daemon` (a parked `--standby` once it takes over) and every TUI check GitHub for a newer release in the background, each once when it starts and then every hour, and a binary install replaces itself once every check passes:

1. the release tag is newer than the running build and of the same major version, and the release lists this platform's binary at 256 MiB or less
2. `sha256sums.txt` downloads
3. its minisign signature verifies against a public key compiled into the binary
4. the platform asset downloads, no larger than the size the release lists, and its SHA-256 matches that sums file
5. the new binary is swapped in: renamed over the old one in one step on Linux and macOS; on Windows the old one is moved aside first, so a failure right there leaves no `clauth.exe` until you reinstall

Any failing step skips the update and writes one line to `~/.clauth/clauth.log` (a daemon writes to its own log, `~/.clauth/daemon.log` when the TUI started it); through step 4 the running binary is left alone, and the next hourly check tries again. A daemon or TUI whose update thread the OS refuses to start logs `update check failed: cannot start the update thread: <OS error>` once and checks nothing until it restarts. Quitting the TUI ends its wait between checks at once and gives a running check 5 s to finish, longer only while the new binary is being swapped in; past that the update is dropped silently; the next check of a running daemon or TUI, or the next TUI start, tries again. Each check reads the saved toggle first, so turning auto-update off (the Config tab's `auto-update` row, persisted as `[update] auto_update = false` in profiles.toml) stops the next check of every running daemon and TUI, and turning it back on resumes them at their next hourly check; a daemon or TUI that found it off when its first check was due checks nothing until it restarts. `CLAUTH_NO_UPDATE=1` turns the whole thing off even when the toggle is on, for each process whose environment holds it (a daemon the TUI starts inherits the TUI's). Full chain: [SECURITY.md](https://github.com/uwuclxdy/clauth/blob/mommy/SECURITY.md#auto-update-verification).

Whichever process finds a newer release first installs it, a daemon included, and logs `updated to v0.19.1` (for a v0.19.1 release). Two processes that find the same release at once may both download it; only the first swaps it in, and the other installs nothing, unless both reach the swap in the same instant: then both land the same bytes, except that on Linux and Windows the later one may fail instead and log one `update check failed` line. A process whose own binary was replaced or removed on disk since it started (checked before the download and again right before the swap) installs nothing more until it restarts; it still reports a higher major version when the latest is one, and says nothing of a newer same-major release. A build that cannot replace itself (a `cargo` install, or a platform with no prebuilt binary) logs `update available: v0.19.1` instead. `updated to` and `update available` each go out once per version per process, and a daemon that restarts itself on a new binary counts as a new process.

A release of a higher major version than the running one (0.x to 1.0) is never downloaded or installed, on any install. For a v1.0.0 release the log gets `v1.0.0 is out and is not installed automatically: read its release note, then update by hand (https://github.com/uwuclxdy/clauth/releases/tag/v1.0.0)` once per version per process, and a TUI shows the warning toast `v1.0.0 is out` / `not installed automatically: read the release note` once per run. Read the note, then install that release by hand. The check reads only the release GitHub marks as latest, so a release not marked latest is never offered: install it by hand.

A `clauth daemon` left running restarts itself on the new binary within seconds on Linux and macOS, whether an update or a reinstall replaced it ([Daemon](Daemon#process-model)); a `--listen` daemon whose new binary cannot load the API certificate keeps running the old one and logs the command that restarts it. A daemon still running a release from before that restart, and every daemon on Windows, keeps running the old binary until you restart it with `clauth daemon --replace`.

A binary install on v0.17.0 or older cannot download a current release and never updates itself: replace it once with the latest binary from the releases page (or rerun the install script with `--nocargo`), and updates resume from there.

## Shell completions

The first TUI launch offers to install completions for your shell. bash and zsh get a `source` line appended to the rc file, asked for with `[Y/n]` first; fish writes straight into `~/.config/fish/completions/`. The answer is remembered in `~/.clauth/.completions_installed`. A declined prompt is not asked again, but the Services tab's `plugin` row keeps offering `install completions` without warning.

Install or refresh them any time:

```bash
clauth completions install          # detects your shell from $SHELL
clauth completions install zsh      # or name it
clauth completions bash             # print the script to stdout instead
```

`CLAUTH_NO_COMPLETIONS=1` skips the first-run prompt entirely.

On Windows, completions work in Git Bash; cmd and PowerShell get none.

`clauth resume` and `clauth info` complete session ids (and `latest`), `clauth switch` completes the session ids `clauth switch --list` shows and `--list` itself (then `--json` after `--list`), and codex profile names complete beside the Claude Code ones after `clauth`, `start`, `delete`, `switch` and `limit-reset`. An installed script never updates by itself: when the script at the path `clauth completions install` writes (`~/.clauth/completions/clauth.<shell>`; fish `~/.config/fish/completions/clauth.fish`) differs from what this clauth prints, select the `plugin` row and press <kbd>f</kbd> until `update completions` is focused, then <kbd>⏎</kbd>, or run `clauth completions install`.

## Claude Code plugin

The plugin is a separate step, installed from the TUI's Services tab and covered on [Claude Code plugin](Claude-Code-Plugin). Because that install drives the `claude plugin` CLI, it needs a recent `claude`: an older one fails the install naming the version it wants.
