# ccsbar UX research — CBAR-4 input corpus

> Produced 2026-07-04 by a multi-agent research pass (3 parallel researchers → 4
> independent design proposals → 3-judge panel → synthesis; workflow run
> `wf_be89c094-898`). This file is the **evidence base**; the resulting design is
> `CBAR-4-DESIGN.md` and the build plan is `CBAR-4-PLAN.md`. All file:line
> references were verified against clauth @ `65c089a` and ccsbar @ `4dfc26a`.

## 1. Why this research exists — the three operator complaints

Filed 2026-07-04 against the CBAR-3 panel (SwiftUI `MenuBarExtra(.window)`,
ccsbar `745dae3`):

1. **"文字太小了"** — text too small. Root cause measured: primary content set in
   SwiftUI `subheadline`/`footnote`/`caption` = 11/10/10pt on macOS, against the
   13pt menu-item convention (§5).
2. **"点一下就切换了…之前可以一个 view 看到全部账号的 usage，现在不能点过去看"** —
   clicking a tile switches the global account immediately; there is no way to
   *inspect* a non-active account, and the previous all-accounts-usage-in-one-view
   glanceability was lost. This is a selection≠activation failure on a
   consequential action (a Keychain rewrite affecting running claude sessions).
3. **"整体的 uiux 用户逻辑可以再提升一下"** — the overall UX logic: switch
   feedback gap, daemon-death invisibility, config comprehension, color-semantics
   overload.

Both live incidents in §6 are these complaints materializing with real damage.

## 2. Product-logic map (code-grounded)

**Summary:** Complete product-logic map of clauth daemon + ccsbar. The hero job is unattended auto-switch down a fallback chain before the 5h window blocks a session (daemon executes it headless; the menu bar exists to give confidence in it). The daemon rewrites ~/.clauth/status.json every 1s tick and serves 8 socket commands on clauthd.sock; ccsbar polls the file every 4s, fires commands over the socket, and re-reads 1.2s after each command. Field audit: of ~20 status.json fields, the panel fully shows 7, partially shows 6, and silently drops 7 — including every daemon-liveness signal (generated_at, next_refresh_at, fetched_at) and has_live_session. The two built-but-unused Swift helpers (statusMtime, daemonSocketExists) prove daemon-death detection was intended but never wired: a dead daemon leaves the panel showing frozen numbers indefinitely with zero cue. Command coverage is 7/8 (snapshot unused by design); all command errors are silently swallowed. Critical designer semantics: armed = the chain member auto-switch would rotate AWAY from (the active one, max one at a time; zero armed = auto-switch disabled); threshold = leave-at %, 100% = last-resort sink; exhaustion requires a LIVE 5h window; wrap-off switches everything OFF when the chain is spent; third-party profiles have no windows, only an availability bool; and the daemon silently drops manual switches to busy profiles and silently stalls auto-switch on credential divergence — neither is visible in status.json.

### Jobs-to-be-done, ranked

1) HERO: unattended auto-switch before the 5h window blocks a session — the daemon's stated reason to exist ('this is what makes unattended auto-switch work with the TUI closed, the operator's core requirement'); the menu bar's reason to exist beyond 'a pretty switcher' is the auto-switch status readout: is the chain armed, when will it rotate. 2) Glance the active account + its 5h % from the menu bar without opening anything (gauge glyph + name + 5h%, '—' when no data). 3) Switch now with one click (AccountTile tap; PanelView comment calls the switcher 'the hero — switching is the point'; shells `clauth <name>` if the daemon is down). 4) Compare 5h headroom across all accounts at a glance (tile row, tiny 3pt meters). 5) Read active-account detail: Session/Weekly/Fable bars, reset countdowns, tier, staleness caption. 6) Monitor + configure the chain from the bar: add/remove/reorder members, per-member threshold, wrap-off toggle (CBAR-2 scope extension). 7) Force a usage re-fetch ('Refresh now'). 8) Detect daemon death — a real job the code half-builds: empty state covers only 'status.json never existed'; the stale-file case (daemon died after running once) is unhandled despite helpers existing for it. 9) Monitor third-party endpoint availability (red/green dot). 10) Invisible daemon-side jobs a designer should surface: wrap-off recovery (find_recovered_member re-activates a member that regained headroom) and auto_start (a 1-token Haiku ping keeps a 5h window open).

> Evidence: clauth/src/daemon/mod.rs:5-8, clauth/docs/ccsbar/DESIGN.md:22-29, ccsbar PanelView.swift:47,156-183, AppMain.swift:37-53, DaemonClient.swift:38-41, ConfigView.swift:9-121, PanelView.swift:131,139-149, clauth/src/fallback.rs:237-258, clauth/src/profile.rs:138-139

### Field inventory — top-level status.json

schema (int, currently 1): decoded, never branched on or shown — NO. generated_at: decoded into DaemonStatus.generatedAt, used NOWHERE — NO (the daemon-death signal, see separate finding). active_profile: decoded but the UI derives active from profiles[].active instead — YES effectively (tile fill, header, menu-bar label). wrap_off: YES twice — chain-section caption 'wrap-off on'/'stay on last' (PanelView:114) and the Configure On/Off pill (ConfigView:106-121). refresh_interval_ms: decoded, never shown — NO. fallback_chain (ordered names, exists so the bar can render without sorting per-profile positions): YES — ChainStrip chips joined by arrows, empty-chain fallback text 'None — add accounts below' (PanelView:109-125,214-244).

> Evidence: clauth/src/daemon/status_json.rs:135-151, ccsbar DaemonStatus.swift:5-37, PanelView.swift:109-125,214-244, ConfigView.swift:106-121; grep confirms generatedAt/refreshIntervalMs/schema appear only in DaemonStatus.swift

### Field inventory — per-profile

name: YES (tiles, header, chips, config rows, menu-bar label). active: YES (accent-filled tile, active-first ordering, menu-bar label). provider: PARTIAL — shown as header text only when provider != 'anthropic' AND tier is nil; also branches the usage section (windows vs availability dot). base_url: NO (decoded, never rendered). tier: PARTIAL — active-account header only; tiles never show plan (PanelView:66-67). has_live_session: NO. fetch_status: PARTIAL — active account only, as a '· Cached/Failed/RateLimited' caption via isStale (fetchStatus != 'Fresh'); a Failed fetch on a non-active chain member is invisible (PanelView:62-64, DaemonStatus:91). fetched_at: NO. next_refresh_at: NO. auto_start: NO. bell_threshold: NO. fallback.position: PARTIAL — only gates move-button disabling (ConfigView:44-45); chip order conveys it, number never shown; 1-based. fallback.threshold: YES — chain chip '95%' label (PanelView:234), Configure threshold menu (ConfigView:65-84), and it recolors the Session bar (PanelView:84 → Theme.usageColor threshold). fallback.armed: YES — bolt icon + accent glow + semibold on the armed chip (PanelView:228-243). windows[]: PARTIAL — active account gets Session(5h)/Weekly(7d)/Fable(7d fable) rows with %, bar, 'resets in …'; inactive accounts get ONLY the tiny 5h tile meter — their 7d/fable data is fetched, serialized, decoded, and dropped; any other window label (e.g. '7d sonnet') is dropped even for the active account. third_party.available: PARTIAL — dot + Available/Unavailable/No-data-yet, active account only; an inactive third-party tile renders a misleading empty 5h bar (fiveHourPct defaults to 0 with no windows).

> Evidence: clauth/src/daemon/status_json.rs:116-131, ccsbar DaemonStatus.swift:39-92, PanelView.swift:59-105,156-183,228-243, ConfigView.swift:41-50,65-84, AppMain.swift:41-51; grep shows hasLiveSession/fetchedAt/nextRefreshAt/autoStart/bellThreshold/baseUrl never referenced outside DaemonStatus.swift

### High-value UNSHOWN fields (designer opportunities)

