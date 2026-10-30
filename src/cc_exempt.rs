//! Claude Code's config-dir permission exemptions, restored for a SHARED
//! runtime whose entries resolve through symlinks into `~/.claude`.
//!
//! Claude Code builds each exempt dir (auto memory, a session's tool results)
//! from the literal `CLAUDE_CONFIG_DIR` and requires BOTH the literal and the
//! symlink-resolved spelling of a path to land inside it. Through the runtime's
//! `projects` symlink the resolved spelling never does, so every auto-memory
//! write and every tool-result read prompts, and a headless run loses it.
//!
//! Two repairs ride one per-spawn `--settings` FILE (never inline, so a user's
//! settings — credentials included — stay off the child's argv), written into
//! the runtime's profile dir and removed when the runtime drops unless
//! [`crate::claude_jobs::JobTrees::spares_settings`] spares it:
//! - `autoMemoryDirectory` pointed at the resolved memory dir, so both
//!   spellings agree;
//! - a `PreToolUse` `Read` hook that allows a read whose resolved path is this
//!   session's own tool result. A hook allow clears a read prompt but not a
//!   write one, so writes cannot take this route.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::outln;
use serde_json::{Map, Value, json};

/// The hidden subcommand the injected `Read` hook runs.
pub(crate) const READ_HOOK_SUBCOMMAND: &str = "hook-read-exempt";

/// The `PreToolUse` `Read` allow, byte-for-byte.
const READ_ALLOW: &str = r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","permissionDecisionReason":"clauth: this session's own tool result, read through the runtime's projects link"}}"#;

/// The shared tail of every user-`--settings` warning: what the user asked for
/// still reaches `claude`, what clauth skips is its own exemption.
const TAIL: &str = "passed to claude unchanged, so auto-memory writes and tool-result reads may prompt this session";

/// Byte cap for a settings file, user `--settings` and probed tiers alike.
/// A cloned repo can commit `.claude/settings.json` as a symlink to
/// `/dev/zero`; the shared reader refuses non-regular and oversize files
/// instead of ballooning the long-lived MCP server.
const SETTINGS_FILE_CAP: u64 = 1 << 20;

/// Claude Code's project key for `base`: every UTF-16 code unit outside
/// `[A-Za-z0-9]` becomes `-`; past 200 units the key is cut to 200 and gets
/// `-` plus the base36 absolute value of the 32-bit `h = h*31 + unit` hash of
/// the UNSANITIZED input.
pub(crate) fn project_key(base: &str) -> String {
    const CAP: usize = 200;
    let mut sanitized = String::with_capacity(base.len());
    for unit in base.encode_utf16() {
        if matches!(unit, 48..=57 | 65..=90 | 97..=122) {
            sanitized.push(unit as u8 as char);
        } else {
            sanitized.push('-');
        }
    }
    // Every unit mapped to exactly one ASCII byte, so byte length == unit length.
    if sanitized.len() <= CAP {
        return sanitized;
    }
    format!("{}-{}", &sanitized[..CAP], base36(hash_code_units(base)))
}

/// Claude Code's `mle`: the 32-bit `h = h*31 + unit` hash over the UTF-16
/// code units, widened before `Math.abs` so `i32::MIN` cannot overflow.
fn hash_code_units(s: &str) -> i64 {
    let mut h: i32 = 0;
    for unit in s.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(i32::from(unit));
    }
    if h < 0 { -(h as i64) } else { i64::from(h) }
}

/// Lowercase base36 of a non-negative integer.
fn base36(mut n: i64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut digits = Vec::new();
    while n > 0 {
        digits.push(DIGITS[(n % 36) as usize] as char);
        n /= 36;
    }
    if digits.is_empty() {
        digits.push('0');
    }
    digits.into_iter().rev().collect()
}

