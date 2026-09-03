#!/bin/sh
# Publishes the clauth account a herdr pane burns as pane metadata, so a
# sidebar row can name it. Runs from the agent event hooks and from the
# `clauth.which` action, which is why it takes the pane from the injected
# context rather than an argument.
#
# herdr's process-info names the pane's own session in
# `foreground_process_group_id` — the `clauth start` supervisor for a
# clauth-started pane — and the live-session registry carries that pid, so
# the walk up the parent chain is the join. Delegate sessions share the pane:
# they run as children of its `clauth mcp` and their rows are keyed on their
# own processes, so the foreground chain resolves first and the compat sweep
# never matches through an mcp.
set -u

herdr_bin="${HERDR_BIN_PATH:-herdr}"
# clauth resolves its home off $HOME alone (no CLAUTH_HOME override exists in
# the binary), so the registry the walk reads is the tree clauth actually
# writes. An unset HOME trips `set -u` by design: nothing the walk needs can
# resolve without it.
sessions_dir="$HOME/.clauth/live_sessions"
pane="${HERDR_PANE_ID:-}"

# Prints the registry row owning $1 or one of its ancestors, empty if none.
# A `clauth mcp` hop is never matched: rows keyed on it belong to delegate runs
# the pane hosts, and matching one would name a delegate's account for the
# pane. The climb passes through it — the pane's own supervisor sits beyond.
session_row() {
    _pid=$1
    _depth=0
    while [ "${_pid:-0}" -gt 1 ] && [ "$_depth" -lt 8 ]; do
        # The trailing wildcard keeps the hook's own `clauth mcp-await-job`
        # out: that cmdline continues with a dash, never a space or nothing.
        _args=$(ps -o args= -p "$_pid" 2>/dev/null)
        case "$_args" in
            'clauth mcp '* | 'clauth mcp') : ;;
            *)
                # The delimiter alternative keeps the prefix exclusion a bare
                # "pid":$_pid would lose (123 against 1234) without pinning
                # `pid` to its current slot in the row. Newest row wins: a
                # recycled pid can match a stale row and a live one at once,
                # and the stale one sorts first alphabetically.
                _matches=$(grep -lE "\"pid\":$_pid(,|})" "$sessions_dir"/*.json 2>/dev/null)
                if [ -n "$_matches" ]; then
                    _row=$(printf '%s\n' "$_matches" | xargs ls -td 2>/dev/null | head -n 1)
                    printf '%s\n' "$_row"
                    return 0
                fi
                ;;
        esac
        _pid=$(ps -o ppid= -p "$_pid" 2>/dev/null | tr -d ' ')
        _depth=$((_depth + 1))
    done
    return 1
}

# The account a row names: the member a --with-fallback session swapped onto,
# else its launch member.
row_profile() {
    _row=$1
    _p=$(sed -n 's/.*"current_member":"\([^"]*\)".*/\1/p' "$_row")
    [ -n "$_p" ] || _p=$(sed -n 's/.*"start_profile":"\([^"]*\)".*/\1/p' "$_row")
    printf '%s\n' "$_p"
}

# The agent hooks fire for every agent herdr detects, codex and cursor
# included, and those panes spend no clauth account. Both hooked events carry
# `agent`; the `clauth.which` action carries `focused_pane_agent` in its
# context instead, and that fallback is consulted ONLY when no pane id is set
# (actions have none) — an event hook reading the context's focused pane would
# answer for whichever pane holds focus, not the pane the event fired for.
# Neither is set for a plain shell pane, which is the one case that still gets
# an answer.
agent=$(printf '%s' "${HERDR_PLUGIN_EVENT_JSON:-}" | sed -n 's/.*"agent":"\([^"]*\)".*/\1/p')
if [ -z "$agent" ] && [ -z "$pane" ]; then
    agent=$(printf '%s' "${HERDR_PLUGIN_CONTEXT_JSON:-}" | sed -n 's/.*"focused_pane_agent":"\([^"]*\)".*/\1/p')
fi
case "$agent" in
    "" | claude) ;;
    *) exit 0 ;;
esac

