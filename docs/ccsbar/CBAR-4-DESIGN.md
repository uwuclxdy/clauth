# CBAR-4 design spec — "Preflight": the inspect-first ccsbar

> The winning design from a 4-proposal / 3-judge panel (workflow `wf_be89c094-898`,
> 2026-07-04). **Preflight** won unanimously (judges: operator, macOS-HIG purist,
> systems engineer), then absorbed 18 grafts from the losing proposals. Evidence
> base: `UX-RESEARCH.md`. Build plan: `CBAR-4-PLAN.md`.
>
> One sentence: **clicking is looking, switching is a verb, and every silent state
> the daemon has (death, refusal, drop, rotation, wrap-off) gets a loud, fixed home.**

## 1. Final design narrative

FINAL DIRECTION: "Preflight" — the inspect-first ccsbar, with ten grafts from the other three proposals. One sentence: clicking is looking, switching is a verb, and every silent state the daemon has (death, refusal, drop, rotation, wrap-off) gets a loud, fixed home.

CORE MODEL (Preflight, unchanged). The panel is a stable-file-order account LIST where every account shows all three windows with zero interaction (restores complaint 2 verbatim), a detail card below that re-targets on single click (pure view state, zero daemon traffic), and exactly one switch surface: the terracotta "Switch to X" verb in the detail card, plus right-click and Cmd+Return as one-gesture power paths. Two orthogonal markers: terracotta checkmark = ACTIVE (the only terracotta in data display), selection wash + hairline ring = INSPECTED. Rows never reorder; the checkmark badge moves instead. Inspection resets to the active account on every open so the first glance is always live truth.

