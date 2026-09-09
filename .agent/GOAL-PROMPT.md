# Next-session kickoff prompts — CBAR-4 (UX rebuild) and TECH (audit backlog)

> Two independent implementation tracks, researched + planned 2026-07-04. Paste
> the relevant **Kickoff prompt** as the first message of a fresh session. Both
> assume `~/projects/devtools/clauth` on branch `feat/macos-keychain` (the
> `-oauth` ref is a stale ancestor) and `~/projects/devtools/ccsbar` (master).
>
> Reading order for the implementing agent:
> 1. `docs/ccsbar/UX-RESEARCH.md` — evidence + the three live incidents
> 2. `docs/ccsbar/CBAR-4-DESIGN.md` — the binding "Preflight" design spec
> 3. `docs/ccsbar/CBAR-4-PLAN.md` — UX build plan (AUTH-1/2 + CBAR4-1…7)
> 4. `.agent/TECH-PLAN.md` — 14-milestone audit backlog (TECH-1…14)
>
> Recommended sequencing across tracks: **TECH-1 first** (~1h: branch backup +
> ledger truth + CI — it is the substrate that makes every later acceptance
> verifiable), then **AUTH-1/AUTH-2** (the operator was burned twice by
> Incident C — stale-token switches logging out every running Claude process),
> then either track: CBAR-4 (user-facing) or TECH-3…8 (daemon reliability).
> TECH-11 overlaps CBAR4-3 — if CBAR-4 ships first, TECH-11 shrinks to the reto.

---

## Track 1 — CBAR-4: the "Preflight" UX rebuild

### Kickoff prompt

```
Implement CBAR-4 per docs/ccsbar/CBAR-4-PLAN.md in ~/projects/devtools/clauth
(Rust: AUTH-1, AUTH-2) and ~/projects/devtools/ccsbar (Swift: CBAR4-1 … CBAR4-7).

Read docs/ccsbar/UX-RESEARCH.md and docs/ccsbar/CBAR-4-DESIGN.md first —
the design doc is binding (wireframes, interaction map, type scale, color
semantics, menu-bar ladder); no "could either" reinterpretation. Work milestones
in dependency order: AUTH-1 → AUTH-2 → CBAR4-1 → … → CBAR4-7. TDD the pure
engines (forecast chain-walk mirror, liveness ladder, menu-bar label ladder,
switch state machine) — they are the truthfulness core.

After each milestone: run its acceptance gate (cargo fmt/clippy -D warnings/test
on Rust; swift build + swift test + --snapshot render on Swift), commit locally,
update .agent/PROGRESS.md with the commit hash + evidence, continue to the next
milestone automatically.

Guardrails:
- NEVER touch my real ~/.claude/.credentials.json or the real
  "Claude Code-credentials" Keychain item in dev or tests. OAuth/refresh tests
  use fixtures; keychain tests use a throwaway service name.
- A REAL account switch logs out every running Claude Code process on this
  machine (see UX-RESEARCH.md Incidents B/C). Do NOT run live switches — verify
  against a scratch config dir / fixtures; live switch acceptance is mine.
- Do NOT push or open PRs without my explicit OK. Commit locally as you go.
```

### /goal predicate (pair with autonomous-grind)

```
/goal In ~/projects/devtools/clauth and ~/projects/devtools/ccsbar, all hold
with evidence pasted in the transcript: (1) cargo test and cargo clippy
--all-targets -- -D warnings exit 0 including new AUTH-1/AUTH-2 tests (auth
gate refuse/refresh paths, chain-walk skip of auth-broken members, auth_status
+ pending_switch + error_code in status.json/socket tests); (2) swift build and
swift test exit 0 including forecast-engine, liveness-ladder, label-ladder, and
switch-state-machine tests; (3) --snapshot renders exist for the four canonical
states (default / inspecting / mid-switch / daemon-dead); (4) .agent/PROGRESS.md
records every AUTH-* and CBAR4-* milestone with commit hashes. OR stop after 50
turns.
```

Then immediately:

```
Skill(skill="autonomous-grind", args="start CBAR-4: AUTH gate + Preflight panel — cargo test/clippy + swift build/test green, four snapshot states rendered, ledger updated, or 50 turns")
```

---

## Track 2 — TECH: the architecture-audit backlog

### Kickoff prompt

```
Work the TECH backlog per .agent/TECH-PLAN.md in ~/projects/devtools/clauth
(branch feat/macos-keychain) and ~/projects/devtools/ccsbar.

Read .agent/TECH-PLAN.md fully first (triage narrative + your milestone's
findings in Appendix A — each carries file:line evidence and a verified failure
scenario). Work milestones in order TECH-1 → TECH-14 unless I scope a subset;
each milestone is one focused session's work with deterministic acceptance
criteria — run them, paste the real output.

Non-negotiables:
- TECH-5 (Daemon::tick extraction + test harness) MUST land before TECH-6/7 —
  those concurrency fixes are TDD-able only on the extracted harness.
- Respect the "NOT TOUCH" list inside each milestone's scope.
- The hero invariant rules every tradeoff: unattended auto-switch before the 5h
  window blocks a session. When a fix risks it, prefer the smaller change.
- NEVER touch my real ~/.claude/.credentials.json or the real
  "Claude Code-credentials" Keychain item; no live account switches (Incidents
  B/C in docs/ccsbar/UX-RESEARCH.md).
- TECH-1's push step: pushing branch feat/macos-keychain to MY origin fork is
  pre-authorized as part of TECH-1 (it is the backup). Everything else: no push
  / no PR without my explicit OK.

After each milestone: acceptance commands green → commit locally → update
.agent/PROGRESS.md (TECH section, commit hash + evidence) → next milestone
automatically.
```

### /goal predicate (scope to the session's chosen subset; example for TECH-1…5)

```
/goal In ~/projects/devtools/clauth: TECH-1 through TECH-5 from .agent/TECH-PLAN.md
are complete with their per-milestone acceptance commands' real output pasted in
the transcript (TECH-1: origin has feat/macos-keychain + CI green; TECH-2:
install/update surfaces retargeted + windows gate decision recorded; TECH-3:
watchdog + supervision fixes with tests; TECH-4: ccsbar staleness + schema
gate visible in a --snapshot render; TECH-5: Daemon::tick extracted with the
inline test harness passing), cargo test + clippy -D warnings exit 0 at every
milestone boundary, and .agent/PROGRESS.md records each with commit hashes. OR
stop after 40 turns.
```

Then immediately:

```
Skill(skill="autonomous-grind", args="start TECH backlog: milestones per TECH-PLAN.md with acceptance output pasted, cargo test/clippy green each boundary, ledger updated, or 40 turns")
```

---

## Standing operator constraints (both tracks — from the original fork brief)

1. NEVER touch the real `~/.claude/.credentials.json` or the real
   `Claude Code-credentials` Keychain item during dev/tests.
2. No push / no PR without explicit OK (sole exception: TECH-1's pre-authorized
   backup push of `feat/macos-keychain` to the operator's own fork origin).
3. Browser OAuth login and real two-account switch tests are MANUAL operator
   acceptance — hand over exact commands, never run them unattended.
4. Model routing for any subagents/workflows: Sonnet 5 floor, Opus 4.8 default
   fan-out tier, Fable 5 only where the task genuinely needs it
   (`~/.claude/rules/workflow.md` § Model Selection).
