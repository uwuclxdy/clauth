# Codex harness support: implementation spec (PR #51)

The plan for adding OpenAI `codex` as a second harness alongside Claude Code. This is the spec for whoever implements it. It is self-contained: the verified codex behavior, the pinned decisions, the integration anchors, and the settled questions are all here.

Ownership: the contributor's agent implemented the whole thing (harness groundwork + codex engine) as a reviewable series, PR #69; the maintainer designed it, reviewed it, and landed the review's asks on the branch before the merge. The decisions under "Pinned decisions" are settled; do not re-open or freelance them. "Deviations as shipped" at the end records where the merged code departs from this spec's letter.

Names are unique across both state files because `LiveTally` (`live_sessions.rs:129`), the pending-switch set (`scheduler.rs:163`) and all five per-profile cache files (`profile_cache.rs:38`) key on the bare name; that uniqueness is why codex dirs carry no `-cx` suffix.

## Pinned decisions (settled, do not freelance)

1. **Harness is implied by WHICH STATE FILE a profile lives in.** Claude profiles live in `~/.clauth/profiles.toml` (`AppState`, unchanged). Codex profiles live in a NEW `~/.clauth/codex-profiles.toml`. There is no in-`AppState` harness field, no dir-name parsing, no load-order chicken-and-egg. `ProfileConfig.harness` may still be written into the per-profile `config.toml` as a self-describing marker; FILE MEMBERSHIP is authoritative. `Harness { Claude, Codex }` is the in-memory type; cross-harness conversion is delete + recreate.
2. **Profile names are globally unique across both files.** `validate_profile_name` (`actions.rs:31`) checks both state files and rejects a name held by the other harness, naming which harness holds it. It reads the rosters itself rather than trusting its `existing` argument: one TUI call site passes an empty slice deliberately (`tui/app.rs:6943`) and would otherwise skip the check. This supersedes the old "may coexist" rule and removes the need for a `--codex` / `--claude` disambiguator on `switch`.
3. **Dirs are bare for both harnesses.** `profiles/<name>/`, no suffix, whichever file the name came from. `profile_dir`, `profile_subpath`, `rotation_lock_path`, `session_marker_dirs`, rename, and delete need NO harness awareness: they already do the right thing. The liveness layer works for codex unchanged provided a codex session stamps the same `sessions[-isolated]-<sid>` marker a claude session does.
4. **Per-harness independent state.** `profiles.toml`/`AppState` keeps its exact current meaning: the CLAUDE active slot, claude `fallback_chain`, claude `wrap_off`, claude profile list. `codex-profiles.toml` holds the same four for codex plus its own `weekly_switch_threshold` (absent = the claude default, 98). A codex switch writes `codex-profiles.toml` directly, and moves the operator's `~/.codex/auth.json` with it when that slot is a link clauth installed: there is no codex pending-switch queue, because a codex session binds `auth.json` at start and nothing mid-session consumes a pending handshake (see "Deviations as shipped"). Since codex 0.157 every task runs inside one shared app-server daemon that reads `auth.json` once at its own start, so a switch that moved the link reaches running and resumed tasks only through a daemon restart: an auto-switch off a spent account restarts a daemon older than the move (`src/codex_daemon.rs`), a hand switch prints the restart command. Chains are strictly per-harness.
5. **Back-compat is trivial by construction.** `profiles.toml` is UNCHANGED, so old and new binaries read and write it identically: no dual-write, no migration, no serde-drop hazard. An old binary never reads or writes `codex-profiles.toml`, so it cannot drop or corrupt codex state. An existing install has an all-claude `profiles.toml` and no codex file, correct with no migration. During a mixed-version window an old standby daemon manages claude profiles and ignores codex ones. Bare dirs remove the last residual the `-cx` scheme carried (an old binary creating a claude profile whose name collided with a codex dir).
6. **Store mode: force `-c cli_auth_credentials_store="file"` on every clauth-controlled codex spawn.** Not "refuse unless file": that blocks users who run keyring for their own interactive codex. The CAPTURE path is the other half and gets the opposite treatment (decision 11).
7. **Single-writer refresh.** Codex refresh tokens are single-use rotating and `refresh_token_reused` is PERMANENT (browser re-login only). Route every clauth-side codex rotation through the existing per-profile `RotationGuard` + lease. Two further rules, both load-bearing:
   - a codex refresh that FAILS must persist nothing and must not retry the same token on the next tick. The claude path tolerates a replay (one `400 invalid_grant` to the loser); codex does not.
   - clauth never refreshes a chain whose access token is inside codex's own 5-minute pre-expiry window. That race is lost by construction.
