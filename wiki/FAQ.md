# FAQ

## Using it

**How do I switch between multiple Claude Code accounts without logging out?** Save each logged-in session as a profile once, then switch with `clauth <name>` or one keypress in the TUI. No browser, no re-login.

**Can I run Claude Code with two accounts at the same time?** Yes. `clauth start <profile>` launches `claude` in its own `CLAUDE_CONFIG_DIR`, so parallel sessions share no identity, settings, or billing caches.

**How do I run Claude Code without my global `CLAUDE.md`, plugins, or hooks?** `clauth start --isolated <profile>` keeps the account's auth and drops the rest. Run it in an empty directory to skip project memory too. The MCP `delegate` tool takes `isolated: true` for the same thing.

**Can Claude Code switch accounts automatically when I hit the 5-hour limit?** Put the accounts in the fallback chain. clauth switches to the next member with headroom the moment the active one crosses its threshold, from the TUI or from `clauth daemon` with the TUI closed. [Auto-switch](Auto-Switch).

**Does it work with Pro, Max, Team, and Enterprise?** Yes, plan tier detected automatically, Max 5x and 20x included. Endpoint profiles cover the Anthropic API and any compatible proxy.

**Where does clauth store my credentials?** Under `~/.clauth/`, owner-only on Unix. Tokens go to Anthropic and nowhere else. [Security](Security).

**Can I add an account without logging out of the one I am using?** `clauth login <name>` opens a browser, runs Claude Code's OAuth flow, and writes the tokens into a new profile. The session you are in is untouched.

**Is there an MCP server for switching accounts from inside a chat?** Yes. Install the plugin, then a live session can call `profiles`, `switch_profile`, or `delegate` a whole prompt to another account. [Claude Code plugin](Claude-Code-Plugin).

**How do I stop clauth updating itself?** `CLAUTH_NO_UPDATE=1`. A cargo install never self-replaces anyway.

## When something looks wrong

**An account shows 0% but I have been using it.** The 5h window opens on a real inference call, and a usage poll does not trip it. Either the window genuinely has not started, or the reading is cached. The refresh countdown on the Overview tab turns yellow on last-known numbers and red when the last fetch failed.

**Usage numbers are stuck.** Only one clauth instance fetches at a time. If a daemon holds the lease, an open TUI reads its results instead of polling itself, and picks the lease back up within a tick of the daemon exiting. `clauth daemon --status` says whether one is up.

**An account has a `×` next to it.** Its login was rejected for good, so it is quarantined and excluded from the chain. Run `clauth login <name>` to re-authenticate it. On an account that authenticates by api key, what died is the stored subscription login its usage figures came from, so `clauth login <name> --api-key <key>` is the one that clears the quarantine against the credential that account actually runs on. A bare browser login clears it too and leaves the endpoint and key standing.

**The chain will not switch to an account that looks fine.** Check the reason on its row. Weekly windows, per-model weekly windows, a spend ceiling, a canceled subscription, or a `disabled` flag all take a member out of rotation independently of its 5h number. The [exclusion table](Auto-Switch#excluded-members) lists all of them.

**Auto-switch does nothing at all.** The active account has to be a chain member for the walk to start. An account outside the chain is never switched away from.

**On macOS a switch does not reach my running session.** Claude Code reads the Keychain first there, and it deletes the credentials file once it migrates. clauth mirrors each fresh login into the Keychain for exactly this reason, but an account holding a live `clauth start` session is skipped by force-rotate, since its Keychain item belongs to that session's own config dir. [Security](Security#per-platform-behavior).

**Claude Code does not show clauth's tools.** Open the Plugin tab. It checks `clauth` on `PATH`, the `mcpServers` entry, the plugin install record, and whether `clauth mcp` actually answers a handshake. <kbd>f</kbd> on a row applies that row's fix: install the plugin at user scope, write the `mcpServers` entry into `~/.claude.json`, repair or relink the active account's credentials, or add clauth's keybinding and sidebar row to herdr's config.

**A `delegate` run did nothing and the tree is unchanged.** A delegate spawns with the permission gate armed and nobody to answer it. Pass the permission flag through `args` for a delegate that writes files, and read the `permission_denials` array in the envelope. [Claude Code plugin](Claude-Code-Plugin#delegate).

**My custom endpoint shows no usage bars.** Only DeepSeek, Z.ai, OpenRouter, MiniMax and Alibaba Model Studio have typed panels. Everything else gets a best-effort scan of the usual usage paths, which can come back empty. Press <kbd>r</kbd> to retry an endpoint clauth gave up on. An Alibaba account is the one case where an api key is not enough: run `clauth login <account>` to capture the console session its quota is read with ([Configuration](Configuration#the-alibaba-console-session)).

**The Tokens tab shows `$X+` instead of a figure.** Some model in that period has no published price, or the period reaches into days that carry no cache split. The number is a floor. [Tokens and cost](Tokens-And-Cost#period-lens).

**Two daemons, or none.** A second `clauth daemon` exits immediately by default. `--standby` parks one that takes over when the first dies; `--replace` terminates the running one and takes its place. [Daemon](Daemon).
