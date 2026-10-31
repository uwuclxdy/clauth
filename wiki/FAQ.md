# FAQ

## Using it

**How do I switch between multiple Claude Code accounts without logging out?** Save each logged-in session as a profile once, then switch with `clauth <name>` or one keypress in the TUI. No browser, no re-login.

**Can I run Claude Code with two accounts at the same time?** Yes. `clauth start <profile>` launches `claude` in its own `CLAUDE_CONFIG_DIR`, so parallel sessions share no identity, settings, or billing caches.

**How do I run Claude Code without my global `CLAUDE.md`, plugins, or hooks?** `clauth start --isolated <profile>` keeps the account's auth and drops the rest. Run it in an empty directory to skip project memory too. The MCP `delegate` tool takes `isolated: true` for the same thing.

**Can Claude Code switch accounts automatically when I hit the 5-hour limit?** Put the accounts in the fallback chain. clauth switches to the next member with headroom the moment the active one crosses its threshold, from the TUI or from `clauth daemon` with the TUI closed. [Auto-switch](Auto-Switch).

**Does it work with Pro, Max, Ultra, Team, and Enterprise?** Yes, plan tier detected automatically: the label is the plan name Anthropic's own profile sends when it sends one, else clauth's map of Pro, Max, Ultra, Team, and Enterprise. Endpoint profiles cover the Anthropic API and any compatible proxy.

**Where does clauth store my credentials?** Under `~/.clauth/`, owner-only on Unix. Claude tokens go to Anthropic and codex tokens to OpenAI, nowhere else. [Security](Security).

**Can clauth run codex accounts?** Yes. `clauth login <name> --codex` adopts the ChatGPT login your own `codex` holds (or `--codex --browser` mints a fresh one), `clauth start <name>` then runs `codex` under that profile's own `CODEX_HOME`, and a separate codex chain rotates accounts between sessions. [Codex](Codex).

**Can I add an account without logging out of the one I am using?** `clauth login <name>` opens a browser, runs Claude Code's OAuth flow, and writes the tokens into a new profile. The session you are in is untouched.

**Can I log in on a server with no browser, over ssh?** Yes. `clauth login <name>` prints the link under `Browser didn't open? Use the url below to sign in` and prompts `Paste code here if prompted:`: open the link on any device, sign in, and paste back the code the page shows. Same credential as a browser login, no flag. The Setup tab's login modal has the same: <kbd>c</kbd> copies the link to your local clipboard through the terminal, <kbd>p</kbd> turns its row into a code field (type or paste the code, <kbd>⏎</kbd> submits, <kbd>esc</kbd> brings the row back). [Quickstart](Quickstart#commands).

**Is there an MCP server for switching accounts from inside a chat?** Yes. Install the plugin, then a live session can call `profiles`, `switch_profile`, or `delegate` a whole prompt to another account. [Claude Code plugin](Claude-Code-Plugin).

**How do I stop clauth updating itself?** `CLAUTH_NO_UPDATE=1`. A cargo install never self-replaces anyway.

## When something looks wrong

**An account shows 0% but I have been using it.** The 5h window opens on a real inference call, and a usage poll does not trip it. On an account the usage probe reads, each usage read is a probe, which is inference and may open the window. Either the window genuinely has not started, or the reading is cached. The refresh countdown on the Overview tab turns yellow on last-known numbers and red when the last fetch failed.

**Usage numbers are stuck.** Only one clauth instance fetches at a time. If a daemon holds the lease, an open TUI reads its results instead of polling itself, and picks the lease back up within a tick of the daemon exiting. `clauth daemon --status` says whether one is up.

**An account has a `×` next to it.** Either its login was rejected for good, so it is quarantined, or it holds no login at all, and its Fallback card reads `logged out`; both exclude it from the chain. On an account that runs on a setup token it can also mean the usage probe found the token refused: the card reads `token rejected`, and a new token `claude setup-token` mints replaces it once `clauth login <name> --setup-token` captures it. A rejected login: run `clauth login <name>` to re-authenticate it. On an account that authenticates by api key, what died is the stored subscription login its usage figures came from, so `clauth login <name> --api-key <key>` is the one that clears the quarantine against the credential that account actually runs on. A bare browser login clears it too and leaves the endpoint and key standing. An account that holds no login: log in with `clauth login <name>`, or add an api key on the Setup tab. On a codex row the `×` means the chain is quarantined; only a new login clears it: `clauth login <name> --codex --browser` ([Codex](Codex#when-a-chain-dies)).

**The chain will not switch to an account that looks fine.** Check the reason on its row. Weekly windows, per-model weekly windows, a spend ceiling, a canceled subscription or an organization's refusal, or a `disabled` flag all take a member out of rotation independently of its 5h number. The [exclusion table](Auto-Switch#excluded-members) lists all of them. One shows on the Usage card and not on the row: `credits refused`, an account that refused a credit-billed request, which the walks pass over for a session that may need credits until nothing else is left, and the global account's walk in its headroom and home passes, until a request on it succeeds or a window reset leaves it headroom ([Auto-switch](Auto-Switch#running-it)). If what excludes it is a per-model week and the sessions you run do not use that model, `start --auto` judges that week against the models a session will run instead of excluding the account outright ([Auto-switch](Auto-Switch#choosing-where-a-session-starts)).

**Auto-switch does nothing at all.** The active account has to be a chain member for the walk to start. An account outside the chain is never switched away from.

**On macOS a switch does not reach my running session.** Claude Code reads the Keychain first there, and it deletes the credentials file once it migrates. clauth mirrors each fresh login into the Keychain for exactly this reason, but an account holding a live `clauth start` session, or an unfinished background job holding the rotating login, is skipped by force-rotate, since its Keychain item belongs to that session's own config dir. [Security](Security#per-platform-behavior).

**Claude Code does not show clauth's tools.** Open the Services tab, move to the `plugin` row. It checks the plugin install record, the `mcpServers` entry, (after <kbd>r</kbd>) whether `clauth mcp` answers a handshake, and `clauth` on `PATH`. <kbd>f</kbd> moves to the row's next fix and <kbd>⏎</kbd> opens it: `install plugin` installs at user scope, and `wire mcp server` writes the `mcpServers` entry into `~/.claude.json` and the copy in every shared runtime tree ([Claude Code plugin](Claude-Code-Plugin)); on the `herdr` row, `heal herdr config` adds clauth's keybinding and sidebar row to herdr's config.

**A `delegate` run did nothing and the tree is unchanged.** A delegate spawns with the permission gate armed and nobody to answer it. Pass the permission flag through `args` for a delegate that writes files, and read the `permission_denials` array in the envelope. [Claude Code plugin](Claude-Code-Plugin#delegate).

**My custom endpoint shows no usage bars.** Only DeepSeek, Z.ai, OpenRouter, MiniMax and Alibaba Model Studio have typed panels. Everything else gets a best-effort scan of the usual usage paths, which can come back empty; the scan retries at most once every five minutes (or once per refresh interval, whichever is longer), and <kbd>r</kbd> forces one immediately. An Alibaba account is the one case where an api key is not enough: run `clauth login <account>` to capture the console session its quota is read with ([Configuration](Configuration#the-alibaba-console-session)).

**The Tokens tab shows `$X+` instead of a figure.** Some model in that period has no published price, or the period reaches into days that carry no cache split. The number is a floor. [Tokens and cost](Tokens-And-Cost#period-lens).

**Two daemons, or none.** A second `clauth daemon` exits immediately by default. `--standby` parks one that takes over when the first dies; `--replace` terminates the running one and takes its place. [Daemon](Daemon).