(1) generated_at — the ONLY way to know the daemon is alive: it rewrites status.json every 1s, so generated_at older than a few seconds = daemon dead; currently the panel shows frozen data as if live. (2) next_refresh_at — a per-profile 'next refresh in Xs' countdown is the cheapest continuous proof-of-life + data-currency signal (DESIGN.md v1 feature list explicitly planned it; never built). (3) has_live_session — true when a `clauth start` isolated session is live on that profile; a 'in use' badge would warn before switching away from an account mid-run, and the daemon itself already refuses auto-switch while a target is busy. (4) fetch_status per non-active profile — a chain whose backup member's fetches are Failed/RateLimited is a hero-feature hazard the user cannot see. (5) fetched_at — 'as of 12s ago' honesty stamp. (6) bell_threshold — per-profile alert % (fires a bell toast in the TUI overview); the bar could mirror the alert. (7) auto_start — profile opted into keep-window-open pings ('Fires a 1-token Haiku ping each 30s tick while no 5h window is active'); invisible, yet it changes how usage numbers behave. (8) third_party balance is deliberately deferred in the contract — only the bool ships ('expose only the availability flag for now — balance deferred').

> Evidence: clauth/src/daemon/status_json.rs:106-114,122-127,137, clauth/src/daemon/mod.rs:62,266-276, clauth/src/profile.rs:138-139,147-149, clauth/src/runtime.rs:131-135, clauth/docs/ccsbar/DESIGN.md:257-261, ccsbar DaemonStatus.swift:46-51

### Command inventory — socket → UI gesture

8 socket commands (newline-delimited JSON, one per connection, on ~/.clauth/clauthd.sock, 0600). snapshot → NOT used by ccsbar (by design: display polls the file; snapshot exists for other clients/debugging). switch {profile} → EXPOSED: tap an account tile; falls back to shelling `clauth <name>` when the socket is absent — the only command with a no-daemon fallback. refresh {profile?} → EXPOSED: 'Refresh now' action row, always sends nil = all profiles (per-profile refresh supported by the wire but not surfaced); socket-only, silently no-ops when the daemon is dead. fallback_add {profile} → EXPOSED: 'Add' (+circle) button on non-member rows in Configure. fallback_remove {profile} → EXPOSED: minus.circle icon button. fallback_move {profile, dir:up|down} → EXPOSED: chevron buttons, disabled at chain boundaries via fallback.position. set_threshold {profile, value 0..=100} → EXPOSED: capsule menu but limited to 5 presets [50,80,90,95,100] though the wire accepts any 0-100 float (socket validates range, daemon clamps). set_wrap_off {value:bool} → EXPOSED: custom On/Off pill. Socket-side validation: unknown profile/cmd, missing dir, non-numeric/out-of-range threshold, non-bool wrap_off all return {ok:false,error}. Config edits require a live daemon — no CLI fallback for any fallback_*/set_* command.

> Evidence: clauth/src/daemon/socket.rs:3-17,102-176, ccsbar DaemonClient.swift:38-82, StatusModel.swift:37-43, PanelView.swift:131,161, ConfigView.swift:13,44-59,65-121

### Command feedback: errors are swallowed, ok means 'accepted' not 'done'

Two-layer gap. (a) Protocol: every socket command only ENQUEUES (switch→pending_switch, refresh→refetch_queue, config edits→pending_config_ops); the daemon's main loop drains them on its next 1s tick. 'An ok reply means accepted; the caller polls status.json to see it land' — there is no completion signal. (b) UI: DaemonClient.sendCommand returns nil for ANY failure ({ok:false}, no socket, short read) and every StatusModel command is fire-and-forget (@discardableResult, results ignored) — a rejected command, a dead daemon, or a dropped edit produce zero user feedback; the panel just never changes. A designer needs a pending/failed affordance: currently 'nothing happened' is the UI for at least four distinct states (in flight, rejected, daemon dead, silently dropped).

> Evidence: clauth/src/daemon/socket.rs:14-17,113-173, ccsbar DaemonClient.swift:88-95, StatusModel.swift:35-49

### Timing constants and the full feedback loop

Daemon main loop tick: 1s (TICK, daemon/mod.rs:62) — executes queued switches/config edits and rewrites status.json atomically every tick. Scheduler tick: 1s (TICK_INTERVAL, scheduler.rs:20); actual usage fetches run on refresh_interval_ms — default 90s, clamped 10s..1h. ccsbar poll: 4s repeating Timer (StatusModel init). Post-command settle: one extra reload 1.2s after firing any command ('the daemon applies queued edits on its next ~1s tick; re-read shortly after so the panel updates without waiting for the 4s poll'). Click-to-visible-switch path: tap → socket enqueue (ms) → daemon drains on next tick (0-1s) → switch_profile executes + status.json rewritten (≤1s later) → UI re-reads at +1.2s (best case) or the next 4s poll (worst case ≈4s+). So the tile highlight moves 1.2-4+ seconds after the tap with NO optimistic or in-flight state — the user can tap again in the gap. Usage numbers after 'Refresh now' take longer still: enqueue → scheduler fetch on its own throttled cadence → cache write → next status.json tick → next poll.

> Evidence: clauth/src/daemon/mod.rs:60-62,266-276, clauth/src/usage/scheduler.rs:20, clauth/src/profile.rs:260-268, ccsbar StatusModel.swift:14-19,45-49

### Daemon-dead detection: designed for, never wired

When the daemon dies, status.json stays on disk frozen; ccsbar's readStatus() decodes it fine, so the panel keeps rendering stale numbers as if live — the hero feature (auto-switch) is dead and the confidence UI actively lies. The empty state ('clauth daemon not running — start it with clauth daemon') fires ONLY when status.json is absent or unparseable, i.e. first-run before any daemon ever ran. The detection primitives already exist unused: DaemonClient.statusMtime() ('cheap change detection') and DaemonClient.daemonSocketExists ('a daemon is likely live') are defined and never called anywhere; DaemonStatus.generatedAt is decoded and never read. Correct signal for a designer: generated_at (or file mtime) older than a few seconds = daemon dead — the daemon guarantees a rewrite every 1s tick, so even ~10s of age is unambiguous. Note the socket file is weaker evidence (a crashed daemon leaves a stale socket; the next daemon removes it on bind).

> Evidence: ccsbar DaemonClient.swift:19-33 (statusMtime, daemonSocketExists — zero call sites per grep), DaemonStatus.swift:7,30, PanelView.swift:12-17,139-149, clauth/src/daemon/mod.rs:262-276, clauth/src/daemon/socket.rs:66-68

### Two silent hero-feature stalls invisible in status.json

(1) Busy-target drop: drain_pending_switch drains the queue then `continue`s past any target that is not idle (mid-fetch/rotation) — for a MANUAL socket switch the target is simply lost (auto-switch self-heals because the scheduler re-evaluates every tick while the threshold stays crossed, re-enqueueing). The user taps a tile, gets ok, and nothing ever happens. (2) Divergence guard: when the active profile's live credentials have diverged unsaved, the daemon SKIPS both auto-switch and wrap-off switch-off, logging only to stderr ('resolve in the TUI') — the daemon cannot prompt. status.json carries NO field for either condition, so the menu bar cannot display 'auto-switch blocked — open the TUI' even though this is precisely the moment the confidence UI matters most. A designer should treat 'armed but blocked' as a required state and this as a contract gap to raise (needs a status.json field).

> Evidence: clauth/src/daemon/mod.rs:114-119 (active_diverged_unsaved), 321-345 (is_idle continue + divergence skip), 363-385 (switch-off skip), clauth/src/usage/scheduler.rs:1382-1391 (re-enqueue each tick), clauth/src/daemon/status_json.rs:116-151 (no such field)

### Semantics: armed — the member auto-switch would rotate AWAY from

armed = the profile is a chain member AND is currently the active profile ('armed = in the chain AND currently active (the account auto-switch would rotate away from)'). It is NOT 'the next target' and NOT 'auto-switch is on for this account'. Exactly zero or one chip is ever armed. Zero armed chips = the active profile is outside the chain = auto-switch is ENTIRELY disabled ('If the active profile isn't in the chain, auto-switch is disabled… It's opt-in' — snapshot_chain returns None and the evaluator never runs). The bolt-glowing chip therefore reads as 'the account being watched; when its Session bar crosses its % label, the daemon hops to the next chip rightward (wrapping)'. A chain with members but no armed chip is a subtle misconfiguration state the current UI renders with no warning.

> Evidence: clauth/src/daemon/status_json.rs:53-67, clauth/src/fallback.rs:72-93 (snapshot_chain None cases), clauth/README.md:224, ccsbar PanelView.swift:213-243

