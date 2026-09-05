#!/bin/sh
# Detached per-pane watcher, spawned by `report-profile.sh` for Claude Code
# panes. Re-publishes the pane's account on a timer, so the sidebar tag follows
# an account swap that fires no herdr event: a `--with-fallback` session moving
# onto the next chain member, or a bare `claude` following a `clauth switch`.
# Exits once the pane is gone; `report-profile.sh` spawns a fresh watcher for
# any claude pane it sees without a live one.
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

# Own the pidfile so `report-profile.sh` sees a live watch and does not spawn a
# second one; drop it on the way out so a later run can. A pidfile left behind
# by a killed watch self-heals: the next spawn sees a dead pid and takes over.
echo "$$" > "$pidfile" 2>/dev/null
trap 'rm -f "$pidfile"' EXIT

fails=0
while :; do
    # A gone pane (and a down server) answers non-zero. Retry a few times so a
    # transient blip does not kill the watch, then end it once it is persistent.
    if ! "$herdr_bin" pane process-info --pane "$pane" >/dev/null 2>&1; then
        fails=$((fails + 1))
        if [ "$fails" -ge 3 ]; then
            break
        fi
        sleep "$interval"
        continue
    fi
    fails=0
    # Empty the event/context JSON so the report resolves `agent` from nothing
    # instead of inheriting the spawn hook's stale value.
    HERDR_PANE_ID="$pane" HERDR_PLUGIN_EVENT_JSON='' HERDR_PLUGIN_CONTEXT_JSON='' \
        "$dir/report-profile.sh" >/dev/null 2>&1 || true
    sleep "$interval"
done
