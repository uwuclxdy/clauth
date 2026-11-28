//! The `held question dot` override: what each pass writes into herdr's
//! Claude detection override, what it reloads, and what it undoes. The
//! subprocess half runs against a herdr shim (`SHIM`), never a real herdr: it
//! logs every call, answers the four commands the passes run, and reads its
//! verdicts off marker files a test drops beside it.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use super::*;
use crate::profile::{AppState, HerdrSettings, save_app_state};
use crate::testutil::{EnvPin, HomeSandbox, write_shim};

/// herdr's cached remote Claude manifest, cut down to the keys `read_cache`
/// and herdr's loader read.
pub(crate) const CACHE_V1: &str = r#"id = "claude"
version = "2026.10.11.1"
min_engine_version = 2

[[rules]]
id = "osc_title_working"
state = "working"
priority = 1100
region = "osc_title"
regex = ['^[\x{2800}-\x{28FF}\x{25D0}-\x{25D3}] ']
"#;

/// The same cache at a newer version, its rules moved too.
pub(crate) const CACHE_V2: &str = r#"id = "claude"
version = "2026.11.30.2"
min_engine_version = 2

[[rules]]
id = "osc_title_working"
state = "working"
priority = 1101
region = "osc_title"
regex = ['^[\x{2800}-\x{28FF}\x{25D0}-\x{25D3}] ']
"#;

/// clauth's own file's first line, as written.
pub(crate) const HEADER: &str = "# Written by clauth for the Services -> herdr `held question dot` toggle: herdr's cached remote Claude manifest, then clauth's rule. clauth rebuilds it when herdr caches a newer manifest and deletes it when the toggle goes off, unless you edit it.\n";

/// The block clauth appends, the rule's bytes as the 2026-11-12 probe
/// measured them in `box-override-2026-11-12.toml`.
pub(crate) const BLOCK: &str = concat!(
    "\n",
    "# >>> clauth held question dot: clauth removes this block when the toggle goes off\n",
    "[[rules]]\n",
    "id = \"clauth_held_ask\"\n",
    "state = \"blocked\"\n",
    "priority = 1150\n",
    "region = \"last_non_empty_above_prompt_box\"\n",
    "visible_blocker = true\n",
    r"line_regex = ['^\s*(?:Allow this\? Answer here or on your phone\.\s+1: Allow\s+2: Deny|Answer on your phone, or here\.\s+1: Answer here\s+Esc cancels)\s*$']",
    "\n",
    "# <<< clauth held question dot\n",
);

/// A user's own override with no clauth rule, ending without a newline.
const USER_OVERRIDE: &str = r#"id = "claude"
version = "2026.10.11.1"
min_engine_version = 2

# my own tweak
[[rules]]
id = "live_blocked_form_bordered"
state = "blocked"
priority = 979
region = "prompt_box_body"
contains = ["esc to cancel"]"#;

/// A user's override already holding a hand-kept `clauth_held_ask` rule (no
/// clauth marker).
const HAND_KEPT: &str = r#"id = "claude"
version = "2026.10.11.1"
min_engine_version = 2

[[rules]]
id = "clauth_held_ask"
state = "blocked"
priority = 1150
region = "last_non_empty_above_prompt_box"
line_regex = ['^\s*Allow this\?.*$']
"#;

/// The band's line 2 in both shapes, as the probe captured them
/// (`q1-hold-screen.txt`, `question-band.txt`, 2026-11-12).
const CAPTURED_DECISION: &str = "  Allow this? Answer here or on your phone.   1: Allow   2: Deny";
const CAPTURED_QUESTION: &str = "  Answer on your phone, or here.   1: Answer here   Esc cancels";

/// The herdr shim. `$0`'s directory holds its log, its verdict files and an
/// optional `appdir` naming herdr's app dir (`herdr` by default, `herdr-dev`
/// for a debug build): `sessions.json` (the `session list` answer; none = no
/// server), `loader_rejects` (the loader check answers a source other than
/// the candidate), `loader_errors` (the loader check prints a verdict and
/// exits 1), `loader_saves` / `reload_saves` (the loader check / a reload
/// appends a line to the real override, a user's save landing in the pass's
/// window), `reload_fails` (every reload exits 1), `server_rejects` (a
/// reload reports claude served from the remote manifest), `config_dir_fails`
/// (the `plugin config-dir` call answers nothing and exits 1),
/// `config_dir_relative` (it prints a root relative to herdr's own cwd, as a
/// relative `XDG_CONFIG_HOME` makes it). The loader check runs with the env
/// cleared, so that branch uses builtins only.
pub(crate) const SHIM: &str = r#"d=${0%/*}
app=herdr
if [ -f "$d/appdir" ]; then read -r app < "$d/appdir"; fi
echo "$*" >> "$d/herdr.log"
case "$1 $2" in
  "plugin config-dir")
    if [ -f "$d/config_dir_fails" ]; then printf 'cannot read the config dir\n' >&2; exit 1; fi
    if [ -f "$d/config_dir_relative" ]; then printf '%s\n' 'rel/herdr/plugins/config/clauth'; exit 0; fi
    printf '%s\n' "$d/$app/plugins/config/clauth"; exit 0 ;;
  "session list")
    if [ -f "$d/sessions.json" ]; then cat "$d/sessions.json"; else printf '%s\n' '{"sessions":[]}'; fi
    exit 0 ;;
  "agent explain")
    if [ -f "$d/loader_errors" ]; then printf '%s\n' '{"manifest_source":"bundled"}'; exit 1; fi
    if [ -f "$d/loader_saves" ] && [ -f "$d/$app/agent-detection/claude.toml" ]; then
      printf '\n# the user saved this while the pass ran\n' >> "$d/$app/agent-detection/claude.toml"
    fi
    if [ -f "$d/loader_rejects" ]; then printf '%s\n' '{"manifest_source":"bundled"}'
    else printf '{"manifest_source":"%s"}\n' "$XDG_CONFIG_HOME/$app/agent-detection/claude.toml"; fi
    exit 0 ;;
esac
if [ "$1" = "--session" ] && [ "$3 $4" = "server reload-agent-manifests" ]; then
  [ -f "$d/reload_fails" ] && exit 1
  if [ -f "$d/reload_saves" ] && [ -f "$d/$app/agent-detection/claude.toml" ]; then
    printf '\n# the user saved this during the reload\n' >> "$d/$app/agent-detection/claude.toml"
  fi
  kind=remote
  if [ -f "$d/$app/agent-detection/claude.toml" ] && [ ! -f "$d/server_rejects" ]; then kind="local override"; fi
  printf '{"id":"cli:server:reload-agent-manifests","result":{"type":"agent_manifest_reload","manifests":[{"agent":"claude","source":"x","source_kind":"%s","local_override_shadowing_remote":false}]}}\n' "$kind"
  exit 0
fi
exit 2"#;

/// One running default server, as `herdr session list --json` prints it.
pub(crate) const ONE_SERVER: &str = r#"{"sessions":[{"name":"default","default":true,"running":true,"socket_path":"/s/herdr.sock","session_dir":"/s"}]}"#;

struct Fixture {
    home: HomeSandbox,
    dir: PathBuf,
    /// herdr's cached remote manifest for the shim's app dir.
    remote: PathBuf,
    ctx: Ctx,
}

impl Fixture {
    /// A sandbox with the shim and one running server; no cache, no override.
    fn new() -> Self {
        let home = HomeSandbox::new();
        let dir = home.home().join("shim");
        std::fs::create_dir_all(&dir).unwrap();
        // The shim's own file must not sit where herdr's config root does.
        let bin = write_shim(&dir, "herdr-shim", SHIM);
        std::fs::write(dir.join("sessions.json"), ONE_SERVER).unwrap();
        let remote = remote_path(home.home());
        let ctx = Ctx {
            bin,
            clauth_dir: home.home().join(".clauth"),
        };
        Self {
            home,
            dir,
            remote,
            ctx,
        }
    }

    fn override_path(&self) -> PathBuf {
        self.dir.join("herdr/agent-detection/claude.toml")
    }

    /// The shim answers as a debug herdr (`herdr-dev`) under this dir.
    fn debug_build(&self) {
        std::fs::write(self.dir.join("appdir"), "herdr-dev\n").unwrap();
    }

