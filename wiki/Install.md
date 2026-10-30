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

A binary install checks GitHub for a newer release in the background whenever the TUI opens and replaces itself once every check passes:

1. the release tag is newer than the running build, and the release lists this platform's binary at 256 MiB or less
2. `sha256sums.txt` downloads
3. its minisign signature verifies against a public key compiled into the binary
4. the platform asset downloads, no larger than the size the release lists, and its SHA-256 matches that sums file
5. the new binary is swapped in: renamed over the old one in one step on Linux and macOS; on Windows the old one is moved aside first, so a failure right there leaves no `clauth.exe` until you reinstall

Any failing step skips the update and writes one line to `~/.clauth/clauth.log`; through step 4 the running binary is left alone. Quitting the TUI gives a running check 5 s to finish, longer only while the new binary is being swapped in; past that the update is dropped silently until the TUI next opens. The check starts when the TUI opens, so turning auto-update off (the Config tab's `auto-update` row, persisted as `[update] auto_update = false` in profiles.toml) takes effect the next time the TUI opens. `CLAUTH_NO_UPDATE=1` turns the whole thing off even when the toggle is on. Full chain: [SECURITY.md](https://github.com/uwuclxdy/clauth/blob/mommy/SECURITY.md#auto-update-verification).

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

`clauth resume` and `clauth info` complete session ids (and `latest`), and codex profile names complete beside the Claude Code ones after `clauth`, `start`, `delete` and `switch`, and alone after `limit-reset`. An installed script never updates by itself: when the script at the path `clauth completions install` writes (`~/.clauth/completions/clauth.<shell>`; fish `~/.config/fish/completions/clauth.fish`) differs from what this clauth prints, select the `plugin` row and press <kbd>f</kbd> until `update completions` is focused, then <kbd>⏎</kbd>, or run `clauth completions install`.

## Claude Code plugin

The plugin is a separate step, installed from the TUI's Services tab and covered on [Claude Code plugin](Claude-Code-Plugin). Because that install drives the `claude plugin` CLI, it needs a recent `claude`: an older one fails the install naming the version it wants.
