#!/bin/sh
# Publishes the clauth account a herdr pane burns as pane metadata, so a
# sidebar row can name it. Runs from the agent event hooks and from the
# `clauth.which` action, which is why it takes the pane from the injected
# context rather than an argument.
#
# herdr's process-info names the pane's own session in
# `foreground_process_group_id` — the `clauth start` supervisor for a
# clauth-started pane — and `clauth which --pid` joins that pid (or one of
# its ancestors) to the live-session registry, answering whose account the
# pane spends. Delegate sessions share the pane: they run as children of its
# `clauth mcp`, and the join skips an mcp hop, so the foreground chain
# resolves to the pane's own session first.
set -u

herdr_bin="${HERDR_BIN_PATH:-herdr}"
pane="${HERDR_PANE_ID:-}"

# The parent pid and the argv line of process $1, from /proc where there is
# one. `ps -o … -p <pid>` reads the whole process table to answer for a single
# pid: ~57 ms of CPU a call on a Linux host running ~1,200 processes, against
# ~2 ms for the /proc read (measured 2026-09-25), and every watcher tick of
# every pane makes several. macOS has no /proc, so ps answers there. The tests
# point CLAUTH_PROC_ROOT at a faked tree.
proc_root="${CLAUTH_PROC_ROOT:-/proc}"
proc_ppid() {
    if [ -d "$proc_root" ]; then
        # comm sits in parentheses and may itself hold ") ", so cut after the
        # LAST one: what follows is the state letter, then the ppid. A newline
        # inside comm spans stat across lines; the one-line read then finds no
        # ") " and the sweep breaks off with no row, as for a vanished pid.
        _stat=
        read -r _stat 2>/dev/null <"$proc_root/$1/stat" || return 0
        _stat=${_stat##*") "}
        # The state letter is one char, the ppid runs to the next space.
        _ppid=${_stat#? }
        printf '%s\n' "${_ppid%% *}"
    else
        ps -o ppid= -p "$1" 2>/dev/null | tr -d ' '
    fi
}
proc_args() {
    if [ -d "$proc_root" ]; then
        # NUL-separated argv, joined by spaces.
        _cmd=$(tr '\0\n' '  ' 2>/dev/null <"$proc_root/$1/cmdline") || return 0
        printf '%s\n' "${_cmd% }"
    else
        ps -o args= -p "$1" 2>/dev/null
    fi
}

# Reads a herdr pane JSON line's own `agent` field from stdin. The nested
# objects that carry their own `agent` key — a persisted session record
# (`agent_session`, whose `agent` field is always present), state labels, and
# metadata tokens — are cut off first, so the greedy match reads the pane's
# top-level `agent`, never a nested one; an absent top-level `agent` (an idle
# pane) reads empty.
pane_agent() {
    sed -n 's/"state_labels":.*//; s/"tokens":.*//; s/"agent_session":.*//; s/.*"agent":"\([^"]*\)".*/\1/p'
}

# The agent hooks fire for every agent herdr detects. Claude Code and codex
# panes spend a clauth account; cursor and the rest do not. Both hooked events
# name the agent when herdr has one; the watcher's re-report empties the event
# JSON on purpose and hands the live agent it just read from `pane get` in
# CLAUTH_PANE_AGENT, sparing a second `pane get`; the `clauth.which` action and
# the TUI knob push carry neither. The `"agent":"` anchor keeps the pane's
# `"display_agent":"` from matching.
if [ -n "$pane" ]; then
    # A pane's live agent outranks the event's: a release event names the agent
    # that just exited, while `pane get` names what runs now. The watcher's
    # CLAUTH_PANE_AGENT is its own live read a moment earlier, so it stands in
    # for a fresh one.
    agent="${CLAUTH_PANE_AGENT:-}"
    if [ -z "$agent" ]; then
        if raw=$("$herdr_bin" pane get "$pane" 2>/dev/null); then
            agent=$(printf '%s\n' "$raw" | pane_agent)
        else
            # A failed `pane get` is not a definitive "no agent": fall back to
            # the event's agent, and with none keep the last tag — publish
            # nothing, spawn nothing.
            agent=$(printf '%s' "${HERDR_PLUGIN_EVENT_JSON:-}" | pane_agent)
            [ -n "$agent" ] || exit 0
        fi
    fi
else
    agent=$(printf '%s' "${HERDR_PLUGIN_EVENT_JSON:-}" | pane_agent)
fi
# A pane whose live agent is neither claude nor codex (its live read names no
# agent, or another one) spends no clauth account. It publishes the matching
# clears and spawns no watcher, from every caller, like the watcher's own exit,
# so an idle shell pane stops showing an account instead of inheriting the
# claude-arm answer. Without a pane there is nothing to clear, and the claude
# arm below keeps its answer.
agentless=""
case "$agent" in
    claude | codex) ;;
    *)
        if [ -n "$pane" ]; then
            agentless=1
        fi
        ;;
esac