    fn write_cache(&self, text: &str) {
        let path = self.remote.clone();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// herdr's cache under another app dir, a debug build's `herdr-dev`.
    fn write_cache_as(&self, app: &str, text: &str) {
        let path = self.home.home().join(format!(
            ".local/state/{app}/agent-detection/remote/claude.toml"
        ));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn write_override(&self, text: &str) {
        let path = self.override_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn read_override(&self) -> Option<String> {
        std::fs::read_to_string(self.override_path()).ok()
    }

    fn flag(&self, name: &str) {
        std::fs::write(self.dir.join(name), "").unwrap();
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("herdr.log")).unwrap_or_default()
    }

    fn clear_log(&self) {
        let log = self.dir.join("herdr.log");
        if log.exists() {
            std::fs::remove_file(log).unwrap();
        }
    }

    fn reloads(&self) -> usize {
        self.log()
            .lines()
            .filter(|l| *l == "--session default server reload-agent-manifests")
            .count()
    }

    fn record(&self) -> Option<Record> {
        load_record(&self.ctx.clauth_dir.join(RECORD_FILE))
    }

    fn card(&self) -> Vec<String> {
        self.record().map_or_else(Vec::new, |r| card_lines(&r))
    }

    /// Run one pass with the toggle persisted at `on`: the pass reads the
    /// toggle from `profiles.toml`, as the production triggers do.
    fn run(&self, on: bool) -> Result<()> {
        save_app_state(&on_state(on)).unwrap();
        pass(&self.ctx)
    }
}

/// Where the env mirror puts herdr's cache under a sandbox home with
/// `XDG_STATE_HOME` unset.
fn remote_path(home: &Path) -> PathBuf {
    home.join(".local/state/herdr/agent-detection/remote/claude.toml")
}

fn strings(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|l| (*l).to_string()).collect()
}

/// An app state with the `held question dot` toggle at `on`: what a pass that
/// reads the persisted toggle sees.
fn on_state(on: bool) -> AppState {
    AppState {
        herdr: HerdrSettings {
            held_question_dot: on,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// The record file under a sandbox `~/.clauth`, as the pass and the due check
/// name it, for tests outside this module.
pub(crate) fn record_path(clauth_dir: &Path) -> PathBuf {
    clauth_dir.join(RECORD_FILE)
}

/// The `clauth_held_ask` rule of a parsed override.
fn held_ask_rule(text: &str) -> toml::Table {
    let table: toml::Table = text.parse().expect("the override parses as TOML");
    let rules = table["rules"].as_array().expect("rules array");
    let mut found = rules
        .iter()
        .filter(|r| r.get("id").and_then(toml::Value::as_str) == Some("clauth_held_ask"));
    let rule = found.next().expect("one clauth_held_ask rule").clone();
    assert!(found.next().is_none(), "exactly one clauth_held_ask rule");
    rule.as_table().unwrap().clone()
}

// ── check 1: on with no override ────────────────────────────────────────────

#[test]
fn on_with_no_override_writes_the_cache_then_one_marked_rule() {
    let fx = Fixture::new();
    fx.write_cache(CACHE_V1);

    fx.run(true).unwrap();

    let written = fx.read_override().expect("clauth's file appears");
    assert_eq!(written, format!("{HEADER}{CACHE_V1}{BLOCK}"));
    assert!(
        written.starts_with(HEADER),
        "marked as clauth's in its own bytes"
    );
    let rule = held_ask_rule(&written);
    assert_eq!(rule["state"].as_str(), Some("blocked"));
    assert_eq!(rule["priority"].as_integer(), Some(1150));
    assert_eq!(
        rule["region"].as_str(),
        Some("last_non_empty_above_prompt_box")
    );
    assert_eq!(rule["visible_blocker"].as_bool(), Some(true));
    let regex = rule["line_regex"].as_array().unwrap()[0].as_str().unwrap();
    let regex = regex::Regex::new(regex).expect("the line_regex compiles");
    assert!(
        regex.is_match(CAPTURED_DECISION),
        "the permission band shape"
    );
    assert!(regex.is_match(CAPTURED_QUESTION), "the question band shape");
    assert_eq!(
        fx.card(),
        strings(&["detection: herdr manifest 2026.10.11.1 + clauth rule"])
    );
    assert_eq!(fx.reloads(), 1, "the write is applied by one reload");
}

/// The rule reads only the band's whole line: a quoted copy with text before
/// or after it on its line does not match (MB-11's anchoring).
#[test]
fn the_rule_matches_only_the_whole_band_line() {
    let rule = held_ask_rule(&format!("{CACHE_V1}{BLOCK}"));
    let regex =
        regex::Regex::new(rule["line_regex"].as_array().unwrap()[0].as_str().unwrap()).unwrap();
    assert!(!regex.is_match(&format!("● quoted:{CAPTURED_DECISION}")));
    assert!(!regex.is_match(&format!("{CAPTURED_DECISION} and more")));
    assert!(!regex.is_match("  Allow this? Answer here or on your phone."));
}

/// No cache and no override: nothing is written and nothing reloaded; the
/// card says what it waits for, and once herdr caches a manifest the next
/// pass applies the rule and the card shows check 1's row.
#[test]
fn on_with_neither_cache_nor_override_shows_the_waiting_row() {
    let fx = Fixture::new();
    fx.run(true).unwrap();
    assert_eq!(fx.read_override(), None);
    assert_eq!(fx.reloads(), 0);
    assert_eq!(
        fx.card(),
        strings(&["detection: waiting for herdr to download its claude manifest"])
    );
    assert_eq!(fx.record().unwrap().held, Held::Unapplied);

    fx.write_cache(CACHE_V1);
    let record = fx.record();
    let cache = || read_cache(&fx.remote).map(|c| c.version);
    assert!(due(true, record.as_ref(), cache), "the first cache is due");
    fx.run(true).unwrap();
    assert_eq!(
        fx.read_override().unwrap(),
        format!("{HEADER}{CACHE_V1}{BLOCK}")
    );
    assert_eq!(
        fx.card(),
        strings(&["detection: herdr manifest 2026.10.11.1 + clauth rule"])
    );
}

// ── check 3: a moved cache version rebuilds; an unmoved one does nothing ────

#[test]
fn a_moved_cache_version_rebuilds_clauths_file_and_reloads() {
    let fx = Fixture::new();
    fx.write_cache(CACHE_V1);
    fx.run(true).unwrap();
    fx.write_cache(CACHE_V2);
    fx.clear_log();

    let record = fx.record();
    let cache = || read_cache(&fx.remote).map(|c| c.version);
    assert!(
        due(true, record.as_ref(), cache),
        "the moved version is due"
    );
    fx.run(true).unwrap();

    assert_eq!(
        fx.read_override().unwrap(),
        format!("{HEADER}{CACHE_V2}{BLOCK}")
    );
    assert_eq!(fx.reloads(), 1);
    assert_eq!(
        fx.card(),
        strings(&["detection: herdr manifest 2026.11.30.2 + clauth rule"])
    );
}

/// A cache that itself carries clauth's block makes clauth's own file hold
/// two blocks; the record's digest proves the file clauth's, so the pass
/// rebuilds it and off deletes it whatever the block count.
#[test]
fn a_cache_that_carries_clauths_block_still_leaves_clauths_file_clauths_own() {
    let fx = Fixture::new();
    fx.write_cache(&format!("{CACHE_V1}{BLOCK}"));

    fx.run(true).unwrap();
    let written = format!("{HEADER}{CACHE_V1}{BLOCK}{BLOCK}");
    assert_eq!(fx.read_override().expect("clauth's file appears"), written);
    assert_eq!(
        fx.card(),
        strings(&["detection: herdr manifest 2026.10.11.1 + clauth rule"])
    );

    fx.run(true).unwrap();
    assert_eq!(
        fx.read_override().unwrap(),
        written,
        "a second pass keeps it"
    );

    fx.run(false).unwrap();
    assert_eq!(fx.read_override(), None, "off deletes clauth's own file");
    assert_eq!(fx.record(), None);
}

/// The tick's own entry: env-resolved herdr (`HERDR_BIN_PATH`) and state
/// dir (`XDG_STATE_HOME`). A moved version runs one pass; the next call with
/// the version unmoved runs no subprocess and writes nothing.
#[test]
fn the_watch_rebuilds_on_a_moved_version_and_is_silent_otherwise() {
    let mut fx = Fixture::new();
    let state = fx.home.home().join("xdg-state");
    fx.remote = state.join("herdr/agent-detection/remote/claude.toml");
    let _env = EnvPin::new(
        &fx.home,
        &[
            ("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str())),
            ("XDG_STATE_HOME", Some(state.as_os_str())),
        ],
    );
    save_app_state(&on_state(true)).unwrap();
    fx.write_cache(CACHE_V1);
    fx.run(true).unwrap();
    fx.write_cache(CACHE_V2);
    fx.clear_log();

    watch_detached(true);
    assert_eq!(
        fx.read_override().unwrap(),
        format!("{HEADER}{CACHE_V2}{BLOCK}")
    );
    assert_eq!(fx.reloads(), 1);

    fx.clear_log();
    let before = std::fs::metadata(fx.override_path())
        .unwrap()
        .modified()
        .unwrap();
    watch_detached(true);
    assert_eq!(fx.log(), "", "an unmoved version starts no subprocess");
    assert_eq!(
        std::fs::metadata(fx.override_path())
            .unwrap()
            .modified()
            .unwrap(),
        before
    );
}

// ── check 4: on over a user override with no clauth rule ───────────────────

#[test]
fn on_over_a_user_override_appends_one_block_and_is_idempotent() {
    let fx = Fixture::new();
    fx.write_cache(CACHE_V1);
    fx.write_override(USER_OVERRIDE);

    fx.run(true).unwrap();
    let after = fx.read_override().unwrap();
    assert_eq!(after, format!("{USER_OVERRIDE}{BLOCK}"));
    assert_eq!(fx.reloads(), 1);
    assert_eq!(
        fx.card(),
        strings(&[
            "detection: your override + clauth rule",
            "  your override hides herdr's manifest updates",
        ])
    );

    fx.clear_log();
    fx.run(true).unwrap();
    assert_eq!(
        fx.read_override().unwrap(),
        after,
        "on again changes nothing"
    );
    fx.write_cache(CACHE_V2);
    fx.run(true).unwrap();
    assert_eq!(
        fx.read_override().unwrap(),
        after,
        "a moved cache never rebuilds a user's file"
    );
    assert_eq!(fx.reloads(), 0, "no change, no reload");
}

// ── check 5: on over a user override that already has the rule ─────────────

#[test]
fn on_over_a_hand_kept_rule_leaves_the_file_and_reloads_nothing() {
    let fx = Fixture::new();
    fx.write_cache(CACHE_V1);
    fx.write_override(HAND_KEPT);

    fx.run(true).unwrap();
    assert_eq!(fx.read_override().unwrap(), HAND_KEPT);
    assert_eq!(fx.log().matches("reload-agent-manifests").count(), 0);
    assert_eq!(
        fx.card(),
        strings(&[
            "detection: your override, which already has the rule",
            "  a hand-kept rule won't follow clauth's band changes",
        ])
    );
}

// ── check 6: off undoes exactly what clauth wrote ──────────────────────────

#[test]
fn off_deletes_clauths_own_file_and_reloads() {
    let fx = Fixture::new();
    fx.write_cache(CACHE_V1);
    fx.run(true).unwrap();
    fx.clear_log();

    fx.run(false).unwrap();
    assert_eq!(fx.read_override(), None);
    assert_eq!(fx.reloads(), 1);
    assert_eq!(fx.record(), None);
}

#[test]
fn off_returns_a_user_override_to_its_exact_prior_bytes() {
    let fx = Fixture::new();
    fx.write_override(USER_OVERRIDE);
    let before = std::fs::read(fx.override_path()).unwrap();
    fx.run(true).unwrap();
    fx.clear_log();
    LAST_IN_PLACE_WRITTEN.store(usize::MAX, Ordering::Relaxed);

    fx.run(false).unwrap();
    assert_eq!(std::fs::read(fx.override_path()).unwrap(), before);
    assert_eq!(fx.reloads(), 1);
    assert_eq!(
        LAST_IN_PLACE_WRITTEN.load(Ordering::Relaxed),
        0,
        "a trailing block's removal truncates in place"
    );
}

/// The band copy moved between on and off: off removes the block clauth
/// appended then, which the record carries, not today's.
#[test]
fn off_removes_the_block_the_record_names_after_a_band_copy_change() {
    let fx = Fixture::new();
    let old_block = BLOCK.replace("Esc cancels", "Esc dismisses");
    fx.write_override(&format!("{USER_OVERRIDE}{old_block}"));
    save_record(
        &fx.ctx.clauth_dir.join(RECORD_FILE),
        &Record {
            override_path: Some(fx.override_path()),
            held: Held::Block { block: old_block },
            seen: None,
            failure: None,
            attempted: None,
            refused: None,
        },
    )
    .unwrap();

    fx.run(false).unwrap();
    assert_eq!(fx.read_override().unwrap(), USER_OVERRIDE);
}

/// And with the toggle still on, the old block is swapped for today's.
#[test]
fn on_refreshes_a_block_written_under_an_older_band_copy() {
    let fx = Fixture::new();
    let old_block = BLOCK.replace("Esc cancels", "Esc dismisses");
    fx.write_override(&format!("{USER_OVERRIDE}{old_block}"));
    let record = Record {
        override_path: Some(fx.override_path()),
        held: Held::Block { block: old_block },
        seen: None,
        failure: None,
        attempted: None,
        refused: None,
    };
    save_record(&fx.ctx.clauth_dir.join(RECORD_FILE), &record).unwrap();
    assert!(due(true, Some(&record), || panic!(
        "a user's file needs no cache"
    )));
    LAST_IN_PLACE_WRITTEN.store(usize::MAX, Ordering::Relaxed);

    fx.run(true).unwrap();
    assert_eq!(
        fx.read_override().unwrap(),
        format!("{USER_OVERRIDE}{BLOCK}")
    );
    assert_ne!(
        LAST_IN_PLACE_WRITTEN.load(Ordering::Relaxed),
        usize::MAX,
        "a trailing block's refresh rewrites the block's own tail in place"
    );
}

#[test]
fn off_leaves_a_hand_kept_rule_untouched() {
    let fx = Fixture::new();
    fx.write_override(HAND_KEPT);
    fx.run(true).unwrap();
    fx.clear_log();

    fx.run(false).unwrap();
    assert_eq!(fx.read_override().unwrap(), HAND_KEPT);
    assert_eq!(fx.log().matches("reload-agent-manifests").count(), 0);
}

/// clauth's file the user edited is theirs: never rebuilt, never deleted
/// whole; off takes only clauth's intact block out of it.
#[test]
fn an_edited_clauth_file_is_never_rebuilt_or_deleted() {
    let fx = Fixture::new();
    fx.write_cache(CACHE_V1);
    fx.run(true).unwrap();
    let edited = fx
        .read_override()
        .unwrap()
        .replace("priority = 1100", "priority = 1099");
    fx.write_override(&edited);
    fx.write_cache(CACHE_V2);

    fx.run(true).unwrap();
    assert_eq!(fx.read_override().unwrap(), edited, "not rebuilt");
    assert_eq!(
        fx.card(),
        strings(&[
            "detection: your override + clauth rule",
            "  your override hides herdr's manifest updates",
        ])
    );

    fx.run(false).unwrap();
    assert_eq!(
        fx.read_override().unwrap(),
        edited.replace(BLOCK, ""),
        "only the block goes"
    );
}

/// A block the user edited is no longer clauth's: off leaves the file.
#[test]
fn off_leaves_an_edited_block_in_place() {
    let fx = Fixture::new();
    fx.write_override(USER_OVERRIDE);
    fx.run(true).unwrap();
    let edited = fx.read_override().unwrap().replace("1150", "1200");
    fx.write_override(&edited);

    fx.run(false).unwrap();
    assert_eq!(fx.read_override().unwrap(), edited);
    assert_eq!(fx.record(), None, "the toggle reads off");
}

// ── a symlinked override ───────────────────────────────────────────────────

/// A symlinked override (a dotfiles-managed file) is the user's link: clauth
/// appends its block through the link, and off strips the block from the real
/// file, so the link stays a link and the user's bytes after the block stay.
#[cfg(unix)]
#[test]
fn a_symlinked_override_with_a_tail_keeps_its_link() {
    let fx = Fixture::new();
    let real = fx.home.home().join("dotfiles/claude.toml");
    std::fs::create_dir_all(real.parent().unwrap()).unwrap();
    std::fs::write(&real, USER_OVERRIDE).unwrap();
    std::fs::create_dir_all(fx.override_path().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&real, fx.override_path()).unwrap();
    let is_link = || {
        std::fs::symlink_metadata(fx.override_path())
            .unwrap()
            .file_type()
            .is_symlink()
    };

    fx.run(true).unwrap();
    assert_eq!(
        fx.read_override().unwrap(),
        format!("{USER_OVERRIDE}{BLOCK}")
    );
    assert!(is_link(), "the on pass keeps the link");
    std::fs::write(
        &real,
        format!("{USER_OVERRIDE}{BLOCK}\n# my rules after clauth's block\n"),
    )
    .unwrap();

    fx.run(false).unwrap();
    assert!(is_link(), "the off pass keeps the link");
    assert_eq!(
        std::fs::read_to_string(&real).unwrap(),
        format!("{USER_OVERRIDE}\n# my rules after clauth's block\n"),
        "the target keeps the user's bytes and loses only clauth's block"
    );
}

/// A dangling link is refused like a file clauth cannot undo exactly: nothing
/// is written, no card row shows, an unmoved check starts no pass, and the
/// link that resolves again is applied through.
#[cfg(unix)]
#[test]
fn a_dangling_link_is_refused_with_nothing_written() {
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    fx.write_cache(CACHE_V1);
    let real = fx.home.home().join("dotfiles/claude.toml");
    std::fs::create_dir_all(fx.override_path().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&real, fx.override_path()).unwrap();
    save_app_state(&on_state(true)).unwrap();

    watch_detached(true);
    assert!(
        !real.exists(),
        "nothing is written through the dangling link"
    );
    assert!(
        std::fs::symlink_metadata(fx.override_path())
            .unwrap()
            .file_type()
            .is_symlink(),
        "the user's link stays"
    );
    assert_eq!(fx.card(), Vec::<String>::new(), "no card row for a refusal");
    fx.clear_log();
    watch_detached(true);
    assert_eq!(fx.log(), "", "an unmoved dangling link starts no pass");

    std::fs::create_dir_all(real.parent().unwrap()).unwrap();
    std::fs::write(&real, USER_OVERRIDE).unwrap();
    let ctx = due_pass(true).expect("a link that resolves again is due");
    pass(&ctx).unwrap();
    assert_eq!(
        fx.read_override().unwrap(),
        format!("{USER_OVERRIDE}{BLOCK}")
    );
}

/// A held record whose override becomes a dangling link is not a deleted
/// file: the link is still the user's, so the refusal the unreadable link
/// leaves gates the retry, and only the link resolving again starts a pass.
#[cfg(unix)]
#[test]
fn a_dangling_link_under_a_held_record_is_not_retried() {
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    fx.write_cache(CACHE_V1);
    fx.run(true).unwrap();
    assert!(matches!(fx.record().unwrap().held, Held::File { .. }));
    std::fs::remove_file(fx.override_path()).unwrap();
    let real = fx.home.home().join("dotfiles/claude.toml");
    std::fs::create_dir_all(real.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&real, fx.override_path()).unwrap();
    fx.clear_log();

    assert!(
        due_pass(true).is_some(),
        "a link the watcher cannot read is due once"
    );
    watch_detached(true);
    assert!(
        fx.record().unwrap().refused.is_some(),
        "the refusal is recorded"
    );
    fx.clear_log();

    assert!(
        due_pass(true).is_none(),
        "a dangling link the record names is not a deleted file"
    );
    watch_detached(true);
    assert_eq!(fx.log(), "", "an unmoved dangling link starts no pass");
}

/// A link is never clauth's own file, whatever its bytes: turning the toggle
/// off keeps the link and its target, taking only clauth's intact block out.
#[cfg(unix)]
#[test]
fn a_link_to_clauths_own_bytes_keeps_the_target_on_off() {
    let fx = Fixture::new();
    fx.write_cache(CACHE_V1);
    fx.run(true).unwrap();
    let real = fx.home.home().join("dotfiles/claude.toml");
    std::fs::create_dir_all(real.parent().unwrap()).unwrap();
    std::fs::rename(fx.override_path(), &real).unwrap();
    std::os::unix::fs::symlink(&real, fx.override_path()).unwrap();

    fx.run(false).unwrap();

    assert!(
        std::fs::symlink_metadata(fx.override_path())
            .unwrap()
            .file_type()
            .is_symlink(),
        "the user's link stays"
    );
    assert_eq!(
        std::fs::read_to_string(&real).unwrap(),
        format!("{HEADER}{CACHE_V1}"),
        "the target is kept; only clauth's block goes"
    );
    assert_eq!(fx.record(), None);
}

// ── a pass that cannot read or write the override ──────────────────────────

/// An override clauth cannot write is refused once: the first check records a
/// refusal, and the next unmoved check starts no herdr at all.
#[cfg(unix)]
#[test]
fn a_read_only_override_is_not_retried_until_the_file_moves() {
    use std::os::unix::fs::PermissionsExt as _;
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    fx.write_override(USER_OVERRIDE);
    std::fs::set_permissions(fx.override_path(), std::fs::Permissions::from_mode(0o444)).unwrap();
    save_app_state(&on_state(true)).unwrap();

    watch_detached(true);
    assert!(!fx.log().is_empty(), "the first check ran its pass");
    assert_eq!(
        fx.read_override().as_deref(),
        Some(USER_OVERRIDE),
        "the user's file is untouched"
    );
    assert!(
        fx.record()
            .expect("the refusal is recorded")
            .refused
            .is_some()
    );
    fx.clear_log();

    watch_detached(true);
    assert_eq!(fx.log(), "", "an unmoved state starts no subprocess");
}

/// An unreadable override is refused once: a file that stays unreadable
/// starts no pass, and the check that finds it readable again applies the
/// rule.
#[cfg(unix)]
#[test]
fn an_unreadable_override_is_not_retried_until_it_becomes_readable() {
    use std::os::unix::fs::PermissionsExt as _;
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    fx.write_override(USER_OVERRIDE);
    std::fs::set_permissions(fx.override_path(), std::fs::Permissions::from_mode(0o000)).unwrap();
    save_app_state(&on_state(true)).unwrap();

    watch_detached(true);
    assert!(
        fx.record()
            .expect("the refusal is recorded")
            .refused
            .is_some()
    );
    fx.clear_log();
    watch_detached(true);
    assert_eq!(fx.log(), "", "a file that stays unreadable starts no pass");

    std::fs::set_permissions(fx.override_path(), std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(due_pass(true).is_some(), "a readable file is due again");
    let ctx = due_pass(true).unwrap();
    pass(&ctx).unwrap();
    assert_eq!(
        fx.read_override().unwrap(),
        format!("{USER_OVERRIDE}{BLOCK}")
    );
}

/// An override that becomes unreadable while the record holds what clauth last
/// applied is due once: the pass records the refusal, the next unmoved check
/// starts no herdr, and a readable file is applied again. The refusal leaves
/// the held row, never a new line.
#[cfg(unix)]
#[test]
fn an_unreadable_override_with_a_held_record_is_due_once() {
    use std::os::unix::fs::PermissionsExt as _;
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    fx.write_override(USER_OVERRIDE);
    fx.run(true).unwrap();
    assert_eq!(fx.record().unwrap().held, Held::Block { block: block() });
    std::fs::set_permissions(fx.override_path(), std::fs::Permissions::from_mode(0o000)).unwrap();
    fx.clear_log();

    assert!(
        due_pass(true).is_some(),
        "a file the watcher cannot read is due once"
    );
    watch_detached(true);
    let record = fx.record().expect("the refusal is recorded");
    assert_eq!(record.refused.as_deref(), Some(REFUSED_UNREADABLE));
    assert_eq!(record.held, Held::Block { block: block() });
    assert_eq!(
        fx.card(),
        strings(&[
            "detection: your override + clauth rule",
            "  your override hides herdr's manifest updates",
        ]),
        "the refusal draws the held row, not a new line"
    );
    assert!(!fx.log().is_empty(), "the first check ran its pass");
    fx.clear_log();

    watch_detached(true);
    assert_eq!(fx.log(), "", "an unreadable unmoved file starts no pass");

    std::fs::set_permissions(fx.override_path(), std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(due_pass(true).is_some(), "a readable file is due again");
    let ctx = due_pass(true).expect("a readable file is due again");
    pass(&ctx).unwrap();
    assert_eq!(
        fx.read_override().unwrap(),
        format!("{USER_OVERRIDE}{BLOCK}")
    );
}

/// A hand-kept rule that becomes unreadable is due once, like a block: the
/// pass records the refusal with the hand-kept row kept, the next unmoved
/// check starts no herdr, and a readable file is due again.
#[cfg(unix)]
#[test]
fn an_unreadable_hand_kept_override_is_due_once() {
    use std::os::unix::fs::PermissionsExt as _;
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    fx.write_override(HAND_KEPT);
    fx.run(true).unwrap();
    assert_eq!(fx.record().unwrap().held, Held::HandKept);
    std::fs::set_permissions(fx.override_path(), std::fs::Permissions::from_mode(0o000)).unwrap();
    fx.clear_log();

    assert!(
        due_pass(true).is_some(),
        "a hand-kept file the watcher cannot read is due once"
    );
    watch_detached(true);
    let record = fx.record().expect("the refusal is recorded");
    assert_eq!(record.refused.as_deref(), Some(REFUSED_UNREADABLE));
    assert_eq!(record.held, Held::HandKept);
    assert_eq!(
        fx.card(),
        strings(&[
            "detection: your override, which already has the rule",
            "  a hand-kept rule won't follow clauth's band changes",
        ]),
        "the refusal draws the held row, not a new line"
    );
    fx.clear_log();

    watch_detached(true);
    assert_eq!(fx.log(), "", "an unreadable unmoved file starts no pass");

    std::fs::set_permissions(fx.override_path(), std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(due_pass(true).is_some(), "a readable file is due again");
    let ctx = due_pass(true).expect("a readable file is due again");
    pass(&ctx).unwrap();
    assert_eq!(fx.read_override().unwrap(), HAND_KEPT);
    assert_eq!(fx.record().unwrap().held, Held::HandKept);
}

/// An off pass that cannot write keeps the record as a refusal: the next
/// check starts no herdr, and the user's file keeps the block for a later
/// retry.
#[cfg(unix)]
#[test]
fn an_off_pass_that_cannot_write_is_not_retried_until_the_file_moves() {
    use std::os::unix::fs::PermissionsExt as _;
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    fx.write_override(USER_OVERRIDE);
    fx.run(true).unwrap();
    let with_block = fx.read_override().unwrap();
    std::fs::set_permissions(fx.override_path(), std::fs::Permissions::from_mode(0o444)).unwrap();
    save_app_state(&on_state(false)).unwrap();

    pass(&fx.ctx).unwrap_err();
    assert_eq!(fx.read_override().unwrap(), with_block, "nothing was lost");
    let record = fx.record().expect("the failed undo leaves a record");
    assert!(record.refused.is_some());
    assert_eq!(record.held, Held::Block { block: block() });
    fx.clear_log();

    watch_detached(false);
    assert_eq!(fx.log(), "", "an unmoved state starts no subprocess");
}

/// An off pass that cannot read the override keeps the record as a refusal:
/// the next unmoved check starts no herdr while the file stays unreadable.
#[cfg(unix)]
#[test]
fn an_off_pass_that_cannot_read_the_override_is_not_retried() {
    use std::os::unix::fs::PermissionsExt as _;
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    fx.write_override(USER_OVERRIDE);
    fx.run(true).unwrap();
    std::fs::set_permissions(fx.override_path(), std::fs::Permissions::from_mode(0o000)).unwrap();
    save_app_state(&on_state(false)).unwrap();

    pass(&fx.ctx).unwrap_err();
    let record = fx.record().expect("the failed undo leaves a record");
    assert_eq!(record.refused.as_deref(), Some(REFUSED_UNREADABLE));
    assert_eq!(record.held, Held::Block { block: block() });
    fx.clear_log();

    assert!(due_pass(false).is_none(), "an unreadable file is no change");
    watch_detached(false);
    assert_eq!(fx.log(), "", "an unreadable unmoved file starts no pass");
}

/// An off pass whose removal fails keeps the record as a refusal: the file
/// stays for a later pass, and the next unmoved check starts no herdr.
#[cfg(unix)]
#[test]
fn an_off_pass_that_cannot_remove_clauths_file_is_not_retried() {
    use std::os::unix::fs::PermissionsExt as _;
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    fx.write_cache(CACHE_V1);
    fx.run(true).unwrap();
    let own = fx.read_override().unwrap();
    let digest = sha_hex(own.as_bytes());
    let dir = fx.override_path().parent().unwrap().to_path_buf();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    save_app_state(&on_state(false)).unwrap();

    pass(&fx.ctx).unwrap_err();
    assert_eq!(fx.read_override().unwrap(), own, "the file stays");
    let record = fx.record().expect("the failed undo leaves a record");
    assert_eq!(record.refused.as_deref(), Some(digest.as_str()));
    assert!(matches!(record.held, Held::File { .. }));
    fx.clear_log();

    assert!(due_pass(false).is_none(), "an unmoved file is no change");
    watch_detached(false);
    assert_eq!(fx.log(), "", "an unmoved file starts no pass");

    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A refusal recorded when no file was there (the create failed) reads as the
/// state it recorded: the next unmoved check starts no pass, and a file that
/// appears there retries it.
#[cfg(unix)]
#[test]
fn a_refusal_with_no_file_there_is_not_retried_until_the_file_moves() {
    use std::os::unix::fs::PermissionsExt as _;
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    fx.write_cache(CACHE_V1);
    let dir = fx.override_path().parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    save_app_state(&on_state(true)).unwrap();

    watch_detached(true);
    let record = fx.record().expect("the failed create is recorded");
    assert_eq!(record.refused.as_deref(), Some(REFUSED_ABSENT));
    assert_eq!(record.held, Held::Unapplied);
    assert!(!fx.log().is_empty(), "the first check ran its pass");
    fx.clear_log();

    assert!(
        due_pass(true).is_none(),
        "no file appeared and nothing moved"
    );
    watch_detached(true);
    assert_eq!(fx.log(), "", "a still-absent file starts no pass");

    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    fx.write_override(USER_OVERRIDE);
    assert!(
        due_pass(true).is_some(),
        "a file that appears retries the refusal"
    );
    let ctx = due_pass(true).expect("a file that appears retries the refusal");
    pass(&ctx).unwrap();
    assert_eq!(
        fx.read_override().unwrap(),
        format!("{USER_OVERRIDE}{BLOCK}")
    );
}

/// A refused create under a held record is not retried: the record still says
/// clauth's file while the override is gone and the pass cannot create it.
/// One pass records the refusal, the next unmoved check starts no herdr, and
/// the file appearing or the cache moving is due again.
#[cfg(unix)]
#[test]
fn a_refused_absent_file_under_a_held_record_is_not_retried() {
    use std::os::unix::fs::PermissionsExt as _;
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    fx.write_cache(CACHE_V1);
    fx.run(true).unwrap();
    assert!(matches!(fx.record().unwrap().held, Held::File { .. }));
    let dir = fx.override_path().parent().unwrap().to_path_buf();
    std::fs::remove_file(fx.override_path()).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    fx.clear_log();

    assert!(due_pass(true).is_some(), "the deleted override is due");
    watch_detached(true);
    let record = fx.record().expect("the failed create is recorded");
    assert_eq!(record.refused.as_deref(), Some(REFUSED_ABSENT));
    assert!(
        matches!(record.held, Held::File { .. }),
        "the record keeps the state it had"
    );
    assert!(!fx.log().is_empty(), "the first check ran its pass");
    fx.clear_log();

    assert!(
        due_pass(true).is_none(),
        "a still-absent file under a held record starts no pass"
    );
    watch_detached(true);
    assert_eq!(fx.log(), "", "the next unmoved check starts no herdr");

    fx.write_cache(CACHE_V2);
    assert!(
        due_pass(true).is_some(),
        "a moved cache retries the refusal"
    );
    fx.write_cache(CACHE_V1);

    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    fx.write_override(USER_OVERRIDE);
    assert!(
        due_pass(true).is_some(),
        "a file that appears retries the refusal"
    );
    let ctx = due_pass(true).expect("a file that appears retries the refusal");
    pass(&ctx).unwrap();
    assert_eq!(
        fx.read_override().unwrap(),
        format!("{USER_OVERRIDE}{BLOCK}")
    );
}

/// The same refusal under a directory the watcher cannot search: the read the
/// watcher makes fails, and the refusal stands until the directory allows it
/// again.
#[cfg(unix)]
#[test]
fn a_refused_file_under_an_unsearchable_dir_is_not_retried() {
    use std::os::unix::fs::PermissionsExt as _;
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    fx.write_cache(CACHE_V1);
    fx.run(true).unwrap();
    assert!(matches!(fx.record().unwrap().held, Held::File { .. }));
    let dir = fx.override_path().parent().unwrap().to_path_buf();
    std::fs::remove_file(fx.override_path()).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    let err = std::fs::symlink_metadata(fx.override_path())
        .expect_err("precondition: the fixture's stat fails (a root run would read it)");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::PermissionDenied,
        "precondition: the stat fails for want of search permission"
    );
    fx.clear_log();

    assert!(due_pass(true).is_some(), "the deleted override is due");
    watch_detached(true);
    let record = fx.record().expect("the failed pass is recorded");
    assert_eq!(record.refused.as_deref(), Some(REFUSED_UNREADABLE));
    assert!(
        matches!(record.held, Held::File { .. }),
        "the record keeps the state it had"
    );
    assert!(!fx.log().is_empty(), "the first check ran its pass");
    fx.clear_log();

    assert!(
        due_pass(true).is_none(),
        "an unstatable file under a held record starts no pass"
    );
    watch_detached(true);
    assert_eq!(fx.log(), "", "the next unmoved check starts no herdr");

    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        due_pass(true).is_some(),
        "a readable path retries the refusal"
    );
    let ctx = due_pass(true).expect("a readable path retries the refusal");
    pass(&ctx).unwrap();
    assert_eq!(
        fx.read_override().unwrap(),
        format!("{HEADER}{CACHE_V1}{BLOCK}"),
        "clauth's own file comes back from the cache"
    );
}

/// A refused block's only cache reader is `due`'s refused branch: the block's
/// own arm reads no cache. An unmoved cache starts no herdr; a moved one makes
/// one pass due, and that pass re-refuses with the refusal kept.
#[cfg(unix)]
#[test]
fn a_refused_block_record_retries_on_a_moved_cache() {
    use std::os::unix::fs::PermissionsExt as _;
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    fx.write_cache(CACHE_V1);
    fx.write_override(USER_OVERRIDE);
    fx.run(true).unwrap();
    assert_eq!(fx.record().unwrap().held, Held::Block { block: block() });
    std::fs::set_permissions(fx.override_path(), std::fs::Permissions::from_mode(0o000)).unwrap();
    fx.clear_log();

    assert!(
        due_pass(true).is_some(),
        "a block the watcher cannot read is due once"
    );
    watch_detached(true);
    let record = fx.record().expect("the refusal is recorded");
    assert_eq!(record.refused.as_deref(), Some(REFUSED_UNREADABLE));
    assert_eq!(record.held, Held::Block { block: block() });
    fx.clear_log();

    assert!(
        due_pass(true).is_none(),
        "an unmoved cache under the refused block starts no pass"
    );
    watch_detached(true);
    assert_eq!(fx.log(), "", "an unmoved cache starts no herdr");

    fx.write_cache(CACHE_V2);
    assert!(
        due_pass(true).is_some(),
        "a moved cache retries the refused block"
    );
    watch_detached(true);
    let record = fx.record().expect("the retried pass re-refuses");
    assert_eq!(record.refused.as_deref(), Some(REFUSED_UNREADABLE));
    assert_eq!(record.held, Held::Block { block: block() });
    assert_eq!(
        record.seen.as_deref(),
        Some("2026.11.30.2"),
        "the retried pass read the moved cache"
    );
    fx.clear_log();

    assert!(
        due_pass(true).is_none(),
        "the re-refused state starts no further pass"
    );
    watch_detached(true);
    assert_eq!(fx.log(), "", "the re-refused state starts no herdr");
}

/// The hand-kept sibling: its arm reads no cache either, so the refused
/// branch is the only retry. An unmoved cache starts no herdr; a moved one
/// makes one pass due, and that pass re-refuses with the refusal kept.
#[cfg(unix)]
#[test]
fn a_refused_hand_kept_record_retries_on_a_moved_cache() {
    use std::os::unix::fs::PermissionsExt as _;
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    fx.write_cache(CACHE_V1);
    fx.write_override(HAND_KEPT);
    fx.run(true).unwrap();
    assert_eq!(fx.record().unwrap().held, Held::HandKept);
    std::fs::set_permissions(fx.override_path(), std::fs::Permissions::from_mode(0o000)).unwrap();
    fx.clear_log();

    assert!(
        due_pass(true).is_some(),
        "a hand-kept file the watcher cannot read is due once"
    );
    watch_detached(true);
    let record = fx.record().expect("the refusal is recorded");
    assert_eq!(record.refused.as_deref(), Some(REFUSED_UNREADABLE));
    assert_eq!(record.held, Held::HandKept);
    fx.clear_log();

    assert!(
        due_pass(true).is_none(),
        "an unmoved cache under the refused hand-kept rule starts no pass"
    );
    watch_detached(true);
    assert_eq!(fx.log(), "", "an unmoved cache starts no herdr");

    fx.write_cache(CACHE_V2);
    assert!(
        due_pass(true).is_some(),
        "a moved cache retries the refused hand-kept rule"
    );
    watch_detached(true);
    let record = fx.record().expect("the retried pass re-refuses");
    assert_eq!(record.refused.as_deref(), Some(REFUSED_UNREADABLE));
    assert_eq!(record.held, Held::HandKept);
    assert_eq!(
        record.seen.as_deref(),
        Some("2026.11.30.2"),
        "the retried pass read the moved cache"
    );
    fx.clear_log();

    assert!(
        due_pass(true).is_none(),
        "the re-refused state starts no further pass"
    );
    watch_detached(true);
    assert_eq!(fx.log(), "", "the re-refused state starts no herdr");
}

/// A link loop is refused as a link clauth cannot follow: not retried while
/// it stays a loop, and retried once the path resolves to a readable file.
#[cfg(unix)]
#[test]
fn a_refused_link_loop_is_not_retried_until_it_is_retargeted() {
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    fx.write_cache(CACHE_V1);
    let dir = fx.override_path().parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink("b", dir.join("b")).unwrap();
    std::os::unix::fs::symlink("b", fx.override_path()).unwrap();
    save_app_state(&on_state(true)).unwrap();

    watch_detached(true);
    let record = fx.record().expect("the loop is recorded");
    assert_eq!(record.refused.as_deref(), Some("link:b"));
    assert_eq!(record.held, Held::Unapplied);
    assert!(!fx.log().is_empty(), "the first check ran its pass");
    fx.clear_log();

    assert!(due_pass(true).is_none(), "an unmoved loop is not retried");
    watch_detached(true);
    assert_eq!(fx.log(), "", "an unmoved loop starts no pass");

    let real = fx.home.home().join("dotfiles/claude.toml");
    std::fs::create_dir_all(real.parent().unwrap()).unwrap();
    std::fs::write(&real, USER_OVERRIDE).unwrap();
    std::fs::remove_file(fx.override_path()).unwrap();
    std::os::unix::fs::symlink(&real, fx.override_path()).unwrap();
    assert!(
        due_pass(true).is_some(),
        "a link that resolves to a file is due"
    );
    let ctx = due_pass(true).expect("a link that resolves to a file is due");
    pass(&ctx).unwrap();
    assert_eq!(
        fx.read_override().unwrap(),
        format!("{USER_OVERRIDE}{BLOCK}")
    );
    assert!(
        std::fs::symlink_metadata(fx.override_path())
            .unwrap()
            .file_type()
            .is_symlink(),
        "the user's link stays"
    );
}

// ── check 7: herdr cannot load the write ───────────────────────────────────

#[test]
fn a_write_herdrs_loader_rejects_is_never_made() {
    let fx = Fixture::new();
    fx.write_override(USER_OVERRIDE);
    fx.flag("loader_rejects");

    fx.run(true).unwrap();
    assert_eq!(fx.read_override().unwrap(), USER_OVERRIDE);
    assert_eq!(fx.reloads(), 0);
    assert_eq!(
        fx.card(),
        strings(&[
            "detection: waiting for herdr to download its claude manifest",
            "  herdr couldn't load the override, so clauth put the previous one back",
        ])
    );
}

/// A server that reloads and still serves the remote manifest: clauth's new
/// file goes away again and a second reload applies that.
#[test]
fn a_new_file_the_server_does_not_load_is_removed_and_reloaded() {
    let fx = Fixture::new();
    fx.write_cache(CACHE_V1);
    fx.flag("server_rejects");

    fx.run(true).unwrap();
    assert_eq!(fx.read_override(), None);
    assert_eq!(fx.reloads(), 2, "the write's reload, then the restore's");
    assert_eq!(
        fx.card(),
        strings(&[
            "detection: waiting for herdr to download its claude manifest",
            "  herdr couldn't load the override, so clauth put the previous one back",
        ])
    );
}

#[test]
fn a_user_override_the_server_does_not_load_gets_its_prior_bytes_back() {
    let fx = Fixture::new();
    fx.write_override(USER_OVERRIDE);
    fx.flag("server_rejects");

    fx.run(true).unwrap();
    assert_eq!(fx.read_override().unwrap(), USER_OVERRIDE);
    assert_eq!(fx.reloads(), 2);
}

/// A rebuild the server rejects puts clauth's previous file back and keeps
/// its card, and the same cache version is not retried.
#[test]
fn a_rebuild_the_server_rejects_keeps_the_previous_file() {
    let fx = Fixture::new();
    fx.write_cache(CACHE_V1);
    fx.run(true).unwrap();
    let previous = fx.read_override().unwrap();
    fx.write_cache(CACHE_V2);
    fx.flag("server_rejects");

    fx.run(true).unwrap();
    assert_eq!(fx.read_override().unwrap(), previous);
    assert_eq!(
        fx.card(),
        strings(&[
            "detection: herdr manifest 2026.10.11.1 + clauth rule",
            "  herdr couldn't load the override, so clauth put the previous one back",
        ])
    );
    let record = fx.record();
    assert!(!due(true, record.as_ref(), || Some(
        "2026.11.30.2".to_string()
    )));
}

// ── check 8: the reload fails ──────────────────────────────────────────────

#[test]
fn a_failed_reload_keeps_the_written_file() {
    let fx = Fixture::new();
    fx.write_cache(CACHE_V1);
    fx.flag("reload_fails");

    fx.run(true).unwrap();
    assert_eq!(
        fx.read_override().unwrap(),
        format!("{HEADER}{CACHE_V1}{BLOCK}")
    );
    assert_eq!(
        fx.card(),
        strings(&[
            "detection: herdr manifest 2026.10.11.1 + clauth rule",
            "  herdr didn't reload, so the change applies at its next start",
        ])
    );
}

/// No server running: the write stands, no reload, no failure line.
#[test]
fn no_running_server_means_no_reload_and_no_failure() {
    let fx = Fixture::new();
    std::fs::remove_file(fx.dir.join("sessions.json")).unwrap();
    fx.write_cache(CACHE_V1);

    fx.run(true).unwrap();
    assert!(fx.read_override().is_some());
    assert_eq!(fx.log().matches("reload-agent-manifests").count(), 0);
    assert_eq!(
        fx.card(),
        strings(&["detection: herdr manifest 2026.10.11.1 + clauth rule"])
    );
}

/// Every running server is reloaded, each by name.
#[test]
fn every_running_server_is_reloaded_by_name() {
    let fx = Fixture::new();
    std::fs::write(
        fx.dir.join("sessions.json"),
        r#"{"sessions":[{"name":"default","running":true},{"name":"work","running":true},{"name":"old","running":false}]}"#,
    )
    .unwrap();
    fx.write_cache(CACHE_V1);

    fx.run(true).unwrap();
    let reloads: Vec<String> = fx
        .log()
        .lines()
        .filter(|l| l.ends_with("reload-agent-manifests"))
        .map(str::to_string)
        .collect();
    assert_eq!(
        reloads,
        strings(&[
            "--session default server reload-agent-manifests",
            "--session work server reload-agent-manifests",
        ])
    );
}

// ── check 9: the toggle off reads nothing of herdr's ───────────────────────

#[test]
fn off_with_nothing_to_undo_reads_no_cache_and_runs_no_herdr() {
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    fx.write_cache(CACHE_V1);
    assert!(!due(false, None, || panic!("the cache is read while off")));

    watch_detached(false);
    assert_eq!(fx.log(), "");
    assert_eq!(fx.record(), None);
}

/// The due rule's cache read runs only where the cache decides: never while
/// off, never for a user's file, never for a block whose write herdr already
/// refused. A band copy that moved on clauth's own file is due without one.
#[test]
fn due_reads_the_cache_only_for_clauths_file_or_nothing_applied() {
    let record = |held| Record {
        override_path: None,
        held,
        seen: Some("1".to_string()),
        failure: None,
        attempted: None,
        refused: None,
    };
    let no_read = || -> Option<String> { panic!("cache read") };
    assert!(due(false, Some(&record(Held::HandKept)), no_read));
    assert!(!due(true, Some(&record(Held::HandKept)), no_read));
    assert!(!due(
        true,
        Some(&record(Held::Block { block: block() })),
        no_read
    ));
    assert!(due(true, None, no_read));
    let file = Held::File {
        version: "1".to_string(),
        digest: String::new(),
        block: block(),
    };
    assert!(!due(true, Some(&record(file.clone())), || Some(
        "1".to_string()
    )));
    assert!(due(true, Some(&record(file)), || Some("2".to_string())));
    assert!(!due(true, Some(&record(Held::Unapplied)), || Some(
        "1".to_string()
    )));
    assert!(due(true, Some(&record(Held::Unapplied)), || None));

    // A refused apply (nothing applied, `attempted` naming the block herdr
    // refused) is retried when a later build's block changes, and a moved
    // cache retries it too.
    let mut refused = record(Held::Unapplied);
    refused.attempted = Some(BLOCK.replace("Esc cancels", "Esc dismisses"));
    assert!(
        due(true, Some(&refused), || Some("1".to_string())),
        "a changed block retries a refused apply"
    );
    refused.attempted = Some(block());
    assert!(!due(true, Some(&refused), || Some("1".to_string())));
    assert!(
        due(true, Some(&refused), || Some("2".to_string())),
        "a moved cache retries it"
    );

    // A band copy change makes clauth's own file due with the cache unmoved,
    // and the block path due for a user's file; a refused attempt is not
    // retried until the block changes.
    let old = BLOCK.replace("Esc cancels", "Esc dismisses");
    let old_file = Held::File {
        version: "1".to_string(),
        digest: String::new(),
        block: old.clone(),
    };
    assert!(due(true, Some(&record(old_file.clone())), || Some(
        "1".to_string()
    )));
    let mut attempted = record(Held::Block { block: old.clone() });
    attempted.attempted = Some(block());
    assert!(!due(true, Some(&attempted), no_read));
    let mut stale_file = record(old_file);
    stale_file.attempted = Some(block());
    assert!(!due(true, Some(&stale_file), || Some("1".to_string())));
    assert!(due(
        true,
        Some(&record(Held::Block { block: old })),
        no_read
    ));
}

// ── the trigger: the persisted toggle, the lock, the in-flight gate ────────

/// A pass follows the toggle persisted under the lock, not any state a
/// caller decided earlier: a pass queued after a flip-off deletes the file,
/// and one queued after a flip-on applies it.
#[test]
fn a_pass_follows_the_persisted_toggle() {
    let fx = Fixture::new();
    fx.write_cache(CACHE_V1);
    save_app_state(&on_state(true)).unwrap();
    pass(&fx.ctx).unwrap();
    assert!(fx.read_override().is_some(), "on applies the rule");

    save_app_state(&on_state(false)).unwrap();
    pass(&fx.ctx).unwrap();
    assert_eq!(fx.read_override(), None, "off deletes it");
    assert_eq!(fx.record(), None);

    save_app_state(&on_state(true)).unwrap();
    pass(&fx.ctx).unwrap();
    assert!(fx.read_override().is_some(), "on applies it again");
}

/// A pass waits for the cross-process lock: while another process holds it
/// the pass fails after its wait and writes nothing, and it runs once the
/// lock is free. The holder runs on its own thread: the lock's rank forbids
/// a second acquisition on the thread that holds it, which is the order the
/// pass itself relies on.
#[test]
fn a_pass_takes_the_lock_and_fails_while_it_is_held() {
    let fx = Fixture::new();
    fx.write_cache(CACHE_V1);
    // A record naming the override, a normal state after a first pass: the
    // pass under test finds its path there if herdr's own probe misses its
    // bound, so this test's subject stays the lock's wait.
    save_record(
        &fx.ctx.clauth_dir.join(RECORD_FILE),
        &Record {
            override_path: Some(fx.override_path()),
            held: Held::Unapplied,
            seen: None,
            failure: None,
            attempted: None,
            refused: None,
        },
    )
    .unwrap();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let clauth_dir = fx.ctx.clauth_dir.clone();
    let holder = std::thread::spawn(move || {
        let held = hold_lock(&clauth_dir).unwrap();
        held_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        drop(held);
    });
    held_rx.recv().unwrap();

    let err = fx.run(true).unwrap_err();
    assert!(
        format!("{err:#}").contains("waiting for another clauth process"),
        "{err:#}"
    );
    assert_eq!(fx.read_override(), None);
    release_tx.send(()).unwrap();
    holder.join().unwrap();

    fx.run(true).unwrap();
    assert!(fx.read_override().is_some());
}

/// The in-flight gate runs one pass at a time in a process: while one runs
/// the next is skipped, and the gate clears afterwards.
#[test]
fn a_second_pass_in_flight_is_skipped() {
    let _home = HomeSandbox::new();
    let ran = std::sync::atomic::AtomicBool::new(false);
    IN_FLIGHT.store(true, Ordering::Release);
    run_guarded(|| ran.store(true, Ordering::Release));
    assert!(!ran.load(Ordering::Acquire), "a busy gate skips the pass");
    assert!(IN_FLIGHT.load(Ordering::Acquire), "the holder's flag stays");
    IN_FLIGHT.store(false, Ordering::Release);

    run_guarded(|| ran.store(true, Ordering::Release));
    assert!(ran.load(Ordering::Acquire), "a free gate runs the pass");
    assert!(
        !IN_FLIGHT.load(Ordering::Acquire),
        "the gate clears after the run"
    );
}

/// A pass error that repeats on every cadence writes one line: the log names
/// a persistent failure once, until a pass runs clean.
#[test]
fn a_repeated_pass_error_is_logged_once() {
    let _home = HomeSandbox::new();
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    clear_pass_error();
    log_pass_error(&anyhow::anyhow!("herdr will not answer (dedup probe)"));
    log_pass_error(&anyhow::anyhow!("herdr will not answer (dedup probe)"));
    log_pass_error(&anyhow::anyhow!(
        "herdr will not answer, differently (dedup probe)"
    ));
    let got = lines.snapshot();
    assert_eq!(
        got,
        strings(&[
            "clauth: herdr held question dot: herdr will not answer (dedup probe)",
            "clauth: herdr held question dot: herdr will not answer, differently (dedup probe)",
        ]),
        "{got:?}"
    );
    clear_pass_error();
}

/// The dedup holds only the last pass error, and the flip's own entry resets
/// it after a clean pass: the same failure that follows logs again.
#[test]
fn the_flip_logs_a_pass_error_again_after_a_clean_pass() {
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    clear_pass_error();
    fx.write_cache(CACHE_V1);
    let duplicated = format!("{USER_OVERRIDE}{BLOCK}{BLOCK}");
    fx.write_override(&duplicated);
    save_app_state(&on_state(true)).unwrap();

    run_now();
    fx.write_override(&format!("{USER_OVERRIDE}{BLOCK}"));
    run_now();
    fx.write_override(&duplicated);
    run_now();
    let got = lines.snapshot();
    assert_eq!(got.len(), 2, "the same failure twice: {got:?}");
    assert_eq!(got[0], got[1], "the same pass error");
    clear_pass_error();
}

/// And the tick's own entry resets it too, on a clean on pass and on a clean
/// off pass.
#[test]
fn the_tick_logs_a_pass_error_again_after_a_clean_pass() {
    let mut fx = Fixture::new();
    let state = fx.home.home().join("xdg-state");
    fx.remote = state.join("herdr/agent-detection/remote/claude.toml");
    let _env = EnvPin::new(
        &fx.home,
        &[
            ("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str())),
            ("XDG_STATE_HOME", Some(state.as_os_str())),
        ],
    );
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    clear_pass_error();
    fx.write_cache(CACHE_V1);
    let duplicated = format!("{USER_OVERRIDE}{BLOCK}{BLOCK}");
    fx.write_override(&duplicated);
    save_app_state(&on_state(true)).unwrap();

    watch_detached(true);
    fx.write_override(&format!("{USER_OVERRIDE}{BLOCK}"));
    watch_detached(true);
    save_app_state(&on_state(false)).unwrap();
    watch_detached(false);
    fx.write_override(&duplicated);
    save_app_state(&on_state(true)).unwrap();
    watch_detached(true);
    let got = lines.snapshot();
    assert_eq!(got.len(), 2, "the same failure twice: {got:?}");
    assert_eq!(got[0], got[1], "the same pass error");
    clear_pass_error();
}

// ── a lost record, a user's save, a duplicated block ───────────────────────

/// With no record, no file is known to be clauth's: a cache move never
/// rebuilds one, so an edit of clauth's own file survives, and off takes only
/// the intact block out of it. The file that stays is the user's; they delete
/// it by hand if they want it gone.
#[test]
fn a_file_with_no_record_is_the_users_and_is_never_rebuilt() {
    let fx = Fixture::new();
    fx.write_cache(CACHE_V1);
    fx.run(true).unwrap();
    let edited = fx
        .read_override()
        .unwrap()
        .replace("priority = 1100", "priority = 1099");
    fx.write_override(&edited);
    std::fs::remove_file(fx.ctx.clauth_dir.join(RECORD_FILE)).unwrap();
    fx.write_cache(CACHE_V2);

    fx.run(true).unwrap();
    assert_eq!(fx.read_override().unwrap(), edited, "an edit is the user's");
    assert_eq!(
        fx.card(),
        strings(&[
            "detection: your override + clauth rule",
            "  your override hides herdr's manifest updates",
        ])
    );

    fx.run(false).unwrap();
    assert_eq!(
        fx.read_override().unwrap(),
        edited.replace(BLOCK, ""),
        "only the block goes"
    );
}

/// The spec's root-move residual: a pass acts only on the override at the
/// root herdr answers with, and a file it wrote under a root herdr no longer
/// uses stays as it is. Toggle on over a user override at root A, then herdr
/// answering with root B: A's file keeps the bytes the on pass left, and the
/// record and B's file are what a first pass at B gives.
#[test]
fn a_config_root_move_leaves_the_old_roots_file_and_the_users_bytes_alone() {
    let fx = Fixture::new();
    fx.write_cache(CACHE_V1);
    fx.write_override(USER_OVERRIDE);
    fx.run(true).unwrap();
    let after_on = format!("{USER_OVERRIDE}{BLOCK}");
    assert_eq!(
        fx.read_override().unwrap(),
        after_on,
        "the on pass appended the block"
    );
    fx.debug_build();
    fx.write_cache_as("herdr-dev", CACHE_V1);

    fx.run(true).unwrap();

    assert_eq!(
        fx.read_override().unwrap(),
        after_on,
        "the root herdr no longer answers with keeps the bytes clauth left"
    );
    let live = fx.dir.join("herdr-dev/agent-detection/claude.toml");
    let at_b = std::fs::read_to_string(&live).expect("the live root holds clauth's own file");
    assert_eq!(
        at_b,
        format!("{HEADER}{CACHE_V1}{BLOCK}"),
        "what a first pass at B gives"
    );
    assert_eq!(
        fx.record().expect("the pass records the live root"),
        Record {
            override_path: Some(live),
            held: Held::File {
                version: "2026.10.11.1".to_string(),
                digest: sha_hex(at_b.as_bytes()),
                block: BLOCK.to_string(),
            },
            seen: Some("2026.10.11.1".to_string()),
            failure: None,
            attempted: None,
            refused: None,
        },
        "the record is what a first pass at B gives"
    );
}

/// An override deleted while the toggle stays on is noticed: the next pass
/// writes clauth's own file there (the remedy a hand-kept rule's card names:
/// delete your copy once to let clauth own it).
#[test]
fn a_deleted_override_is_noticed_while_the_toggle_is_on() {
    let mut fx = Fixture::new();
    let state = fx.home.home().join("xdg-state");
    fx.remote = state.join("herdr/agent-detection/remote/claude.toml");
    let _env = EnvPin::new(
        &fx.home,
        &[
            ("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str())),
            ("XDG_STATE_HOME", Some(state.as_os_str())),
        ],
    );
    fx.write_cache(CACHE_V1);
    fx.write_override(HAND_KEPT);
    fx.run(true).unwrap();
    assert!(
        due_pass(true).is_none(),
        "nothing moved while the file is there"
    );

    std::fs::remove_file(fx.override_path()).unwrap();
    let ctx = due_pass(true).expect("the deleted override is due");
    pass(&ctx).unwrap();

    assert_eq!(
        fx.read_override().unwrap(),
        format!("{HEADER}{CACHE_V1}{BLOCK}")
    );
}

/// The remedy a hand-kept rule's card names: once the user deletes their copy,
/// clauth owns the file. The next check is due and one pass appends its block.
#[test]
fn deleting_the_hand_kept_rule_lets_clauth_own_the_override() {
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    let with_rule = format!(
        "{USER_OVERRIDE}\n\n[[rules]]\nid = \"clauth_held_ask\"\nstate = \"blocked\"\n\
         priority = 1150\nregion = \"last_non_empty_above_prompt_box\"\nline_regex = ['^x$']\n"
    );
    fx.write_override(&with_rule);
    fx.run(true).unwrap();
    assert_eq!(fx.record().unwrap().held, Held::HandKept);
    assert!(
        due_pass(true).is_none(),
        "the kept rule is where the record says"
    );

    fx.write_override(USER_OVERRIDE);
    assert!(
        due_pass(true).is_some(),
        "deleting the hand-kept copy lets clauth own the override"
    );
    fx.run(true).unwrap();
    assert_eq!(
        fx.read_override().unwrap(),
        format!("{USER_OVERRIDE}{BLOCK}")
    );
    assert_eq!(fx.record().unwrap().held, Held::Block { block: block() });
}

/// A block the user deleted by hand comes back while the toggle is on: the
/// file no longer carries the block the record holds, so the next pass
/// appends it again.
#[test]
fn a_block_deleted_by_hand_is_put_back_while_the_toggle_is_on() {
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    fx.write_override(USER_OVERRIDE);
    fx.run(true).unwrap();
    assert_eq!(
        fx.read_override().unwrap(),
        format!("{USER_OVERRIDE}{BLOCK}")
    );
    assert!(
        due_pass(true).is_none(),
        "the block is where the record says"
    );

    fx.write_override(USER_OVERRIDE);
    assert!(
        due_pass(true).is_some(),
        "a block gone from the user's file is noticed"
    );
    fx.run(true).unwrap();
    assert_eq!(
        fx.read_override().unwrap(),
        format!("{USER_OVERRIDE}{BLOCK}")
    );
}

/// Each held state is trusted only while the override still carries it: a
/// kept rule, an intact block and clauth's own file are not due unmoved, and
/// an edit of clauth's file is. Nothing applied holds no state to lose, so a
/// file appearing while the waiting row shows is not due either.
#[test]
fn an_unmoved_kept_override_is_not_due() {
    {
        let fx = Fixture::new();
        let _env = EnvPin::new(
            &fx.home,
            &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
        );
        fx.write_override(HAND_KEPT);
        fx.run(true).unwrap();
        assert!(
            due_pass(true).is_none(),
            "the kept rule is where the record says"
        );
    }
    {
        let fx = Fixture::new();
        let _env = EnvPin::new(
            &fx.home,
            &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
        );
        fx.write_override(USER_OVERRIDE);
        fx.run(true).unwrap();
        assert!(
            due_pass(true).is_none(),
            "the block is where the record says"
        );
    }
    {
        let fx = Fixture::new();
        let _env = EnvPin::new(
            &fx.home,
            &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
        );
        fx.write_cache(CACHE_V1);
        fx.run(true).unwrap();
        assert!(
            due_pass(true).is_none(),
            "clauth's file is what the record says"
        );
        let edited = fx
            .read_override()
            .unwrap()
            .replace("priority = 1100", "priority = 1099");
        fx.write_override(&edited);
        assert!(
            due_pass(true).is_some(),
            "an edit of clauth's own file is noticed"
        );
    }
}

/// Nothing applied holds no state to lose: a file that appears while the
/// waiting row shows does not start a pass, and herdr's cache moving is what
/// applies a rule over it.
#[test]
fn a_file_appearing_while_nothing_is_applied_waits_for_the_cache() {
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    save_app_state(&on_state(true)).unwrap();
    watch_detached(true);
    let record = fx.record().expect("the waiting state is recorded");
    assert_eq!(record.held, Held::Unapplied);
    assert_eq!(record.override_path, Some(fx.override_path()));

    fx.write_override(USER_OVERRIDE);
    assert!(due_pass(true).is_none(), "nothing clauth held is missing");

    fx.write_cache(CACHE_V1);
    assert!(
        due_pass(true).is_some(),
        "the cache is what applies the rule"
    );
}

/// The waiting state holds nothing to lose: with no override, no cache and
/// the record unmoved, no check is due, and no missing-file read may make one.
#[test]
fn a_waiting_record_with_no_override_and_no_cache_is_not_due() {
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    save_app_state(&on_state(true)).unwrap();
    watch_detached(true);
    let record = fx.record().expect("the waiting state is recorded");
    assert_eq!(record.held, Held::Unapplied);
    assert_eq!(record.override_path, Some(fx.override_path()));
    assert_eq!(record.seen, None, "no cache was readable");
    assert_eq!(fx.read_override(), None, "nothing was written");
    fx.clear_log();

    assert!(
        due_pass(true).is_none(),
        "the waiting state with the file and the cache absent starts no pass"
    );
    watch_detached(true);
    assert_eq!(fx.log(), "", "the waiting state starts no herdr");
}

/// A refusal keeps the watcher from retrying a file whose bytes the held state
/// no longer matches: the file stays as it is until it or the cache moves.
#[test]
fn a_refused_edited_clauth_file_is_not_retried_until_it_moves() {
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    fx.write_cache(CACHE_V1);
    fx.run(true).unwrap();
    let duplicated = format!("{}{BLOCK}", fx.read_override().unwrap());
    fx.write_override(&duplicated);
    assert!(
        due_pass(true).is_some(),
        "the edit is what makes the refusal due"
    );

    watch_detached(true);
    let record = fx.record().expect("the refusal is recorded");
    assert_eq!(record.refused, Some(sha_hex(duplicated.as_bytes())));
    fx.clear_log();

    watch_detached(true);
    assert_eq!(
        fx.log(),
        "",
        "an unchanged refused file starts no subprocess"
    );
    assert_eq!(fx.read_override().unwrap(), duplicated, "the file stays");
}

/// A herdr whose `plugin config-dir` fails leaves a record that names no
/// root; the pass records the version of the cache the next due check reads
/// (the default app dir's), so an unmoved cache starts no pass and no
/// subprocess, and a moved cache is due again.
#[test]
fn a_failing_config_dir_does_not_loop_tick_passes() {
    let mut fx = Fixture::new();
    let state = fx.home.home().join("xdg-state");
    fx.remote = state.join("herdr/agent-detection/remote/claude.toml");
    let _env = EnvPin::new(
        &fx.home,
        &[
            ("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str())),
            ("XDG_STATE_HOME", Some(state.as_os_str())),
        ],
    );
    fx.write_cache(CACHE_V1);
    save_app_state(&on_state(true)).unwrap();
    fx.flag("config_dir_fails");

    watch_detached(true);
    let record = fx.record().expect("the failed resolve is recorded");
    assert_eq!(record.override_path, None);
    assert_eq!(record.held, Held::Unapplied);
    assert_eq!(
        record.seen.as_deref(),
        Some("2026.10.11.1"),
        "the version of the cache the due check reads"
    );
    assert_eq!(
        fx.log()
            .lines()
            .filter(|line| *line == "plugin config-dir clauth")
            .count(),
        1,
        "the pass ran the resolve once"
    );
    fx.clear_log();

    watch_detached(true);
    assert_eq!(
        fx.log(),
        "",
        "an unmoved cache starts no pass and no subprocess"
    );
    assert!(due_pass(true).is_none());

    fx.write_cache(CACHE_V2);
    assert!(due_pass(true).is_some(), "a moved cache is due again");
}

/// A relative `plugin config-dir` print names no root clauth can resolve: the
/// pass writes no file (a relative one would land under clauth's own cwd) and
/// records no path.
#[test]
fn a_relative_config_dir_print_writes_no_file_and_records_no_path() {
    let fx = Fixture::new();
    fx.write_cache(CACHE_V1);
    fx.flag("config_dir_relative");

    fx.run(true).unwrap();

    assert!(
        !Path::new("rel/herdr/agent-detection/claude.toml").exists(),
        "nothing is written relative to clauth's cwd"
    );
    let record = fx.record().expect("the failed resolve is recorded");
    assert_eq!(record.override_path, None);
    assert_eq!(record.held, Held::Unapplied);
    assert_eq!(record.seen.as_deref(), Some("2026.10.11.1"));
    assert_eq!(fx.reloads(), 0);
}

/// A record naming clauth's override is still acted on while herdr prints a
/// relative root: the record's path alone is undone, never a relative one.
#[test]
fn a_relative_config_dir_print_still_undoes_the_record_named_path() {
    let fx = Fixture::new();
    fx.write_cache(CACHE_V1);
    fx.run(true).unwrap();
    assert_eq!(
        fx.read_override().unwrap(),
        format!("{HEADER}{CACHE_V1}{BLOCK}")
    );
    fx.flag("config_dir_relative");
    fx.clear_log();

    fx.run(false).unwrap();

    assert_eq!(fx.read_override(), None, "the record's path is undone");
    assert!(
        !Path::new("rel/herdr/agent-detection/claude.toml").exists(),
        "nothing is written relative to clauth's cwd"
    );
    assert_eq!(fx.record(), None);
    assert_eq!(fx.reloads(), 1);
}

/// A herdr that cannot answer while the record names the override: the pass
/// falls back to the record's path, so off still takes clauth's block out of
/// the file the record speaks for, and the record goes.
#[test]
fn an_unanswerable_herdr_still_undoes_the_record_named_path() {
    let fx = Fixture::new();
    fx.write_override(USER_OVERRIDE);
    fx.run(true).unwrap();
    assert_eq!(
        fx.read_override().unwrap(),
        format!("{USER_OVERRIDE}{BLOCK}"),
        "the on pass appended the block"
    );
    fx.flag("config_dir_fails");
    fx.clear_log();

    fx.run(false).unwrap();

    assert_eq!(
        fx.read_override().unwrap(),
        USER_OVERRIDE,
        "the record's path is undone"
    );
    assert_eq!(fx.record(), None);
    assert_eq!(
        fx.log()
            .lines()
            .filter(|line| *line == "plugin config-dir clauth")
            .count(),
        1,
        "the pass tried the live root once and fell back"
    );
}

/// With no record, off still resolves the override and takes only clauth's
/// intact block out of a user's file.
#[test]
fn off_with_no_record_takes_clauths_block_out_of_a_user_override() {
    let fx = Fixture::new();
    fx.write_override(USER_OVERRIDE);
    let before = std::fs::read(fx.override_path()).unwrap();
    fx.run(true).unwrap();
    std::fs::remove_file(fx.ctx.clauth_dir.join(RECORD_FILE)).unwrap();

    fx.run(false).unwrap();
    assert_eq!(std::fs::read(fx.override_path()).unwrap(), before);
}

/// A user save landing between the pass's read and its write (the loader
/// check runs a subprocess in that window) is never written over.
#[test]
fn a_user_save_during_the_pass_is_never_written_over() {
    let fx = Fixture::new();
    fx.write_override(USER_OVERRIDE);
    fx.flag("loader_saves");

    let err = fx.run(true).unwrap_err();
    assert!(
        format!("{err:#}").contains("changed since the pass read it"),
        "{err:#}"
    );
    let after = fx.read_override().unwrap();
    assert!(after.starts_with(USER_OVERRIDE), "the user's bytes stay");
    assert!(after.contains("# the user saved this while the pass ran"));
    assert!(!after.contains(BLOCK_BEGIN), "clauth wrote nothing");
    assert_eq!(fx.reloads(), 0);

    // The next pass reads the user's saved bytes afresh: their save stays and
    // clauth's block goes after it.
    drop(std::fs::remove_file(fx.dir.join("loader_saves")));
    fx.run(true).unwrap();
    let after = fx.read_override().unwrap();
    assert!(after.contains("# the user saved this while the pass ran"));
    assert!(after.ends_with(BLOCK), "clauth's block follows the save");
}

/// A file holding both a record-named old block and today's is a shape
/// clauth cannot undo exactly: the on side refuses it and leaves it alone,
/// and the off side leaves it and drops the record, so the watcher stops
/// retrying with the toggle off.
#[test]
fn a_hand_duplicated_block_is_refused() {
    let fx = Fixture::new();
    let old = BLOCK.replace("Esc cancels", "Esc dismisses");
    fx.write_override(&format!("{USER_OVERRIDE}{old}{BLOCK}"));
    let before = std::fs::read(fx.override_path()).unwrap();
    save_record(
        &fx.ctx.clauth_dir.join(RECORD_FILE),
        &Record {
            override_path: Some(fx.override_path()),
            held: Held::Block { block: old },
            seen: None,
            failure: None,
            attempted: None,
            refused: None,
        },
    )
    .unwrap();

    let err = fx.run(true).unwrap_err();
    assert!(
        format!("{err:#}").contains("more than one clauth block"),
        "{err:#}"
    );
    fx.run(false).unwrap();
    assert_eq!(std::fs::read(fx.override_path()).unwrap(), before);
    assert_eq!(fx.reloads(), 0);
    assert_eq!(fx.record(), None, "the refusal drops the record");
    fx.clear_log();
    watch_detached(false);
    assert_eq!(fx.log(), "", "no pass with the toggle off and no record");
}

/// An on-side refusal is remembered in clauth's record: ticks with nothing
/// moved start no pass and no herdr subprocess, and a moved cache or a
/// changed file retries it.
#[test]
fn an_on_side_refusal_is_not_retried_until_the_file_or_cache_moves() {
    let mut fx = Fixture::new();
    let state = fx.home.home().join("xdg-state");
    fx.remote = state.join("herdr/agent-detection/remote/claude.toml");
    let _env = EnvPin::new(
        &fx.home,
        &[
            ("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str())),
            ("XDG_STATE_HOME", Some(state.as_os_str())),
        ],
    );
    fx.write_cache(CACHE_V1);
    let duplicated = format!("{USER_OVERRIDE}{BLOCK}{BLOCK}");
    fx.write_override(&duplicated);
    save_app_state(&on_state(true)).unwrap();

    watch_detached(true);
    let record = fx.record().expect("the refusal is recorded");
    assert_eq!(record.refused, Some(sha_hex(duplicated.as_bytes())));
    assert_eq!(fx.card(), Vec::<String>::new(), "no card row for a refusal");

    fx.clear_log();
    watch_detached(true);
    assert_eq!(fx.log(), "", "an unmoved tick runs no pass and no herdr");
    assert_eq!(
        fx.record().unwrap().refused,
        Some(sha_hex(duplicated.as_bytes())),
        "the refusal stands"
    );

    fx.write_cache(CACHE_V2);
    assert!(
        due_pass(true).is_some(),
        "a moved cache retries the refusal"
    );
    fx.write_cache(CACHE_V1);
    assert!(due_pass(true).is_none(), "nothing moved");

    fx.write_override(&format!("{USER_OVERRIDE}{BLOCK}"));
    let ctx = due_pass(true).expect("a changed file retries the refusal");
    pass(&ctx).unwrap();
    assert_eq!(fx.record().unwrap().refused, None, "the retry landed");
    assert_eq!(
        fx.read_override().unwrap(),
        format!("{USER_OVERRIDE}{BLOCK}"),
        "the user's file keeps its one block"
    );
    assert_eq!(
        fx.card(),
        strings(&[
            "detection: your override + clauth rule",
            "  your override hides herdr's manifest updates",
        ])
    );
}

/// A create never lands over a file that appeared since the pass's read.
#[test]
fn an_appearing_file_is_never_overwritten() {
    let home = HomeSandbox::new();
    let path = home.home().join("appeared.toml");
    std::fs::write(&path, "user bytes").unwrap();

    let err = write_atomic(&path, None, b"clauth bytes").unwrap_err();
    assert!(format!("{err:#}").contains("appeared since"), "{err:#}");
    assert_eq!(std::fs::read(&path).unwrap(), b"user bytes");

    let missing = home.home().join("fresh.toml");
    write_atomic(&missing, None, b"clauth bytes").unwrap();
    assert_eq!(std::fs::read(&missing).unwrap(), b"clauth bytes");
}

/// The temp a rename write creates is born at the target's mode, never at the
/// umask. The target is `0o400`: the umasks this pin was run under (000, 002,
/// 022, 027, 077) leave that mode alone, while `0o666` masked by each of them
/// gives another mode, so a temp born at the umask can never read as the
/// target and the pin reds under any of them. A umask that clears an owner
/// bit (0777) reds every `HomeSandbox` test before this pin's assert: the
/// sandbox's own directories are born 000 there, so its writes fail first.
#[cfg(unix)]
#[test]
fn write_atomic_births_its_temp_at_the_targets_mode() {
    use std::os::unix::fs::PermissionsExt as _;
    let home = HomeSandbox::new();
    let path = home.home().join("private.toml");
    std::fs::write(&path, "old").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
    LAST_TEMP_MODE.store(u32::MAX, Ordering::Relaxed);

    write_atomic(&path, Some(b"old"), b"new").unwrap();

    assert_eq!(
        LAST_TEMP_MODE.load(Ordering::Relaxed),
        0o400,
        "the temp is born at the target's mode"
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o400,
        "the result keeps the target's mode"
    );
}

/// A user's bytes are never rewritten: an append writes the block's own bytes
/// and a trailing block's removal only truncates, so a crash mid-write cannot
/// lose a byte the user wrote.
#[test]
fn edit_in_place_rewrites_only_the_bytes_after_the_common_prefix() {
    let fx = Fixture::new();
    let path = fx.dir.join("user.toml");
    std::fs::write(&path, USER_OVERRIDE).unwrap();
    let appended = format!("{USER_OVERRIDE}{BLOCK}");

    edit_in_place(&path, USER_OVERRIDE.as_bytes(), appended.as_bytes()).unwrap();
    assert_eq!(
        LAST_IN_PLACE_WRITTEN.load(Ordering::Relaxed),
        BLOCK.len(),
        "an append writes the block alone"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), appended);

    edit_in_place(&path, appended.as_bytes(), USER_OVERRIDE.as_bytes()).unwrap();
    assert_eq!(
        LAST_IN_PLACE_WRITTEN.load(Ordering::Relaxed),
        0,
        "a trailing block's removal only truncates"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), USER_OVERRIDE);
}

/// Off over a file holding clauth's block with the user's own bytes after it
/// removes the block and keeps those bytes, without ever rewriting them in
/// place: a death between a truncate and a write cannot lose them.
#[test]
fn off_keeps_the_users_bytes_after_the_block() {
    let fx = Fixture::new();
    let tail = "\n# my own rule\n[[rules]]\nid = \"mine\"\nstate = \"idle\"\n";
    fx.write_override(&format!("{USER_OVERRIDE}{BLOCK}{tail}"));
    LAST_IN_PLACE_WRITTEN.store(usize::MAX, Ordering::Relaxed);

    fx.run(false).unwrap();

    assert_eq!(
        fx.read_override().unwrap(),
        format!("{USER_OVERRIDE}{tail}")
    );
    assert_eq!(
        LAST_IN_PLACE_WRITTEN.load(Ordering::Relaxed),
        usize::MAX,
        "the user's bytes after the block are never rewritten in place"
    );
}

/// A band copy change over a user's file whose own bytes follow clauth's
/// block swaps the block and keeps the tail, through the rename path.
#[test]
fn a_block_refresh_keeps_the_users_bytes_after_the_block() {
    let fx = Fixture::new();
    let old = BLOCK.replace("Esc cancels", "Esc dismisses");
    let tail = "\n# my own rule\n";
    fx.write_override(&format!("{USER_OVERRIDE}{old}{tail}"));
    save_record(
        &fx.ctx.clauth_dir.join(RECORD_FILE),
        &Record {
            override_path: Some(fx.override_path()),
            held: Held::Block { block: old },
            seen: None,
            failure: None,
            attempted: None,
            refused: None,
        },
    )
    .unwrap();
    LAST_IN_PLACE_WRITTEN.store(usize::MAX, Ordering::Relaxed);

    fx.run(true).unwrap();

    assert_eq!(
        fx.read_override().unwrap(),
        format!("{USER_OVERRIDE}{BLOCK}{tail}")
    );
    assert_eq!(
        LAST_IN_PLACE_WRITTEN.load(Ordering::Relaxed),
        usize::MAX,
        "the user's bytes after the block are never rewritten in place"
    );
}

/// A refused block refresh puts the user's prior bytes, tail included, back
/// through the rename path: a death in the restore cannot lose the tail.
#[test]
fn a_refused_block_refresh_restores_the_users_tail() {
    let fx = Fixture::new();
    let old = BLOCK.replace("Esc cancels", "Esc dismisses");
    let prior = format!("{USER_OVERRIDE}{old}\n# my own rule\n");
    fx.write_override(&prior);
    save_record(
        &fx.ctx.clauth_dir.join(RECORD_FILE),
        &Record {
            override_path: Some(fx.override_path()),
            held: Held::Block { block: old },
            seen: None,
            failure: None,
            attempted: None,
            refused: None,
        },
    )
    .unwrap();
    fx.flag("server_rejects");
    LAST_IN_PLACE_WRITTEN.store(usize::MAX, Ordering::Relaxed);

    fx.run(true).unwrap();

    assert_eq!(fx.read_override().unwrap(), prior);
    assert_eq!(
        LAST_IN_PLACE_WRITTEN.load(Ordering::Relaxed),
        usize::MAX,
        "the restore never rewrites the user's bytes in place"
    );
    assert_eq!(
        fx.card(),
        strings(&[
            "detection: your override + clauth rule",
            "  your override hides herdr's manifest updates",
            "  herdr couldn't load the override, so clauth put the previous one back",
        ])
    );
}

/// A user's save landing between clauth's write and the reload's post-check
/// (the first-run shape, no file before the write) is not removed with the
/// file clauth wrote: the file stays, the user's bytes in it.
#[test]
fn a_user_save_during_the_reload_keeps_the_file() {
    let fx = Fixture::new();
    fx.write_cache(CACHE_V1);
    fx.flag("server_rejects");
    fx.flag("reload_saves");

    let err = fx.run(true).unwrap_err();
    assert!(
        format!("{err:#}").contains("changed since the pass read it"),
        "{err:#}"
    );
    let after = fx.read_override().expect("the file stays");
    assert!(after.starts_with(HEADER), "clauth's new file stays");
    assert!(
        after.contains("# the user saved this during the reload"),
        "the user's save stays"
    );
    assert_eq!(fx.reloads(), 1, "the refused restore runs no second reload");
}

/// Off's unlink refuses a file that changed since the pass read it: a save
/// that landed in the window is the user's, so the file stays.
#[test]
fn off_never_removes_a_file_that_changed_since_the_read() {
    let fx = Fixture::new();
    let path = fx.dir.join("clauth.toml");
    std::fs::write(&path, "clauth's bytes").unwrap();

    let err = remove_if_unchanged(&path, Some(b"stale bytes")).unwrap_err();
    assert!(
        format!("{err:#}").contains("changed since the pass read it"),
        "{err:#}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "clauth's bytes");

    remove_if_unchanged(&path, Some(b"clauth's bytes")).unwrap();
    assert!(!path.exists());
}

/// A loader check that exits nonzero gives no verdict even when it prints a
/// well-formed answer, so the pass reports the failure and writes nothing.
#[test]
fn a_loader_error_is_a_load_failure_with_nothing_written() {
    let fx = Fixture::new();
    fx.write_cache(CACHE_V1);
    fx.flag("loader_errors");
    herdr_loads(
        &fx.ctx.bin,
        CACHE_V1.as_bytes(),
        &fx.ctx.clauth_dir,
        "herdr",
    )
    .unwrap_err();

    fx.run(true).unwrap();
    assert_eq!(fx.read_override(), None);
    assert_eq!(fx.reloads(), 0);
    assert_eq!(
        fx.card(),
        strings(&[
            "detection: waiting for herdr to download its claude manifest",
            "  herdr couldn't load the override, so clauth put the previous one back",
        ])
    );
}

/// A band copy change rebuilds clauth's own file even with the cache
/// unmoved, and a failed attempt is not retried until the copy moves again.
#[test]
fn a_band_copy_change_rebuilds_clauths_own_file_once() {
    let fx = Fixture::new();
    fx.write_cache(CACHE_V1);
    let old = BLOCK.replace("Esc cancels", "Esc dismisses");
    let old_file = format!("{HEADER}{CACHE_V1}{old}");
    fx.write_override(&old_file);
    save_record(
        &fx.ctx.clauth_dir.join(RECORD_FILE),
        &Record {
            override_path: Some(fx.override_path()),
            held: Held::File {
                version: "2026.10.11.1".to_string(),
                digest: sha_hex(old_file.as_bytes()),
                block: old,
            },
            seen: Some("2026.10.11.1".to_string()),
            failure: None,
            attempted: None,
            refused: None,
        },
    )
    .unwrap();
    let record = fx.record();
    let cache = || read_cache(&fx.remote).map(|c| c.version);
    assert!(due(true, record.as_ref(), cache), "the band copy moved");

    fx.flag("server_rejects");
    fx.run(true).unwrap();
    assert_eq!(
        fx.read_override().unwrap(),
        old_file,
        "the write was refused"
    );
    let record = fx.record();
    assert_eq!(record.as_ref().unwrap().failure, Some(Failure::LoadFailed));
    assert_eq!(
        record.as_ref().unwrap().attempted.as_deref(),
        Some(block().as_str())
    );
    let cache = || read_cache(&fx.remote).map(|c| c.version);
    assert!(
        !due(true, record.as_ref(), cache),
        "the same failed write is not retried"
    );
}

/// The card is empty while the toggle is off, whatever record is left.
#[test]
fn the_card_is_empty_while_the_toggle_is_off() {
    let fx = Fixture::new();
    fx.write_cache(CACHE_V1);
    fx.run(true).unwrap();

    assert_eq!(card(false), Vec::<String>::new());
    assert_eq!(
        card(true),
        strings(&["detection: herdr manifest 2026.10.11.1 + clauth rule"])
    );
}

// ── herdr's paths and cache, mirrored ──────────────────────────────────────

/// herdr's `state_dir` (non-Windows): `XDG_STATE_HOME` wins, else
/// `$HOME/.local/state`, else the temp dir, each under the app dir name
/// `app_dir_name()` gives (`herdr`, `herdr-dev` in a debug build); a relative
/// result is no path.
#[test]
fn the_state_dir_mirrors_herdrs_resolution() {
    assert_eq!(
        state_dir_from(Some("/x/state".into()), Some("/home/u".into()), "herdr"),
        Some(PathBuf::from("/x/state/herdr"))
    );
    assert_eq!(
        state_dir_from(Some("/x/state".into()), Some("/home/u".into()), "herdr-dev"),
        Some(PathBuf::from("/x/state/herdr-dev"))
    );
    assert_eq!(
        state_dir_from(None, Some("/home/u".into()), "herdr"),
        Some(PathBuf::from("/home/u/.local/state/herdr"))
    );
    assert_eq!(
        state_dir_from(None, None, "herdr"),
        Some(std::env::temp_dir().join("herdr-state"))
    );
    assert_eq!(
        state_dir_from(None, None, "herdr-dev"),
        Some(std::env::temp_dir().join("herdr-dev-state"))
    );
    assert_eq!(
        state_dir_from(Some(String::new()), Some("/home/u".into()), "herdr"),
        None
    );
    assert_eq!(state_dir_from(Some("rel".into()), None, "herdr"), None);
}

/// herdr's app dir name is its config root's last component, and an override
/// path names that root two levels up.
#[test]
fn the_app_dir_name_is_the_config_roots_last_component() {
    assert_eq!(
        app_dir(Path::new("/home/u/.config/herdr")),
        Some("herdr".to_string())
    );
    assert_eq!(
        app_dir(Path::new("/home/u/.config/herdr-dev")),
        Some("herdr-dev".to_string())
    );
    assert_eq!(app_dir(Path::new("/")), None);
    assert_eq!(
        app_dir_of_override(Path::new("/c/herdr-dev/agent-detection/claude.toml")),
        Some("herdr-dev".to_string())
    );
    assert_eq!(app_dir_of_override(Path::new("claude.toml")), None);
}

/// A debug herdr (`herdr-dev`) reads its override and its cache under that
/// app dir: the pass follows herdr's own config root, so the loader check's
/// scratch root and the cache path follow it too.
#[test]
fn a_debug_herdr_reads_its_own_app_dir() {
    let fx = Fixture::new();
    fx.debug_build();
    fx.write_cache_as("herdr-dev", CACHE_V1);

    fx.run(true).unwrap();

    let file = std::fs::read_to_string(fx.dir.join("herdr-dev/agent-detection/claude.toml"))
        .expect("the override lands under herdr-dev");
    assert_eq!(file, format!("{HEADER}{CACHE_V1}{BLOCK}"));
    assert_eq!(
        fx.card(),
        strings(&["detection: herdr manifest 2026.10.11.1 + clauth rule"])
    );
    assert!(
        !fx.override_path().exists(),
        "nothing under the release name"
    );
}

/// `due_pass` reads the cache under the app dir the record's own path names:
/// a steady `herdr-dev` user's moved cache is due, and a move under the
/// release app dir alone is not.
#[test]
fn the_due_check_reads_the_cache_under_the_records_own_app_dir() {
    let fx = Fixture::new();
    fx.debug_build();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    fx.write_cache_as("herdr-dev", CACHE_V1);
    fx.run(true).unwrap();
    assert!(
        due_pass(true).is_none(),
        "the cache is where the pass left it"
    );

    fx.write_cache(CACHE_V2);
    assert!(
        due_pass(true).is_none(),
        "another app dir's cache is not the record's"
    );

    fx.write_cache_as("herdr-dev", CACHE_V2);
    assert!(
        due_pass(true).is_some(),
        "the record's own app dir's cache moved"
    );
}

/// The cache counts only as herdr would take it for Claude.
#[test]
fn a_cache_counts_only_with_claudes_id_and_a_dotted_numeric_version() {
    let home = HomeSandbox::new();
    let path = home.home().join("claude.toml");
    let read = |text: &str| {
        std::fs::write(&path, text).unwrap();
        read_cache(&path).map(|c| c.version)
    };
    assert_eq!(read(CACHE_V1), Some("2026.10.11.1".to_string()));
    assert_eq!(read(&CACHE_V1.replace("\"claude\"", "\"codex\"")), None);
    assert_eq!(
        read(&CACHE_V1.replace("id = \"claude\"", "id = \"x\"\naliases = [\"claude\"]")),
        Some("2026.10.11.1".to_string())
    );
    assert_eq!(read(&CACHE_V1.replace("2026.10.11.1", "2026.10.x")), None);
    assert_eq!(read(&CACHE_V1.replace("2026.10.11.1", "2026..1")), None);
    assert_eq!(read(&CACHE_V1.replace("2026.10.11.1", "")), None);
    assert_eq!(
        read(&CACHE_V1.replace("2026.10.11.1", "2026.10.11.+1")),
        None
    );
    assert_eq!(
        read(&CACHE_V1.replace("2026.10.11.1", "2026.10.11.99999999999999999999")),
        None
    );
    assert_eq!(read("not toml ["), None);
    std::fs::remove_file(&path).unwrap();
    assert_eq!(read_cache(&path), None);
}

/// The loader check runs herdr with the env cleared and a scratch config
/// root holding only the candidate, and takes herdr's word for the source.
#[test]
fn the_loader_check_reads_herdrs_manifest_source() {
    let fx = Fixture::new();
    assert!(
        herdr_loads(
            &fx.ctx.bin,
            CACHE_V1.as_bytes(),
            &fx.ctx.clauth_dir,
            "herdr"
        )
        .unwrap()
    );
    fx.flag("loader_rejects");
    assert!(
        !herdr_loads(
            &fx.ctx.bin,
            CACHE_V1.as_bytes(),
            &fx.ctx.clauth_dir,
            "herdr"
        )
        .unwrap()
    );
    drop(std::fs::remove_file(fx.dir.join("loader_rejects")));
    fx.flag("loader_errors");
    herdr_loads(
        &fx.ctx.bin,
        CACHE_V1.as_bytes(),
        &fx.ctx.clauth_dir,
        "herdr",
    )
    .unwrap_err();
    let leftovers: Vec<_> = std::fs::read_dir(&fx.ctx.clauth_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(".herdr-detect-")
        })
        .collect();
    assert!(leftovers.is_empty(), "the scratch root is removed");
}

/// The loader check's scratch (its config root, the candidate and the screen)
/// is born owner-only under `~/.clauth`: every dir 0700, every file 0600, so
/// herdr's manifest and the user's override bytes are never readable by
/// another local user, even while the check runs.
#[cfg(unix)]
#[test]
fn the_loader_checks_scratch_is_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;
    let fx = Fixture::new();
    let scratch = loader_scratch(&fx.ctx.clauth_dir, CACHE_V1.as_bytes(), "herdr").unwrap();
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&fx.ctx.clauth_dir), 0o700, "the dir clauth creates");
    assert_eq!(mode(scratch.path()), 0o700, "the scratch root");
    assert_eq!(mode(&scratch.config_home()), 0o700, "the config home");
    assert_eq!(
        mode(scratch.override_path("herdr").parent().unwrap()),
        0o700,
        "the override's dir"
    );
    assert_eq!(
        mode(&scratch.override_path("herdr")),
        0o600,
        "the candidate"
    );
    assert_eq!(mode(&scratch.screen()), 0o600, "the screen");
}

// ── check 11: the rule follows the band copy ───────────────────────────────

/// `name`'s single-quoted string in a `const NAME = '...'` line.
fn ts_const(source: &str, name: &str) -> String {
    let re = regex::Regex::new(&format!(r"(?m)^const {name} = '([^']*)'$")).unwrap();
    re.captures(source)
        .unwrap_or_else(|| panic!("register.ts defines {name}"))[1]
        .to_string()
}

/// One band line 2 as the mod draws it from its `children` list: `Text`
/// children verbatim (a constant resolved), a plain `Button` as
/// `<hotkey>: <label>`, the shape the 2026-11-12 captures show.
fn compose(source: &str, children: &str) -> String {
    let piece = regex::Regex::new(
        r"Text\(\{ children: (?:'([^']*)'|([A-Z0-9_]+)) \}\)|Button\(\{[^\n]*?plain: true, hotkey: '([^']*)', label: '([^']*)'",
    )
    .unwrap();
    let mut line = String::new();
    let mut buttons = 0;
    for cap in piece.captures_iter(children) {
        if let Some(text) = cap.get(1) {
            line.push_str(text.as_str());
        } else if let Some(name) = cap.get(2) {
            line.push_str(&ts_const(source, name.as_str()));
        } else {
            buttons += 1;
            line.push_str(&cap[3]);
            line.push_str(": ");
            line.push_str(&cap[4]);
        }
    }
    assert!(buttons > 0, "the band line holds its buttons: {children}");
    line
}

/// The scan's own control: a synthetic composition it must read whole, and
/// one with no button it must refuse.
#[test]
fn compose_reads_texts_constants_and_plain_buttons_in_order() {
    let source = "const LEAD = '  Go on.   '\n";
    let children = "children: [\n  Text({ children: LEAD }),\n  Button({ key: 'a', plain: true, hotkey: '7', label: 'Yes please', onPress: () => x }),\n  Text({ children: '   tail' }),\n]";
    assert_eq!(compose(source, children), "  Go on.   7: Yes please   tail");
    let no_button = std::panic::catch_unwind(|| compose(source, "Text({ children: LEAD })"));
    assert!(no_button.is_err(), "a line with no button is not the band");
}

/// The rule's `line_regex` matches both band line 2 shapes `register.ts`
/// draws, and each composed line is the 2026-11-12 capture's own bytes, so
/// changing either lead (its spacing included), a button or the regex reds
/// here.
#[test]
fn the_rule_matches_both_band_lines_register_ts_draws() {
    let source = include_str!("../../plugins/hooks/register.ts");
    let start = source
        .find("const line2 =")
        .expect("register.ts composes the band's line 2");
    let rest = &source[start..];
    let end = rest.find("return Box(").expect("the band's return follows");
    let line2 = &rest[..end];
    let (decision, question) = line2
        .split_once(": Box(")
        .expect("a decision shape and a question shape");
    assert!(decision.contains("row.kind === 'decision'"));
    let decision = compose(source, decision);
    let question = compose(source, question);
    assert_eq!(decision, CAPTURED_DECISION);
    assert_eq!(question, CAPTURED_QUESTION);

    let rule = held_ask_rule(&format!("{CACHE_V1}{}", block()));
    let regex =
        regex::Regex::new(rule["line_regex"].as_array().unwrap()[0].as_str().unwrap()).unwrap();
    assert!(regex.is_match(&decision), "{decision:?}");
    assert!(regex.is_match(&question), "{question:?}");
}

// ── each read refuses a path that is not a regular file ─────────────────────

/// Run `check` on its own thread and give up after a few seconds: a read that
/// opened a FIFO, a socket or a device the watcher must refuse would never
/// return, so its red is a failed assert rather than a hung suite.
#[cfg(unix)]
fn within_deadline(check: impl FnOnce() -> bool + Send + 'static) -> Option<bool> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(check());
    });
    rx.recv_timeout(std::time::Duration::from_secs(5)).ok()
}

/// A FIFO at the override path is a file the watcher cannot read: the first
/// check is due once and records the refusal, every later unmoved check on or
/// off starts no herdr, and a regular file put back is due again. A plain read
/// of the FIFO or of the socket that replaces it would never return.
#[cfg(unix)]
#[test]
fn a_fifo_at_the_override_path_is_due_once() {
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    fx.write_override(USER_OVERRIDE);
    fx.run(true).unwrap();
    assert_eq!(fx.record().unwrap().held, Held::Block { block: block() });
    std::fs::remove_file(fx.override_path()).unwrap();
    crate::testutil::mkfifo(&fx.override_path());
    fx.clear_log();

    assert_eq!(
        within_deadline(|| due_pass(true).is_some()),
        Some(true),
        "a FIFO is a file the watcher cannot read, not one it waits on"
    );
    watch_detached(true);
    let record = fx.record().expect("the refusal is recorded");
    assert_eq!(record.refused.as_deref(), Some(REFUSED_UNREADABLE));
    assert_eq!(record.held, Held::Block { block: block() });
    assert!(!fx.log().is_empty(), "the first check ran its pass");
    fx.clear_log();

    assert_eq!(
        within_deadline(|| due_pass(true).is_none()),
        Some(true),
        "an unreadable unmoved FIFO starts no pass"
    );
    assert_eq!(
        within_deadline(|| due_pass(false).is_none()),
        Some(true),
        "the same refused state with the toggle off starts none either"
    );

    std::fs::remove_file(fx.override_path()).unwrap();
    std::os::unix::net::UnixListener::bind(fx.override_path()).unwrap();
    assert_eq!(
        within_deadline(|| due_pass(true).is_none()),
        Some(true),
        "a socket is refused like the FIFO it replaced"
    );
    watch_detached(true);
    assert_eq!(fx.log(), "", "an unreadable unmoved socket starts no herdr");

    std::fs::remove_file(fx.override_path()).unwrap();
    fx.write_override(USER_OVERRIDE);
    let ctx = due_pass(true).expect("a regular file put back is due again");
    pass(&ctx).unwrap();
    assert_eq!(fx.record().unwrap().held, Held::Block { block: block() });
}

/// A FIFO at herdr's cache path reads as no cache: the check returns, and the
/// held file's moved-version rule makes it due. A plain read would never
/// return.
#[cfg(unix)]
#[test]
fn a_fifo_at_the_cache_path_reads_as_no_cache() {
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    fx.write_cache(CACHE_V1);
    fx.run(true).unwrap();
    assert!(matches!(fx.record().unwrap().held, Held::File { .. }));
    std::fs::remove_file(&fx.remote).unwrap();
    crate::testutil::mkfifo(&fx.remote);

    assert_eq!(
        within_deadline(|| due_pass(true).is_some()),
        Some(true),
        "a cache that cannot be read is due, not waited on"
    );
}

/// A symlink at the cache path to a path that is not a regular file reads as
/// no cache: the metadata check follows the link and refuses the FIFO before
/// any open, so the check returns instead of waiting in it.
#[cfg(unix)]
#[test]
fn a_symlink_at_the_cache_path_to_a_fifo_is_refused() {
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    fx.write_cache(CACHE_V1);
    fx.run(true).unwrap();
    assert!(matches!(fx.record().unwrap().held, Held::File { .. }));
    std::fs::remove_file(&fx.remote).unwrap();
    let fifo = fx.home.home().join("cache-fifo");
    crate::testutil::mkfifo(&fifo);
    std::os::unix::fs::symlink(&fifo, &fx.remote).unwrap();

    assert_eq!(
        within_deadline(|| due_pass(true).is_some()),
        Some(true),
        "a symlink to a FIFO is refused by the metadata check, not opened"
    );
}

/// The cache read is bounded at a mebibyte: a manifest at that size still
/// reads and leaves the held file quiet, one byte more reads as no cache and
/// makes the moved-version rule due.
#[test]
fn a_cache_over_the_byte_cap_reads_as_no_cache() {
    const CAP: usize = 1 << 20;
    let fx = Fixture::new();
    let _env = EnvPin::new(
        &fx.home,
        &[("HERDR_BIN_PATH", Some(fx.ctx.bin.as_os_str()))],
    );
    fx.write_cache(CACHE_V1);
    fx.run(true).unwrap();
    assert!(matches!(fx.record().unwrap().held, Held::File { .. }));
    let at_cap = format!("{CACHE_V1}{}", "#".repeat(CAP - CACHE_V1.len()));
    fx.write_cache(&at_cap);
    assert!(due_pass(true).is_none(), "a cache at the cap still reads");
    fx.write_cache(&format!("{at_cap}#"));
    assert!(
        due_pass(true).is_some(),
        "a cache one byte over the cap reads as no cache"
    );
}

/// The compare-to-write re-read refuses a path that is not a regular file
/// instead of waiting on it.
#[cfg(unix)]
#[test]
fn unchanged_since_read_refuses_a_path_that_is_not_a_regular_file() {
    let fx = Fixture::new();
    let fifo = fx.home.home().join("override-fifo");
    crate::testutil::mkfifo(&fifo);
    assert_eq!(
        within_deadline(move || unchanged_since_read(&fifo, Some(b"old".as_slice())).is_err()),
        Some(true),
        "the re-read does not wait on a FIFO"
    );
}