### Semantics: threshold, exhaustion, sinks, and switch order

threshold (default 95, per-profile fallback_threshold override, clamped 0-100) is the 5h utilization % at which the daemon switches AWAY from that member — a leave-at level, not an alert level (bell_threshold is the separate alert knob). Exhaustion requires a LIVE window: only a 5h window with a future resets_at can exhaust; a lapsed or absent window means headroom again regardless of the last-known % ('a lapsed or windowless snapshot means the account has headroom again whatever its last-known utilization says'). Target selection walks the chain starting ONE SLOT AFTER the active member, wrapping: first pass takes any member below its own threshold or never-fetched; second pass takes a threshold==100 member as a last-resort SINK, accepted even at 100% ('Claude Code shows its own out-of-5h-limit message on arrival') — but never sink-to-sink, to avoid ping-pong ('Two maxed sinks switching to each other indefinitely gains nothing'). So a 100% threshold chip means 'terminal parking spot', qualitatively different from 95% — worth a distinct visual treatment. The Session bar's color already keys off the active member's threshold (danger at ≥threshold, warning at ≥0.8×threshold).

> Evidence: clauth/src/fallback.rs:20-44,117-181, clauth/src/fallback_config.rs:126-144, clauth/src/profile.rs:145-149, ccsbar Theme.swift:20-24, PanelView.swift:84

### Semantics: wrap-off and recovery

wrap_off governs what happens when the WHOLE chain is spent and no 100% sink exists (every threshold < 100): ON → the daemon switches every account OFF — clears live credentials, active_profile becomes null, all usage halts (SwitchAction::Off); OFF (default, 'stay on last') → it stays parked on the last member. The ConfigView caption is the canonical user-facing wording: 'When the whole chain is spent: on switches every account off; off stays on the last.' Recovery is automatic: find_recovered_member re-activates the first chain member whose 5h window lapsed or dropped below its threshold. Designers must handle the resulting states: active_profile == null → panel renders the switcher with NO header/usage section (model.active nil) and the menu-bar label collapses to a bare gauge glyph with no name/% — currently indistinguishable from 'no data', though it's actually the system announcing 'I turned everything off on purpose'.

> Evidence: clauth/src/fallback.rs:11-18,175-181,225-258, clauth/src/fallback_config.rs:146-156, ccsbar ConfigView.swift:23, PanelView.swift:31-35, AppMain.swift:41-51, clauth/src/daemon/mod.rs:363-399

### Semantics: provider anthropic vs third-party, and fetch_status values

provider is 'anthropic' for OAuth accounts (which have tier + usage windows) or a display name like 'DeepSeek' for recognised base_url providers. Third-party/api-key profiles have NO usage windows and NO tier — the only signal is third_party.available (bool; structured balance explicitly deferred because it lives in free-text rows). UI rule: never render %-bars for third-party accounts (the current inactive-tile 0%-bar is a known misread). fetch_status enum: 'Fresh' (live fetch within cadence) | 'Cached' (serving last-persisted numbers) | 'Failed' (fetch errored) | 'RateLimited' (429s) | null (never fetched — distinct from stale!). With no live daemon the single-shot derives it from cache mtime: Fresh if younger than one refresh interval, else Cached. Swift's isStale = non-nil && != 'Fresh'; numbers are trustworthy only when Fresh. Window labels are '5h', '7d', and '7d <model display name lowercased>' — the fable weekly window must be matched leniently ('7d fable', '7d fable 5', …). resets_at carries 6-digit fractional-second ISO timestamps that break the stock Swift parser (Theme.parseISO has a 3-step fallback). fallback.position is 1-based. generated_at/fetched_at/next_refresh_at are ISO-8601 UTC.

> Evidence: clauth/src/profile_json.rs:14-46, clauth/src/daemon/status_json.rs:39-51,89-114, ccsbar DaemonStatus.swift:68-92,113-115, PanelView.swift:78-105,156-183, Theme.swift:28-39


## 3. Reference patterns (CodexBar / macOS HIG / menu-bar conventions)

**Summary:** CodexBar's panel is a "one card at a time + tile switcher" design where clicking a provider tile ONLY changes which provider's usage is viewed (pure view selection, accent-highlighted tile); actual account activation is a separate, explicit checkmark-radio submenu ("System Account") plus login-flow menu items. Its typography is dominated by 10pt (.footnote/.caption) with 13pt (.body/.headline) reserved for section titles and provider names, on a 310pt-wide card with 6pt-tall progress bars and 30-39pt switcher rows. CCSwitcher takes the opposite approach — single click on an account row immediately activates it (Keychain + ~/.claude.json swap) with no inspect/activate separation — and its top open issues are silent "switch failed" bugs (one filed by AX himself, 2026-07-02). Empirical measurement on this Mac (macOS 26.5.1) confirms: body=13, callout=12, subheadline=11, footnote=10, caption1=10, caption2=10-medium, headline=13-bold; NSMenu item=24pt tall @13pt font, separator=11pt. macOS convention: single-click-activates rows carry a checkmark/radio affordance (Sound output, Wi-Fi, exit nodes), while click-reveals rows carry a trailing chevron (Control Center "click an item or its arrow to show more options"); CodexBar follows exactly this split and ccsbar should too — tiles/rows select-for-inspection, an explicit checkmarked control or dedicated "Activate"/"Use this account" affordance switches accounts.

### CodexBar interaction model — provider tiles are VIEW switchers, not activators

In Merge Icons mode the menu top shows a segmented tile row (ProviderSwitcherView with .overview + .provider(X) segments). Clicking a tile runs onSelect which sets `self.selectedMenuProvider = selectedProvider` and rebuilds the open menu's card in place — nothing about auth/accounts changes. Selected tile = controlAccentColor background + white icon/text; unselected = clear background + secondaryLabelColor. Each tile carries a 2pt-tall weekly-quota strip (8pt horizontal inset, 2pt bottom inset) so you can compare providers at a glance without switching views.

> Evidence: scratchpad/codexbar: Sources/CodexBar/StatusItemController+Menu.swift:943-968 (onSelect sets selectedMenuProvider), StatusItemController+SwitcherViews.swift:5-46 (ProviderSwitcherSelection enum, selectedBackground=controlAccentColor, quotaIndicatorHeight=2)

### CodexBar account activation is a SEPARATE explicit checkmark submenu

For multiple Codex accounts, CodexBar splits three concerns: (1) account chips in the menu select which account's usage is DISPLAYED (handleCodexVisibleAccountSelection -> settings.selectDisplayedCodexVisibleAccount + scoped usage refresh — view only); (2) a 'System Account' SUBMENU lists accounts as radio items where the live account isChecked AND disabled, and clicking an unchecked item 'promotes' it — atomically swapping the live Codex auth.json (CodexLiveAuthSwapping protocol); (3) 'Add Account...' / 'Switch Account...' menu items start a login flow. This is the inspect-vs-activate separation ccsbar wants: browse freely via tiles, activate only via an explicitly-labeled checkmarked submenu.

> Evidence: Sources/CodexBar/StatusItemController+Menu.swift:1019-1062, Providers/Codex/CodexProviderImplementation.swift:233-258 ('System Account' submenu, isChecked=liveVisibleAccountID, checked item disabled), CodexAccountPromotionService.swift (swapLiveAuthData), StatusItemController+Actions.swift:403-417

### CodexBar panel typography — 10pt dominates, 13pt for titles only

Font tally across Sources/CodexBar UI: .footnote (10pt) x85, .caption (10pt) x31, .subheadline.semibold (11pt) x12, .body (13pt) x11, .caption2 (10pt) x9, .headline (13pt bold) x5. Pattern per metric row: section title .body(13).fontWeight(.medium) -> 6pt-tall UsageProgressBar (single Canvas draw) -> detail lines .font(.footnote)(10pt) in secondary color, VStack spacing 6. Card header: provider name .headline (13 bold), email .subheadline (11), status lines .footnote (10). Switcher tile labels: 11pt (inline, <=3 tiles) or 9pt/8pt (stacked, NSFont.smallSystemFontSize-2/-3).

