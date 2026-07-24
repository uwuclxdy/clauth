# CLA-FEED — daemon-fed session token (Fable-capable long-lived slot)

Status: CLA-FEED-1 in progress (2026-07-21). Fork-only feature; upstream PR is a
separate later decision (record in SYNC.md fork-delta inventory).

## Problem

The CLA-SPLIT static setup-token carries only `user:inference
user:sessions:claude_code` and no `subscriptionType` — Claude Code's Fable 5
gate ("requires usage credits") refuses it. The vanilla OAuth pair carries the
full scope set + `subscriptionType: max` and unlocks Fable, but sharing a
rotating pair with CC is the refresh-race death CLA-SPLIT exists to prevent.
Manual `/login` after every rotation is the current workaround.

## Insight

The clauth-private **usage chain is already a full-scope, `subscriptionType:
max` vanilla mint** (browser login via `oauth_login.rs`, SCOPES includes
`user:profile`). Its ~8-12 h **access tokens** are Fable-capable bearers. An
access token has no refresh capability — handing one to CC shares no chain.

## Design

**The daemon feeds `session-token.json` with the usage chain's current access
token.** CC keeps seeing the exact long-lived shape CLA-SPLIT proved out
(`{accessToken, expiresAt, scopes}` + `subscriptionType`, **no refreshToken**)
— but the value is re-stamped on every usage-chain rotation.

```
credentials.json  (usage chain: access+refresh)  — clauth-private, daemon single-writer
        │  rotation (existing oauth.rs machinery)
        ▼
session-token.json  (fed: access only, real expiry, full scopes, subscriptionType)
        │  install/symlink (existing CLA-SPLIT machinery, UNCHANGED)
        ▼
CC (Keychain + live symlink) — read-only consumer, re-reads Keychain per request
```

Single-writer invariants, all preserved:
- usage chain: daemon only (unchanged)
- session-token.json: daemon only; CC has nothing to rotate (no refresh token)
- Keychain: write-only from clauth (unchanged)

Why CC survives the value changing under it: CC re-reads the Keychain **per
request** (verified on-device 2026-07-07, see `oauth.rs` rotation-coherence #1
comment) — the same property that makes vanilla rotation mirroring work.

## Mechanics

1. **Opt-in flag**: `Profile.session_feed: bool` (config.toml `session_feed`,
   default false). Sidecar SHAPE is unchanged — no marker key; feed mode is
   derived from config (status.json exposes it for ccsbar).
2. **Feed writer** (`claude.rs::feed_session_token`): under the state flock,
   write the sidecar from the just-persisted chain fields: accessToken, real
   `expiresAt` (honest countdown — a dead feed must LOOK dead; no far-future
   lie), chain scopes, chain subscriptionType, refreshToken absent. Classifier
   result stays `LongLived` — every split guard keeps working unmodified.
3. **Static-mint preservation**: before the FIRST feed overwrites a genuine
   setup-token mint, copy it to `session-token.static.json`. Feed disable or
   terminal chain death restores it (degrade to Sonnet-cap, never sign out).
4. **Rotation hook** (`oauth.rs::apply_rotated_tokens_locked`): in the existing
   CLA-SPLIT "quiet, never mirror the pair" branch — if `session_feed`, feed
   the sidecar (fast disk write inside the locked section) and, when the
   profile is ACTIVE, ship the fed sidecar through the existing post-flock
   `mirror` Keychain write. Parked profiles feed the file only.
5. **Switch-in freshness** (`oauth.rs::ensure_installable` LongLived branch):
   feed profiles don't return `Broken` on a clock-dead sidecar — refresh the
   usage chain via the injected refresher (RotationGuard held; the rotation
   hook re-feeds as a side effect), then Ready. Terminal refresh failure:
   restore the static backup if present (Ready, degraded + loud), else Broken.
   Sidecar with comfortable life installs as-is (no spend).
6. **Proactive rotation** (`scheduler.rs::proactive_rotation_due`): a
   feed-enabled ACTIVE profile forces the preemptive leg regardless of the
   global `preemptive_rotation` toggle — a stale fed token has a live CC
   behind it. Parked feed profiles stay lazy (401-triggered); their sidecar
   staleness window is one poll interval, and gate #5 covers switch-in.
7. **CLI**: `clauth feed <profile> on|off`. Enable validates the usage chain
   looks Fable-capable (has `user:profile` scope / subscriptionType — warn
   otherwise), preserves the static backup, and arms immediately (refresh +
   feed) so the sidecar is live without waiting for a 401. Disable restores
   the static mint (and re-installs it when the profile is active).
8. **Surfaces**: status.json gains per-profile `session_feed` (wire-key
   additive only, SYNC.md rule); TUI token row gets a "fed" marker. ccsbar
   rendering is CLA-FEED-2.

## Failure ladder

fed token (Fable) → static mint restored (Sonnet-cap, sessions never sign out)
→ vanilla credentials.json (pre-split behavior). Daemon down: fed token dies at
its real expiry and every surface shows the true countdown (the CLA-SPLIT-3
display-gap lesson: never render a dead credential as healthy).

## Live rollout constraint

Enabling/arming on PARKED profiles is agent-safe (writes profile stores via
established paths). Arming the ACTIVE profile rewrites the live Keychain —
AX-manual trigger (a ccsbar switch bounce or the next natural rotation), per
the standing "live slot mutations are AX-manual" rule.

## Explicit non-goals (CLA-FEED-1)

ccsbar UI (CLA-FEED-2), codex side (untouched), upstream PR (separate
decision), TUI config-tab editing of the flag (CLI + config.toml is the API).
