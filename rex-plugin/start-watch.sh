#!/bin/sh
# Runs tag-watch.lua once for the Rex server that loaded init.lua. init.lua
# starts this on every config load and reload, so a live watcher for the same
# server makes it a no-op. The watcher exits when its server goes away; the
# next server's config load starts a fresh one. `clauth rex install` and
# `clauth rex uninstall` stop it through its pidfile, so a reinstall's reload
# starts one on the new files; keep the pidfile's name and contents as they are.
#
# Several Rex servers can run at once (one per installed version), so the
# watcher is pinned to the server that started us: our parent, whose
# `--listen` address names its socket. The pidfile is keyed on that address.
set -u

root=$(cd "$(dirname "$0")" && pwd)
state="${XDG_STATE_HOME:-$HOME/.local/state}/clauth-rex"
mkdir -p "$state" || exit 1
log="$state/watch.log"

# Rex runs this with a bare system PATH. The usual install dirs go after it,
# so a clauth or rex the server's own PATH names still wins.
PATH="${PATH:+$PATH:}$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin"

proc_args() {
    if [ -r "/proc/$1/cmdline" ]; then
        tr '\0' ' ' <"/proc/$1/cmdline"
    else
        ps -o args= -p "$1" 2>/dev/null
    fi
}

# The server's own binary: macOS `ps -o comm=` prints the full path, Linux
# needs /proc/<pid>/exe. The `rex` beside it is the CLI that matches it.
server_bin() {
    if [ -r "/proc/$1/exe" ]; then
        readlink "/proc/$1/exe"
    else
        ps -o comm= -p "$1" 2>/dev/null
    fi
}

# The server's --listen value, whatever its form: a `unix://` URL on macOS, a
# bare socket path such as Linux's shared /run/rex/server.sock. /proc keeps
# each argument whole; ps joins them with spaces, so there the value runs to
# the next flag, which keeps a path with a space in it.
listen_of() {
    if [ -r "/proc/$1/cmdline" ]; then
        tr '\0' '\n' <"/proc/$1/cmdline" | awk '
            take { print; exit }
            $0 == "--listen" { take = 1; next }
            sub(/^--listen=/, "") { print; exit }'
    else
        ps -o args= -p "$1" 2>/dev/null | awk '
            match($0, / --listen[ =]/) {
                v = substr($0, RSTART + RLENGTH)
                sub(/ --.*/, "", v)
                print v
            }'
    fi
}

listen=$(listen_of "$PPID")

rex_bin="${CLAUTH_REX_BIN:-}"
if [ -z "$rex_bin" ]; then
    server=$(server_bin "$PPID")
    case "$server" in
        /*) [ -x "$(dirname "$server")/rex" ] && rex_bin="$(dirname "$server")/rex" ;;
    esac
fi
[ -n "$rex_bin" ] || rex_bin=$(command -v rex) || {
    echo "clauth-rex: no rex CLI found" >>"$log"
    exit 1
}

key=$(printf '%s' "${listen:-default}" | cksum | cut -d' ' -f1)
pidfile="$state/watch-$key.pid"
lock="$state/watch-$key.lock"

# A pid only counts as live when it is still our watcher: pids get reused.
watcher_alive() {
    [ -n "$1" ] || return 1
    case "$(proc_args "$1")" in
        *tag-watch.lua*) return 0 ;;
    esac
    return 1
}

stale() {
    [ -n "$(find "$1" -maxdepth 0 -mmin +1 2>/dev/null)" ]
}

# mkdir is atomic, so two loads racing on a stale pidfile start one watcher.
# A lock older than a minute was left by a killed run and is taken over. The
# takeover renames it first: of two loads that both find it stale, only one
# rename succeeds, and the other backs off. A rename that caught a fresh lock
# instead (a third load took it meanwhile) puts a lock back for that load,
# whose exit removes it, and backs off too.
if ! mkdir "$lock" 2>/dev/null; then
    stale "$lock" || exit 0
    mv "$lock" "$lock.$$" 2>/dev/null || exit 0
    if ! stale "$lock.$$"; then
        mkdir "$lock" 2>/dev/null
        rmdir "$lock.$$" 2>/dev/null
        exit 0
    fi
    rmdir "$lock.$$" 2>/dev/null
    mkdir "$lock" 2>/dev/null || exit 0
fi
trap 'rmdir "$lock" 2>/dev/null' EXIT

if watcher_alive "$(cat "$pidfile" 2>/dev/null)"; then
    exit 0
fi

# Keep the log from growing without bound.
[ -f "$log" ] && [ "$(wc -c <"$log")" -gt 1048576 ] && mv "$log" "$log.1"
printf '%s start-watch: %s %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$rex_bin" \
    "${listen:-default server}" >>"$log"

set -- "do" --all --autostart=false "$root/tag-watch.lua"
[ -n "$listen" ] && set -- --server "$listen" "$@"
"$rex_bin" "$@" </dev/null >>"$log" 2>&1 &
echo "$!" >"$pidfile"