> Evidence: grep '.font(' over Sources/CodexBar (tally above); MenuCardView.swift:265-269, 395-434 (MetricRow), UsageProgressBar.swift:120 (.frame(height: 6)), ProviderSwitcherButtons.swift (smallSystemFontSize / -2)

### CodexBar panel geometry — exact numbers

menuCardBaseWidth = 310pt. UsageMenuCardLayout: horizontalPadding 20, sectionTopPadding 6, usageSectionTopPadding 10, sectionBottomPadding 6, headerLineSpacing 4, headerColumnSpacing 12. Switcher: icon 16x16pt exact assets (never resampled), inline row height 30pt, stacked row height 36pt (39pt when >=3 rows), row spacing 2 (inline) / 4 (stacked), button padding 4/7 (inline) or 2/4 (stacked), outer padding targets 16pt to align with card grid. Overview KPI blocks: caption2(10) label over headline/subheadline value, mini bar-chart height 58pt, KPI grid spacing 6.

> Evidence: StatusItemController+Menu.swift:10, UsageMenuCardLayout.swift, StatusItemController+SwitcherViews.swift:566-578 (switcherButtonHeight 30/36/39), ProviderSwitcherButtons.swift, InlineUsageDashboardContent.swift:738-789

### CodexBar screenshot confirms the model (codexbar.png, viewed)

Panel = tile row (icon-over-label tiles, selected 'Claude' tile in accent blue with white glyph, thin green/orange quota strips under every tile) -> single provider card below: 'Claude / Updated just now / Max' header, then Session, Weekly, Sonnet sections each as [13pt title / 6pt bar / 10pt '2% used' left + 'Resets in 3h 53m' right], 'Pace: Behind (-42%)' secondary line, then Extra usage and a 'Cost' section with a TRAILING CHEVRON '>' indicating a hover submenu (drill-in), then icon action rows (Add Account..., Usage Dashboard, Status Page) and plain 13pt menu items (Settings.../About/Quit). One provider's detail at a time; comparison happens via the tiles' quota strips or the separate Overview segment.

> Evidence: scratchpad/codexbar/codexbar.png (README line 16), https://github.com/steipete/CodexBar

### CodexBar Overview mode — the 'compare N' pattern