8. **Concurrent codex sessions are allowed, and safe because they share one physical `auth.json`.** Not because clauth propagates anything. Every session home links `auth.json` to `profiles/<name>/auth.json`; codex's own reload-and-skip guard (`manager.rs:2366-2400`) handles codex-versus-codex, and `RotationGuard` handles clauth-versus-codex. Any design that COPIES the file creates a second carrier that cannot see the first, which is the permanent-death case.
9. **Behavior-preserving for claude.** `profiles.toml`, the credential/switch path, and the claude usage/fallback code are untouched by the file split. The existing suite gates it. The four latent fixes in "Folded fixes" are the deliberate exception and each ships as its own commit.
10. **Published surface is ADDITIVE, no schema bump.** `status.json` stays `SCHEMA_VERSION` 2; top-level `active_profile` + `wrap_off` remain the claude slot (`status_json.rs:235,237`); per-harness fields are added alongside (`active_codex_profile`, `codex_fallback_chain`, `codex_wrap_off`, `clauth_version`, and `harness` on every `profiles[]` entry, codex entries appended after the claude ones). `which --json` gains codex fields additively; the MCP tools stay claude-only (the parity map's MCP row). Note there are now TWO in-binary readers of that feed, `probe.rs` and `list.rs`, so additive means additive for `list`'s column derivation too.
11. **Capture refuses under keyring/auto.** `clauth login <p> --codex` reads the user's real `~/.codex/auth.json`, which under `keyring`/`auto` is absent or stale by design. Capture errors naming `cli_auth_credentials_store` and the fix, rather than silently snapshotting nothing. A capture copies codex's own file re-stamped with `last_refresh` = the capture time (every unmodelled key kept): the capture is a chain event, and under `LinkMode::Fake` the stamp is what the convergence decides on.
12. **Ship as a reviewable series** (see "Delivery"), never one entangled diff. That is what made #51 hard to review.

## Verified codex-0.145 behavior

Source: openai/codex at tag `rust-v0.145.0` plus live probes. `file:line` are under `codex-rs/`.

- **CODEX_HOME** (`utils/home-dir/src/lib.rs:13-63`): if set, the path must exist (fatal `NotFound` otherwise) and be a directory, then it is `canonicalize()`d, fully resolving symlinks. Empty is treated as unset (defaults to `~/.codex`). Create the dir before spawn, and expect a symlinked `~/.clauth` to resolve to its real target (canonicalize both sides of any identity check).
- **auth.json** (`login/src/auth/storage.rs:40-61`): `{ auth_mode?, OPENAI_API_KEY?, tokens{ id_token, access_token, refresh_token, account_id }, last_refresh?, agent_identity?, personal_access_token?, bedrock_api_key? }`, at `$CODEX_HOME/auth.json`.
- **auth.json is written IN PLACE** (`storage.rs:202-218`): `truncate + write` at mode 0600, no temp file, no rename. A symlink at that path is followed to its target. A concurrent reader can catch a partial body, so every clauth read treats a short or unparseable file as "retry", never as "the chain is gone".
- **Store modes** (`config/src/types.rs:105-117`, `storage.rs:498-540`, key `cli_auth_credentials_store`), default `File`: `File` reads/writes the file; `Keyring` uses the OS keyring keyed by `sha256(canonical CODEX_HOME)` and ignores + deletes the file; `Auto` is keyring-first with file fallback and deletes the file on a keyring save; `Ephemeral` is memory-only. The per-session `config.toml` is a copy of the user's, so an `auto`/`keyring` setting there makes codex ignore the linked `auth.json` and delete it on the first refresh. Forcing `-c cli_auth_credentials_store="file"` fixes it (verified live: keyring config plus a present `auth.json` reported `no Codex credentials were found`; adding the override reported `auth is configured` / `auth storage mode: File`).
- **Refresh** (`login/src/auth/manager.rs:1306-1445`): `POST https://auth.openai.com/oauth/token`, body `{client_id: app_EMoamEEZ73f0CkXaXp7hrann, grant_type: "refresh_token", refresh_token}`. The response rotates the refresh_token and `persist_tokens` writes it back with `last_refresh = now`. `refresh_token_reused` -> `Exhausted` -> `Permanent`. `refresh_token_expired` -> Expired, `refresh_token_invalidated` -> Revoked. Test hooks: env `CLIENT_ID_OVERRIDE_ENV_VAR`, `REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR`.
- **Refresh trigger** (`manager.rs:2506-2528`, constants `:180-181`): the access token's JWT `exp` is tested first, refreshing within `CHATGPT_ACCESS_TOKEN_REFRESH_WINDOW_MINUTES = 5` of it. `last_refresh` older than `TOKEN_REFRESH_INTERVAL = 8` days is the fallback for an unparseable exp. A live session rotates on the access token's clock, not on a day timer.
- **Codex re-reads before it spends** (`manager.rs:2366-2400`, reload at `:2109-2143`): `refresh_token()` reloads `auth.json` from disk and returns early with `"Skipping token refresh because auth changed after guarded reload"` when the on-disk auth differs from its cache. It POSTs only on `ReloadedNoChange`. Reload-then-compare, so a narrow TOCTOU window remains.
- **Usage** (`backend-client/src/client/rate_limit_resets.rs:82-83`): `GET https://chatgpt.com/backend-api/wham/usage`, windows duration-keyed by `limit_window_seconds`. The body carries a top-level `plan_type` that is the live counterpart of the id_token's stale `chatgpt_plan_type` claim. A window (`codex-backend-openapi-models/src/models/rate_limit_window_snapshot.rs`) carries `used_percent`, `limit_window_seconds`, `reset_after_seconds`, `reset_at` and nothing else; `rate_limit.limit_reached` is one flag for the whole status; `rate_limit_reached_type.type` is a REASON, one of `rate_limit_reached`, `workspace_owner_credits_depleted`, `workspace_member_credits_depleted`, `workspace_owner_usage_limit_reached`, `workspace_member_usage_limit_reached` (`backend-client/src/client.rs:551-573`), never a window designation.
- **Managed config outranks session flags** (`config/src/config_layer_source.rs:44-46`): `SessionFlags` (the `-c` overrides clauth forces) is precedence 30, the legacy `managed_config.toml` file 40, the macOS MDM managed preference (`config/src/loader/macos.rs:20-22`, domain `com.openai.codex`, key `config_toml_base64`) 50. The file path is `/etc/codex/managed_config.toml` on unix and `<CODEX_HOME>/managed_config.toml` elsewhere (`config/src/loader/layer_io.rs:20,168-183`). `debug.config_lockfile.load_path` rebuilds the whole layer stack from the lockfile alone (`core/src/config/mod.rs:1384-1396`), erasing the `-c` layer. A managed `cli_auth_credentials_store` other than `file`, or a managed lockfile, therefore defeats the forced file store: a clauth codex spawn refuses on either and warns on a managed `sqlite_home`; the MDM layer is not read.
- **config.toml is written by codex** (`codex mcp add` mutates it, verified live), so a session's `config.toml` is a COPY, never a symlink onto the user's real `~/.codex/config.toml`.
- **A codex home fills with more than auth/config/sessions**: `goals_1.sqlite`, `logs_2.sqlite`, `memories_1.sqlite`, `state_5.sqlite` (plus `-shm`/`-wal`), `history.jsonl`, `models_cache.json`, `installation_id`, `version.json`, `log/`, `cache/`, `tmp/`.
- codex refuses to plant PATH-alias helper binaries when `CODEX_HOME` is under `/var/tmp` (non-fatal warning). Irrelevant for `~/.clauth`, bites only if a home ever lands under a temp path.

## Architecture

### State: two files, disjoint profile sets, one namespace
`profiles.toml` (`AppState`, claude, unchanged) and `codex-profiles.toml` (codex, new). Both sit under the same global state flock (`~/.clauth/.lock`, `with_state_lock`), so cross-file operations serialize. No shared fields, so no cross-file consistency problem. Names are unique across both (decision 2). The daemon iterates both and dispatches per subsystem.

### The two shared seams (a `Harness` trait)
- **credential-install:** claude = `.credentials.json` link/snapshot/detach plus the macOS keychain mirror behind the `ensure_installable` gate. codex = a local atomic rewrite of `profiles/<name>/auth.json` preserving unknown keys.
- **runtime-spawn:** claude = `CLAUDE_CONFIG_DIR` pin + `claude_command()` + `MANAGED_ENV_KEYS` scrub. codex = `CODEX_HOME` pin + `codex_command()` + a codex scrub list that must include `CODEX_HOME` itself and `OPENAI_API_KEY`.

### Inline `match harness` (not seamed; lower claude-regression risk)
- usage-fetch: the codex standby refresh and the codex usage poll run as two sequential legs on the scheduler thread after the claude fan-out (`scheduler.rs`, `standby_tick` then `codex_usage_tick`), not as a third `std::thread::scope` arm (see "Deviations as shipped").
- fallback chain-member: codex quota semantics feed the reused `walk_chain`. There are now TWO snapshot entry points over one shared body, so the codex arm covers both.
- `which.rs` session-to-profile resolution: a codex arm off `CODEX_HOME`.
- sync-skip: `settings_sync`, `claude_json`, the session index, runtime GC, and `live_isolated_stores` are claude-only. Most already skip codex structurally (see below); the two that did not are named in "Folded fixes". `settings_sync`'s union still walks the directory listing and skips the dirs the codex roster claims (claude-first on a dual claim); an unreadable codex roster reads as an empty one, so a codex file can never pause the claude sync.

### The codex runtime mirrors the claude runtime exactly
Same shape, same reasons, same failure modes. Per session, keyed by the session id minted in `acquire`:

| entry | shared flavor | `--isolated` |
|-------|---------------|--------------|
| `auth.json` | symlink to `profiles/<name>/auth.json` | symlink to the same file |
| `config.toml` | copy of the user's `~/.codex/config.toml` | copy |
| `skills/`, `rules/`, `agents/`, `templates/`, `references/`, `AGENTS.md` | symlink to the user's | absent (isolated links nothing from the user) |
| `hooks.json` | symlink ONLY when the profile opts in; default off | absent |
| the four sqlite DBs plus their `-wal`/`-shm` | symlink to the profile-global home | per-session |
| `history.jsonl` | symlink to the profile-global home | per-session |
| `sessions/`, `archived_sessions/` | symlink to the profile-global home (the store's dirs exist before the link, so codex can create through it) | per-session, discarded |
| everything codex creates (`log/`, `cache/`, `tmp/`, `models_cache.json`, `installation_id`) | per-session | per-session |

Rules that fall out of it:

- the home dir name must NOT match `runtime*` or `sessions*`. Both config reconcilers walk `shared_runtime_dirs()` (`runtime.rs:762`), which filters on exactly those stems, so a codex home named `runtime-<sid>` would receive Claude Code's `settings.json`.
- `hooks.json` is opt-in per profile because hooks execute code and the home is one clauth built.
- Windows `LinkMode::Fake` gets the same treatment claude gets: copy plus mtime mirror, and the per-session keying collapses to a shared tree there, with the same consequences already documented for claude. The `auth.json` copy converges with the store at build and at teardown: the later `last_refresh` wins, mtime where a side has no parseable stamp or both stamps are equal, and a remaining tie goes to the side that was the only possible writer at that boundary (the copy at teardown, the store at build).
- the one teardown sync-back left is the corrupt-DB recovery copy (codex renames the linked DB aside and writes a fresh real file; the healed file is copied into the store): best-effort, never failing a completed session, matching `rescue_teardown`. Rollouts need none: they land in the store through the link.
- `auth.json` being one physical file is what makes decision 8 hold. There is no seed step, no adopt-back watchdog, and no whitelist-diff of the home.

### Codex sessions in the live-session registry
Rows are registered, with a harness tag on the row. They count in the live tally, they gate delete and disable and rotation, and they show in the TUI live column. They never enter the swap executor (`SessionSwap`) or the per-session decision leg (`scheduler.rs:2762`): codex reads `auth.json` at start and a mid-session rewrite is a no-op the executor would publish as a successful member change. Codex fallback lands at the next start, which is the truth about codex. `--with-fallback` on a codex profile refuses, naming the reason.

## Integration anchors (current `mommy` tree, re-derived 2026-07-27; re-verify before editing)

| what | where |
|------|-------|
| `AppState` (claude slot) | `profile.rs:327`; `active_profile` `:328`, `profiles` `:329`, `fallback_chain` `:331`, all `ProfileName` (newtype `profile.rs:28`) |
| state write / read | `save_app_state` `profile.rs:1281`, `load_app_state` `:1246` |
| `Profile` (not serde) | `profile.rs:145-147`; built by `load_profile` `:1300` from `ProfileConfig` `:776`; empty-or-blank config maps to `ProfileConfig::default()` `:1302-1312` |
| path layer | `profiles_root` `profile.rs:908`, `profile_dir` `:916`, `profile_subpath` `:920` (in `profile.rs`, NOT `runtime.rs`) |
| per-profile caches | `profile_cache.rs:38-42`, five files |
| `active_profile` write sites (11, all staying claude-scoped) | `profile.rs:708,742`; `actions.rs:346,366,791,827,898,968`; `tui/app.rs:6587,6614,6983`. Test helper `testutil.rs:196`. No `format!`-built writer exists |
| credential install | `link_profile_credentials` `claude.rs:445`, `force_link_profile_credentials` `:920`; gate `oauth::ensure_installable` `oauth.rs:1298`, whose FIRST branch (`:1305-1321`) short-circuits a `session-token.json` profile. The codex arm lands after that branch, not before |
| install source resolution | `canonical_credentials` `runtime.rs:848` -> `claude::install_source_path` `claude.rs:175`. Both flavors resolve here, which is why `--isolated` already shares the chain |
| runtime acquire | `ProfileRuntime::acquire` `runtime.rs:1624` (struct `:1599`), signature `(&Profile, Isolation, &[String], follows_chain: bool)`; registers a `live_sessions` row inside its state-lock hold |
| spawn env | pin `start.rs:227` and `mcp/mod.rs:711`; `claude_command()` `runtime.rs:1980`; `MANAGED_ENV_KEYS` `:1923`, `scrub_profile_env` `:1939`, `guard_home_project_settings` `:1967` |
| rotation | `RotationGuard` `runtime.rs:885`, `rotation_lock_path` `:856`; six acquisition sites incl. the swap executor `:1486` and `acquire` `:1646` |
| liveness | `has_live_session` `runtime.rs:464`, `session_marker_dirs` `:430` (readdir + `sessions*` prefix match), `session_marker_paths` `:359`, `session_row_is_live` `:380` |
| GC | `gc_stale_runtimes` `runtime.rs:646` -> `gc_runtime_trees` `:652`, `gc_bare_markers` `:702`, `gc_live_session_rows` `:730`, `gc_one_pair` `:743`; `shared_runtime_dirs` `:762`, `live_isolated_stores` `:808` |
| registry | `live_sessions.rs`: `LiveSession` `:42` (`start_profile: String`), `intended_member` `:57`, `current_member` `:63`, `starting` `:80`, `LiveTally` `:129`, `from_live_rows` `:200`, `add_bare_sessions` `:184` |
| swap executor | `SessionSwap` `runtime.rs:1310`, `SwapCell` `:1327`, swap body `:1481-1546`; `unsupported_swap_transport` `:1008`; `swap_eligible` `:1122` |
| fallback | `walk_chain` `fallback.rs:1018` (reusable as-is); `ChainMember` `:740`, `ChainSnapshot` `:766`; `snapshot_chain` `:821`, `snapshot_session_chain` `:850`, shared body `build_chain_snapshot` `:894`; `is_exhausted_from_usage` `:951` |
| pending switch | `PendingSwitch` `scheduler.rs:163`, `PendingSwitchOff` `:167`; drained `daemon/tick.rs:83` and `:278`, re-queued `:270`, resolved `:109`, applied `:214` |
| usage fan-out | `TokenEntry` `scheduler.rs:171`, `ThirdPartyEntry` `:188`, `NamedEntry` `:196`; `fn tick` `:2133`, fan-out scope `:2281-2296` |
| locks | `with_state_lock` `lock.rs:220`, `STATE_LOCK_TIMEOUT` `:44`; ranks `lockorder.rs:63`, `SwapCell = 550` `:150`. A codex-state mutex fits existing ranks; no new rank |
| config reconcilers | `settings_sync::sync_once` and `claude_json::sync_once` run in the per-session watchdog `runtime.rs:1770/1773` and its Drop `:1848/1851`; `known_paths` `settings_sync.rs:150` and `claude_json.rs:71`, both routed through `shared_runtime_dirs`; `per_profile_env_keys` `settings_sync.rs:180` |
| name validation | `validate_profile_name` `actions.rs:31`; call sites `main.rs:421`, `tui/app.rs:5585,6277,6369,6943` (the last passes an empty `existing` on purpose) |
| profile CRUD | `save_profile` `profile.rs:1435` (no dir-exists check), `rename_profile` `actions.rs:492-496` (two-sided dir move), `delete_profile` `actions.rs:517` (live gate `:523`, dir removal `:555`, state `:560-561`) |
| perms walk | `enforce_clauth_perms` `profile.rs:1218`, callers `profile.rs:1525` and `daemon/mod.rs:126` |
| hot reload | `ReloadFingerprint` `profile.rs:865`, `reload_fingerprint` `:878`; `app_state_mtime` `:879` covers `profiles.toml` only; the dir walk `:881-901` already picks up any profile's `config.toml` |
| published feed | `SCHEMA_VERSION` `daemon/status_json.rs:30`; body `:337-345`. Readers: `daemon/probe.rs:101` (untyped, `generated_at` only); `list.rs` renders the typed `ProfileEntry` rows `build_profile_entries` builds |
| shared JSON view | `profile_json.rs` feeds MCP, the daemon writer, and `status --json`. One additive change covers three surfaces |
| which | `session_auth` `which.rs:111`, `session_profile_from_config_dir` `:133-146` (gated on `is_shared_runtime_dir_name`, returns the dir's `file_name()` verbatim) |
| clean slate | zero `codex` tokens in `src/`. Three `harness` hits, all the test-harness sense (`testutil.rs:60,80`, `usage/fetch.rs:19`), so a bare `rg harness` sweep has noise |

## Folded fixes (latent today, each its own commit)

These are claude-side defects found while re-deriving the anchors. They ship inside this series because codex is what makes three of them bite.

1. `is_exhausted_from_usage` (`fallback.rs:951`) returns false when a member has no usage entry at all, so an account with no window data reads as full headroom and becomes the chain's preferred target. Codex accounts start in exactly that state.
2. `per_profile_env_keys` (`settings_sync.rs:180`) readdirs the profiles root with no membership filter and reads every `<dir>/config.toml`. With bare codex dirs it would treat a codex profile's `[env]` as claude per-profile env. Shipped as a dir walk that SKIPS the dirs the codex roster claims (claude-first on a dual claim) rather than a roster-driven union, so an off-roster claude dir keeps the fail-closed leak protection the walk exists for (see "Deviations as shipped"); an unreadable codex roster is treated as empty so a codex file never pauses the claude sync.
3. The rotate error path (`oauth.rs:740-741`) persists nothing, so the next tick replays the same token. Harmless for anthropic, permanent death for codex. The codex arm needs its own outcome that marks the attempt spent.
4. `live_isolated_stores` (`runtime.rs:801-804`) carries an `#[allow(dead_code)]` claiming it is unwired while `sessions.rs:531` uses it. Drop the attribute, then add the codex-membership skip.

Also in this bucket, both from the contributor's review of the previous revision:

5. `enforce_clauth_perms` (`profile.rs:1218`) recursively chmods every file under `~/.clauth` to 0600, stripping the exec bit off the PATH-alias helper binaries codex plants in its home. It must skip DESCENDING into a codex home rather than skipping the dir node, and `auth.json`'s writer owns its own 0600 rather than relying on the sweep to retighten it.
6. `reload_fingerprint` (`profile.rs:878`) stats `profiles.toml` only, so a codex state edit (a switch, a chain reorder) never shifts the fingerprint. Add `codex-profiles.toml`. The dir walk already covers a codex profile's `config.toml`, so this is narrower than it looks.

## Refactors landing in parallel (maintainer-side, on `mommy`)

A sweep on 2026-07-27 found a set of patterns that this series would otherwise duplicate across two harnesses. They are NOT a gate on the fork and they are not scope on the series: they land on `mommy` while the series is in flight, each as its own commit, sequenced so they arrive before the phase whose diff they shrink. Seven of the eight landed 2026-08-27 (shas in the table); only `AccountId` remains. Listed here so a rebase is never a surprise.

| refactor | landed / lands before | why it matters to this series |
|---|---|---|
| `credentials` + `active_profile` behind witness-gated writers (`LockedSlot`); remaining slots still `pub(crate)` | landed `6a68c7aa` (2026-08-27) | the codex state type is born with the guarded surface instead of inheriting the slots that stayed `pub(crate)`. This one CHANGES phase 1: do not copy `AppState`'s current shape |
| `ProfileName` at the API boundary (public fns take `&str` today, so the newtype is cosmetic) | landed `f9f42bfc` (2026-08-27) | cross-file name uniqueness is exactly the case where a typed parameter catches "passed the other harness's name" |
| reconciler dedup in `jsonsync` (`known_paths` and the `LAST_SYNCED` mtime fast-path are duplicated between `settings_sync.rs:85,150` and `claude_json.rs:62,71`) | landed `e2e93363` (2026-08-27) | folded fix 2 becomes one edit on a shared helper |
| `AccountId` newtype over the bare-`String` account-uuid equality checks | phase 2, riding the state template | the identity comparisons multiply across two state files with no compile-time guard against an argument-order swap. The type already exists in `profile.rs` guarding some sites; anchor the extension to the template's account-uuid field so it lands with the shape it guards |
| `SessionSwap::publish_swap` extracted, with the existing `holds::<State>()` rank assert on entry | landed `f8eb04f3` (2026-08-27) | the cross-function sequencing contracts around the swap live in prose today. A codex runtime that calls a primitive cannot inherit prose |
| `ShutdownFlag` newtype over `SessionSwap`'s bare `AtomicBool` (`runtime.rs:1305`) | landed `884b0fbc` (2026-08-27) | the Acquire/Release contract travels in the type instead of being re-derived per carrier |
| typed-view consolidation across `list.rs`, `which.rs`, `profile_json.rs`, `format::account_tier` | partial: `list.rs` landed `94c7e9cf` (2026-08-27) | the daemon writes a shape two readers re-index untyped. All three tier axes are now closed: the `canceled` override in `2000338`, then form and freshness together in `5673eca`, which moved `which --json` onto `tier_label` so every JSON surface answers from the same helper. `endpoint_label` was renamed `account_tier` and its base-url branch deleted outright (it had gone unreachable behind both callers' `is_oauth()` gates); it now returns a typed `PlanTier`, not a string, so it already stopped multiplying the untyped-`Value` pattern. What is left here is `which.rs`/`profile_json.rs`'s own untyped `Value` indexing, which additive codex columns would triple |
| one shared login-flag table behind the three completion dialects | landed `3c47b23a` (2026-08-27) | a `--codex` flag otherwise means hand-editing `bash`, `zsh`, and `fish` separately |

## Parity map

| clauth feature | codex disposition |
|---|---|
| account switch | port (local `auth.json` rewrite, session-boundary) |
| fallback chain + walk | port (`walk_chain` reused, codex members, both snapshot entry points) |
| standby refresh | port, single-writer plus the no-replay rule |
| usage tracking | codex-specific (`wham/usage`, duration windows mapped onto the named slots) |
| isolated `start` | port (`CODEX_HOME` pin, forced file store, the runtime table above) |
| `clauth which` | port (codex arm off `CODEX_HOME`) |
| burn rate + ETA | port unchanged (`burn.rs` is label-driven and names no window) |
| `status.json` / `which --json` / completions | ADDITIVE codex fields, schema stays 2; the completion scripts gained the `--codex` / `--browser` flags while name completion (`clauth __complete`) still offers the claude roster alone |
| TUI `c codex` / `c claude` filter + header chip | in scope |
| TUI Overview active-account marker animation | a maintainer-side follow-up after the series (maintainer ruling 2026-09-01); the codex row carries no active marker: the active codex account reads by its bold name alone |
| daemon version in the feed (`clauth_version`) | in scope, additive; the old-daemon CLI warning line is dropped (maintainer ruling 2026-09-01) |
| per-session fallback / `SessionSwap` / `--with-fallback` | SKIP, refuse on a codex profile |
| settings sync / `.claude.json` sync | SKIP codex |
| sessions index / `resume` / `info` / rescue | SKIP codex (codex owns its own `sessions/`) |
| Tokens tab, `token_ledger`, the cost lens | SKIP codex this series (claude-shaped JSONL) |
| MCP server, `delegate`, Services tab | claude-only, copy fixes only (below) |
| kick / auto-start / `kick_block` | claude-only. Codex has no window-opening endpoint, and the kick's 401 arm (`oauth.rs:576`) falls through to a refresh against `platform.claude.com` |
| spend ceiling, weekly soft line, `weekly_scoped` | no codex wire equivalent for the ceiling or the scoped windows. The weekly soft line has its own codex setting (`weekly_switch_threshold` in `codex-profiles.toml`, hand-edited, defaulting to the claude default). `check_scoped` defaults ON (`fallback.rs:913`) and is disarmed for codex members or an unmapped window becomes a rotation-blocking phantom |
| macOS keychain mirror | N/A (forced file mode) |
| `clauth proxy` | out of scope, separate feature |

### MCP and the Services tab stay Claude-Code-only

Both fail closed already: `load_config` builds `AppConfig.profiles` from `profiles.toml` only (`profile.rs:1528`), so codex profiles are invisible to every MCP tool, and the Services tab has no per-profile selector to hand one to. Nothing behavioral changes. What ships is copy:

- the MCP init block asserting clauth manages "Claude Code accounts" (`mcp/render.rs:104`).
- the Tokens tab's missing Claude-Code-only qualifier.
- `delegate`'s, `switch_profile`'s and the `profiles` filter's "profile not found", which is what a user gets for a codex name that demonstrably exists. It says so: the refusal names the codex account.

`delegate` over `codex exec` is its own PR after this series. Until then a codex profile reaching it gets the corrected message.

## Gotchas

- CI (`.github/workflows/ci.yml`) runs `fmt --check`, `cargo-deny` and `cargo-audit` as parallel jobs beside a per-OS check job: release clippy `-D warnings` + nextest, then debug clippy + nextest, the only leg where the `cfg(debug_assertions)` lock-rank checks run.
- Do not reintroduce codex fields into `AppState`. The file split is what dissolves the mixed-version write-hole class.
- Lock order: the codex-state mutex reuses existing `RankedMutex` ranks. No new rank, no inversion. Both files sit under `with_state_lock`.
- `gc_runtime_trees`, `shared_runtime_dirs`, and the GC pairing rule are STRICT by design: they act only on names matching `runtime*`/`sessions*` predicates, so a codex home falls through untouched. Do not loosen them into membership lookups. A dead codex marker dir with no paired runtime sibling is collected by the existing orphan branch (`runtime.rs:679-694`), which is the behavior you want.
- Mechanical-sweep discipline: grep-verify every `active_profile` / `fallback_chain` occurrence before and after each edit. A `format!`-built name defeats symbol grep, and `runtime.rs:238-265` plus `live_sessions.rs:235` build names that way.

## Delivery (reviewable series)

1. **Harness axis.** `Harness` enum; the `codex-profiles.toml` state type plus load/save plus codex profile CRUD (its slots private behind writers that take a lock witness, per "Refactors landing in parallel", rather than mirroring `AppState`'s current public slots); cross-file name uniqueness in `validate_profile_name` (reading the rosters itself); `codex-profiles.toml` in `reload_fingerprint`; the harness tag on live-session rows; `which.rs` dispatch; the `settings_sync` roster fix; the `live_isolated_stores` attribute + skip; `enforce_clauth_perms` codex-home exemption. `profiles.toml` and every claude path untouched. No codex-engine code. The path layer needs no work at all under decision 3, which is the point of dropping the suffix. Pin the codex CRUD/switch/delete/rename CLI grammar before this phase's CLI surface.
2. **The two `Harness` trait seams** (credential-install, runtime-spawn) with claude behind them, a pure behavior-preserving refactor. Note `ensure_installable`'s CLA-SPLIT first branch: the seam goes after it.
3. **Codex runtime.** Per-session home per the table above, `CODEX_HOME` pin, forced file store, config copy, the symlink set, the scrub list including `CODEX_HOME`, the managed-config check at spawn (`/etc/codex/managed_config.toml` on unix, nothing on windows: a `cli_auth_credentials_store` other than `"file"` or a `debug.config_lockfile.load_path` refuses the spawn naming the file, the key and the fix; a `sqlite_home` warns and proceeds; an absent or unparseable file is clear; the macOS MDM layer at precedence 50 is not read, a documented gap), the `--with-fallback` refusal.
4. **Codex refresh** under `RotationGuard`, with the no-replay-on-error rule and the 5-minute pre-expiry stand-down. Plus the contributor's health-triggered kick: a `PollError::Unauthorized` feeds a set the standby leg drains and force-refreshes, bypassing the age gate AND the no-replay memo (one forced attempt per kick, see "Deviations as shipped"), with a consecutive-kick breaker (2, reset on any successful poll). A `refresh_token_reused`, `refresh_token_expired` or `refresh_token_invalidated` verdict, or a rotation the wire accepted whose new pair never reached the store (`lost`), quarantines the chain durably: a record beside the store bound to the token it judged, so the member is excluded from the walk, refused as a start or switch target with the fix named (`clauth login <name> --codex --browser`; the bare capture is a no-op for an adopted slot), and published `broken`, until a later successful rotation clears it or a fresh chain lands in the store by any path (the record speaks only for the token it judged). An unrecognized 4xx never quarantines: it keeps the memo, the kick and the breaker, since it is not a verdict about the chain. Naming: this is unrelated to the claude 5h-window kick.
5. **Codex usage leg** (`wham/usage`) plus the chain member. Windows map by duration onto the named slots: over 24h to the weekly slot, otherwise the 5h slot, with a positional fallback and a collision rule. `rate_limit.limit_reached` forces the fuller window to 100 (the wire names no blocking window); `rate_limit_reached_type.type` is carried verbatim as the block's reason. `plan_type` from the response body is authoritative over the id_token claim, which is the fallback for an account not yet polled. Disarm `check_scoped` for codex members. Fold fix 1 here. Watch `parse_nh_nd_label` (`fetch.rs:498-503`): it accepts whole-hour and whole-day labels only, so a 90-minute codex window silently drops the pace line.
6. **Published surface and parity.** Additive codex fields in `status.json` / `which --json`, the `--codex` / `--browser` flags in the completion scripts (codex names do not tab-complete); the `c codex` / `c claude` view filter and header chip; the daemon version in the feed (`clauth_version`; no old-daemon warning line, maintainer ruling 2026-09-01); the copy fixes listed above. The Overview marker animation is a maintainer-side follow-up (`⬢`/`⬣` beside the active codex account, the claude glyphs captured from a live CC session); the codex row carries no active marker: the active codex account reads by its bold name alone.

## Settled questions (were "open")

1. **`delegate` / `clauth start {codex}`.** `start` is interactive `codex`. `delegate` is out of this series and ships as its own PR: `codex exec`'s output contract differs from `claude -p`'s, so the MCP tool needs a per-harness result formatter. Until then a codex profile gets a clear message.
2. **Shared config in the home.** Allowlist symlinks per the runtime table. `hooks.json` is opt-in per profile, default off, because hooks execute code.
3. **Usage model mapping.** Reuse the named `five_hour`/`seven_day` model, map by duration. No codex window shape. The TUI renders the existing columns unchanged.
4. **Auto-start / kick.** Claude-only. Codex usage is passive and there is no window to open.
5. **Plan-tier staleness.** The `wham/usage` body's `plan_type` is authoritative; the JWT claim is the fallback for an account not yet polled.
6. **`clauth proxy`.** Deferred, standalone. If it ever lands: a cached "spent" reading is an advisory rank in selection, never a synthetic 429.
7. **codex login.** Reimplement PKCE; never shell out to `codex login`, which writes the user's LIVE `~/.codex/auth.json` and forks the chain. Traps: the registered loopback ports are fixed (1455, fallback 1457, path `/auth/callback`); the code exchange is form-urlencoded while the refresh at the same endpoint is JSON; write `auth_mode: "chatgpt"` explicitly, since `resolved_mode()` infers ApiKey from a bare `OPENAI_API_KEY`; `tokens.account_id` comes from the id_token's `chatgpt_account_id` claim; the RFC-8693 secondary exchange that mints `OPENAI_API_KEY` is best-effort and its failure must not fail the login.
8. **Cross-harness CLI.** Names are globally unique (decision 2), so `switch <name>` is unambiguous and no `--codex`/`--claude` override is needed on it. `--codex` stays on create and login, where it selects which file the new profile lands in. The uniqueness rejection names the harness that holds the name. The claude-only verbs (`disable`, `enable`, `rolling-token`, `static-token`) refuse a codex name as a codex profile and list claude names alone. The codex-side copy of `delete` says remove/removed (maintainer ruling 2026-09-01); the verb and the shared confirm gate stay. A codex delete runs under the profile's rotation guard and detaches the user's `~/.codex/auth.json` link when the capture installed it.
9. **Tokens tab.** Codex excluded this series. `token_ledger` storage is harness-agnostic; the feeder (`tokens.rs`) is claude-shaped. A codex cost lens is its own feature.

## Deviations as shipped

Where the merged series departs from this spec's letter, recorded so the letter stops being read as the mechanism:

| spec said | shipped | why it stands |
|---|---|---|
| a third usage leg inside the claude `std::thread::scope` fan-out | two sequential legs on the scheduler thread, after the claude fan-out: the standby refresh, then the usage poll | a codex refresh is a blocking single-use token round trip; sequencing it after the claude legs keeps it off the daemon watchdog's abort and keeps one cross-process writer, which the no-replay rule needs. The usage poll runs after the refresh so a fresh access token polls instead of spending a tick on a 401 |
| `per_profile_env_keys` takes the claude roster instead of the directory listing | the directory walk stays and skips the dirs the codex roster claims, claude-first on a dual claim | the member set the union must cover is directory-derived (`shared_runtime_dirs`), and a roster-driven union would let an off-roster dir keep merging its settings while its `[env]` keys read as shared, the exact leak the walk fails closed against |
| a per-harness pending-switch queue (`PendingSwitch` / `PendingSwitchOff` keyed per harness) | no codex pending queue; a codex switch writes the state slot directly | codex binds `auth.json` at start, so nothing mid-session consumes a pending handshake; the slot IS the switch and it lands at the next codex session |
| the kick bypasses only the age gate | the kick bypasses the age gate and the no-replay memo, one forced attempt per kick, breaker at two consecutive kicks | a memoed token is one a tick already spent its shot on; a 401 that persists past that memo is the one case where re-sending it is the only way to learn whether the chain is alive, and the breaker bounds the replay cost to two attempts before the quarantine verdict |
| a capture copies codex's own `auth.json` byte for byte | the copy is re-stamped with `last_refresh` = the capture time, every other key kept | the fake-link convergence decides by `last_refresh` first; an unstamped capture of a chain codex minted before the last session's final rotation would lose to the session copy at the next build, discarding the user's re-capture |
| a `c codex` / `c claude` filter plus a header chip | one `c` cycling both → claude → codex; the filtered harness named in a left title of its own, one border cell after the Overview's bare `ACCOUNTS` title (`ACCOUNTS ─ codex`), and the per-harness counts at that title row's right edge; the header carries neither | the header row stays counts-free at every width, and the counts describe the table the panel lists, whatever the filter shows |
| `debug.config_lockfile` present refuses the spawn | `debug.config_lockfile.load_path` present refuses; the table's other keys (`export_dir`, `allow_codex_version_mismatch`, `save_fields_resolved_from_model_catalog`) do not | only `load_path` rebuilds the config stack from one file (`core/src/config/mod.rs:1385-1396`); the others export or modify a replay that `load_path` alone triggers |
