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
        # ") " and the climb breaks off, so the pane falls back to `clauth
        # which` while that process lives.
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

# Prints the registry row owning $1 or one of its ancestors, empty if none.
# A `clauth mcp` hop is never matched: rows keyed on it belong to delegate runs
# the pane hosts, and matching one would name a delegate's account for the
# pane. The climb passes through it — the pane's own supervisor sits beyond.
session_row() {
    _pid=$1
    _depth=0
    while [ "${_pid:-0}" -gt 1 ] && [ "$_depth" -lt 8 ]; do
        _args=$(proc_args "$_pid")
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
        _pid=$(proc_ppid "$_pid")
        _depth=$((_depth + 1))
    done
    return 1
}

# The profile the operator's own codex login is adopted into, or nothing.
# `clauth login <name> --codex` leaves `$CODEX_HOME/auth.json` (default
# `~/.codex/auth.json`) a symlink onto `~/.clauth/profiles/<name>/auth.json`,
# so the link target names the profile; a login that was never adopted, or a
# host that could not repoint (a separate copy), spends no clauth account.
adopted_codex_profile() {
    _home="${CODEX_HOME:-$HOME/.codex}"
    # A dash-leading path reads as readlink options on GNU and as a filename
    # on BSD; refuse it rather than resolve either (readlink has no portable
    # `--`).
    case "$_home" in
        -*) return 0 ;;
    esac
    _link=$(readlink "$_home/auth.json" 2>/dev/null) || return 0
    case "$_link" in
        /*) ;;
        *) _link="$_home/$_link" ;;
    esac
    case "$_link" in
        "$HOME/.clauth/profiles/"*/auth.json)
            _p=${_link#"$HOME/.clauth/profiles/"}
            _p=${_p%/auth.json}
            case "$_p" in
                '' | */*) return 0 ;;
            esac
            printf '%s\n' "$_p"
            ;;
    esac
}

# The account a row names: the member a --with-fallback session swapped onto,
# else its launch member.
row_profile() {
    _row=$1
    _p=$(sed -n 's/.*"current_member":"\([^"]*\)".*/\1/p' "$_row")
    [ -n "$_p" ] || _p=$(sed -n 's/.*"start_profile":"\([^"]*\)".*/\1/p' "$_row")
    printf '%s\n' "$_p"
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
            _pp=$(proc_ppid "$pid")
            _pargs=$(proc_args "$_pp")
            case "$_pargs" in 'clauth mcp '* | 'clauth mcp') continue ;; esac
            row=$(session_row "$pid") || continue
            profile=$(row_profile "$row")
            [ -n "$profile" ] && break
        done
    fi
fi

# No clauth-managed session in this pane. A bare `claude` burns whatever owns
# the global credentials, so that answer is right rather than a guess. A bare
# `codex` burns the operator's own login, which is a clauth account only once
# adopted — and `clauth which` is never asked for it: a caller holding no
# CODEX_HOME gets the Claude Code answer there, a different harness's account.
definitive_empty=""
if [ -z "$agentless" ] && [ -z "$profile" ]; then
    if [ "$agent" = codex ]; then
        profile=$(adopted_codex_profile)
        # A codex pane with no adopted login is a DEFINITIVE empty resolution:
        # the pane spends no clauth account, so the empty side publishes the
        # clear and spawns the watcher below. The claude arm's empty is a
        # `clauth which` failure (`clauth which` prints `unknown` at exit 0
        # when nothing resolves), not a statement about the pane, and keeps
        # today's silent exit — whatever the caller.
        [ -z "$profile" ] && definitive_empty=1
    else
        profile=$(clauth which 2>/dev/null) || profile=""
    fi
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