Overview segment shows one compact row per provider (OverviewMenuCardRowView) separated by NSMenu separators. Each row: click = selectOverviewProvider (drill into that provider's full card, i.e., click reveals detail, never activates); hover = attached submenu (makeOverviewRowSubmenu) with the full usage card. Rows are height-cached with fingerprints for cheap in-place reconciliation. Max visible providers is capped (maxOverviewProviders) with per-provider opt-in.

> Evidence: StatusItemController+Menu.swift:532-586 (addOverviewRows, onClick -> selectOverviewProvider, submenu wiring), StatusItemController+OverviewSubmenus.swift:5

### CCSwitcher — click IS activation; its failure mode is silent switch failures

CCSwitcher (XueshiQiao) is a menu-bar dropdown where clicking an account entry activates it immediately — atomically swapping the macOS Keychain entry + ~/.claude.json, no confirmation, no inspect state. Per-account backups live in ~/.ccswitcher/backups.json; token refresh is delegated to `claude auth status`; Keychain reads go through /usr/bin/security to allow 'Always Allow'. Usage monitoring (5h session, weekly, daily cost) is display-only alongside. UX complaints: the only 2 open issues are '#18 switch failed' (2026-07-01) and '#19 Switch 失败' (2026-07-02, filed by xingfanxia) — i.e., the one-click-activate model's weak point is trust: when a swap fails there is no distinct inspect step or verification affordance to catch it. Related comp: Symbioose/claude-account-switcher also does click=activate (restores auth.json instantly) and adds auto-switch-at-limit within the same provider.

> Evidence: https://github.com/XueshiQiao/CCSwitcher (README + issues page), https://github.com/Symbioose/claude-account-switcher

### macOS text style sizes — VERIFIED EMPIRICALLY on macOS 26.5.1

NSFont.preferredFont(forTextStyle:) measured via Swift on this machine: largeTitle=26 regular, title1=22 regular, title2=17 regular, title3=15 regular, headline=13 BOLD (not semibold), body=13 regular, callout=12 regular, subheadline=11 regular, footnote=10 regular, caption1=10 regular, caption2=10 MEDIUM. Correction to the task's priors: caption1 and footnote are BOTH 10pt on macOS (they differ only on iOS: 13/12), and caption2 is also 10pt but medium weight. System constants: systemFontSize=13, smallSystemFontSize=11, labelFontSize=10, menuFont=13, menuBarFont=13, controlContentFont=12, toolTipsFont=11. Control-size fonts: regular=13, small=11, mini=9, large=13. SwiftUI Dynamic Type does not scale these on macOS (fixed sizes). Practical floor: 10pt is the smallest named style; 9pt exists only as the mini-control font — CodexBar's rare 8-9pt uses are stacked-tile labels, its readable floor is 10pt footnote.

> Evidence: scratchpad/fontprobe.swift run output on macOS 26.5.1 (sw_vers: ProductVersion 26.5.1); corroborated by https://developer.apple.com/design/human-interface-guidelines/typography

### NSMenu metrics — measured

On macOS 26.5.1: standard NSMenuItem row = 24.0pt tall (measured by diffing 1/2/3-item NSMenu.size), separator = 11.0pt, menu vertical chrome = ~10pt total, menu item font = 13pt (NSFont.menuFont(ofSize:0)). So a custom panel whose interactive rows are ~24pt with 13pt labels reads as native; CodexBar's 30-39pt switcher rows are deliberately taller because they're two-element (icon+label+quota strip) toggle tiles.

> Evidence: scratchpad/menuprobe.swift output: '1 item: 34.0, 2 items: 58.0, 3 items: 82.0, per-item delta: 24.0; separator height: 11.0'

### HIG rules for menu bar extras and click semantics

Current HIG ('The menu bar'): 'Display a menu — not a popover — when people click your menu bar extra. Unless the app functionality you want to expose is too complex for a menu, avoid presenting it in a popover.' Checkmark rule (HIG Menus, stable since classic HIG): use a checkmark when a toggled item represents an attribute currently in effect; checkmarks mark the selected member of a mutually exclusive group (e.g., active document, font size). Submenu rule: 'Menu items that have a submenu include a triangle [chevron] to differentiate them' — the chevron is the canonical this-click/hover-REVEALS affordance, the checkmark is the canonical this-is-ACTIVE affordance. A row with neither is read as a verb (fires an action).

> Evidence: https://developer.apple.com/design/human-interface-guidelines/the-menu-bar; https://developer.apple.com/design/human-interface-guidelines/menus-and-actions; https://leopard-adc.pepas.com/documentation/UserExperience/Conceptual/AppleHIGuidelines/XHIGMenus/XHIGMenus.html

### Selection-vs-activation conventions in system menus

Sound menu / Control Center Sound: clicking an output device row activates it immediately — the active device is marked (checkmark/highlighted icon), there is no preview state. Wi-Fi menu: clicking a network JOINS it; detail is a modifier path (Option-click reveals diagnostics; hovering an SSID shows condensed info). Control Center composite modules: 'click an item or its arrow [chevron] to show more options' — chevron/arrow = drill-in, direct control = act. Tailscale menu bar popover: sectioned (This Device / My Devices / Shared Devices / Exit Nodes / Quick Actions); the on/off toggle is a switch, exit-node click = activate, device click = utility action (copy IP) with detail one level deeper. 1Password: menu bar icon opens Quick Access, a search-first popover — the 'too complex for a menu' escape hatch the HIG allows. Synthesis for ccsbar: single-click may activate ONLY when the row visually reads as a radio/checkmark group of interchangeable endpoints; anything shown as a tile/card with rich data reads as select-for-inspection, and activation there needs its own explicitly-verbed control ('Use this account' button, or checkmark radio list).

> Evidence: https://support.apple.com/guide/mac-help/quickly-change-settings-mchl50f94f8f/mac; https://support.apple.com/guide/mac-help/change-the-sound-output-settings-mchlp2256/mac; https://support.apple.com/guide/mac-help/use-the-wi-fi-status-menu-on-mac-mchlfad426fa/mac; https://tailscale.com/blog/macos-notch-escape; https://support.1password.com/quick-access/

### Compare-N-metrics patterns (iStat Menus, Stats, CodexBar)

iStat Menus (bjango): one status item per module or combined items; the dropdown stacks sections of label+value rows and mini charts; users reorder/hide sections; hover submenus carry per-process detail. Its designer's guidance for the bar icon: 22pt working area, 16x16pt glyph, template images. Stats (exelban): per-module menu bar widget; click opens a per-module popup with a dashboard block (chart on top, label+value rows below, top-process list). CodexBar's three-layer answer to comparing N providers is the most transferable to ccsbar: (L1) 2pt quota strips under every tile = always-visible comparison; (L2) Overview = one compact row per account with name + bars, hover submenu for full card; (L3) single-account card = full detail. Compact-row recipe from its overview KPIs: 10pt caption2 label over 11-13pt value, 6pt bars, 6pt section spacing, 310pt width.

> Evidence: https://bjango.com/articles/designingmenubarextras/; https://bjango.com/mac/istatmenus/; https://github.com/exelban/stats; scratchpad/codexbar InlineUsageDashboardContent.swift:738-818

### Concrete redesign implications for ccsbar

(1) Separate inspect from activate exactly like CodexBar: account tiles/rows change the viewed card; activation lives behind a checkmarked radio group or an explicit 'Use this account' verb — never bare row-click on a data-rich card (CCSwitcher's bare-click model correlates with its 'switch failed' trust complaints). (2) Give activation a state signal: checkmark on the live account, disabled state on the already-active item (CodexBar disables the checked item). (3) Chevron only on rows that reveal detail; no chevron on rows that act. (4) Type ramp that reads native at 310pt width: 13pt bold headline for account name, 13pt medium section titles, 11pt for email/plan, 10pt for all metric/status lines; nothing below 10pt except optional 9pt stacked-tile labels. (5) Rows: 24pt for plain menu-style action rows, 30pt+ for icon+label toggle tiles, 6pt progress bars, 20pt horizontal card padding, 6/10pt section spacing. (6) Always-visible comparison layer: thin per-account quota strips under the switcher tiles are cheaper than a full compare table and proven in CodexBar.

> Evidence: Synthesis of all above; CodexBar sources at /private/tmp/claude-501/-Users-xingfanxia-projects-devtools-clauth/c81e7368-33a7-4fcc-8a9d-8b1803cf50a0/scratchpad/codexbar


## 4. Heuristic UX audit of the current panel

**Summary:** Heuristic evaluation of the ccsbar panel confirms all three operator complaints with code-level evidence, plus systemic issues. (1) Typography: the panel is set almost entirely at 10-11pt — usage percentages, tile names, reset hints, and all Configure controls are .footnote/.caption (10pt on macOS), below the 13pt menu-item convention; the ONLY 13pt text is the two ActionRows, and only because they never set a font. (2) The AccountTile conflates inspection with activation: its sole gesture is an unlabeled single click that immediately rewrites the global Keychain credential (affecting live claude sessions), with no confirmation, no per-account usage detail (tiles carry only a 3pt-tall 5h bar, no %, no 7d/Fable), and no way to view a non-active account — a direct regression from the NSMenu design that showed every account's three windows in one view. (3) Systemic UX gaps: zero feedback after switching (fire-and-forget socket commands, Void/discarded results, 1.2s settle + 4s poll, no optimistic state or spinner), tiles reorder under the cursor after a switch, generated_at staleness is decoded but never checked so a dead daemon renders frozen data as healthy, the terracotta accent carries five distinct meanings while the sapphire color documented for 'armed' is dead code, Catppuccin Mocha dark-palette status hues fail contrast in light mode (danger ≈2.3:1), the wrap-off explainer copy is unparseable, chain-chip threshold '95%' is misreadable as usage, config hit targets are 18pt with a 7pt chevron glyph, and keyboard navigation regressed to nothing versus NSMenu. Findings below: 3 blockers, 12 majors, 10 minors, each tagged with heuristic, severity, evidence, and the complaint it maps to.

### BLOCKER | Click-to-switch conflation — selection IS activation

Heuristic: User control & freedom + Error prevention. Severity: blocker. Maps to complaint 2. AccountTile's only interaction is Button(action: onTap) wired directly to model.switchTo(p.name) — an immediate global side effect (Keychain credential rewrite affecting any running claude session) behind an unlabeled single click. No confirmation, no 'view without switching' mode, no undo affordance beyond firing a second destructive switch. The only hint it switches at all is a hover-delayed tooltip .help("Switch to X"). A destructive-ish global action should never be the zero-friction default gesture on what looks like a selectable tab.

> Evidence: PanelView.swift:52 (AccountTile(p:){ model.switchTo(p.name) }), PanelView.swift:161-181, StatusModel.swift:37

### BLOCKER | Non-active accounts' usage is uninspectable

Heuristic: Recognition rather than recall + Visibility of system status. Severity: blocker. Maps to complaint 2 verbatim ('之前可以一个view看到全部账号的usage'). usage() renders ONLY model.active — the sole per-account data on a non-active tile is a 3pt-tall five-hour bar with no percentage label, no Weekly, no Fable. The previous NSMenu showed every account's 5h/7d/fable in one view; now inspecting account B's weekly usage REQUIRES mutating global state (switching to it), then switching back — two Keychain rewrites to answer a read-only question. Fix direction: per-tile popover/expand on hover or secondary click, or restore an all-accounts usage list with an explicit 'Switch' affordance separated from the row.

> Evidence: PanelView.swift:31-35 (if let active = model.active { … usage(active) }), PanelView.swift:166-170 (UsageBar height: 3, fiveHourPct only)

### BLOCKER | Typography scale — primary content set at caption sizes

Heuristic: macOS conventions (13pt menu-item standard) + Aesthetic & minimalist design. Severity: blocker. Maps to complaint 1 verbatim. Full audit (macOS pt): title2=22 active account name (PanelView:61, the only large text); body=13 ONLY the two ActionRow titles — and only because no font is set (PanelView:257); subheadline=11: tier/provider (67-69), availability text (103), 'Fallback chain' header (112), UsageRow labels Session/Weekly/Fable (193), empty-state label (141), 'Configure' (ConfigView:28-29); footnote=10: 'X% used' and reset hints — THE core data of the app (197-201), 'no data yet' (206), 'None — add accounts below' (118-119), config profile names (ConfigView:37-38), 'Add' (55-56), threshold values (75), 'Wrap-off mode' (108); caption=10: tile names in the hero switcher (164), stale indicator (63), chain chip names (233), 'wrap-off on'/'stay on last' (114-115), On/Off pill (111-112); caption2=10: wrap-off explainer (ConfigView:24), chain arrows (221); explicit micro-sizes: chip threshold system 10pt (235), bolt 9pt (232), menu chevron 7pt (ConfigView:73). Tile names additionally allow minimumScaleFactor(0.8) → 8pt effective (165). Verdict: primary content wrongly at caption sizes = switcher tile names (the self-described 'hero'), the usage percentages and reset times (the app's raison d'être), and every Configure control. All should be ≥13pt body; labels/metadata ≥11pt.

> Evidence: PanelView.swift:61,63,67-69,103,112-119,164-165,193,197-206,221,232-235,257; ConfigView.swift:24,28-29,37-38,55-56,73-76,108,111-112

### MAJOR | No feedback after switching — up to ~4s of dead air

Heuristic: Visibility of system status. Severity: major. Maps to complaint 3 (and compounds complaint 2). switchTo fires the socket command then schedules a single re-read 1.2s later; if the daemon's tick hasn't landed by then, the UI stays stale until the 4s poll. Meanwhile: no optimistic active-state on the clicked tile, no spinner, no disabled state, no highlight change on press. The user clicks, nothing visibly happens for 1.2-4s, so they click again — firing redundant global switches. Same gap applies to Refresh (no spinner, no 'last refreshed' feedback).

> Evidence: StatusModel.swift:16 (4s timer), StatusModel.swift:37,43,47-49 (settle() single 1.2s delayed reload); no @Published pending/loading state anywhere in StatusModel.swift

### MAJOR | Silent command failure — all daemon errors swallowed

Heuristic: Help users recognize, diagnose, and recover from errors. Severity: major. Maps to complaint 3. Every daemon command is fire-and-forget: DaemonClient.switchTo returns Void; fallbackAdd/Remove/Move/setThreshold/setWrapOff return Bool but are all @discardableResult and every StatusModel call site discards them. If the socket is gone, the payload malformed, or the daemon rejects the command, the UI shows the old state with zero explanation — indistinguishable from the feedback gap above. Nielsen violation is structural: there is no error surface (no toast, no inline message, no published error state) in the entire app.

> Evidence: DaemonClient.swift:38 (switchTo → Void), DaemonClient.swift:55-88 (@discardableResult chain), StatusModel.swift:37-43 (results discarded)

### MAJOR | Tiles reorder under the cursor after a switch

Heuristic: Consistency & standards (spatial stability). Severity: major. Maps to complaints 2+3. orderedProfiles sorts active-first, so clicking tile #3 causes it to jump to slot #1 — ~1.2s AFTER the click, when settle() lands. The layout shifts under the user's cursor mid-interaction, breaks spatial muscle memory ('my work account is the third tile'), and makes the delayed feedback (finding above) read as a glitch. File order is stable; active-pinning destroys that stability for no inspection benefit since the active account already gets the accent fill.

> Evidence: StatusModel.swift:31-33 (sorted { a.active && !b.active }), interacting with StatusModel.swift:47-49 (1.2s delayed reload)

### MAJOR | Daemon-dead / stale-data detection is silent

Heuristic: Visibility of system status. Severity: major. Maps to complaint 3. status.json's generated_at IS decoded (DaemonStatus.generatedAt) but never read by any view — verified by grep: zero usages outside the decoder. If the daemon dies leaving a stale status.json, the panel renders frozen usage numbers as if healthy, forever. The only staleness signal is per-profile fetchStatus rendered as a 10pt secondary caption ('· stale') next to the header. The menu-bar label degrades to a bare gauge icon when status is nil — indistinguishable from 'healthy, no account'. Needed: panel-level generated_at age check with a loud banner ('daemon not responding — data from 12m ago') and a menu-bar visual state for dead/stale.

> Evidence: DaemonStatus.swift:7,30 (generatedAt decoded); grep confirms no view usage; PanelView.swift:62-64 (10pt caption stale hint); AppMain.swift:40-52 (no dead-daemon signal)

### MAJOR | One terracotta, five meanings — and the armed color is dead code

Heuristic: Consistency & standards. Severity: major. Maps to complaint 3 + explicit color-semantics audit item. Theme.accent (#D97757) simultaneously encodes: (1) ACTIVE account tile background (PanelView:175), (2) ARMED chain chip glow/text (PanelView:239-242), (3) HEALTHY usage-bar fill (Theme.usageColor, pct < 0.8×threshold), (4) interactive tint — Configure DisclosureGroup + Add button (ConfigView:31,56), (5) wrap-off ON pill (ConfigView:114). A user cannot learn 'terracotta = X'. The kicker: Theme.sapphire (#43ABE5) is defined with the comment '(focus/armed)' — the disambiguation was designed — but grep confirms it is never referenced anywhere. The armed chip should be sapphire per the theme's own documentation; the code/comment drift is also an agent-maintainability bug (one source of truth violated).

> Evidence: Theme.swift:8-9 (sapphire '(focus/armed)', unused — grep: only the definition line), Theme.swift:20-24, PanelView.swift:175,239-242, ConfigView.swift:31,56,114

### MAJOR | Catppuccin Mocha status hues fail contrast in light mode

Heuristic: Accessibility / error visibility. Severity: major. Maps to complaint 3. The palette is lifted from the TUI's Catppuccin MOCHA — a dark-background theme — but the panel renders in both appearances. Against a light panel: warning #F9E2AF ≈ 1.3:1, danger #F38BA8 ≈ 2.3:1, success #A6E3A1 ≈ 1.7:1 — all fail the WCAG 3:1 non-text minimum. The design intent ('high usage loud', Theme.swift:19) inverts in light mode: the danger bar is the QUIETEST element exactly when the user most needs it. Needs light-mode variants (Catppuccin Latte equivalents) or dynamic colors.

> Evidence: Theme.swift:10-12 (hex values; computed WCAG ratios vs white: 1.28, 2.33, 1.7), Theme.swift:19 comment

### MAJOR | Active tile fails WCAG AA — 10pt white on terracotta at 3.1:1

Heuristic: Accessibility. Severity: major. Maps to complaints 1+3. The active tile renders its 10pt caption name (shrinkable to 8pt) in white on #D97757 — computed contrast ≈ 3.1:1, below the 4.5:1 WCAG AA minimum for small text. So the single most important label in the panel (which account is active) is both the smallest text AND under-contrast. Larger text (≥18pt/14pt-bold) would pass at 3:1; bumping the tile name to 13pt semibold + darkening the fill fixes both complaints at once.

> Evidence: PanelView.swift:164-165 (.caption, minimumScaleFactor 0.8), 175 (Theme.accent bg), 178 (white foreground); Theme.swift:8 (#D97757, relative luminance ≈ 0.287 → 3.12:1 vs white)

### MAJOR | Wrap-off copy is unparseable and vocabulary is inconsistent

Heuristic: Match between system and the real world. Severity: major. Maps to complaint 3 (copy explicitly flagged). The explainer — 'When the whole chain is spent: on switches every account off; off stays on the last.' — embeds the toggle states 'on'/'off' as sentence subjects, forcing a parse like 'on switches...off' (reads as a verb phrase); 'switches every account off' does not say what actually happens to the user's session. It is also 10pt caption2 tertiary. Vocabulary then fragments across three surfaces for one setting: chain header shows 'wrap-off on' / 'stay on last' (PanelView:114), the Configure pill shows 'On'/'Off' (ConfigView:111), and the row is labeled 'Wrap-off mode' — jargon defined nowhere. Rewrite in outcome language, e.g. 'If every account is over its limit: [Rotate to first] / [Stay on last account]'.

> Evidence: ConfigView.swift:23-24 (explainer, .caption2 tertiary), ConfigView.swift:108-119, PanelView.swift:114-115

### MAJOR | Chain chip '95%' is misreadable as current usage

Heuristic: Match between system and real world + Consistency. Severity: major. Maps to complaint 3. The chain chip renders the fallback THRESHOLD as a bare percentage ('name 95%') in a panel whose every other percentage is CURRENT USAGE ('34% used', menu-bar '12%'). Nothing distinguishes the two semantics — a user scanning the chain will read '95%' as that account being nearly spent. Needs a discriminator: '@95%', '→95%', or 'at 95%'.

> Evidence: PanelView.swift:234-235 (Text("\(Int(fb?.threshold ?? 95))%")) vs PanelView.swift:197 ("% used") and AppMain.swift:46

### MAJOR | Armed-chip meaning is undiscoverable

Heuristic: Recognition rather than recall + Help & documentation. Severity: major. Maps to complaint 3 (armed-chip audit item). The bolt icon + accent glow on the armed chain member is explained only in a source comment ('the one auto-switch would rotate away from'). The chip has no .help tooltip (every other interactive element has one), no legend, no label. A user sees a random account glowing terracotta — which ALSO means 'active' on tiles two sections up — with a lightning bolt, and has no path to learn what it means. Minimum fix: .help("Auto-switch armed: will rotate away from this account at N%"); better: a one-line status sentence under the chain ('Watching work — switches to personal at 95%').

> Evidence: PanelView.swift:212-213 (meaning lives in comment only), PanelView.swift:228-243 (chip(for:) — no .help modifier)

### MAJOR | Config hit targets: 18pt buttons, 7pt glyph, 0.2-opacity disabled state

Heuristic: macOS conventions (comfortable click targets ~24pt+) + Accessibility. Severity: major. Maps to complaint 3 (chevron audit item). The reorder/remove iconButtons are 18×18pt frames with 12pt glyphs; the threshold menu capsule is ~18pt tall with a 7pt chevron.down — 7pt is decoration-sized and near-invisible as the sole 'this is a menu' affordance; the wrap-off pill is ~19pt tall. All sit below the ~24pt comfortable mouse-target floor, packed adjacently so mis-clicks hit the neighbor (Move-down sits beside Remove-from-chain — a destructive-adjacent slip). Disabled chevrons at Color.primary.opacity(0.2) are barely distinguishable from enabled .secondary ones.

> Evidence: ConfigView.swift:95-103 (18×18 frame, 12pt glyph), ConfigView.swift:73 (chevron .system(size: 7)), ConfigView.swift:76,113 (2-3pt vertical padding), ConfigView.swift:88 (opacity 0.2 disabled tint)

### MAJOR | Keyboard access regressed to zero versus NSMenu

Heuristic: Flexibility & efficiency of use + Accessibility. Severity: major. Maps to complaints 2+3. The old NSMenu design gave arrow-key navigation, type-select, Return-to-activate, and per-item key equivalents for free. The MenuBarExtra(.window) panel is built entirely from .buttonStyle(.plain) custom buttons with no .keyboardShortcut, no FocusState, no focus ring styling — grep shows zero keyboard affordances. Refresh, Quit, switching, and every Configure control are mouse-only. Also worth noting: UsageBar's accessibility label omits which window it measures ('34 percent used' ×3 identical for VoiceOver), and AccountTile's a11y role reads as a plain button named after the account with no hint that activation switches global state.

> Evidence: AppMain.swift:30 (.menuBarExtraStyle(.window)), PanelView.swift:180,268 + ConfigView.swift:81,101 (.plain buttons; no keyboardShortcut/focus in any file), Theme.swift:74 (context-free a11y label)

### MINOR | No hover state on tiles or config buttons — inconsistent affordance

Heuristic: Consistency & standards + affordance visibility. Severity: minor. Maps to complaints 2+3. ActionRow implements a hover highlight (onHover + 0.08 primary wash), but AccountTile — the most consequential control in the app — has no hover response at all, nor do the config icon buttons or chain area. The panel teaches 'rows light up when clickable' then leaves the destructive tiles inert-looking. Hover feedback on tiles would also partially mitigate the missing click feedback.

> Evidence: PanelView.swift:247-271 (ActionRow hovering state) vs PanelView.swift:156-183 (AccountTile: no onHover), ConfigView.swift:95-103

### MINOR | Visual hierarchy inverted — hero has the smallest text

Heuristic: Aesthetic & minimalist design / scannability. Severity: minor (symptom of blocker #3). Maps to complaints 1+3. Priority order top-to-bottom is defensible (switcher → active usage → chain → config → actions), but the type scale fights it: the self-described 'hero' switcher is set at 10pt caption; the 22pt title2 (active name) sits mid-panel; section headers (11pt semibold) vs body content (10pt) differ by 1pt — near-zero scanning contrast. The eye lands on the account name and the panel reads middle-out instead of top-down.

> Evidence: PanelView.swift:47 ('the hero — switching is the point') vs 164 (.caption); 61 (.title2 mid-panel); 112 vs 197 (11pt header over 10pt content)

### MINOR | Tile row does not scale past ~4 accounts

Heuristic: Flexibility. Severity: minor today, major for a 6-account operator. Maps to complaints 1+2. The switcher is a non-wrapping, non-scrolling HStack inside a fixed 320pt panel; N tiles split ~296pt minus spacing. At 5-6 accounts each tile is ~45-55pt wide, and lineLimit(1) + minimumScaleFactor(0.8) drives names to 8pt effective before truncation. Combined with blocker #2 (no detail without switching), more accounts = less information per account.

> Evidence: PanelView.swift:19 (.frame(width: 320)), 49-55 (plain HStack ForEach), 165 (minimumScaleFactor(0.8))

### MINOR | Zero-profile state missing; empty state only covers unreadable status.json

Heuristic: Help & documentation / error recovery. Severity: minor. Maps to complaint 3. emptyState (daemon-not-running guidance) renders only when status is nil. A running daemon with zero profiles renders: an empty switcher HStack, no header/usage, 'None — add accounts below', and a collapsed Configure that lists nothing — with no pointer to `clauth login`. The daemon-dead copy also puts the critical recovery command in 10pt tertiary caption.

> Evidence: PanelView.swift:13-17, 117-119, 139-149 (recovery text .caption tertiary); ConfigView.swift:18-20 (ForEach over empty profiles)

### MINOR | 'None — add accounts below' misdirects

Heuristic: Match between system and real world. Severity: minor. Maps to complaint 3. The empty-chain hint says 'add accounts below', but (a) Configure is collapsed by default so 'below' points at nothing visible, and (b) Configure's Add button adds an EXISTING profile to the fallback chain — it does not add accounts (that's `clauth login`). Two wrong mental models in seven words.

> Evidence: PanelView.swift:118, StatusModel.swift:10 (showConfig = false), ConfigView.swift:54-59 (fallbackAdd, not account creation)

### MINOR | Third-party row information poverty

Heuristic: Visibility of system status. Severity: minor. Maps to complaint 3 (explicit audit item). A third-party/api-key account's entire status is an 8pt dot + one word ('Available'/'Unavailable'/'No data yet') at 11pt secondary. No last-checked timestamp, no failure reason on Unavailable, no endpoint/model context; the header shows the raw provider id string (e.g. 'openrouter') as-is. 'Unavailable' with no diagnosis and no recovery path is a dead end.

> Evidence: PanelView.swift:92-105 (availabilityRow), PanelView.swift:68-70 (raw p.provider)

### MINOR | Menu-bar label: no warning/danger/dead signaling, unbounded width

Heuristic: Visibility of system status. Severity: minor. Maps to complaint 3. The label communicates active account + 5h% — good — but stays visually identical whether usage is 5% or 99% (no color/symbol change mirroring Theme.usageColor) and degrades to a bare gauge icon when the daemon is dead, indistinguishable from a healthy no-account state. Text(active.name) has no truncation cap, so a long profile name eats menu-bar real estate. Also 5h-only: an account can be weekly-exhausted while the label reads a reassuring '12%'.

> Evidence: AppMain.swift:40-52 (MenuBarLabel; no color state, no lineLimit/fixed width, fiveHour only)

### MINOR | Configure density and discoverability

Heuristic: Aesthetic & minimalist design + Recognition. Severity: minor. Maps to complaint 3 (explicit audit item). Each profile row packs a 64pt-fixed truncating name (no .help to reveal the full name), threshold menu, two reorder chevrons, and remove into ~288pt at 10pt type. Reorder-by-chevron requires N clicks and mental position tracking versus drag-to-reorder; the threshold presets [50,80,90,95,100] are unexplained (why 50?). Collapsed-by-default is a fine calm-panel choice, but since Configure is the ONLY place thresholds/ordering are editable, a first-run user has no scent that the chain is editable at all — the chain strip above is display-only with no affordance linking it to Configure.

> Evidence: ConfigView.swift:13 (presets), 16 (collapsed via model.showConfig=false at StatusModel.swift:10), 35-62 (row layout, 64pt name frame), 86-93 (chevron reorder)

### MINOR | Refresh has no completion feedback

Heuristic: Visibility of system status. Severity: minor (subsumed by the feedback-gap major, listed separately since it's a distinct action). Maps to complaint 3. 'Refresh now' fires DaemonClient.refresh and hopes the 1.2s settle catches the result; nothing spins, no timestamp updates visibly, no 'refreshed 2s ago'. The user cannot distinguish 'refreshed, numbers unchanged' from 'refresh silently failed'.

> Evidence: PanelView.swift:131, StatusModel.swift:43,47-49

### MINOR | Stale indicator is a 10pt appendage on the header

Heuristic: Visibility of system status. Severity: minor (panel-level version is the major above). Maps to complaints 1+3. When a profile's fetch fails, the signal is '· <fetchStatus>' at 10pt caption secondary, baseline-aligned after a 22pt bold name — visually a footnote to the thing it invalidates. The usage bars below continue to render their last numbers at full confidence with no dimming or timestamp.

> Evidence: PanelView.swift:62-64, DaemonStatus.swift:91 (isStale)

## 5. Empirical type audit (measured on-device)


Measured on this machine (`NSFont.preferredFont(forTextStyle:)`, macOS 15 / Darwin 25.5):

| SwiftUI style | macOS pt |
|---|---|
| largeTitle | 26 |
| title1 | 22 |
| title2 | 17 |
| title3 | 15 |
| headline | 13 (semibold) |
| **body** | **13** ← system standard; menu items render at 13pt |
| callout | 12 |
| subheadline | 11 |
| footnote | 10 |
| caption1 / caption2 | 10 |

`NSFont.systemFontSize` = 13, `smallSystemFontSize` = 11.

NOTE: earlier session assumed title2=22 — WRONG, that's title1. title2 = 17pt on macOS.
So the current header `Text(p.name).font(.title2)` is 17pt, not 22.

## Every Text in the current panel → actual pt

| Element | File:line | Style | pt | Verdict |
|---|---|---|---|---|
| Header account name | PanelView:61 | title2 bold | 17 | ok |
| Stale suffix | PanelView:63 | caption | 10 | too small |
| Tier / provider badge | PanelView:67,69 | subheadline | 11 | small |
| Availability text (3p) | PanelView:103 | subheadline | 11 | small |
| "Fallback chain" label | PanelView:112 | subheadline semibold | 11 | too small (section header!) |
| wrap-off state hint | PanelView:115 | caption | 10 | too small |
| "None — add accounts" | PanelView:119 | footnote | 10 | too small |
| Daemon-dead label | PanelView:142 | subheadline | 11 | small |
| Daemon-dead hint | PanelView:144 | caption | 10 | too small |
| **Tile account name** | PanelView:164 | caption | **10** | WAY too small (primary switch target!) |
| Section labels (Session/Weekly/Fable) | PanelView:193 | subheadline semibold | 11 | too small |
| "% used" | PanelView:198 | footnote | 10 | too small (primary metric!) |
| "resets in …" | PanelView:201 | footnote | 10 | too small |
| "no data yet" | PanelView:206 | footnote | 10 | ok-ish (tertiary) |
| Chain chip name | PanelView:233 | caption | 10 | too small |
| Chain chip threshold | PanelView:235 | system 10 | 10 | too small |
| Configure label | ConfigView:29 | subheadline semibold | 11 | small |
| Config row name | ConfigView:38 | footnote | 10 | too small |
| Config Add | ConfigView:56 | footnote | 10 | too small |
| Threshold menu | ConfigView:75 | footnote | 10 | too small |
| Wrap-off label | ConfigView:108 | footnote | 10 | too small |
| Wrap-off pill | ConfigView:112 | caption | 10 | too small |
| Wrap-off explainer | ConfigView:24 | caption2 | 10 | ok (fine print) |
| Action rows (Refresh/Quit) | PanelView (no .font) | body default | 13 | CORRECT — why they look right |

Diagnosis: everything except the 17pt header and the 13pt action rows sits at 10–11pt.
The action rows look right precisely because they use the 13pt default. Primary content
(tile names, % used, section labels) must move to body 13 / callout 12; nothing primary
below 12; fine print only at 11.

Hit targets: config chevrons are 18×18pt frames (ConfigView:99) — Apple minimum
comfortable click target in menus ≈ 24pt; hover highlight missing on config icon buttons.

## Timing facts (verified in source, for the feedback-loop design)

- Daemon main loop `TICK = 1s` (`src/daemon/mod.rs:62`) — socket commands (switch/config) apply within ~1s.
- Usage refresh interval = `refresh_interval_ms` = 90s live (status *data* freshness; per-account fetch staggered by scheduler).
- ccsbar polls status.json every 4s (`StatusModel.swift:16`), plus a one-shot re-read 1.2s after any command (`settle()`).
- Worst-case click→UI-reflects gap: ~1s daemon apply + up to 4s poll = ~5s with NO feedback today (the switch-feedback gap).
- Daemon liveness: `generated_at` advances every tick while alive; if its age ≫ a few seconds the daemon is dead/hung → auto-switch protection is OFF (hero-feature failure state; currently only surfaced if status.json is entirely missing).

## 6. Live incidents (2026-07-04, operator-reported + source-verified)

### Incident A — rate-limited usage fetch = auto-switch blindness (hero-invariant gap)
Operator report (verbatim): "kept getting this rate limited error in querying usage..
plan Claude Max 20x [active] / cl-ax / status [rate limited] · retry in 891s".
Verified in source:
- `usage/scheduler.rs:284,332` — 429 → FetchOutcome::cached(RateLimited, retry_after);
  honors server retry_after (capped, `:530-541`), exponential backoff w/o hint (`:648-651`),
  falls back to disk cache. Sane fetch-side handling.
- `fallback.rs:34-40,96-99` — threshold evaluation reads the UsageStore snapshot and only
  requires `five_hour_live()`; it has NO freshness gate. During a 891s rate-limit window the
  active account's utilization is FROZEN at the last fetch → if the real usage crosses the
  threshold during the window, the daemon cannot see it → session gets blocked before
  auto-switch fires. The exact failure the product exists to prevent.
- status.json exposes fetch_status="RateLimited" + next_refresh_at per profile, but the menu
  bar shows staleness ONLY for the active account as a 10pt caption; a rate-limited chain
  member is invisible.
Design consequences: (1) surface RateLimited + "retry in Xs" per account in the bar;
(2) treat prolonged RateLimited on the ACTIVE account as a DEGRADED-PROTECTION state (loud);
(3) audit question: adaptive fetch cadence / burn-rate extrapolation during blindness — open
policy question, offer options in plan.

### Incident B — mid-run global switch killed 11 subagents ("Not logged in")
While two research workflows ran on this machine, the active account changed (xfx→cl-ax era;
test2 — a credential-less test profile — also present as a one-click tile). Every in-flight
subagent then failed "Not logged in · Please run /login". Whatever the exact trigger (stray
tile click or manual switch), it demonstrates the operator's complaint #2 with real damage:
a single unconfirmed click rewrites the GLOBAL Keychain and breaks every running Claude
Code process on the machine. Switch must be explicit/guarded; panel must show that a switch
affects running sessions (has_live_session exists in status.json, unused).

### Incident C — switch installed a stale/expired token snapshot → global logout (operator-confirmed 2026-07-04)

Operator report: "this log out behavior seems to happen when I switched my account
back to xfx on the clauth menubar item"; asked for: *detect if a profile is
invalid/expired/somehow lost auth, and prevent switching to that.*

Twice during this session, a ccsbar switch back to `xfx` instantly killed every
running Claude Code process on the machine (30+ workflow subagents, twice) with
"Not logged in / 401 Invalid authentication credentials", requiring a manual
`/login` each time.

Source-verified mechanism:
- `actions.rs:114` (comment on `switch_profile_cli`): **"No token rotation — stale
  chains rotate lazily on first use."** A switch installs whatever token snapshot
  the profile store holds, unconditionally.
- On macOS the install is a Keychain rewrite; every running `claude` re-reads the
  Keychain per request → an expired/invalid snapshot logs out *all* running
  sessions at once. "Lazily on first use" is exactly wrong for the fork's headless
  model: there is no interactive "first use" to absorb the failure.
- clauth already owns the machinery to prevent this — `oauth::refresh()`,
  `rotate_one` / `refresh_all` / `rotation_candidates`, `apply_rotated_tokens_locked`,
  and per-profile `access_token_expires_at()` (`oauth.rs:72,336,433,521,616`) — it
  is simply never consulted on the switch path.
- **Hero-invariant escalation:** `grep expire|rotate|refresh` over `fallback.rs`
  and `daemon/mod.rs` returns zero hits — the unattended auto-switch path can
  rotate INTO an auth-dead account at 3am, installing dead credentials with
  nobody watching. Strictly worse than the manual case.

Design consequences (feeds CBAR-4 + the tech plan):
1. **Pre-switch auth gate** (manual CLI, socket `switch`, and daemon auto-switch):
   if `access_expires_at` is past → `oauth::refresh` → install rotated tokens; if
   refresh fails (revoked) → mark profile auth-broken and refuse (manual override
   allowed with explicit force).
2. **`auth_status` per profile in status.json** (`ok` / `expiring` / `broken`) so
   the menu bar can render login health honestly.
3. **Chain-walk exclusion:** auto-switch must skip auth-broken members exactly as
   it skips exhausted ones, and the forecast/armed display must reflect that.
4. **ccsbar surfacing:** auth-broken row badge ("login expired — run clauth
   login xfx"), switch verb disabled/guarded for broken profiles.
