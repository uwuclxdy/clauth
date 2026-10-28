#!/bin/sh
# Runs tag-watch.lua once for the Rex server that loaded init.lua. init.lua
# starts this on every config load and reload, so a live watcher for the same
# server makes it a no-op. The watcher exits when its server goes away; the
# next server's config load starts a fresh one.
#
# Several Rex servers can run at once (one per installed version), so the
# watcher is pinned to the server that started us: our parent, whose
# `--listen` address names its socket. The pidfile is keyed on that address.
set -u

root=$(cd "$(dirname "$0")" && pwd)
state="${XDG_STATE_HOME:-$HOME/.local/state}/clauth-rex"
mkdir -p "$state" || exit 1
log="$state/watch.log"

# Rex runs this with a bare system PATH.
PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

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

server_args=$(proc_args "$PPID")
listen=$(printf '%s\n' "$server_args" | sed -n 's/.*--listen[ =]\(unix:[^ ]*\).*/\1/p')

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

# mkdir is atomic, so two loads racing on a stale pidfile start one watcher.
# A lock older than a minute was left by a killed run and is taken over.
if ! mkdir "$lock" 2>/dev/null; then
    if [ -n "$(find "$lock" -maxdepth 0 -mmin +1 2>/dev/null)" ]; then
        rmdir "$lock" 2>/dev/null
        mkdir "$lock" 2>/dev/null || exit 0
    else
        exit 0
    fi
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