profile=""
if [ -n "$pane" ]; then
    info=$("$herdr_bin" pane process-info --pane "$pane" 2>/dev/null)
    # The pane's own session is named by the foreground process group id herdr
    # reports — the `clauth start` supervisor for every clauth-started pane
    # (measured 2026-09-03 on 0.8.2). A delegate session runs in its parent
    # pane's process group with its row keyed on a process of that pane, so
    # the foreground chain must resolve FIRST or a delegate's account shadows
    # the pane's own.
    fg_pid=$(printf '%s' "$info" | sed -n 's/.*"foreground_process_group_id":\([0-9]*\).*/\1/p')
    if [ -n "$fg_pid" ]; then
        row=$(session_row "$fg_pid")
        [ -n "$row" ] && profile=$(row_profile "$row")
    fi
    # The pid sweep is the compat path for a process-info without the
    # foreground field. When the field is there and found no row, the pane
    # hosts no clauth session and the global fallback below is the answer —
    # sweeping would hand a bare `claude` pane the account of a delegate it
    # happens to host. The sweep skips a pid whose parent is `clauth mcp`: a
    # delegate child, whose row is keyed on the child itself, and which names
    # a run the pane hosts, never the pane.
    if [ -z "$profile" ] && [ -z "$fg_pid" ]; then
        pids=$(printf '%s' "$info" | grep -o '"pid":[0-9]*' | cut -d: -f2)
        for pid in $pids; do
            _pp=$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d ' ')
            _pargs=$(ps -o args= -p "$_pp" 2>/dev/null)
            case "$_pargs" in 'clauth mcp '* | 'clauth mcp') continue ;; esac
            row=$(session_row "$pid") || continue
            profile=$(row_profile "$row")
            [ -n "$profile" ] && break
        done
    fi
fi

# No clauth-managed session in this pane: a bare `claude` there burns whatever
# owns the global credentials, so that answer is right rather than a guess.
[ -n "$profile" ] || profile=$(clauth which 2>/dev/null) || profile=""
[ -n "$profile" ] || exit 0

printf '%s\n' "$profile"

[ -n "$pane" ] || exit 0
# Each knob owns one artifact, and its off side publishes the matching clear
# instead of nothing: a knob toggled off must not leave its stale artifact
# standing on the pane. pane_tag still gates the watcher spawn below, while
# the resolve above prints either way.
pane_tag=$(clauth herdr config get pane_tag 2>/dev/null || printf 'on')
if [ "$pane_tag" = on ]; then
    token_flag="--token"
    token_value="clauth=$profile"
else
    token_flag="--clear-token"
    token_value="clauth"
fi
# border_label on also names the account on the pane's border; off publishes
# the display-agent clear instead of leaving the stale label standing.
border_label=$(clauth herdr config get border_label 2>/dev/null || printf 'off')
# The pane id goes BEFORE the flags. `report-metadata --help` prints it last,
# and that order answers `unknown option: <value>` at exit 2 on 0.8.0. Named
# flags may sit in any order; only the positional-first order is load-bearing.
set -- "$pane" --source "${HERDR_PLUGIN_ID:-clauth}" "$token_flag" "$token_value"
if [ "$border_label" = on ]; then
    set -- "$@" --display-agent "$profile"
else
    set -- "$@" --clear-display-agent
fi
"$herdr_bin" pane report-metadata "$@"
[ "$pane_tag" = on ] || exit 0

# A --with-fallback session moves onto another account mid-run with no herdr
# event, so the one-shot report above goes stale until the next status change.
# Spawn a detached per-pane watcher to re-report on a timer instead. Only
# claude panes spend a clauth account; a plain shell pane resolves `agent`
# empty and is left alone. The pidfile makes later invocations skip the spawn
# while that watch lives, and the watcher removes it when the pane closes.
[ "$agent" = claude ] || exit 0
[ -n "$pane" ] || exit 0
state_dir="${HERDR_PLUGIN_STATE_DIR:-${TMPDIR:-/tmp}/clauth}"
mkdir -p "$state_dir" 2>/dev/null || exit 0
pidfile="$state_dir/watch-$pane.pid"
# Claim the pidfile atomically (noclobber): two hook runs firing together both
# reach the check-empty pidfile, so the create itself is the gate — the loser
# falls through to the liveness check and skips the spawn. A plain existence
# check + kill -0 races, and both runs would spawn a watcher.
if ! ( umask 077; set -C; echo "$$" > "$pidfile" ) 2>/dev/null; then
    [ -f "$pidfile" ] && kill -0 "$(cat "$pidfile" 2>/dev/null)" 2>/dev/null && exit 0
fi
dir=$(dirname "$0")
"$dir/watch-profile.sh" "$pane" "$pidfile" </dev/null >/dev/null 2>&1 &
