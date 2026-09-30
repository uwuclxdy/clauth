#!/bin/sh
# Detached per-pane watcher, spawned by `report-profile.sh` for Claude Code
# and codex panes. Re-publishes the pane's account on a timer, so the sidebar
# tag follows an account swap that fires no herdr event: a `--with-fallback`
# session moving onto the next chain member, or a bare `claude` following a
# `clauth switch`. Each tick it re-reads herdr's own per-pane agent record and
# re-reports as THAT agent, so the tag follows the harness too — a codex
# watcher never clears the tag a later claude pane publishes, and vice versa.
# When the pane runs neither claude nor codex, it clears the tag and exits, so
# an idle pane stops showing an account. Exits once the pane is gone;
# `report-profile.sh` spawns a fresh watcher for any claude or codex pane it
# sees without a live one.
set -u

pane="${1:?usage: watch-profile.sh <pane-id> <pidfile>}"
pidfile="${2:?usage: watch-profile.sh <pane-id> <pidfile>}"
herdr_bin="${HERDR_BIN_PATH:-herdr}"
# The tag_watch_secs knob wins over the env, which wins over the 5s default;
# a predating clauth answers nothing, so the env/default chain still holds.
interval=$(clauth herdr config get tag_watch_secs 2>/dev/null || printf '%s' "${CLAUTH_PROFILE_WATCH_INTERVAL:-5}")
# A non-numeric interval would make `sleep` fail instantly, zero would hot-spin
# the loop, and a hand-edited knob of absurd magnitude overflows `sleep`'s
# parser; clamp non-numeric to the default, and the range to [1 s, 1 h].
case "$interval" in
    *[!0-9]* | '') interval=5 ;;
esac
[ "$interval" -lt 1 ] && interval=1
[ "$interval" -gt 3600 ] && interval=3600
dir=$(dirname "$0")

# Reads the pane's own `agent` field from a `pane get` JSON line on stdin. The
# nested `agent_session` record (whose `agent` field is always present) and the
# state-label / token maps are cut off first, so the match reads the pane's
# top-level `agent`, never a nested one; an absent top-level `agent` (an idle
# pane) reads empty.
pane_agent() {
    sed -n 's/"state_labels":.*//; s/"tokens":.*//; s/"agent_session":.*//; s/.*"agent":"\([^"]*\)".*/\1/p'
}

# Own the pidfile so `report-profile.sh` sees a live watch and does not spawn a
# second one; drop it on the way out so a later run can. A pidfile left behind
# by a killed watch self-heals: the next spawn sees a dead pid and takes over.
echo "$$" > "$pidfile" 2>/dev/null
trap 'rm -f "$pidfile"' EXIT

fails=0
while :; do
    # The live agent herdr reports for the pane now, from `pane get` (carries
    # `agent` since v0.8.0; the `"agent":"` anchor keeps `display_agent` from
    # matching, the nested-object strip keeps `agent_session.agent` out, and an
    # absent top-level agent reads empty — a shell or unknown program).
    # A gone pane (and a down server) answers non-zero: retry a few times so a
    # transient blip does not kill the watch, then end it once it is persistent.
    raw=$("$herdr_bin" pane get "$pane" 2>/dev/null) || {
        fails=$((fails + 1))
        if [ "$fails" -ge 3 ]; then
            break
        fi
        sleep "$interval"
        continue
    }
    fails=0
    live=$(printf '%s\n' "$raw" | pane_agent)
    # The pane no longer runs claude or codex (a shell, or another agent):
    # clear the tag and release the pidfile, so an idle pane stops showing an
    # account instead of keeping a stale one forever.
    case "$live" in
        claude | codex) ;;
        *)
            "$herdr_bin" pane report-metadata "$pane" --source "${HERDR_PLUGIN_ID:-clauth}" --clear-token clauth --clear-display-agent >/dev/null 2>&1
            exit 0
            ;;
    esac
    # Empty the event/context JSON so the report resolves `agent` from nothing
    # instead of inheriting the spawn hook's stale value, and hand it the live
    # agent just read: a codex watcher on a pane that now runs claude re-reports
    # as claude (and the reverse), never its spawn-time harness.
    HERDR_PANE_ID="$pane" HERDR_PLUGIN_EVENT_JSON='' HERDR_PLUGIN_CONTEXT_JSON='' \
        CLAUTH_PANE_AGENT="$live" \
        "$dir/report-profile.sh" >/dev/null 2>&1 || true
    sleep "$interval"
done