ROW ANATOMY (grafted from Roster — replaces Preflight's three equal micro-meters). Each Anthropic row is three lines: (1) 13pt semibold name + 11pt tier + badge cluster (sapphire "⚡ watching" chip = armed — a labeled chip, not a bare bolt, so the sapphire hue teaches itself; terracotta check = active; danger "spent"/"week spent"/"5h spent" pill = a 5h and/or weekly window at its cap, and the name mutes to secondary — a pre-attentive "unavailable until it resets", suppressed on frozen data; "in use" = has_live_session; amber "cached/failed" = per-row fetch_status; flag glyph = 100% sink) — the badge cluster holds layout priority so a long name is the sole truncation sink; (2) a full-width 6pt 5h bar with the account's OWN threshold tick rendered inside the track + 13pt monospaced numeral + 11pt reset countdown — distance-to-auto-switch is a visible gap, pre-attentive, zero clicks; (3) half-width 7d and Fable bars at 4pt with 12pt monospaced numerals. Third-party rows (zai) NEVER render %-bars: availability dot + word + "checked Ns ago" honesty stamp.

STATUS STRIP (Preflight's, formalized per Relay as the single exception surface). The top strip is the one place exceptional truth appears, in priority order: dead-daemon banner > switch lifecycle (Switching / Switched / Didn't apply / Refused) > wrap-off card > zero-armed warning > the normal forecast sentence. The forecast sentence is grafted from Watchtower + Roster: "Watching xfx — would switch to cl-ax at 95% · now 62%", computed by ONE pure Swift function mirroring fallback.rs:117-181's chain-walk (skip-active, below-own-threshold-or-never-fetched first pass, 100%-sink second pass, never sink-to-sink), line-pinned to the Rust source, unit-tested against fixture JSON, and worded as prediction ("would switch"), never promise. Naive chain-position+1 is banned everywhere a next-target is named (sentence, detail-card chain line, chip tooltips).

LIVENESS (grafted from Roster — replaces Preflight's 10s binary cliff). A 1s UI clock evaluates generated_at age (statusMtime() as fallback): <5s = green pulse "live"; 5-15s = grey "syncing…"; >15s = red banner "Daemon not responding — data frozen Xm ago. Auto-switch is NOT running." with a one-click [Start daemon] button (grafted from Watchtower/Relay: detached Process spawn, binary resolved via login shell, spinner until generated_at freshens, green "daemon back" flash) plus the copy-command fallback. Rows dim to 60%, every stamp becomes "as of Xm ago", config controls disable, menu bar shows the warning state with age and NO percentage — frozen numbers never impersonate live ones.

SWITCH STATE MACHINE (Preflight's, hardened by two grafts). Click Switch → t=0: button becomes spinner "Switching to cl-ax…", ALL switch affordances disable, poll accelerates 4s→0.5s (strictly bounded: cancelled at confirm or +6s). Instant-failure graft (Roster/systems): a failed socket write or an {ok:false} reply surfaces IMMEDIATELY with cause-specific copy ("Daemon refused the switch: <error>" / "Daemon unreachable") — the 6s timeout is reserved solely for the accepted-then-silently-dropped case ("Switch didn't apply — target busy or daemon stalled. [Retry] [Why?]"). This requires the DaemonClient Result<Void, DaemonError> refactor Preflight already specifies (ok:false vs socket-missing vs short-read, ~7 call sites). Confirm = active flips in the polled file → checkmark animates to the new row, 4s toast in the strip, menu-bar label updates. Live-session guard keeps Preflight's direction (systems judge: guard the CURRENT account, whose credentials a Keychain rewrite strands): if the active account has has_live_session, the first click arms — "Confirm — live session on xfx" in danger tint — and only a second click within 5s fires. CLI-fallback honesty graft (Relay, corrects Preflight's one false state): when the daemon is offline the button reads "Switch via CLI (daemon offline)", shells clauth <name>, and confirms via the PROCESS EXIT CODE — never status.json mtime, because only the daemon's write_status touches that file — labeled "Switched via CLI — auto-switch inactive until daemon starts".

ROTATION HEARTBEAT (grafted from Watchtower). When active_profile changes with no locally-initiated switch pending, the panel (if open) flashes "⇄ rotated to cl-ax" in the strip and the menu bar shows "⇄ cl-ax" for 8s. The hero feature finally has visible proof it fired — derivable entirely from the existing poll.

WRAP-OFF AND MISCONFIGURATION. active_profile = null renders as deliberate system speech, not an error: "All accounts switched off — chain spent. Auto-resumes when a window resets (≤2h 14m)" — the ETA grafted from Watchtower, derived from min(resets_at) across chain members with live windows. Switch verbs stay enabled for manual recovery. Chain-non-empty-but-zero-armed gets an amber strip warning with a one-click [Add xfx to chain] fix, and the menu bar gains Relay's bolt.slash disarmed glyph so a silently broken chain is visible without opening the panel.

CONFIG. Two surfaces, one owner: the right-click context menu (grafted from Relay: Switch / Refresh this account / Add-Remove from chain / Move up / Move down / Leave chain at ▸ 50-80-90-95-Last resort 100% / Copy name — free native NSMenu 13pt/24pt metrics) carries the frequent edits; Preflight's inline Configure disclosure, upgraded to Relay's hit-target standards (28pt rows, 22x22pt glyphs in 28pt targets, 13pt labels, 10pt chevrons), remains the canonical long-form editor with the wrap-off radio in outcome language ("Stay on last account" / "Switch everything off — credentials cleared; resumes when a window resets"). Relay's separate Settings window is REJECTED (see conflicts). Every config command shows optimistic pending shimmer, reverts instantly on {ok:false} with the reason, and flags a red "!" at 6s with no confirmation. Removing the ARMED member requires Watchtower's inline confirm: "This disables auto-switch (the live account leaves the chain). Remove anyway?".

TYPE, COLOR, ACCESSIBILITY. 13pt floor for names, verbs, and the 5h numeral; 11pt metadata; 10pt only for glyph-like meter labels and tertiary stamps; no minimumScaleFactor anywhere — truncate + .help. One meaning per hue (full table below); the dead Theme.sapphire is finally wired to armed; status hues become light/dark dynamic pairs fixing the 2.3:1 light-mode danger failure; the Switch verb fill darkens to #B85C33 so 13pt white passes 4.5:1. VoiceOver graft (Roster): each row narrates its full state in one utterance and the Switch control announces its Keychain consequence. Keyboard: up/down inspection, Cmd+Return switch, R refresh, Esc close, Cmd+Q quit (full nav macOS 14+, shortcut floor on 13).

UPSTREAM ASKS (filed, not blocked on): (1) status.json switch_blocked/blocked_reason field so the 6s heuristic can be replaced and divergence-stall can say "resolve in the TUI"; (2) surface pending_switch in status.json for exact in-flight truth. Scope: Theme rework + StatusModel state machine (pendingSwitch enum, 1s clock, bounded fast-poll, forecast function) + DaemonClient Result refactor + PanelView list/detail rebuild + ConfigView resize. No new dependencies, no daemon release coupling.

## 2. Wireframes — four canonical states

```text
Accounts: xfx (active, Anthropic Max 20x, 5h 62% / 7d 41% / Fable 18%, armed @95, live session), cl-ax (Anthropic Max 5x, 5h 18% / 7d 55% / Fable 41%, chain #2 @100 sink), zai (third-party Z.AI, available). Panel 340pt wide. "|" inside a bar track = that account's threshold tick.

STATE 1 — DEFAULT (panel just opened; inspection reset to active xfx)
╔════════════════════════════════════════════════════╗
║ ⚡ Watching xfx — would switch to cl-ax at 95%      ║ ← forecast sentence, 12pt, sapphire bolt
║    now 62% · ● live · updated 3s ago                ║ ← 11pt; green pulse dot = generated_at < 5s
╟────────────────────────────────────────────────────╢
║ ACCOUNTS                                            ║ ← 11pt semibold uppercase secondary
║ ┌────────────────────────────────────────────────┐ ║
║ │ ✓ xfx           Max 20×           ⚡  ● in use  │ ║ ← ✓ terracotta ACTIVE badge; ⚡ sapphire armed;
║ │   5h ▓▓▓▓▓▓▓▓▓▓▓▓▓░░░░░|░░  62%  resets 1h 12m │ ║   full-width 6pt bar, tick @95, 13pt mono %
║ │   7d ▓▓▓▓░░░░░ 41%    Fb ▓▓░░░░░░ 18%          │ ║ ← half-width 4pt bars, 12pt mono %
║ └────────────────────────────────────────────────┘ ║ ← wash + hairline ring = INSPECTED (view state)
║   ○ cl-ax         Max 5×                        ⚑  ║ ← ⚑ = 100% last-resort sink
║     5h ▓▓▓░░░░░░░░░░░░░░░░░░|  18%  resets 4h 02m  ║ ← sink tick sits at bar end
║     7d ▓▓▓▓▓▓░░░ 55%  Fb ▓▓▓▓▓░░░ 41%              ║
║   ○ zai           Z.AI                             ║ ← third-party: NEVER %-bars
║     ● Available · checked 41s ago                  ║
╟────────────────────────────────────────────────────╢
║ xfx · Max 20×                          Fresh · 3s  ║ ← detail card: 15pt name + 11pt stamp
║ Session 5h ▓▓▓▓▓▓▓▓▓▓▓▓░░░░|░░ 62%  resets 1h 12m  ║ ← 13pt label + 13pt mono % + 11pt reset; 6pt bar
║ Weekly 7d  ▓▓▓▓▓▓▓░░░░░░░░░░░ 41%   resets Tue 9a  ║
║ Fable      ▓▓▓░░░░░░░░░░░░░░░ 18%   resets Tue 9a  ║
║ ⚡ 1st in chain · watched now — would rotate to     ║ ← 12pt; wording driven by forecast engine
║    cl-ax at 95% of the 5h window                   ║
║ ┌────────────────────────────────────────────────┐ ║
║ │ ✓ Active account · live session attached       │ ║ ← static terracotta-outline state, not a button
║ └────────────────────────────────────────────────┘ ║
╟────────────────────────────────────────────────────╢
║ CHAIN  ⚡ xfx @95 → cl-ax @100 ⚑                    ║ ← armed chip sapphire; @ disambiguates threshold
║        when spent: stay on last account    [Edit]  ║
╟────────────────────────────────────────────────────╢
║ ↻ Refresh usage                   updated 3s ago   ║ ← 13pt action rows, 24pt tall
║ ⚙ Configure chain…                                 ║
║ ⏻ Quit ccsbar · daemon keeps running      ⌘Q    ║
╚════════════════════════════════════════════════════╝

STATE 2 — INSPECTING NON-ACTIVE cl-ax (single click on its row; zero daemon traffic)
╔════════════════════════════════════════════════════╗
║ ⚡ Watching xfx — would switch to cl-ax at 95%      ║ ← strip unchanged: inspection ≠ activation
║    now 62% · ● live · updated 6s ago                ║
╟────────────────────────────────────────────────────╢
║ ACCOUNTS                                            ║
║   ✓ xfx           Max 20×           ⚡  ● in use    ║ ← ✓ stays put; rows never reorder
║     5h ▓▓▓▓▓▓▓▓▓▓▓▓▓░░░░░|░░  62%  resets 1h 12m   ║
║     7d ▓▓▓▓░░░░░ 41%    Fb ▓▓░░░░░░ 18%            ║
║ ┌────────────────────────────────────────────────┐ ║
║ │ ○ cl-ax         Max 5×                      ⚑  │ ║ ← selection wash + ring moved here
║ │   5h ▓▓▓░░░░░░░░░░░░░░░░░░|  18%  resets 4h 02m│ ║
║ │   7d ▓▓▓▓▓▓░░░ 55%  Fb ▓▓▓▓▓░░░ 41%            │ ║
║ └────────────────────────────────────────────────┘ ║
║   ○ zai           Z.AI                             ║
║     ● Available · checked 41s ago                  ║
╟────────────────────────────────────────────────────╢
║ cl-ax · Max 5×                        Fresh · 12s  ║ ← detail card re-targeted
║ Session 5h ▓▓▓▓░░░░░░░░░░░░░░| 18%  resets 4h 02m  ║
║ Weekly 7d  ▓▓▓▓▓▓▓▓▓░░░░░░░░ 55%    resets Wed 11a ║
║ Fable      ▓▓▓▓▓▓▓░░░░░░░░░░ 41%    resets Wed 11a ║
║ ⚑ 2nd in chain · last resort — chain parks here    ║
║    even at 100%                                    ║
║ ┌────────────────────────────────────────────────┐ ║
║ │           Switch to cl-ax               ⌘↩     │ ║ ← THE only switch surface; #B85C33 fill,
║ └────────────────────────────────────────────────┘ ║   13pt semibold white, 28pt; .help: Keychain warn
╟────────────────────────────────────────────────────╢
║ CHAIN  ⚡ xfx @95 → cl-ax @100 ⚑                    ║
║        when spent: stay on last account    [Edit]  ║
╟────────────────────────────────────────────────────╢
║ ↻ Refresh usage                   updated 6s ago   ║
║ ⚙ Configure chain…                                 ║
║ ⏻ Quit ccsbar · daemon keeps running      ⌘Q    ║
╚════════════════════════════════════════════════════╝

STATE 3 — MID-SWITCH (clicked "Switch to cl-ax"; xfx has a live session)
step 0 (arm — current account has a live session):
║ ┌────────────────────────────────────────────────┐ ║
║ │  Confirm — live session on xfx           ⌘↩    │ ║ ← danger tint; 2nd click within 5s fires,
║ └────────────────────────────────────────────────┘ ║   else reverts
t=0 → confirm (poll accelerated 4s → 0.5s, hard-capped at 6s):
╔════════════════════════════════════════════════════╗
║ ◌ Switching to cl-ax…                              ║ ← strip = lifecycle surface
║    ● live · updated 1s ago                         ║
╟────────────────────────────────────────────────────╢
║   ✓ xfx  …           (✓ dims to 50%)               ║ ← rows do NOT reorder
║ ┌ ○ cl-ax  … sapphire pulse ring ───────────────┐  ║
╟────────────────────────────────────────────────────╢
║ ┌────────────────────────────────────────────────┐ ║
║ │ ◌ Switching to cl-ax…                          │ ║ ← ALL switch affordances disabled
║ └────────────────────────────────────────────────┘ ║
t≈0.5–1.5s CONFIRMED (active flips in status.json):
║ ✓ Switched to cl-ax                                ║ ← 4s toast; ✓ badge animates xfx → cl-ax;
                                                         menu bar → "◔ cl-ax 18%"
INSTANT failure (no 6s wait — daemon already answered):
║ ⚠ Daemon refused the switch: unknown profile       ║ ← or "Daemon unreachable — Switch via CLI"
t=6s no flip (accepted-then-silently-dropped):
║ ⚠ Switch didn't apply — target busy or daemon      ║
║   stalled.              [Retry]   [Why?]           ║
daemon offline variant:
║ │ Switch via CLI (daemon offline)                │ ║ ← shells clauth cl-ax; confirmed by EXIT CODE;
                                                         "Switched via CLI — auto-switch inactive
                                                          until daemon starts"

STATE 4 — DAEMON DEAD (generated_at age > 15s on the 1s clock; 5–15s showed grey "syncing…")
╔════════════════════════════════════════════════════╗
║ ▌⚠ Daemon not responding — data frozen 4m ago      ║ ← red banner, 13pt semibold; age ticks live
║ ▌  Auto-switch is NOT running.                     ║
║ ▌  [Start daemon]    clauth daemon         [Copy]  ║ ← detached spawn; spinner until fresh;
╟────────────────────────────────────────────────────╢   green "daemon back" flash on recovery
║ ACCOUNTS                        (rows dim to 60%)  ║
║   ✓ xfx           Max 20×                          ║
║     5h ▓▓▓▓▓▓▓▓▓▓▓▓▓░░░░░|░░  62%   as of 4m ago   ║ ← every stamp → "as of 4m ago"; bars greyscale
║     7d ▓▓▓▓░░░░░ 41%    Fb ▓▓░░░░░░ 18%            ║
║   ○ cl-ax         Max 5×                        ⚑  ║
║     5h ▓▓▓░░░░░░░░░░░░░░░░░░|  18%   as of 4m ago  ║
║   ○ zai   ● as of 4m ago                           ║
╟────────────────────────────────────────────────────╢
║ cl-ax · Max 5×                       as of 4m ago  ║
║ ┌────────────────────────────────────────────────┐ ║
║ │ Switch via CLI (daemon offline)                │ ║ ← the only recovery verb; exit-code confirmed
║ └────────────────────────────────────────────────┘ ║
╟────────────────────────────────────────────────────╢
║ CHAIN (controls disabled · .help "needs daemon")   ║
╟────────────────────────────────────────────────────╢
║ ↻ Refresh usage (disabled)         as of 4m ago    ║
╚════════════════════════════════════════════════════╝
Menu bar in this state: "⚠ xfx 4m" — age shown, % withheld.
```

## 3. Interaction map (complete gesture → outcome contract)

1. Open: click menu-bar item -> panel opens; inspection RESETS to the active account (first chain member if active_profile is null); immediate status.json re-read. First glance is always live truth.
2. Single-click any account row -> INSPECT only: selection wash + hairline ring moves, detail card re-targets instantly. Pure view state, zero daemon traffic, zero side effects. Clicking the already-inspected row is a no-op.
3. Hover account row -> 8%-primary wash + .help tooltip (full untruncated name / tier / provider / 'Click to inspect'). Teaches rows are safe to click.
4. Right-click any account row -> native context menu (complete power path): Switch to <name> (with Cmd+Return hint) / Refresh <name> / Add to-Remove from chain / Move up / Move down / Leave chain at > (50-80-90-95-Last resort 100%) / Copy account name. One-gesture switching for operators who miss the old click model.
5. Click 'Switch to <X>' (rendered only when inspected != active; enabled when daemon alive) -> t=0: spinner 'Switching to X...', ALL switch affordances disable, poll accelerates 4s->0.5s (cancelled at confirm or +6s); confirm on active flip: checkmark badge animates to new row (rows never reorder), menu-bar label updates, strip toast 'Switched to X' 4s; t=6s no flip -> 'Switch didn't apply — target busy or daemon stalled' + [Retry] [Why?].
6. Instant failure surfacing (no 6s wait): socket write fails -> 'Daemon unreachable' immediately; daemon replies {ok:false} -> 'Daemon refused the switch: <error>' immediately. The 6s timeout covers ONLY the accepted-then-silently-dropped case. Requires the DaemonClient Result<Void, DaemonError> refactor.
7. Live-session guard (current-account direction): if the ACTIVE account has has_live_session, first Switch click arms -> button becomes 'Confirm — live session on xfx' in danger tint; second click within 5s fires; else reverts. Guards the credentials a Keychain rewrite actually strands.
8. Daemon offline -> button becomes 'Switch via CLI (daemon offline)'; shells `clauth <name>`; confirmation = process EXIT CODE (never status.json mtime); success labeled 'Switched via CLI — auto-switch inactive until daemon starts'; non-zero exit -> inline error with stderr tail.
9. Hover Switch button -> .help: 'Rewrites the macOS Keychain credential — affects running claude sessions.'
10. Status strip is the SINGLE exception surface, priority-ordered: dead-daemon banner > switch lifecycle > wrap-off card > zero-armed warning > forecast sentence. Exceptional truth always appears in the same place.
11. Forecast sentence ('Watching xfx — would switch to cl-ax at 95% · now 62%') is driven exclusively by the pure Swift chain-walk mirror of fallback.rs:117-181; prediction wording; naive position+1 banned. Same engine drives the detail-card chain line and chip tooltips. **[Superseded 2026-07-06:** the daemon now publishes its own `forecast` in status.json (clauth `81c00a2`), computed by the same walk that makes the real switch decision; ccsbar renders it and keeps the Swift mirror only as a fallback for pre-forecast daemons (ccsbar `e4f2d24`).**]**
12. Liveness ladder on the 1s UI clock (generated_at, statusMtime fallback): <5s green pulse 'live'; 5-15s grey 'syncing...'; >15s red banner 'Daemon not responding — data frozen Xm ago. Auto-switch is NOT running.' + rows dim 60% + all stamps 'as of Xm ago' + config disabled + menu bar warning state.
13. [Start daemon] in the dead banner -> spawns `clauth daemon` detached (binary resolved via login shell); button shows spinner; on fresh generated_at the banner dissolves with a green 'daemon back — data live' flash. [Copy] copies the command as fallback.
14. Rotation heartbeat: active_profile changes with NO local pending switch -> panel strip flashes 'rotated to <name>' and menu bar shows the rotation glyph + name for 8s — visible proof auto-switch fired.
15. Wrap-off (active_profile null, daemon alive) -> strip card: 'All accounts switched off — chain spent. Auto-resumes when a window resets (<=2h 14m)' — ETA from min(resets_at) across chain members with live windows. Switch verbs stay enabled for manual recovery. Distinct from 'no data'.
16. Zero armed with non-empty chain -> amber strip: 'Auto-switch idle — xfx isn't in the chain. [Add xfx to chain]' (one-click fallback_add); menu bar gains bolt.slash. Chain empty -> 'Auto-switch off — no fallback chain. [Set up]'.
17. Click a chain chip -> inspects that account (selection + detail move; list scrolls row into view). Hover chip -> forecast-engine tooltip: armed ('Watched now — would rotate away at 95%'), member ('2nd in chain — leaves at @95% of 5h'), sink ('Last resort — chain parks here even at 100%').
18. Click [Edit] or 'Configure chain...' action row -> inline Configure disclosure expands in place: 28pt rows, 13pt labels, up/down/remove as 22x22pt glyphs in 28pt hit targets, threshold menu with presets 50/80/90/95/100 + legend ('auto-switch LEAVES this account at this 5h usage'; 100% labeled 'last resort (sink)'), wrap radio in outcome language.
19. Config command lifecycle: affected row shimmers 'pending' until next status.json confirms (~1.2s); {ok:false} -> instant revert + reason; no confirmation by 6s -> red '!' on the row with .help 'Change didn't apply — is the daemon running?'.
20. Removing the ARMED chain member -> inline confirm: 'This disables auto-switch (the live account leaves the chain). Remove anyway?'
21. Click Refresh usage (or R) -> row spinner + 'Refreshing...'; confirmation = any fetched_at advancing -> 'updated Ns ago' resumes ticking; no movement by 10s -> 'refresh didn't run'. Distinguishes refreshed-unchanged from silently-failed.
22. Hover freshness stamp -> tooltip: 'status.json written 3s ago · usage refetch every 90s · cl-ax next refresh in 41s' (surfaces generated_at, refresh_interval_ms, next_refresh_at).
23. Third-party rows (zai): collapsed = dot + Available/Unavailable + 'checked Ns ago'; detail card = provider, endpoint host, last-check, same Switch verb; never %-bars; 'Unavailable' adds 'since 14:02'.
24. Quit row .help: 'The clauth daemon keeps running — auto-switch continues.'
25. Keyboard: Up/Down move inspection selection (focus ring on inspected row); Cmd+Return switches to inspected (same arm + pending flow); R refresh; Esc closes; Cmd+Q quits. Full nav macOS 14+ (.focusable + .onMoveCommand/.onKeyPress); macOS 13 degrades to keyboardShortcut floor.
26. VoiceOver: each row narrates full state in one utterance ('xfx, active account, Max 20x plan, armed — auto-switch would leave at 95%, session 62% used resets in 1 hour 12 minutes, weekly 41%, Fable 18%, in use'); the Switch control announces 'Switch to cl-ax — changes the account for running claude sessions'.
27. Background cadence: 4s file poll + 1s UI clock (countdowns, 'updated Ns ago', banner age, liveness ladder) so the panel visibly breathes between polls; 0.5s fast poll ONLY while a switch is pending, strictly bounded.

## 4. Type scale (role → SwiftUI font → macOS pt)

- Menu-bar label -> system menuBarFont + .monospacedDigit on the % -> 13pt (never smaller, never scaled; fixed max width, tail-truncate at 12 chars)
- Status-strip forecast sentence -> .callout.weight(.medium) -> 12pt (bolt glyph 12pt sapphire)
- Status-strip freshness / toast line -> .subheadline -> 11pt secondary
- Dead-daemon banner headline -> .body.weight(.semibold) -> 13pt; recovery command 13pt monospaced (never caption)
- Section headers (ACCOUNTS / CHAIN) -> .subheadline.weight(.semibold), uppercase, secondary -> 11pt
- Account row name -> .body.weight(.semibold) -> 13pt, lineLimit(1), truncate + .help (minimumScaleFactor removed)
- Account row tier/provider -> .subheadline -> 11pt secondary
- Row 5h % numeral (hero datum) -> .body.weight(.semibold).monospacedDigit() -> 13pt, on a full-width 6pt bar with in-track threshold tick [Roster graft — was 12pt in Preflight]
- Row 5h reset countdown -> .subheadline -> 11pt secondary
- Row 7d / Fable % numerals -> .callout.monospacedDigit() -> 12pt, on half-width 4pt bars
- Row meter labels (5h / 7d / Fb) -> .caption -> 10pt tertiary — glyph-like labels, the only primary-area 10pt
- Row badges (armed bolt / in use / cached-failed / sink flag) -> .subheadline symbols -> 11pt, each with .help
- Detail card account name -> .title3.weight(.semibold) -> 15pt (the 22pt title2 is deleted; hierarchy from weight + position)
- Detail freshness stamp ('Fresh · 3s' / 'as of 4m ago') -> .subheadline -> 11pt secondary
- Detail window labels (Session 5h / Weekly 7d / Fable) -> .body.weight(.medium) -> 13pt; bars 6pt, Session bar carries the threshold tick
- Detail % values -> .body.monospacedDigit() -> 13pt; reset countdowns -> .subheadline -> 11pt secondary
- Chain-membership / forecast line in detail card -> .callout -> 12pt
- Switch verb button label -> .body.weight(.semibold) -> 13pt white on #B85C33, 28pt-tall control
- Chain chips -> .callout -> 12pt name + .monospacedDigit '@95'
- Configure rows -> .body -> 13pt labels on 28pt rows; threshold menu 13pt with 10pt chevron; explainers .subheadline 11pt; buttons 22x22pt glyphs in 28pt hit areas
- Action rows (Refresh / Configure / Quit) -> .body -> 13pt on 24pt rows (matches measured NSMenu: 13pt font, 24pt item)
- Keyboard hints (Cmd+Return, Cmd+Q) -> .subheadline -> 11pt tertiary
- FLOOR RULES: nothing below 10pt anywhere; 10pt only for glyph-like labels and tertiary stamps, never usage data; all names, verbs, and 5h numerals >= 13pt; no minimumScaleFactor; overflow = truncation + .help

## 5. Color semantics (one meaning per hue)

- Terracotta #D97757 (raw) = ACTIVE, and nothing else: the checkmark badge, active-name tint, and the 'Active account' outline state in the detail card. Large/glyph use only (3.1:1 vs white fails AA for small text). Never armed, never healthy-bar, never generic-interactive.
- Darkened terracotta #B85C33 = the ACT verb: fill of the 'Switch to X' button under 13pt white text (>=4.5:1, passes AA), and equally the 'Log in again' browser-reauth verb (AUTH-3) that recovers a dropped OAuth login. The brand hue acts only where the user acts.
- Sapphire #43ABE5 (Theme.sapphire — defined-but-dead code, now wired) = ARMED / WATCHING / auto-switch identity: forecast-sentence bolt, the row's "⚡ watching" chip, armed chain-chip ring + bolt, pending-switch pulse ring on the target row. Cashes the design cheque Theme.swift already wrote.
- Status green / amber / red = HEADROOM + HEALTH semantics only, as light/dark dynamic pairs via NSColor(name:dynamicProvider:): Catppuccin Latte in light (#40A02B / #DF8E1D / #D20F39), Mocha in dark (#A6E3A1 / #F9E2AF / #F38BA8) — fixes the 1.3-2.3:1 light-mode failures. Applied to: bar fills keyed to each account's OWN threshold (warn at >=0.8x, danger at >= threshold), the liveness dot (green live / grey syncing / red dead), fetch-status badges, the row "spent" pill (a 5h/weekly window at its cap → unavailable until it resets; the pure `ProfileStatus.spentTag` is the single source of truth, gated to anthropic accounts and >=99.5% so it agrees with the rounded bar), error strips, and the AUTH-3 reauth surface's shield glyph (the row's "login-expired" badge + the detail card's "This account's login expired" caption — a dropped OAuth login is a health failure the user must clear).
- Red banner tint = SYSTEM DOWN only (daemon dead). Wrap-off renders in neutral secondary — it is the system speaking deliberately, not an error.
- Selection = 8% primary wash + hairline ring = INSPECTED. Pure view state; deliberately hue-less so it can never be confused with active or armed.
- Danger tint on the Switch button = armed confirm step only ('Confirm — live session on xfx').
- Menu bar = ZERO color semantics. The menu bar template-renders and flattens custom colors; every state is encoded in SF Symbol shape (gauge variants, bolt.slash, warning triangle, rotation arrows, power-slash), never hue.
- Net result: the five-meanings-of-terracotta overload dissolves into exactly four learnable roles — terracotta=active/act, sapphire=auto-switch, green-amber-red=headroom/health, wash=looking.

## 6. Menu-bar label spec

13pt system menuBarFont, SF Symbol + active account name (tail-truncated at 12 chars, lineLimit(1), fixed max width) + active 5h% in monospaced digits. All state encoded in SYMBOL SHAPE, never color (menu bar template-renders). Priority ladder (highest wins): (1) DAEMON DEAD (generated_at age >15s): warning-triangle glyph + name + frozen age, % WITHHELD — '⚠ xfx 4m' (frozen numbers never impersonate live ones). (2) SWITCH IN FLIGHT (<=6s): current label + trailing ellipsis '◔ xfx…'. (3) ROTATION FLASH (8s after a daemon-initiated active change, i.e. active flipped with no local pending switch): '⇄ cl-ax' — the auto-switch heartbeat, visible without opening the panel. (4) WRAP-OFF ALL-OFF (active_profile null, daemon alive): power-slash glyph + 'off'. (5) NO status.json: bare gauge glyph. (6) 5h >= threshold: exclamationmark.gauge variant — '⚠◔ xfx 96%'. (7) 5h >= 0.8x threshold: gauge with small dot badge — '◔· xfx 84%'. (8) AUTO-SWITCH DISARMED (chain non-empty with zero armed, or chain empty): bolt.slash glyph appended — '◔ xfx 62% ⌁̸' — a silently broken chain is visible from the bar. (9) NORMAL: '◔ xfx 62%'. Third-party active: glyph + name + availability dot instead of % ('◔ zai ●'). The % always means the ACTIVE account's 5h window — the same number the forecast sentence explains inside.

## 7. Config surfaces decision

Two surfaces, no Settings window. (1) FAST PATH — Relay's native right-click context menu on every account row carries the complete edit vocabulary at free NSMenu metrics (13pt/24pt): Switch to <name> / Refresh <name> / Add to-Remove from chain / Move up / Move down / Leave chain at > preset submenu (50 / 80 / 90 / 95 / Last resort 100%) / Copy account name. The 80% of config edits never open an editor. (2) CANONICAL EDITOR — Preflight's inline Configure disclosure (expanded via [Edit] beside the chain rail or the 'Configure chain…' action row), upgraded to Relay's hit-target standard: 28pt rows, 13pt labels, up/down/remove as 22x22pt glyphs in 28pt hit areas, 10pt chevrons, threshold menus with a one-line legend ('auto-switch LEAVES this account at this 5h usage'; 100% labeled 'last resort — parks here'), '+ Add' copy that says it adds an EXISTING profile to the chain (account creation is `clauth login`), and the wrap-off setting as an outcome-language radio: 'When every account is over its limit: (•) Stay on last account / ( ) Switch everything off (credentials cleared; resumes automatically when a window resets)' — the 'wrap-off' jargon is retired from all UI copy. Removing the ARMED member requires the inline confirm ('This disables auto-switch — remove anyway?'). Every config command gets optimistic pending shimmer -> instant revert with reason on {ok:false} -> red '!' at 6s with no confirmation. Relay's separate Settings window (Cmd+comma, drag-to-reorder) is REJECTED: it would duplicate the disclosure (two full config surfaces = two sources of truth for the same 8 socket commands), it carries the flakiest machinery in any proposal (Settings scene fronting from an LSUIElement app, drag-reorder diffed into fallback_move sequences with intermediate states landing in status.json), and only one judge asked for it while all three endorsed the context menu. Its non-duplicative lessons — 26pt+ targets and the preset submenu — are grafted into the two surfaces we keep.

## 8. Grafts adopted from losing proposals

- Roster -> row anatomy: 5h-dominant hierarchy — full-width 6pt 5h bar with 13pt monospaced numeral + reset countdown; 7d/Fable demoted to a half-width 12pt secondary line. Replaces Preflight's three equal-weight micro-meters; the window auto-switch keys off gets primary visual weight while all three windows stay zero-click visible.
- Roster -> per-account threshold tick rendered INSIDE every 5h bar track (rows AND detail-card Session bar): distance-to-auto-switch becomes a pre-attentive visible gap; the sink's tick sits at the bar end.
- Roster -> graded liveness ladder on the 1s clock: green pulse 'live' <5s / grey 'syncing…' 5-15s / red banner >15s (with statusMtime() cross-check). Replaces Preflight's binary 10s cliff so a momentary stall doesn't slam into the red banner.
- Roster/systems -> immediate error surfacing: socket-write failure and {ok:false} replies surface instantly with cause-specific copy; the 6s timeout is reserved for the accepted-then-silently-dropped case. Built on Preflight's own DaemonClient Result<Void, DaemonError> refactor.
- Roster -> VoiceOver contract: full row state in one utterance; the Switch control announces its Keychain consequence.
- Roster/Watchtower -> freshness tooltip surfaces generated_at age + refresh_interval_ms + per-account next_refresh_at ('status 3s ago · usage refetch every 90s · cl-ax next refresh in 41s') — the planned-but-never-built proof-of-life.
- Roster -> the forecast sentence carries the live gap: '…at 95% · now 62%'.
- Watchtower -> forecast engine: ONE pure Swift function mirroring fallback.rs:117-181's chain-walk (skip-active; below-own-threshold-or-never-fetched first pass; 100%-sink second pass; never sink-to-sink), annotated with fallback.rs line pins, unit-tested against fixture JSON, worded as prediction ('would switch to'), never promise. Drives every next-target string in the UI; naive position+1 is banned.
- Watchtower -> rotation heartbeat: when active_profile changes with no local pending switch, flash 'rotated to <name>' in the panel strip and show '⇄ <name>' in the menu bar for 8s — the hero feature's only visible proof it fired.
- Watchtower -> wrap-off auto-resume ETA derived from min(resets_at) across chain members with live windows: 'Auto-resumes when a window resets (<=2h 14m)'.
- Watchtower -> inline confirm before removing the ARMED chain member: 'This disables auto-switch (the live account leaves the chain). Remove anyway?'
- Watchtower/Relay -> one-click [Start daemon] button in the dead-daemon banner: detached Process spawn, binary resolved via login shell, spinner until generated_at freshens, green recovery flash; [Copy] command as fallback.
- Relay -> 'Leave chain at >' preset submenu plus Move up/down inside the right-click context menu — the full chain-edit vocabulary without opening Configure.
- Relay/systems -> CLI-fallback honesty fix (corrects Preflight's one false state): confirm the shelled `clauth <name>` via process EXIT CODE, never status.json mtime (only the daemon's write_status writes that file); label it 'Switched via CLI — auto-switch inactive until daemon starts'.
- Relay -> bolt.slash menu-bar glyph when auto-switch is disarmed (zero armed members or empty chain) — a silently broken chain is visible without opening the panel.
- Relay -> status strip formalized as the SINGLE exception surface with a fixed priority order (dead-daemon > switch lifecycle > wrap-off > zero-armed > forecast).
- Relay -> Quit row copy + .help: 'the clauth daemon keeps running' — kills the quit-kills-my-auto-switch fear for free.
- Relay -> Configure hit-target standard (28pt rows, 22x22 glyphs in 28pt targets) absorbed into the inline disclosure.

## 9. Conflicts resolved (and the rule used)

- Row anatomy — Preflight's three equal tri-meters vs Roster's 5h-dominant graft. RULE: a graft that elevates the hero datum without dropping any information beats the winner's original. Adopted Roster's full-width 5h bar + 13pt numeral with 7d/Fable on a secondary half-width line; all three windows remain zero-click visible, and the hig-purist's 'most-scanned data at 12pt' ding is fixed (5h numeral now 13pt).
- Config surface — hig-purist's must_graft 'keep Relay's Settings window' vs Preflight's inline Configure disclosure. RULE: one source of truth — never ship two full config surfaces for the same 8 socket commands; when a graft duplicates a winner surface, keep the winner's and absorb only the graft's non-duplicative parts. Settings window rejected (also the flakiest machinery: LSUIElement fronting, drag-diff into fallback_move sequences); its hit-target standard and preset submenu grafted into the disclosure + context menu. 2 of 3 judges endorsed the context menu; only 1 asked for the window.
- Daemon-death detection — Preflight's 10s binary cliff vs Roster's 3-tier ladder. RULE: when two designs measure the same signal, the graded signal supersedes the binary one. Adopted live <5s / syncing 5-15s / dead >15s; the banner threshold moves from 10s to 15s so a momentary stall shows 'syncing…' instead of a false red alarm.
- CLI-fallback confirmation — Preflight watches status.json mtime vs Relay confirms via process exit code. RULE: never display a state the data source cannot produce. Only the daemon's write_status ever writes status.json, so every successful CLI switch would false-fail Preflight's 6s watch in the exact mode where trust is most fragile. Exit-code confirmation adopted; mtime-watch deleted.
- Live-session guard direction — Preflight arms on the CURRENT account's live session vs Relay confirms on the TARGET's. RULE: guard the asset the destructive op actually destroys. A Keychain rewrite strands the CURRENT account's running session (systems judge's finding); Preflight's direction kept. The busy-TARGET case needs no guard — the daemon silently drops it, which the 6s timeout + [Retry] already surfaces.
- Switch timeout duration — Relay's 5s vs Preflight/Roster/Watchtower's 6s. RULE: winner's number unless a judge argued otherwise; none did. 6s kept, matching the bounded 0.5s fast-poll window.
- Forecast wording — Preflight's 'switches to personal at @95%' (promise, naive next-target) vs Watchtower's prediction discipline. RULE: UI text must be provable by the client's own computation. All next-target strings now flow from the chain-walk mirror and read 'would switch to' — prediction, not promise.
- Dead-daemon recovery — Preflight's copy-only [Show fix] vs Watchtower/Relay's [Start daemon] button. RULE: a graft that upgrades recovery from instruction to action wins; keep the old affordance as fallback. Button primary, copy-command secondary.
- Menu-bar state collisions introduced by grafts (rotation flash, bolt.slash, threshold badges, dead state). RULE: define an explicit priority ladder rather than letting states race — dead > in-flight > rotation-flash > wrap-off > no-data > over-threshold > near-threshold > disarmed > normal (spec'd in the menu-bar section).
- Detail-card redundancy — rows now carry richer bars, so the card duplicates more. RULE: redundancy is accepted when the two surfaces answer different questions — rows answer 'compare across accounts', the card answers 'when does it reset / how fresh / switch'. Kept, per the operator judge's explicit endorsement.

## 10. Risks the implementer must own

- The 6s switch-timeout remains a client-side heuristic: it cannot distinguish the daemon's silent busy-target drop from a credential-divergence stall, and can false-positive under a pathologically slow tick (copy is hedged; a late confirm resolves the UI on the next poll). Upstream ask filed: status.json switch_blocked/blocked_reason field, plus surfacing pending_switch.
- Forecast chain-walk mirror can drift from fallback.rs as the daemon evolves. Mitigation is contractual, not optional: one pure function, fallback.rs:117-181 line pins in comments, fixture-JSON unit tests in CI, prediction wording so a drifted forecast misleads softly rather than lies hard. **[This risk fired 2026-07-06:** upstream's exclusive-`last_resort` + burn-aware walk changes invalidated the mirror while its fixture tests stayed green — pins made the drift findable, not impossible. Resolved structurally: the daemon publishes its own `forecast` in status.json and ccsbar renders it; the mirror is fallback-only.**]**
- MenuBarExtra(.window) keyboard focus on first open is historically flaky; full arrow-key nav needs macOS 14+ (.focusable/.onMoveCommand/.onKeyPress). Floor: keyboardShortcut-only (Cmd+Return, R, Cmd+Q) on macOS 13; call .defaultFocus and accept mouse-first if focus doesn't land.
- Panel height: ~520pt with 3 accounts (list + detail card + chain + actions); each extra Anthropic account adds ~58pt; past 6 accounts the list scrolls inside maxHeight and glance-at-once erodes. Accepted for the realistic 2-5 account population.
- Menu-bar label is template-rendered: color is unusable (already avoided) and glyph-swap states must be visually distinct at 16pt; the 8s rotation-flash timer must be cancelled if a user-initiated switch starts during the flash, per the priority ladder.
- [Start daemon] depends on resolving the clauth binary via a login shell; PATH-less environments (e.g. GUI login without shell config) fall back to the Copy-command affordance. Spawn must be fully detached so the daemon doesn't die with the panel.
- The 0.5s pending fast-poll must be strictly bounded (cancel at confirm or +6s) to avoid a runaway timer if a switch never lands; same for the 1s UI clock's cost — both are file stats/reads, negligible but worth an instrument pass.
- DaemonClient Result<Void, DaemonError> refactor (~7 call sites) is a hard prerequisite for the instant-error surfacing; shipping the UI before the refactor recreates the swallowed-error hole with prettier chrome.
- Rotation-heartbeat detection ('active changed with no local pending') will also fire when another client (the TUI, CLI) switches — technically a different actor than the daemon. The '⇄ rotated to <name>' copy is actor-agnostic and stays truthful; do not word it 'auto-switch rotated'.
- Checkmark-badge animation across rows plus list scroll-into-view can jank if implemented with matchedGeometryEffect inside a resizing MenuBarExtra window; degrade to a 150ms crossfade if it fights the window resize.
- Two accent hues (terracotta active/act, sapphire armed/watch) add a palette to learn; mitigated by .help on every badge and the forecast sentence teaching sapphire's meaning in prose.
- Light/dark dynamic status colors must be verified in BOTH appearances plus increased-contrast mode; the Catppuccin Latte danger (#D20F39) passes on light panels but all fills need a contrast sweep against the selection wash.

## Appendix — the four proposals and judge rankings

Rankings: operator: Preflight > Watchtower > Roster > Relay · hig-purist:
Preflight > Relay > Watchtower > Roster · systems: Preflight > Roster >
Watchtower > Relay.

### Preflight — the inspect-first ccsbar (browse freely, switch deliberately)

Preflight rebuilds the panel around one rule: clicking is looking, switching is a verb. The tile grid becomes a two-line account LIST where every account shows all three windows (5h/7d/Fable) at once — restoring the lost one-view glanceability — and a single click merely re-targets a detail card below (pure view state, zero daemon traffic). Activation lives in exactly one place: a prominent terracotta "Switch to X" button inside the detail card (plus right-click and ⌘↩ shortcuts), which runs an honest pending→confirmed→timeout state machine instead of today's fire-and-forget silence. Two orthogonal markers make the model self-evident: the ACTIVE account wears a terracotta checkmark badge (the only thing terracotta means in data display), while the INSPECTED account wears the selection wash + hairline ring; armed/auto-switch state moves to the theme's own dormant sapphire. The panel's top strip is the hero-feature confidence readout — a plain-language auto-switch sentence ("Watching work — switches to personal at @95%") backed by a generated_at freshness check that finally makes a dead daemon loud. Type floor rises to 13pt for every primary datum, 11pt for metadata, 10pt only for micro-meter glyph labels.

**Tradeoffs:** (1) Switching goes from one click to two (click row → click Switch) — deliberate friction for a Keychain rewrite that hits live sessions; mitigated by right-click 'Switch to X' and ⌘↩ as one-gesture paths. CCSwitcher's bare-click model is exactly what produced AX's own 'switch failed' issue; CodexBar's inspect/activate split is the proven alternative. (2) The panel grows taller (~500-540pt with 4 accounts vs ~360 today) and list rows replace the compact tile row; past 6 accounts the list scrolls inside a max-height. The win — all three windows visible per account with zero interaction — is complaint 2 verbatim. (3) Row tri-meters duplicate the detail card's bars at lower fidelity; accepted redundancy: rows answer 'compare', the card answers 'when does it reset / how fresh / switch'. (4) Rows keep stable file order (active is NOT pinned first) — fixes reorder-under-cursor but means the active account can sit mid-list; the checkmark badge, status strip, and menu-bar label carry 'which is active'. (5) Two accent hues (terracotta=active/brand-verb, sapphire=armed/watch) add palette complexity but dissolve the five-meanings-of-terracotta overload, and sapphire was already the theme's documented-but-dead intent. (6) No optimistic switch UI — honest '◌ Switching…' costs 0.5-1.5s of visible latency; the accelerated 0.5s poll during pending adds trivial file IO. (7) The 6s switch-timeout is a client-side heuristic for the daemon's SILENT busy-drop and divergence stall — it can false-positive under heavy load and can't name the cause; the real fix is a status.json contract addition (blocked/divergence field), which this design should raise upstream. (8) Full keyboard nav (onKeyPress/onMoveCommand) requires macOS 14+; on 13 it degrades to keyboardShortcut-only (⌘↩, R, ⌘Q). (9) Inspection resets to the active account on every open — loses 'where I was' but guarantees the first glance is always live truth.

### Roster — the all-accounts board (glance-first ccsbar)

The panel stops being a switcher with a detail card and becomes a fixed-order roster where every account's full state — 5h, 7d, Fable, chain slot, threshold, live-session flag, freshness — is permanently visible with zero clicks, because the operator's real question ("which account has headroom, and will the daemon rotate in time?") is a comparison question, not a per-account drill-in. Reading and switching are physically separated: the row body is inert data, and activation lives only in an explicit radio + hover-revealed "Switch" control, so looking can never mutate the Keychain. The three silent failure modes (dead daemon, swallowed commands, invisible auto-switch state) each get a dedicated always-on surface: a generated_at-driven liveness header that turns into a danger banner, a pending→confirmed→timeout switch lifecycle with optimistic row state, and a one-sentence plain-English auto-switch readout ("Watching work — switches to personal when 5h hits 95%") that is the hero of the panel. Type is rebuilt on the 13pt macOS menu convention: names, verbs, and the 5h numeral at 13pt, metadata at 11-12pt, nothing under 10pt; terracotta is demoted to exactly one meaning (active/interactive), sapphire is resurrected for armed, and status hues get light-mode-safe dynamic variants. Density is solved by hierarchy, not shrinking: the 5h bar is full-width with a threshold tick (distance-to-auto-switch becomes a visible gap), while 7d/Fable share a half-width secondary line — three accounts x three windows fits in ~66pt per account, columns aligned so the eye scans straight down.

**Tradeoffs:** Chosen: full roster over tile+detail. Cost: vertical height (~430pt at 4 accounts, ~560pt cap + scroll at 6+) versus the current 320pt-ish compact card — accepted because menus are a vertical medium and the scan-speed win is the whole point; CodexBar's own Overview mode concedes this. Cost: 7d/Fable bars run at half width with no inline reset countdown (tooltip-only), trading weekly-window resolution for row height — defensible because auto-switch keys exclusively off 5h, but an operator who lives by the weekly cap loses one glance-layer. Cost: the hover-revealed Switch capsule is less discoverable than an always-visible button; mitigated three ways (persistent hollow radio signals a radio group, right-click menu, Return/⌘N shortcuts), and the alternative — a visible button per row — adds 8 controls of permanent chrome to a panel whose job is reading. Cost: fixed file-order roster gives up active-always-on-top; the filled terracotta radio + tinted name must carry 'which is active', but spatial stability under the cursor (fixing the reorder-mid-click bug) is worth more than saving one saccade. Cost: no confirmation dialog on switch keeps one-click speed but relies on the inert-row-body + explicit-control separation to prevent accidents; if telemetry later shows misfires, the escalation path is press-and-hold or a Return-to-confirm inline state, not a modal. Cost: width grows 320->340pt to fit the tri-metric line at 12-13pt. Deferred: per-account expandable detail (all other window labels like '7d sonnet' stay dropped — tooltip territory), bell_threshold mirroring, auto_start badge, drag-to-reorder (chevrons at 24pt are keyboard-friendlier and simpler in a menu window), and any representation of divergence-blocked auto-switch pending the status.json contract addition.

### Watchtower — ccsbar as mission-control for auto-switch

The panel stops being "an account list with a switcher" and becomes the cockpit for the daemon's one job: rotating the live Claude session down the fallback chain before the 5h wall hits. It leads with a three-line system header that answers "will I survive the next 5 hours?" — who is live NOW (terracotta), whether the daemon is actually alive (1s mtime check on status.json; a loud red banner when it is not, replacing today's silently frozen numbers), and a derived forecast ("leaves at 95% · now 62% · next → personal") computed by mirroring the daemon's chain-walk. The chain rail is the centerpiece: every account's 5h bar (with a threshold tick), 7d and Fable numbers visible in ONE view at ≥11pt — restoring the lost glanceability — and clicking a row only inspects (inline expansion with full bars, freshness stamp, tier); activation is a separate explicit verb ("Use this account") with real in-flight → confirmed → failed feedback, because a Keychain rewrite that touches a running session deserves CodexBar's inspect/activate split, not CCSwitcher's silent one-click model. Every hue gets exactly one meaning: terracotta = live, sapphire = armed/watching, green/amber/red = headroom semantics (light/dark-corrected), red banner = system down.

**Tradeoffs:** (1) Switching costs two gestures (expand → Use) instead of one tap — deliberate friction for a Keychain rewrite that hits a running session; the right-click 'Use this account' and ⌘↩ fast paths give power users back a one-gesture switch, so the tax falls only on the unfamiliar. CCSwitcher's one-click model is exactly what correlates with its 'switch failed' trust issues. (2) Vertical rows scale honestly to ~6 accounts (+~48pt each); beyond that the chain+bench need an internal ScrollView, which erodes glance-at-once — acceptable for this product's realistic 2–5 account population. (3) Glance mode shows 7d/Fable as numbers, not bars — less pre-attentive than three bars per row, but it keeps rows two lines at ≥11pt type; the full bars live one click away in the inspector. Numbers with monospaced digits still support column-scanning across accounts. (4) The 'next → personal' forecast duplicates fallback.rs's chain-walk (skip-active, below-own-threshold-or-never-fetched, else 100%-sink, never sink-to-sink) in Swift — a drift risk; mitigated by keeping it one pure function annotated with the fallback.rs line pins, and by wording it as prediction, not promise. (5) The 6s switch-failure notice is a heuristic: the daemon has no completion/rejection signal (ok = accepted, not done), so a pathologically slow tick could false-positive — the real fix is a status.json contract addition (pending_switch surfaced + a 'blocked: credentials diverged' flag for the divergence stall); this design flags both as upstream asks and ships the timeout as the honest interim. (6) A second accent (sapphire for armed) adds a color to learn, but it un-overloads terracotta from five meanings to one, and it's already defined in Theme.swift as dead code — this cashes a design cheque the codebase already wrote. (7) Keeping dimmed stale rows visible in the dead-daemon state (rather than hiding them) risks someone reading frozen numbers — accepted because hiding them would also hide the CLI-fallback switch path, which is the only recovery tool the panel has when the daemon is down.

### Relay — calm surface, depth on click (progressive disclosure with macOS-native restraint)

Relay reorganizes ccsbar around one rule: reading is free, changing is deliberate. The default panel is a calm 13pt readout of exactly two things — the auto-switch status sentence (the hero feature's confidence UI, which also becomes the loud surface for daemon-death, chain misconfiguration, and the wrap-off all-off state) and one stable-ordered row per account (checkmark = active, tiny 5h meter + monospaced %, chain-position badge, sapphire bolt on the armed member). Clicking a row never switches anything: it expands that row inline, Wi-Fi-menu style, to reveal full Session/Weekly/Fable bars with threshold notches, reset countdowns, tier, and fetch honesty ("cached 2m ago") — restoring the lost look-without-switching glanceability. Activation is demoted to an explicit terracotta "Switch to <name>" verb inside the expansion (plus ⌘Return and right-click), and it is verified, not optimistic: a fast 0.5s poll drives a visible Switching… → checkmark-moves → confirmation arc, with a timeout error covering the daemon's silent busy-drop. Configuration leaves the panel body entirely — right-click context menus carry the 80% edits (chain membership, threshold, order), a real Settings window carries the rest with drag-to-reorder and outcome-language wrap copy. Color semantics are finally one-meaning-per-hue: terracotta = active/act, sapphire = armed/watching, and status colors get light/dark-adaptive variants that pass contrast.

**Tradeoffs:** 1) Switching costs two gestures (expand → verb) instead of one tile click. Deliberate: a Keychain rewrite that redirects a running claude session shouldn't be the zero-friction gesture (CCSwitcher's one-click model is exactly what produced AX's own 'switch failed' trust issue). Mitigations keep it fast for experts: right-click → Switch is still ~1.5 gestures, ⌘Return is one chord, and the accordion remembers the expanded row. 2) Collapsed rows carry only the 5h meter — full 7d/Fable comparison is one Option-click away rather than always visible. Chosen because N accounts × 3 bars at 13pt no longer fits a glanceable panel; the Option-click expand-all is the escape hatch that fully restores the old all-accounts view. 3) Hover intentionally does NOT expand or popover (only highlights + tooltips): trades a little speed for zero layout jitter and avoids fragile popover-inside-MenuBarExtra behavior; the Wi-Fi-menu pattern is followed for click-to-reveal, not hover-to-reveal. 4) Chain configuration moves out of the panel to a Settings window + context menus: massive win for hit targets (26pt+ controls, drag reorder) and copy comprehension, at the cost of chain edits no longer being zero-clicks-away in the panel body; the status sentence + right-click cover the frequent edits. 5) Verified (non-optimistic) switch feedback means 1–2s of visible 'Switching…' instead of an instant flip — honest by design, because the daemon silently drops switches to busy targets; an optimistic flip would lie exactly when trust matters. 6) The status sentence spends prime top-of-panel space on a set-and-forget feature — accepted because auto-switch IS the product, and that line doubles as the surface for four failure states (dead daemon, unarmed chain, all-off, switch confirmations) that were previously invisible. 7) Two residual daemon-contract gaps the UI can only approximate: busy-drop and credential-divergence stalls have no status.json field, so they surface only as the 5s timeout error ('Open TUI to resolve') — raise upstream for an explicit 'auto_switch_blocked' field. 8) Panel widens to 340pt (vs CodexBar's 310) to afford 13pt type + bar + mono value in one row; still comfortably a compact panel.