profile=""
if [ -z "$agentless" ] && [ -n "$pane" ]; then
    # A failed process-info could not read the pane either: keep the last tag,
    # like a failed `pane get`, rather than read it as "no account".
    info=$("$herdr_bin" pane process-info --pane "$pane" 2>/dev/null) || exit 0
    # The pane's own session is named by the foreground process group id herdr
    # reports — the `clauth start` supervisor for every clauth-started pane
    # (measured 2026-09-03 on 0.8.2). A delegate session runs in its parent
    # pane's process group with its row keyed on a process of that pane, so
    # the foreground chain must resolve FIRST or a delegate's account shadows
    # the pane's own.
    fg_pid=$(printf '%s' "$info" | sed -n 's/.*"foreground_process_group_id":\([0-9]*\).*/\1/p')
    # A `clauth which --pid` that exits nonzero could not read, never "nothing
    # resolved": the plugin is installed at the newest release tag whatever the
    # binary's version, so a binary predating `--pid` answers exit 2 here. Like
    # a failed `pane get`, that keeps the last tag — publish nothing, spawn
    # nothing. An exit-0 JSON answer with a null profile is the definitive
    # unresolved one.
    if [ -n "$fg_pid" ]; then
        if answer=$(clauth which --pid "$fg_pid" --json 2>/dev/null); then
            _p=$(printf '%s' "$answer" | sed -n 's/.*"profile":"\([^"]*\)".*/\1/p')
            _harness=$(printf '%s' "$answer" | sed -n 's/.*"harness":"\([^"]*\)".*/\1/p')
            # Another harness's account is never this pane's: a codex pane
            # reading the Claude Code global login gets no clauth token.
            if [ -n "$_p" ] && [ "$_harness" = "$agent" ]; then
                profile=$_p
            fi
        else
            exit 0
        fi
    fi
    # The pid sweep is the compat path for a process-info without the
    # foreground field. When the field is there, the foreground answer above
    # stands — sweeping would hand a bare `claude` pane the account of a
    # delegate it happens to host. The sweep skips a pid whose parent is
    # `clauth mcp`: a delegate child, whose row is keyed on the child itself,
    # and which names a run the pane hosts, never the pane. A row answer wins
    # over a bare login's; the bare answer is kept only while no pid gives one.
    # A call that exits nonzero is skipped, and a sweep whose every call failed
    # could not read either: publish nothing, as above.
    if [ -z "$profile" ] && [ -z "$fg_pid" ]; then
        fallback=""
        asked=0
        answered=0
        pids=$(printf '%s' "$info" | grep -o '"pid":[0-9]*' | cut -d: -f2)
        for pid in $pids; do
            _pp=$(proc_ppid "$pid")
            _pargs=$(proc_args "$_pp")
            case "$_pargs" in 'clauth mcp '* | 'clauth mcp' | */clauth' mcp '* | */clauth' mcp') continue ;; esac
            asked=$((asked + 1))
            _answer=$(clauth which --pid "$pid" --json 2>/dev/null) || continue
            answered=$((answered + 1))
            _source=$(printf '%s' "$_answer" | sed -n 's/.*"source":"\([^"]*\)".*/\1/p')
            _p=$(printf '%s' "$_answer" | sed -n 's/.*"profile":"\([^"]*\)".*/\1/p')
            if [ "$_source" = live_session ] && [ -n "$_p" ]; then
                profile=$_p
                break
            fi
            if [ -z "$fallback" ] && [ -n "$_p" ]; then
                _harness=$(printf '%s' "$_answer" | sed -n 's/.*"harness":"\([^"]*\)".*/\1/p')
                [ "$_harness" = "$agent" ] && fallback=$_p
            fi
        done
        [ -n "$profile" ] || profile=$fallback
        if [ -z "$profile" ] && [ "$asked" -gt 0 ] && [ "$answered" -eq 0 ]; then
            exit 0
        fi
    fi
fi

# A codex pane with nothing resolved is a DEFINITIVE empty: a bare codex burns
# the operator's own login, a clauth account only once adopted, so no adopted
# login means the pane spends no clauth account. The empty side then publishes
# the clear and spawns the watcher below. The claude arm's empty keeps today's
# silent exit, whatever the caller.
definitive_empty=""
if [ -z "$agentless" ] && [ -z "$profile" ] && [ "$agent" = codex ]; then
    definitive_empty=1
fi
[ -n "$profile" ] || { [ -n "$definitive_empty" ] && [ -n "$pane" ]; } || [ -n "$agentless" ] || exit 0

[ -z "$profile" ] || printf '%s\n' "$profile"

[ -n "$pane" ] || exit 0
# An agentless pane clears both artifacts in every knob combination, so publish
# the clears directly and skip the clauth knob reads below — the watcher's exit
# clear (`watch-profile.sh`) does the same.
if [ -n "$agentless" ]; then
    "$herdr_bin" pane report-metadata "$pane" --source "${HERDR_PLUGIN_ID:-clauth}" --clear-token clauth --clear-display-agent
    exit 0
fi
# Each knob owns one artifact, and its off side publishes the matching clear
# instead of nothing: a knob toggled off, or an empty resolution, must not
# leave its stale artifact standing on the pane. pane_tag still gates the
# watcher spawn below, while the resolve above prints either way.
pane_tag=$(clauth herdr config get pane_tag 2>/dev/null || printf 'on')
if [ -n "$profile" ] && [ "$pane_tag" = on ]; then
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
if [ -n "$profile" ] && [ "$border_label" = on ]; then
    set -- "$@" --display-agent "$profile"
else
    set -- "$@" --clear-display-agent
fi
"$herdr_bin" pane report-metadata "$@"
[ "$pane_tag" = on ] || exit 0

# A --with-fallback session moves onto another account mid-run with no herdr
# event, so the one-shot report above goes stale until the next status change.
# Spawn a detached per-pane watcher to re-report on a timer instead. Only
# claude and codex panes spend a clauth account; an agentless pane publishes
# its clear above and is left watcher-less. The pidfile makes later invocations
# skip the spawn while that watch lives, and the watcher removes it when it
# exits: the pane closed, or it runs neither claude nor codex.
case "$agent" in
    claude | codex) ;;
    *) exit 0 ;;
esac
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
