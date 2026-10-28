#!/bin/sh
# Prints the clauth account a Rex terminal's foreground process spends, as
# "<profile><TAB><how>", or nothing when that process is neither Claude Code
# nor codex. <how> is `clauth` for a session clauth launched, `global` for a
# bare `claude`, which burns whatever owns the global credentials, and `codex`
# for a codex whose login clauth adopted.
#
# $1 is the foreground pid Rex reports for the terminal. For a pane started
# with `clauth start` that is the clauth supervisor, and the live-session
# registry keys its row on that pid, so the walk up the parent chain is the
# join. This mirrors the herdr plugin's report-profile.sh; change them together.
set -u

# Rex runs this with a bare system PATH.
PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
sessions_dir="$HOME/.clauth/live_sessions"
pid="${1:?usage: resolve-profile.sh <pid>}"

# The parent pid and the argv line of process $1, from /proc where there is
# one. `ps -o … -p <pid>` reads the whole process table to answer for one pid
# (~57 ms against ~2 ms for the /proc read on a busy Linux host, measured by
# the herdr plugin), and the watcher asks on every agent process change. macOS
# has no /proc, so ps answers there. Tests point CLAUTH_PROC_ROOT at a fake tree.
proc_root="${CLAUTH_PROC_ROOT:-/proc}"
proc_ppid() {
    if [ -d "$proc_root" ]; then
        # comm sits in parentheses and may itself hold ") ", so cut after the
        # LAST one: what follows is the state letter, then the ppid.
        _stat=
        read -r _stat 2>/dev/null <"$proc_root/$1/stat" || return 0
        _stat=${_stat##*") "}
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

# Only Claude Code and codex panes spend an account. Match on the program's
# own name, never anywhere in its arguments: paths under ~/.codex/worktrees
# would make every command run there look like codex. The clauth dashboard and
# other clauth subcommands run as `clauth` too, so match the subcommand.
args=$(proc_args "$pid")
set -f
# shellcheck disable=SC2086 # split the argv line into words on purpose
set -- $args
set +f
program=${1:-}
case "$program" in
    # Claude Code's real binary is named for its version.
    */claude/versions/*) program=claude ;;
    *) program=${program##*/} ;;
esac
case "$program" in
    clauth)
        case "${2:-}" in
            start | resume) kind=clauth ;;
            *) exit 0 ;;
        esac
        ;;
    claude) kind=claude ;;
    codex) kind=codex ;;
    *) exit 0 ;;
esac

# The registry row owning $1 or one of its ancestors. A `clauth mcp` hop is
# never matched: rows keyed on it belong to delegate runs the pane hosts.
session_row() {
    _pid=$1
    _depth=0
    while [ "${_pid:-0}" -gt 1 ] && [ "$_depth" -lt 8 ]; do
        case "$(proc_args "$_pid")" in
            'clauth mcp '* | 'clauth mcp' | */clauth\ mcp | */clauth\ mcp\ *) : ;;
            *)
                # The (,|}) boundary keeps pid 123 from matching 1234 without
                # pinning `pid` to its slot in the row. Newest row wins: a
                # recycled pid can match a stale row and a live one at once.
                _matches=$(grep -lE "\"pid\":$_pid(,|})" "$sessions_dir"/*.json 2>/dev/null)
                if [ -n "$_matches" ]; then
                    printf '%s\n' "$_matches" | xargs ls -td 2>/dev/null | head -n 1
                    return 0
                fi
                ;;
        esac
        _pid=$(proc_ppid "$_pid")
        _depth=$((_depth + 1))
    done
    return 1
}

# A --with-fallback session names the member it swapped onto in current_member.
row=$(session_row "$pid") || row=""
if [ -n "$row" ]; then
    profile=$(sed -n 's/.*"current_member":"\([^"]*\)".*/\1/p' "$row")
    [ -n "$profile" ] || profile=$(sed -n 's/.*"start_profile":"\([^"]*\)".*/\1/p' "$row")
    [ -n "$profile" ] && printf '%s\tclauth\n' "$profile"
    exit 0
fi

# The profile the operator's own codex login is adopted into, or nothing.
# `clauth login <name> --codex` leaves `$CODEX_HOME/auth.json` a symlink onto
# `~/.clauth/profiles/<name>/auth.json`; a login never adopted spends no
# clauth account. Mirrors adopted_codex_profile in the herdr plugin.
adopted_codex_profile() {
    _home="${CODEX_HOME:-$HOME/.codex}"
    case "$_home" in -*) return 0 ;; esac
    _link=$(readlink "$_home/auth.json" 2>/dev/null) || return 0
    case "$_link" in
        /*) ;;
        *) _link="$_home/$_link" ;;
    esac
    case "$_link" in
        "$HOME/.clauth/profiles/"*/auth.json)
            _p=${_link#"$HOME/.clauth/profiles/"}
            _p=${_p%/auth.json}
            case "$_p" in '' | */*) return 0 ;; esac
            printf '%s\n' "$_p"
            ;;
    esac
}

case "$kind" in
    # No row yet for a clauth launch is a startup race; the watcher asks again.
    clauth) exit 0 ;;
    codex)
        profile=$(adopted_codex_profile)
        [ -n "$profile" ] && printf '%s\tcodex\n' "$profile"
        exit 0
        ;;
esac
profile=$(clauth which 2>/dev/null) || exit 0
case "$profile" in '' | unknown) exit 0 ;; esac
printf '%s\tglobal\n' "$profile"
