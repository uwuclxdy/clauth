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

A binary install checks GitHub for a newer release in the background on launch and replaces itself once every check passes:

1. the release tag is newer than the running build
2. `sha256sums.txt` downloads
3. its minisign signature verifies against a public key compiled into the binary
4. the platform asset's SHA-256 matches that sums file
5. the new binary is written, fsynced, and swapped in atomically

Any failing step skips the update and leaves the running binary alone. `CLAUTH_NO_UPDATE=1` turns the whole thing off. Full chain: [SECURITY.md](https://github.com/uwuclxdy/clauth/blob/mommy/SECURITY.md#auto-update-verification).

## Shell completions

The first TUI launch offers to install completions for your shell. bash and zsh get a `source` line appended to the rc file, asked for with `[Y/n]` first; fish writes straight into `~/.config/fish/completions/`. The answer is remembered in `~/.clauth/.completions_installed`.

Install or refresh them any time:

```bash
clauth completions install          # detects your shell from $SHELL
clauth completions install zsh      # or name it
clauth completions bash             # print the script to stdout instead
```

`CLAUTH_NO_COMPLETIONS=1` skips the first-run prompt entirely.

## Claude Code plugin

The plugin is a separate step, installed from the TUI's Plugin tab and covered on [Claude Code plugin](Claude-Code-Plugin). Because that install drives the `claude plugin` CLI, it needs a recent `claude`: an older one fails the install naming the version it wants.