/// The directory Claude Code keys auto memory on for a session started in
/// `cwd`: the nearest ancestor with a `.git` (dir or file), resolved through
/// Claude Code's `kr` to a linked worktree's common dir, else that root, else
/// the canonicalized `cwd`.
pub(crate) fn memory_base(cwd: &Path) -> PathBuf {
    let canonical = crate::canonical::identity(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut dir = canonical.clone();
    loop {
        let git = dir.join(".git");
        if git.is_dir() || git.is_file() {
            return kr(&dir);
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => break,
        }
    }
    canonical
}

/// Claude Code's `kr(e, read(e/.git))`: `e` is the dir the `.git` walk found.
/// A `.git` directory reads as `e`; a `.git` FILE is a worktree/submodule
/// marker, resolved to the common dir through the worktree admin dir's
/// `commondir` and back-pointing `gitdir`, with the checks as CC writes them.
fn kr(e: &Path) -> PathBuf {
    let Some(content) = std::fs::read_to_string(e.join(".git")).ok() else {
        return e.to_path_buf();
    };
    let r = content.trim();
    if !r.starts_with("gitdir:") {
        return e.to_path_buf();
    }
    let i = r["gitdir:".len()..].trim();
    let admin = lexical_resolve(e, i);
    let Some(s) = read_git_file(&admin.join("commondir")) else {
        return e.to_path_buf();
    };
    let common = lexical_resolve(&admin, s.trim());
    // The admin dir must sit directly under `<common>/worktrees`.
    if normalize(admin.parent().unwrap_or(&admin)) != normalize(&common.join("worktrees")) {
        return e.to_path_buf();
    }
    let Some(u) = read_git_file(&admin.join("gitdir")) else {
        return e.to_path_buf();
    };
    // The back-pointer realpaths to `<e>/.git` (CC's `wr` is `realpathSync`),
    // falling back to the lexical spelling when a realpath fails.
    if realpath(&lexical_resolve(&admin, u.trim())) != realpath(e).join(".git") {
        return e.to_path_buf();
    }
    // The returned base is the LEXICAL `common` / `dirname(common)` CC's `Nn`
    // (identity) yields — a `gitdir:` spelled through a symlink keeps that
    // spelling. Only the back-pointer compare above realpaths.
    if c_is_git_dir(&common) {
        common.parent().map(Path::to_path_buf).unwrap_or(common)
    } else if common.join(".git").exists() {
        e.to_path_buf()
    } else {
        common
    }
}

/// `realpathSync` with a lexical fallback: canonicalize resolves symlinks;
/// a path that does not exist (a stale back-pointer) stays as spelled. Used
/// only by the back-pointer equality, where CC's `wr` realpaths both sides.
fn realpath(p: &Path) -> PathBuf {
    crate::canonical::identity(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Whether the common dir's basename is `.git` (a normal repo's git dir), the
/// split that decides "main working copy" vs "the common dir itself".
fn c_is_git_dir(common: &Path) -> bool {
    common
        .file_name()
        .is_some_and(|n| n == std::ffi::OsStr::new(".git"))
}

/// Read a git bookkeeping file the way `fB` then `ye` do: a symlink, special
/// file or missing entry reads as absent.
fn read_git_file(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// `path.resolve` in Rust: join then lexically collapse `.` and `..`, never
/// touching symlinks (CC's `kr` is lexical, not a realpath).
fn lexical_resolve(base: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    normalize(&joined)
}

fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The hook command for the binary at `exe`: the installed path (a self-update
/// leaves a long-lived server's `current_exe` reading `(deleted)`), POSIX
/// quoted, then the subcommand and a fail-open `|| true` — a binary that
/// cannot parse the subcommand exits 2, which would block every Read.
pub(crate) fn hook_command_for(exe: &Path) -> String {
    let installed = crate::platform::installed_exe_path(exe);
    format!(
        "{} {READ_HOOK_SUBCOMMAND} || true",
        posix_quote(&installed.to_string_lossy())
    )
}

/// The shell command the injected `Read` hook runs: this binary (its installed
/// path), quoted, plus [`READ_HOOK_SUBCOMMAND`] and a fail-open `|| true`.
pub(crate) fn hook_command() -> String {
    match std::env::current_exe() {
        Ok(exe) => hook_command_for(&exe),
        Err(_) => hook_command_for(Path::new("clauth")),
    }
}

/// POSIX single-quote `s`: an embedded `'` becomes `'\''`.
fn posix_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A settings warning detail: the shared reader's refusal text, with a
/// trailing ` (os error N)` stripped so the warning reads the platform's own
/// message, not std's suffix. The prompt path keeps the suffix — this strip is
/// for the settings warning alone.
fn strip_os_error(s: &str) -> String {
    match s.rfind(" (os error ") {
        Some(idx) if s[idx..].ends_with(')') => s[..idx].to_string(),
        _ => s.to_string(),
    }
}

/// Append the exemption `--settings` (when the runtime needs it) and then the
/// caller's `passthrough` args to `command`.
///
/// Injects when the literal `config_dir/projects` differs from its resolved
/// spelling (a symlinked `projects`, or a symlinked ancestor). The merged
/// settings object is written to `settings_file` (0o600, atomic) and passed as
/// `--settings <file>` — never inline, so a folded user settings value stays
/// off the child's argv. The user's last pre-`--` `--settings` is the merge
/// base; `autoMemoryDirectory` is added unless a settings source or the
/// child's env already keys it. A user `--settings` that cannot be read or
/// parsed, or whose `hooks` shape is wrong, disables the injection and comes
/// back as `Some(warning)`, with the passthrough appended untouched.
pub(crate) fn apply_spawn_settings(
    command: &mut Command,
    config_dir: &Path,
    cwd: &Path,
    passthrough: &[String],
    hook_command: &str,
    settings_file: &Path,
) -> Option<String> {
    let literal = config_dir.join("projects");
    let Some(real) = resolve_projects(config_dir) else {
        command.args(passthrough);
        return None;
    };
    let needs_injection = crate::canonical::claude_code_spelling(&literal.to_string_lossy())
        != crate::canonical::claude_code_spelling(&real.to_string_lossy());
    if !needs_injection {
        command.args(passthrough);
        return None;
    }

    let (merge_value, clean) = match fold_settings_args(passthrough) {
        Ok(folded) => folded,
        // A bare `--settings` with no value: CC would refuse it; pass it
        // through exactly and inject nothing.
        Err(()) => {
            command.args(passthrough);
            return None;
        }
    };

    let mut settings = match merge_value {
        Some(value) => {
            let inline = is_inline_settings(&value);
            match parse_settings_value(&value, cwd, inline) {
                Ok(map) => {
                    if let Some(field) = hooks_shape_error(&map) {
                        command.args(passthrough);
                        return Some(hooks_warning(inline, &value, &field));
                    }
                    map
                }
                Err(warning) => {
                    command.args(passthrough);
                    return Some(warning);
                }
            }
        }
        None => Map::new(),
    };

    // `autoMemoryDirectory` is ours only when no settings source or env var
    // already keys it. At the real `$HOME` the spawn gets `--setting-sources
    // user`, so the `<cwd>/.claude` project tier is never read. On unix Claude
    // Code reads the local tier at `memory_base` (a linked worktree's main
    // checkout) unless that root is the real `$HOME`, and still reads
    // `<cwd>/.claude/settings.local.json` beside it as legacy local settings.
    // Claude Code also skips a root unless the root, its `.git` and its
    // `.claude` are owned by the euid; that check is not mirrored here, so a
    // foreign-owned root's file still suppresses ours and its memory writes
    // prompt, where mirroring it would need the euid (`unsafe` or a new dep).
    let at_home = crate::runtime::cwd_is_real_home(cwd);
    let base = memory_base(cwd);
    let mut suppressed = sets_memory(&settings);
    if !suppressed {
        suppressed = settings_file_sets_memory(&config_dir.join("settings.json"));
    }
    if !suppressed && !at_home {
        suppressed = settings_file_sets_memory(&cwd.join(".claude").join("settings.json"))
            || settings_file_sets_memory(&cwd.join(".claude").join("settings.local.json"))
            || (cfg!(unix)
                && !crate::runtime::cwd_is_real_home(&base)
                && settings_file_sets_memory(&base.join(".claude").join("settings.local.json")));
    }
    if !suppressed {
        // CC tests it for truthiness (`if(process.env.CLAUDE_CODE_REMOTE_MEMORY_DIR)`),
        // so an empty value is unset to CC.
        suppressed = child_env_value(command, "CLAUDE_CODE_REMOTE_MEMORY_DIR")
            .is_some_and(|v| !v.is_empty());
    }
    if !suppressed {
        suppressed = child_env_value(command, "CLAUDE_CODE_PROJECT_DIR_NAME")
            .is_some_and(|v| valid_project_dir_name(&v));
    }
    if !suppressed {
        let store = crate::canonical::claude_code_spelling(&real.to_string_lossy());
        let key = project_key(&crate::canonical::claude_code_spelling(
            &base.to_string_lossy(),
        ));
        settings.insert(
            "autoMemoryDirectory".to_string(),
            Value::String(
                Path::new(&store)
                    .join(key)
                    .join("memory")
                    .to_string_lossy()
                    .into_owned(),
            ),
        );
    }

    let our_hook = json!({
        "matcher": "Read",
        "hooks": [{"type": "command", "command": hook_command, "timeout": 10}]
    });
    match settings
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()))
    {
        Value::Object(hooks) => {
            let pre = hooks
                .entry("PreToolUse".to_string())
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Value::Array(arr) = pre {
                arr.push(our_hook);
            } else {
                *pre = Value::Array(vec![our_hook]);
            }
        }
        other => {
            let mut hooks = Map::new();
            hooks.insert("PreToolUse".to_string(), Value::Array(vec![our_hook]));
            *other = Value::Object(hooks);
        }
    }

    let Ok(settings_json) = serde_json::to_string(&Value::Object(settings)) else {
        command.args(passthrough);
        return Some("clauth: --settings: could not serialize the injected settings".to_string());
    };
    if let Err(e) = crate::profile::atomic_write_600(settings_file, settings_json.as_bytes()) {
        command.args(passthrough);
        return Some(format!(
            "clauth: --settings {:?}: cannot write it ({e}); {TAIL}",
            settings_file
        ));
    }
    command.arg("--settings").arg(settings_file);
    command.args(&clean);
    None
}

/// The resolved `config_dir/projects`: the literal's realpath when it exists,
/// else `canonicalize(config_dir)/projects` (an isolated runtime before Claude
/// Code has created `projects`).
fn resolve_projects(config_dir: &Path) -> Option<PathBuf> {
    let literal = config_dir.join("projects");
    if let Ok(real) = crate::canonical::identity(&literal) {
        return Some(real);
    }
    Some(
        crate::canonical::identity(config_dir)
            .ok()?
            .join("projects"),
    )
}

/// Every root flag of claude 2.1.311 that takes a required value: claude hands
/// it the next word whatever that word holds (`--name --zzz -p` names the
/// session `--zzz`, `--name -- -p` names it `--`), so that word is never a
/// flag or the separator. Copied from `VALUE_FLAGS` in cc-system-prompts'
/// `test_shim.py`, which measured each entry against a real claude; the drift
/// test in `tests/inline/cc_exempt.rs` checks the part `claude --help` lists.
const VALUE_FLAGS: &[&str] = &[
    "--add-dir",
    "--advisor",
    "--agent",
    "--agent-color",
    "--agent-id",
    "--agent-name",
    "--agent-type",
    "--agents",
    "--allowed-tools",
    "--allowedTools",
    "--append-subagent-system-prompt",
    "--append-subagent-system-prompt-file",
    "--append-system-prompt",
    "--append-system-prompt-file",
    "--attach-serve",
    "--autocompact",
    "--betas",
    "--channels",
    "--client-data-url",
    "--correlation-id",
    "--dangerously-load-development-channels",
    "--debug-file",
    "--deep-link-last-fetch",
    "--deep-link-repo",
    "--disallowed-tools",
    "--disallowedTools",
    "--effort",
    "--environment",
    "--fallback-model",
    "--fast-mode",
    "--file",
    "--forward-home-settings",
    "--inherit-permission-mode",
    "--input-format",
    "--json-schema",
    "--managed-settings",
    "--max-budget-usd",
    "--max-thinking-tokens",
    "--max-turns",
    "--mcp-config",
    "--messaging-socket-path",
    "--model",
    "--name",
    "--on-branch",
    "--output-format",
    "--parent-session-id",
    "--permission-mode",
    "--permission-prompt-tool",
    "--permission-prompts",
    "--plan-mode-instructions",
    "--plugin-dir",
    "--plugin-dir-no-mcp",
    "--plugin-url",
    "--pool",
    "--prefill",
    "--proactivity",
    "--project-config-root",
    "--ref",
    "--remote",
    "--remote-control-session-name-prefix",
    "--resume-drops-turn",
    "--resume-session-at",
    "--rewind-files",
    "--runner-declares",
    "--sdk-url",
    "--session-id",
    "--setting-sources",
    "--settings",
    "--system-prompt",
    "--system-prompt-file",
    "--system-prompt-snapshot",
    "--task-budget",
    "--team-name",
    "--teammate-mode",
    "--thinking",
    "--thinking-display",
    "--tools",
    "--watch-artifact",
    "--watch-artifact-no-autoreact",
    "--workload",
    "-n",
];

/// claude's boolean short flags (`-c`, `-h`, `-p`, `-v`): the only shorts a
/// cluster may hold before a trailing value-taking `n`.
const BOOLEAN_SHORTS: &str = "chpv";

/// Root flags whose value is optional: they never take a word that starts
/// with `-` (`claude -r --settings f` still reads the settings flag), so only
/// a bare word after one is its value.
const OPTIONAL_VALUE_FLAGS: &[&str] = &[
    "-r",
    "--resume",
    "-d",
    "--debug",
    "-w",
    "--worktree",
    "--from-pr",
    "--teleport",
    "--cloud",
    "--remote-control",
    "--prompt-suggestions",
];

/// What one [`fold_args`] walk of the args yields.
struct FoldedArgs {
    /// The last user `--settings` value.
    settings: Option<String>,
    /// The args with every `--settings` flag (and its value) removed.
    clean: Vec<String>,
    /// The last non-blank `--advisor` value, both spellings.
    advisor: Option<String>,
}

/// Split `passthrough` into the last user `--settings` value (folded in) and
/// the args with every `--settings` flag (and its value) removed. See
/// [`fold_args`] for how the words are walked; a `--settings` flag with no
/// word left to take is `Err`.
pub(crate) fn fold_settings_args(
    passthrough: &[String],
) -> Result<(Option<String>, Vec<String>), ()> {
    fold_args(passthrough).map(|folded| (folded.settings, folded.clean))
}

/// The model the last `--advisor` flag in `args` names, in both spellings.
/// Read off the same walk that folds `--settings` out ([`fold_args`]), so a
/// word another flag consumed as its value is never read as the advisor
/// (`--name --advisor opus` names the session `--advisor`), and nothing past
/// the `--` separator counts. A blank value is ignored: it names no model, so
/// it neither clears an earlier flag nor shadows the settings tiers, and the
/// launch demand over-counts rather than under-counts. An `Err` walk (a bare
/// `--settings` claude refuses) reads no advisor.
pub(crate) fn last_advisor_arg(args: &[String]) -> Option<String> {
    fold_args(args).ok().and_then(|folded| folded.advisor)
}

/// Walk `passthrough` the way claude parses argv: a [`VALUE_FLAGS`] member
/// takes the next word whatever it holds, a short cluster ending in exactly
/// one `n` takes the next word, an [`OPTIONAL_VALUE_FLAGS`] member takes only a
/// word without a leading `-`, and the first `--` that sits in a flag position
/// (never a taken value) ends option parsing. One walk carries both the folded
/// `--settings` and the last `--advisor`, so the two cannot disagree about
/// which words are flags.
fn fold_args(passthrough: &[String]) -> Result<FoldedArgs, ()> {
    let mut value: Option<String> = None;
    let mut advisor: Option<String> = None;
    let mut clean: Vec<String> = Vec::with_capacity(passthrough.len());
    let mut i = 0;
    let mut positional = false; // past the end-of-options marker
    while i < passthrough.len() {
        let arg = passthrough[i].as_str();
        if positional {
            clean.push(passthrough[i].clone());
            i += 1;
            continue;
        }
        if arg == "--" {
            positional = true;
            clean.push(arg.to_string());
            i += 1;
            continue;
        }
        if let Some(rest) = arg.strip_prefix("--settings=") {
            value = Some(rest.to_string());
            i += 1;
            continue;
        }
        if arg == "--settings" {
            match passthrough.get(i + 1) {
                // claude hands the flag the next word whatever it holds
                Some(next) => {
                    value = Some(next.clone());
                    i += 2;
                }
                None => return Err(()),
            }
            continue;
        }
        if let Some(rest) = arg.strip_prefix("--advisor=") {
            note_advisor(&mut advisor, rest);
            clean.push(arg.to_string());
            i += 1;
            continue;
        }
        if VALUE_FLAGS.contains(&arg) {
            // the flag and its value stay in the passthrough: only a
            // `--settings` flag itself is folded out
            clean.push(arg.to_string());
            i += match passthrough.get(i + 1) {
                Some(next) => {
                    if arg == "--advisor" {
                        note_advisor(&mut advisor, next);
                    }
                    clean.push(next.clone());
                    2
                }
                None => 1,
            };
            continue;
        }
        if OPTIONAL_VALUE_FLAGS.contains(&arg) {
            clean.push(arg.to_string());
            i += match passthrough.get(i + 1) {
                // an optional value is never a `-`-led word
                Some(next) if !next.starts_with('-') => {
                    clean.push(next.clone());
                    2
                }
                _ => 1,
            };
            continue;
        }
        if short_cluster_takes_value(arg) {
            clean.push(arg.to_string());
            if let Some(next) = passthrough.get(i + 1) {
                clean.push(next.clone());
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        clean.push(arg.to_string());
        i += 1;
    }
    Ok(FoldedArgs {
        settings: value,
        clean,
        advisor,
    })
}

/// Record one `--advisor` value, trimmed to match the settings values it is
/// compared with, as the last advisor, ignoring a blank one.
fn note_advisor(advisor: &mut Option<String>, value: &str) {
    let value = value.trim();
    if !value.is_empty() {
        *advisor = Some(value.to_owned());
    }
}

/// Whether a short cluster takes the next word: `-n` is claude's only
/// value-taking short, and inside a cluster it takes a value only as the
/// cluster's last letter after boolean shorts (`-pn x` yes; `-pnn`, `-np` and
/// `-pdn` no — the first `n`, or an optional-value short, takes the rest of
/// the cluster as its value). `-n` alone is in [`VALUE_FLAGS`].
fn short_cluster_takes_value(arg: &str) -> bool {
    let Some(rest) = arg.strip_prefix('-') else {
        return false;
    };
    // every char before the trailing `n` must be a boolean short, which also
    // rules out a long flag, a joined `=` value and a malformed word
    rest.len() > 1
        && rest.ends_with('n')
        && rest[..rest.len() - 1]
            .chars()
            .all(|c| BOOLEAN_SHORTS.contains(c))
}

/// Whether a `--settings` value is inline JSON (Claude Code's own rule): a
/// trimmed value that starts with `{` AND ends with `}`. Anything else is a
/// settings file path. Shared by [`apply_spawn_settings`] and the job-store
/// reader, so both parse one value the same way.
pub(crate) fn is_inline_settings(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.starts_with('{') && trimmed.ends_with('}')
}

/// The user's `--settings` value as a JSON object, inline JSON or a settings
/// file path resolved against `cwd` ([`is_inline_settings`] picks). The `Err`
/// is the full, already-rendered warning.
pub(crate) fn parse_settings_value(
    value: &str,
    cwd: &Path,
    inline: bool,
) -> Result<Map<String, Value>, String> {
    let trimmed = value.trim();
    if inline {
        parse_json_object(trimmed).map_err(|detail| inline_warning(&detail))
    } else {
        let path = Path::new(trimmed);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        let content =
            crate::profile::read_capped_file(&path, SETTINGS_FILE_CAP).map_err(|detail| {
                path_warning(
                    value,
                    &format!("cannot read it ({})", strip_os_error(&detail)),
                )
            })?;
        parse_json_object(&content).map_err(|detail| path_warning(value, &detail.refusal()))
    }
}

/// Why a `--settings` value is not a JSON object.
enum NotSettingsObject {
    /// The text parses but is not an object (`[1,2]`, `"str"`).
    NotAnObject,
    /// The text does not parse, carrying its position.
    Unparseable(Option<crate::format::ParsePosition>),
}

impl NotSettingsObject {
    /// The `not a settings object` refusal: `(line L, column C)` — or
    /// `(line L)` for a column-0 error — for a parse error (bare when there is
    /// no position), or the fixed `(not a JSON object)` for a value that
    /// parses but is not an object.
    fn refusal(&self) -> String {
        match self {
            NotSettingsObject::NotAnObject => {
                "not a settings object (not a JSON object)".to_string()
            }
            NotSettingsObject::Unparseable(pos) => {
                crate::format::parse_refusal("not a settings object", *pos).to_string()
            }
        }
    }
}

fn parse_json_object(text: &str) -> Result<Map<String, Value>, NotSettingsObject> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(NotSettingsObject::NotAnObject),
        Err(e) => Err(NotSettingsObject::Unparseable(
            crate::format::json_parse_position(&e),
        )),
    }
}

/// `Some(field)` when the user's `hooks` shape is one clauth refuses to merge:
/// a non-object `hooks` or a non-array `hooks.PreToolUse`. The `field` text is
/// the approved copy's named half, not a key alone.
fn hooks_shape_error(map: &Map<String, Value>) -> Option<String> {
    match map.get("hooks") {
        None => None,
        Some(Value::Object(hooks)) => match hooks.get("PreToolUse") {
            None | Some(Value::Array(_)) => None,
            Some(_) => Some("hooks.PreToolUse is not an array".to_string()),
        },
        Some(_) => Some("hooks is not an object".to_string()),
    }
}

/// Whether a settings file sets a non-null `autoMemoryDirectory`. An
/// unreadable, special, oversize or unparseable file counts as not setting it.
fn settings_file_sets_memory(path: &Path) -> bool {
    let Ok(content) = crate::profile::read_capped_file(path, SETTINGS_FILE_CAP) else {
        return false;
    };
    match serde_json::from_str::<Value>(&content) {
        Ok(Value::Object(map)) => sets_memory(&map),
        _ => false,
    }
}

/// Whether a settings object sets a non-null `autoMemoryDirectory`.
fn sets_memory(map: &Map<String, Value>) -> bool {
    map.get("autoMemoryDirectory").is_some_and(|v| !v.is_null())
}

/// The child's value for `var`: an explicit `env_remove` counts as unset
/// (`None`), else the inherited process env is the answer.
fn child_env_value(command: &Command, var: &str) -> Option<std::ffi::OsString> {
    let key = std::ffi::OsStr::new(var);
    if let Some((_, value)) = command.get_envs().find(|(k, _)| *k == key) {
        return value.map(std::ffi::OsStr::to_os_string);
    }
    std::env::var_os(var)
}

/// Claude Code honours `CLAUDE_CODE_PROJECT_DIR_NAME` only when it matches
/// `^[A-Za-z0-9_-]{1,64}$` and is not a Windows reserved device name
/// (`^(?:con|prn|aux|nul|com[0-9]|lpt[0-9])$`, case-insensitive).
fn valid_project_dir_name(value: &std::ffi::OsStr) -> bool {
    let Some(v) = value.to_str() else {
        return false;
    };
    if v.is_empty() || v.len() > 64 {
        return false;
    }
    if !v
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return false;
    }
    let lower = v.to_ascii_lowercase();
    if matches!(lower.as_str(), "con" | "prn" | "aux" | "nul") {
        return false;
    }
    if lower.len() == 4
        && (lower.starts_with("com") || lower.starts_with("lpt"))
        && lower.as_bytes()[3].is_ascii_digit()
    {
        return false;
    }
    true
}

/// The path-value warning: the value is echoed as the user typed it, Debug
/// quoted, never interpolated raw.
fn path_warning(value: &str, reason: &str) -> String {
    format!("clauth: --settings {value:?}: {reason}; {TAIL}")
}

/// The inline-JSON warning: the value is never echoed.
fn inline_warning(detail: &NotSettingsObject) -> String {
    format!("clauth: --settings JSON is {}; {TAIL}", detail.refusal())
}

/// The malformed-`hooks` warning: the value IS a settings object, one field
/// has the wrong shape. Inline never echoes the value; the path form echoes it
/// as the user typed it.
fn hooks_warning(inline: bool, value: &str, field: &str) -> String {
    if inline {
        format!("clauth: --settings JSON: {field}; {TAIL}")
    } else {
        format!("clauth: --settings {value:?}: {field}; {TAIL}")
    }
}

/// The `hook-read-exempt` verdict for one `PreToolUse` payload: `Some(stdout)`
/// allowing the call when it is a `Read` whose resolved target lies inside the
/// session's own resolved `tool-results` dir, else `None` (print nothing, so
/// Claude Code's own decision stands).
pub(crate) fn read_hook_verdict(payload: &str) -> Option<String> {
    let payload: ReadHookPayload = serde_json::from_str(payload).ok()?;
    if payload.tool_name != "Read" {
        return None;
    }
    let sid = &payload.session_id;
    if sid.is_empty() || !sid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    let transcript = Path::new(&payload.transcript_path);
    if transcript.file_name()?.to_str()? != format!("{sid}.jsonl").as_str() {
        return None;
    }
    let file = Path::new(&payload.tool_input.file_path);
    if !file.is_absolute() {
        return None;
    }
    let file = crate::canonical::identity(file).ok()?;
    let results_dir = transcript.parent()?.join(sid).join("tool-results");
    let results_dir = crate::canonical::identity(&results_dir).ok()?;
    if file.starts_with(results_dir.as_path()) {
        Some(READ_ALLOW.to_string())
    } else {
        None
    }
}

#[derive(serde::Deserialize)]
struct ReadHookPayload {
    session_id: String,
    transcript_path: String,
    tool_name: String,
    tool_input: ReadHookToolInput,
}

#[derive(serde::Deserialize)]
struct ReadHookToolInput {
    file_path: String,
}

/// `clauth hook-read-exempt`: read the payload on stdin, print the verdict.
/// Always exits 0: a hook exit of 2 would block the read.
pub(crate) fn run_read_hook() {
    // Bound the stdin read to 1 MiB. `read_to_end` + `from_utf8_lossy`, never
    // `read_to_string` on the truncated reader: a cut UTF-8 codepoint would
    // read as an error and the whole payload would be dropped.
    let mut bytes = Vec::new();
    let stdin = std::io::stdin().lock();
    let _ = stdin.take(1 << 20).read_to_end(&mut bytes);
    let payload = String::from_utf8_lossy(&bytes);
    if let Some(verdict) = read_hook_verdict(&payload) {
        outln!("{verdict}");
    }
}

#[cfg(test)]
#[path = "../tests/inline/cc_exempt.rs"]
mod tests;
