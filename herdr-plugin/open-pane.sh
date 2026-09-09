#!/bin/sh
# Opens this plugin's pane entrypoint in the shape the popup_width knob picks.
# herdr's popup is a session singleton, so a second open while one is up
# answers "popup already open"; someone pressing the same key twice is not an
# error, so the popup arms take that answer as exit 0. The split arms open
# real panes instead, which have no singleton and size by the pane grid, so
# they take no sizing flags and never swallow that answer.
#
# The popup_width knob picks the open's shape, failing safe to the shipped
# default (fit) when the clauth binary predates the subcommand: fit and half
# size the popup, split-right and split-top open a real pane beside or above
# the focused one. A herdr that refuses the sizing flags (measured
# 2026-08-26: 0.8.2 accepts them, hidden from --help) gets the plain call as
# a retry — popup arms only: retrying a split arm as the plain pair would
# open the entrypoint's manifest placement (a popup), silently abandoning the
# requested split.
#
# `set -e` is load-bearing here: every risky command sits in a condition or a
# `&&`/`||` chain, so a snapshot or open failure falls through to the fallback
# arms rather than aborting the open. report-profile.sh and watch-profile.sh
# deliberately use `set -u` only, because a failed publish must never kill a
# hook.
set -eu

entrypoint="${1:?usage: open-pane.sh <entrypoint-id>}"
herdr_bin="${HERDR_BIN_PATH:-herdr}"
plugin_id="${HERDR_PLUGIN_ID:-clauth}"

width_mode=$(clauth herdr config get popup_width 2>/dev/null || printf 'fit')

# Reads the snapshot into $snap and the focused pane id into $focused. The
# sed program is pinned against the real 0.8.2 snapshot shape by
# `the_fit_sed_reads_the_real_snapshot_shape`; it lives once here so the fit
# and split-top arms resolve the same line. A failed snapshot leaves both
# empty: the `|| snap=''` keeps a failing read from aborting the whole open
# under `set -e`.
read_focused_pane() {
    snap=$("$herdr_bin" api snapshot 2>/dev/null) || snap=''
    focused=$(printf '%s' "$snap" | sed -n 's/.*"focused_pane_id":"\([^"]*\)".*/\1/p')
}

# Whether the knob picked a split placement: splits open real panes, so the
# popup-singleton answer handling and the plain-pair retry below are
# popup-arm only.
split_open=false

# The open argv is the --plugin/--entrypoint pair plus the flags the knob
# picked: sizing flags for the popup arms, placement/target flags for the
# split arms.
set -- --plugin "$plugin_id" --entrypoint "$entrypoint"
case "$width_mode" in
    half)
        # No width flag: herdr's default half width. The height stays pinned.
        set -- "$@" --height 50%
        ;;
    split-right)
        # A real pane right of the focused pane; splits size by the pane
        # grid, so no width/height flags.
        set -- "$@" --placement split --direction right
        split_open=true
        ;;
    split-top)
        # A real pane directly above the focused one. herdr 0.8.2 splits only
        # right|down, so "above" splits the pane ABOVE the focused pane
        # downward: `pane neighbor --direction up` names that pane, and the
        # new pane lands between it and the focused pane. No neighbor (the
        # focused pane is topmost) splits the focused pane downward instead —
        # the new pane lands below it, but the knob keeps its split (a popup
        # fallback would abandon the knob). A failed snapshot skips the
        # target flag: herdr then splits the active pane downward, the same
        # below-the-focused shape.
        read_focused_pane
        target=''
        if [ -n "$focused" ]; then
            neighbor_out=$("$herdr_bin" pane neighbor --direction up --pane "$focused" 2>/dev/null) \
                || neighbor_out=''
            neighbor=$(printf '%s' "$neighbor_out" |
                sed -n 's/.*"neighbor_pane_id":"\([^"]*\)".*/\1/p')
            target="${neighbor:-$focused}"
        fi
        if [ -n "$target" ]; then
            set -- "$@" --target-pane "$target"
        fi
        set -- "$@" --placement split --direction down
        split_open=true
        ;;
    *)
        # fit — the shipped default, and where the retired `full` mode merged
        # (the owner's ruling: the two resolved identically below the 540-col
        # cap). Size against the focused pane's width. The snapshot names the
        # focused pane in `focused_pane_id`, and its layout row spells the
        # rect `{"height":H,"width":W,...}` on 0.8.2 (measured against the
        # real snapshot 2026-08-26; the pane records carry no rect, and the
        # layout rows put `pane_id` right before `rect`). Matching the pane
        # by id keeps the greedy prefix from landing on another tab's focused
        # row. A failed read leaves the flags off entirely, the pre-knob call
        # shape.
        read_focused_pane
        width=$(printf '%s' "$snap" |
            sed -n "s/.*\"pane_id\":\"$focused\",\"rect\":{\"height\":[0-9]*,\"width\":\([0-9]*\).*/\1/p")
        if [ -n "$width" ]; then
            if [ "$width" -ge 540 ]; then
                set -- "$@" --width 540 --height 50%
            else
                set -- "$@" --width 100% --height 50%
            fi
        fi
        ;;
esac

# One open attempt with the caller's argv, for the popup arms: exits 0 on
# success and on the singleton's "popup already open" answer; leaves the
# answer in $out and returns 1 for the caller to retry or log.
open_popup_attempt() {
    out=$("$herdr_bin" plugin pane open "$@" 2>&1) && return 0
    case "$out" in
        *"popup already open"*) return 0 ;;
    esac
    return 1
}

# The split arms' attempt: split panes have no singleton (every open makes a
# new pane), so success is the only exit 0. Leaves the answer in $out.
open_split_attempt() {
    out=$("$herdr_bin" plugin pane open "$@" 2>&1)
}

if [ "$split_open" = true ]; then
    if open_split_attempt "$@"; then
        exit 0
    fi
    # No plain-pair retry: anything past the four-word pair is a placement
    # flag an older herdr answers "unknown option" on, and retrying the pair
    # alone would open the entrypoint's manifest placement (a popup),
    # silently degrading a requested split. Fail the open instead.
    printf '%s\n' "$out" >&2
    exit 1
fi

if open_popup_attempt "$@"; then
    exit 0
fi
# The pair above is four words, so anything past it is a sizing flag an older
# herdr answers "unknown option" on. Retry the plain call once — still a
# popup, the entrypoint's manifest placement; its answer, success or failure,
# decides the exit.
if [ "$#" -gt 4 ]; then
    if open_popup_attempt --plugin "$plugin_id" --entrypoint "$entrypoint"; then
        exit 0
    fi
fi

printf '%s\n' "$out" >&2
exit 1
