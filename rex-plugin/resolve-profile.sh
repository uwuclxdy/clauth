#!/bin/sh
# Prints the clauth account a Rex terminal's foreground process spends, as
# "<profile><TAB><how><TAB><agent>", or nothing when it spends none. <how> is
# `clauth` for a session clauth launched, `global` for a bare `claude`, which
# burns whatever owns the global credentials, and `codex` for a bare codex
# whose login clauth adopted. <agent> is `claude` or `codex`, so the watcher
# can name a pane whose own label says nothing: a `clauth start` pane runs the
# clauth supervisor for either agent.
#
# $1 is the foreground pid Rex reports for the terminal. `clauth which --pid`
# makes the whole join, the one the herdr plugin's report-profile.sh asks for
# too: the live-session row up the parent chain, else a bare codex's adopted
# login, else the global login a bare `claude` reads. A `clauth start` with no
# row yet resolves nothing; the watcher asks again on its timer.
#
# Exits nonzero when clauth could not answer (none on PATH, or one older than
# `which --pid`). The watcher then keeps the pane's label as it is, rather than
# reading the failure as "no account" and clearing it.
set -u

# Rex runs this with a bare system PATH.
PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
pid="${1:?usage: resolve-profile.sh <pid>}"

answer=$(clauth which --pid "$pid" --json 2>/dev/null) || exit 1

# One string field of the one-line JSON answer; a null one reads empty.
field() {
    printf '%s\n' "$answer" | sed -n "s/.*\"$1\":\"\([^\"]*\)\".*/\1/p"
}

# Nothing resolved: every field is null, so the profile reads empty.
profile=$(field profile)
[ -n "$profile" ] || exit 0
case "$(field source)" in
    live_session) how=clauth ;;
    codex_adopted) how=codex ;;
    global) how=global ;;
    *) exit 0 ;;
esac
agent=$(field harness)
case "$agent" in
    claude | codex) ;;
    *) exit 0 ;;
esac
printf '%s\t%s\t%s\n' "$profile" "$how" "$agent"
