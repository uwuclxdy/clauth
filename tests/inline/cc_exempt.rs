#![allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)]

use super::*;

use serde_json::{Value, json};
use std::fs;

const HOOK: &str = "'/opt/clauth bin/clauth' hook-read-exempt || true";

const TAIL: &str = "passed to claude unchanged, so auto-memory writes and tool-result reads may prompt this session";

// ── project key ──────────────────────────────────────────────────────────────
//
// Expected values come from Claude Code 2.1.300's own `FM` function, extracted
// from the shipped binary and run under node on these exact inputs.

#[test]
fn project_key_matches_claude_code_on_a_short_path() {
    assert_eq!(project_key("/home/u/repos/a.b"), "-home-u-repos-a-b");
    assert_eq!(
        project_key("/home/uwuclxdy/repos/rs/clauth"),
        "-home-uwuclxdy-repos-rs-clauth"
    );
}

#[test]
fn project_key_replaces_each_utf16_unit_of_non_ascii() {
    // `ü` and `ï` are one UTF-16 unit each, the emoji is a surrogate pair: two.
    assert_eq!(project_key("/tmp/ünï code/😀x"), "-tmp--n--code---x");
}

#[test]
fn project_key_caps_at_200_units_and_appends_the_hash() {
    let long = format!("/home/u/{}", "abcdefghij/".repeat(24));
    // 8 units of `-home-u-` + 17 whole `abcdefghij-` + `abcde` = the 200 cap.
    let expected = format!("-home-u-{}abcde-pw2vet", "abcdefghij-".repeat(17));
    assert_eq!(project_key(&long), expected);
    assert_eq!(project_key(&long).len(), 207);
}

#[test]
fn project_key_hashes_the_unsanitized_input() {
    // Every `ü` sanitizes to `-`, so only a hash over the original units can
    // yield this suffix.
    let input = format!("/home/u/{}", "ü".repeat(205));
    assert_eq!(
        project_key(&input),
        format!("-home-u-{}-rj2eld", "-".repeat(192))
    );
}

// ── memory base ──────────────────────────────────────────────────────────────
//
// Expected bases were measured off Claude Code 2.1.300 itself (its debug log
// names the memory dir it resolved) on this exact fixture shape: a repo with a
// subdir, a linked worktree of it (`.git` file + `commondir` + back-pointing
// `gitdir`, as `git worktree add` writes them), a symlink into the repo, a
// nested repo, a non-git dir, a worktree of a bare repo, a submodule checkout,
// a worktree of a submodule, and a worktree whose back-pointer is stale.

struct Tree {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

fn write(path: PathBuf, body: String) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

/// A linked worktree at `root/wt` of the repo whose common dir is
/// `root/common`, with `back` as the worktree admin dir's `gitdir` line.
fn worktree(root: &Path, wt: &str, common: &str, back: &Path) {
    let admin = root.join(common).join("worktrees").join(wt);
    write(admin.join("commondir"), "../..\n".to_string());
    write(admin.join("gitdir"), format!("{}\n", back.display()));
    write(
        root.join(wt).join(".git"),
        format!("gitdir: {}\n", admin.display()),
    );
}

fn tree() -> Tree {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    fs::create_dir_all(root.join("repo/sub/deeper")).unwrap();
    fs::create_dir_all(root.join("repo/.git")).unwrap();
    worktree(&root, "wt", "repo/.git", &root.join("wt/.git"));
    fs::create_dir_all(root.join("wt/sub")).unwrap();
    fs::create_dir_all(root.join("repo/nested/.git")).unwrap();
    fs::create_dir_all(root.join("plain/x")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join("repo/sub"), root.join("link")).unwrap();

    // A bare repo: its common dir is the repo dir itself, not a `.git`.
    write(
        root.join("proj.git/HEAD"),
        "ref: refs/heads/main\n".to_string(),
    );
    worktree(&root, "bwt", "proj.git", &root.join("bwt/.git"));

    // A submodule: its git dir lives under the superproject's `.git/modules`.
    fs::create_dir_all(root.join("super/.git/modules/sm")).unwrap();
    write(
        root.join("super/sm/.git"),
        "gitdir: ../.git/modules/sm\n".to_string(),
    );
    worktree(
        &root,
        "smwt",
        "super/.git/modules/sm",
        &root.join("smwt/.git"),
    );

    // A worktree whose admin dir points back at some other checkout.
    fs::create_dir_all(root.join("r/.git")).unwrap();
    worktree(&root, "w", "r/.git", Path::new("/somewhere/else/w/.git"));

    // `git worktree add --relative-paths` (git 2.48+): both pointers relative.
    fs::create_dir_all(root.join("rr/.git/worktrees/rwt")).unwrap();
    write(
        root.join("rr/.git/worktrees/rwt/commondir"),
        "../..\n".to_string(),
    );
    write(
        root.join("rr/.git/worktrees/rwt/gitdir"),
        "../../../../rwt/.git\n".to_string(),
    );
    write(
        root.join("rwt/.git"),
        "gitdir: ../rr/.git/worktrees/rwt\n".to_string(),
    );

    // A worktree admin dir that is not under `<common>/worktrees`.
    write(
        root.join("q/.git/HEAD"),
        "ref: refs/heads/main\n".to_string(),
    );
    write(root.join("q/.git/odd/qwt/commondir"), "../..\n".to_string());
    write(
        root.join("q/.git/odd/qwt/gitdir"),
        format!("{}\n", root.join("qwt/.git").display()),
    );
    write(
        root.join("qwt/.git"),
        format!("gitdir: {}\n", root.join("q/.git/odd/qwt").display()),
    );

    // A common dir not named `.git` that itself holds a `.git`.
    fs::create_dir_all(root.join("p2.git/.git")).unwrap();
    worktree(&root, "pwt", "p2.git", &root.join("pwt/.git"));

    // The worktree's `.git` names its admin dir through a symlinked alias of
    // the tree; the admin dir's back-pointer uses the real spelling.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&root, root.join("alias")).unwrap();
        fs::create_dir_all(root.join("sr/.git/worktrees/swt")).unwrap();
        write(
            root.join("sr/.git/worktrees/swt/commondir"),
            "../..\n".to_string(),
        );
        write(
            root.join("sr/.git/worktrees/swt/gitdir"),
            format!("{}\n", root.join("swt/.git").display()),
        );
        write(
            root.join("swt/.git"),
            format!(
                "gitdir: {}\n",
                root.join("alias/sr/.git/worktrees/swt").display()
            ),
        );

        // The reverse: a real `.git`, a back-pointer spelled through the alias.
        worktree(&root, "bpwt", "br/.git", &root.join("alias/bpwt/.git"));
    }

    Tree { _tmp: tmp, root }
}

#[test]
fn memory_base_is_the_git_root_from_anywhere_inside_the_repo() {
    let t = tree();
    assert_eq!(memory_base(&t.root.join("repo")), t.root.join("repo"));
    assert_eq!(
        memory_base(&t.root.join("repo/sub/deeper")),
        t.root.join("repo")
    );
}

#[test]
fn memory_base_of_a_linked_worktree_is_its_main_repo() {
    let t = tree();
    assert_eq!(memory_base(&t.root.join("wt")), t.root.join("repo"));
    assert_eq!(memory_base(&t.root.join("wt/sub")), t.root.join("repo"));
}

#[test]
fn memory_base_of_a_nested_repo_is_its_own_root() {
    let t = tree();
    assert_eq!(
        memory_base(&t.root.join("repo/nested")),
        t.root.join("repo/nested")
    );
}

#[test]
fn memory_base_outside_git_is_the_cwd_itself() {
    let t = tree();
    assert_eq!(memory_base(&t.root.join("plain/x")), t.root.join("plain/x"));
}

#[cfg(unix)]
#[test]
fn memory_base_resolves_a_symlinked_cwd_before_the_git_walk() {
    let t = tree();
    assert_eq!(memory_base(&t.root.join("link")), t.root.join("repo"));
}

#[test]
fn memory_base_of_a_bare_repo_worktree_is_the_bare_repo_dir() {
    let t = tree();
    assert_eq!(memory_base(&t.root.join("bwt")), t.root.join("proj.git"));
}

#[test]
fn memory_base_of_a_submodule_checkout_is_the_checkout() {
    let t = tree();
    assert_eq!(
        memory_base(&t.root.join("super/sm")),
        t.root.join("super/sm")
    );
}

#[test]
fn memory_base_of_a_submodule_worktree_is_the_submodule_git_dir() {
    let t = tree();
    assert_eq!(
        memory_base(&t.root.join("smwt")),
        t.root.join("super/.git/modules/sm")
    );
}

#[test]
fn memory_base_of_a_worktree_with_a_stale_back_pointer_is_the_worktree() {
    let t = tree();
    assert_eq!(memory_base(&t.root.join("w")), t.root.join("w"));
}

#[test]
fn memory_base_of_a_relative_path_worktree_is_its_main_repo() {
    // Measured: Claude Code resolves the relative `gitdir:` against the
    // worktree and keys the main repo.
    let t = tree();
    assert_eq!(memory_base(&t.root.join("rwt")), t.root.join("rr"));
}

#[cfg(unix)]
#[test]
fn memory_base_keeps_the_gitdir_spelling_it_resolved_through() {
    // Measured: a `.git` naming its admin dir through a symlinked alias keys
    // the alias spelling of the main repo; Claude Code's `Nn` is identity, so
    // the returned base is never realpath'd.
    let t = tree();
    assert_eq!(memory_base(&t.root.join("swt")), t.root.join("alias/sr"));
}

#[cfg(unix)]
#[test]
fn memory_base_resolves_symlinks_on_both_sides_of_the_back_pointer() {
    // Measured: a back-pointer spelled through the alias still names this
    // worktree, because Claude Code realpaths both sides of that compare.
    let t = tree();
    assert_eq!(memory_base(&t.root.join("bpwt")), t.root.join("br"));
}

#[test]
fn memory_base_of_a_worktree_admin_dir_outside_worktrees_is_the_worktree() {
    // `kr`: `dirname(admin)` must equal `<common>/worktrees`, else the root.
    let t = tree();
    assert_eq!(memory_base(&t.root.join("qwt")), t.root.join("qwt"));
}

#[test]
fn memory_base_of_a_non_dot_git_common_dir_holding_a_git_is_the_worktree() {
    // `kr`: a common dir not named `.git` is the base only when it holds no
    // `.git` of its own.
    let t = tree();
    assert_eq!(memory_base(&t.root.join("pwt")), t.root.join("pwt"));
}

// ── spawn settings ───────────────────────────────────────────────────────────

/// A shared-runtime layout: `<tmp>/claude/projects` is the real store and
/// `<tmp>/runtime/projects` links to it, as `materialize_entry` lays it out.
/// The session runs in `<tmp>/work/repo/sub`, inside a git repo. Holds a home
/// sandbox: the builder asks whether the cwd is the real `$HOME`.
struct Layout {
    home: crate::testutil::HomeSandbox,
    _tmp: tempfile::TempDir,
    root: PathBuf,
    runtime: PathBuf,
    store: PathBuf,
    cwd: PathBuf,
    settings_file: PathBuf,
}

fn layout(link_projects: bool) -> Layout {
    let home = crate::testutil::HomeSandbox::new();
    let tmp = tempfile::tempdir().unwrap();
    // Claude Code's spelling, so `memory_dir` and `store` expect what the
    // settings file carries on Windows too.
    let root = crate::testutil::node_path(tmp.path());
    let store = root.join("claude").join("projects");
    fs::create_dir_all(&store).unwrap();
    let runtime = root.join("runtime");
    fs::create_dir_all(&runtime).unwrap();
    if link_projects {
        #[cfg(unix)]
        std::os::unix::fs::symlink(&store, runtime.join("projects")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&store, runtime.join("projects")).unwrap();
    } else {
        fs::create_dir_all(runtime.join("projects")).unwrap();
    }
    fs::create_dir_all(root.join("work/repo/.git")).unwrap();
    let cwd = root.join("work/repo/sub");
    fs::create_dir_all(&cwd).unwrap();
    let settings_file = root.join("state").join("spawn-settings.json");
    fs::create_dir_all(settings_file.parent().unwrap()).unwrap();
    Layout {
        home,
        _tmp: tmp,
        root,
        runtime,
        store,
        cwd,
        settings_file,
    }
}

fn args_of(command: &Command) -> Vec<String> {
    command
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).to_string()).collect()
}

fn our_hook() -> Value {
    json!({
        "matcher": "Read",
        "hooks": [{ "type": "command", "command": HOOK, "timeout": 10 }]
    })
}

/// The memory dir Claude Code keys for `l.cwd`: the git root `work/repo`,
/// spelled by hand so a builder keying on the raw cwd cannot pass.
fn memory_dir(l: &Layout) -> String {
    let root = l
        .root
        .to_string_lossy()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "-");
    l.store
        .join(format!("{root}-work-repo"))
        .join("memory")
        .to_string_lossy()
        .into_owned()
}

/// What one `apply_spawn_settings` call produced: the parsed settings file
/// `--settings` names (asserting exactly one flag, first, naming
/// `l.settings_file`), the args after it, and the warning.
struct Applied {
    settings: Option<Value>,
    rest: Vec<String>,
    warning: Option<String>,
    argv: Vec<String>,
}

fn apply_with(l: &Layout, command: &mut Command, passthrough: &[&str]) -> Applied {
    let warning = apply_spawn_settings(
        command,
        &l.runtime,
        &l.cwd,
        &strings(passthrough),
        HOOK,
        &l.settings_file,
    );
    let argv = args_of(command);
    // Ours is the leading `--settings` naming `l.settings_file`; a pass-through
    // run may lead with the user's own `--settings <value>` instead.
    let injected = argv.first().map(String::as_str) == Some("--settings")
        && argv.get(1).map(Path::new) == Some(l.settings_file.as_path());
    if injected {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&l.settings_file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "settings file mode {mode:o}");
        }
        let settings: Value =
            serde_json::from_str(&fs::read_to_string(&l.settings_file).unwrap()).unwrap();
        let rest = argv[2..].to_vec();
        assert!(
            crate::cc_exempt::fold_settings_args(&rest) == Ok((None, rest.clone())),
            "a second --settings would replace ours: {rest:?}"
        );
        Applied {
            settings: Some(settings),
            rest,
            warning,
            argv,
        }
    } else {
        Applied {
            settings: None,
            rest: argv.clone(),
            warning,
            argv,
        }
    }
}

fn apply(l: &Layout, passthrough: &[&str]) -> Applied {
    apply_with(l, &mut Command::new("claude"), passthrough)
}

#[test]
fn a_symlinked_projects_dir_gets_memory_dir_and_read_hook() {
    let l = layout(true);
    let a = apply(&l, &["--model", "opus", "-p", "hi"]);
    assert_eq!(
        a.settings,
        Some(json!({
            "autoMemoryDirectory": memory_dir(&l),
            "hooks": { "PreToolUse": [our_hook()] }
        }))
    );
    assert_eq!(a.rest, strings(&["--model", "opus", "-p", "hi"]));
    assert_eq!(a.warning, None);
}

#[test]
fn the_memory_dir_names_the_resolved_store_not_the_runtime_spelling() {
    let l = layout(true);
    let a = apply(&l, &[]);
    let dir = a.settings.unwrap()["autoMemoryDirectory"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(Path::new(&dir).starts_with(&l.store), "{dir}");
    assert!(!Path::new(&dir).starts_with(&l.runtime), "{dir}");
}

#[test]
fn a_fully_resolved_runtime_gets_nothing_injected() {
    let l = layout(false);
    let passthrough = strings(&["--settings", "{\"model\":\"x\"}", "-p", "hi"]);
    let mut command = Command::new("claude");
    let warning = apply_spawn_settings(
        &mut command,
        &l.runtime,
        &l.cwd,
        &passthrough,
        HOOK,
        &l.settings_file,
    );
    assert_eq!(args_of(&command), passthrough);
    assert_eq!(warning, None);
    assert!(!l.settings_file.exists());
}

#[cfg(unix)]
#[test]
fn a_runtime_reached_through_a_symlinked_parent_is_injected_too() {
    // `~/.clauth` (or `$HOME`) behind a symlink: `projects` is a real dir, but
    // its literal spelling still differs from its realpath.
    let l = layout(false);
    let alias = l.root.join("alias");
    std::os::unix::fs::symlink(&l.root, &alias).unwrap();
    let mut command = Command::new("claude");
    let warning = apply_spawn_settings(
        &mut command,
        &alias.join("runtime"),
        &l.cwd,
        &[],
        HOOK,
        &l.settings_file,
    );
    assert_eq!(warning, None);
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(&l.settings_file).unwrap()).unwrap();
    let root = l
        .root
        .to_string_lossy()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "-");
    let expected = l
        .runtime
        .join("projects")
        .join(format!("{root}-work-repo"))
        .join("memory");
    assert_eq!(
        settings["autoMemoryDirectory"],
        json!(expected.to_string_lossy())
    );
    assert_eq!(settings["hooks"]["PreToolUse"], json!([our_hook()]));
}

#[cfg(unix)]
#[test]
fn an_isolated_runtime_without_projects_yet_is_judged_by_its_parent() {
    // Isolated: Claude Code creates `projects` itself, after the spawn.
    let l = layout(false);
    fs::remove_dir(l.runtime.join("projects")).unwrap();
    let alias = l.root.join("alias");
    std::os::unix::fs::symlink(&l.root, &alias).unwrap();

    let mut through_link = Command::new("claude");
    apply_spawn_settings(
        &mut through_link,
        &alias.join("runtime"),
        &l.cwd,
        &[],
        HOOK,
        &l.settings_file,
    );
    assert_eq!(args_of(&through_link)[0], "--settings");

    let direct_file = l.root.join("state").join("direct.json");
    let mut direct = Command::new("claude");
    apply_spawn_settings(&mut direct, &l.runtime, &l.cwd, &[], HOOK, &direct_file);
    assert!(args_of(&direct).is_empty());
    assert!(!direct_file.exists());
}

#[test]
fn a_user_settings_json_string_is_folded_in() {
    let l = layout(true);
    let a = apply(
        &l,
        &[
            "--model",
            "opus",
            "--settings",
            "{\"model\":\"x\",\"theme\":\"dark\"}",
            "-p",
            "hi",
        ],
    );
    assert_eq!(
        a.settings,
        Some(json!({
            "model": "x",
            "theme": "dark",
            "autoMemoryDirectory": memory_dir(&l),
            "hooks": { "PreToolUse": [our_hook()] }
        }))
    );
    assert_eq!(a.rest, strings(&["--model", "opus", "-p", "hi"]));
}

#[test]
fn the_equals_form_is_folded_in() {
    let l = layout(true);
    let a = apply(&l, &["--settings={\"theme\":\"dark\"}", "-p", "hi"]);
    assert_eq!(a.settings.unwrap()["theme"], json!("dark"));
    assert_eq!(a.rest, strings(&["-p", "hi"]));
}

#[test]
fn a_user_settings_file_is_read_relative_to_cwd_and_folded_in() {
    let l = layout(true);
    fs::write(l.cwd.join("extra.json"), "{\"theme\":\"light\"}").unwrap();
    let a = apply(&l, &["--settings", "extra.json"]);
    assert_eq!(a.settings.unwrap()["theme"], json!("light"));
    assert!(a.rest.is_empty(), "{:?}", a.rest);
}

#[test]
fn folded_settings_never_reach_the_command_line() {
    let l = layout(true);
    fs::write(
        l.cwd.join("creds.json"),
        "{\"env\":{\"ANTHROPIC_AUTH_TOKEN\":\"folded-marker-file\"}}",
    )
    .unwrap();
    for user in [
        "creds.json",
        "{\"env\":{\"ANTHROPIC_AUTH_TOKEN\":\"folded-marker-inline\"}}",
    ] {
        let a = apply(&l, &["--settings", user]);
        assert!(
            a.argv.iter().all(|arg| !arg.contains("folded-marker")),
            "{user}: {:?}",
            a.argv
        );
        let settings = a.settings.unwrap();
        assert!(
            settings["env"]["ANTHROPIC_AUTH_TOKEN"]
                .as_str()
                .unwrap()
                .contains("folded-marker"),
            "{user}: the folded value lives in the file"
        );
    }
}

#[test]
fn the_last_user_settings_wins_like_claude_code() {
    let l = layout(true);
    let a = apply(
        &l,
        &[
            "--settings",
            "{\"theme\":\"a\"}",
            "--settings",
            "{\"theme\":\"b\"}",
        ],
    );
    assert_eq!(a.settings.unwrap()["theme"], json!("b"));
    assert!(a.rest.is_empty(), "{:?}", a.rest);
}

#[test]
fn user_pre_tool_use_hooks_stay_and_ours_is_appended() {
    let l = layout(true);
    let theirs = json!({ "matcher": "Bash", "hooks": [{ "type": "command", "command": "true" }] });
    let user = json!({ "hooks": { "PreToolUse": [theirs.clone()], "Stop": [] } }).to_string();
    let a = apply(&l, &["--settings", &user]);
    let settings = a.settings.unwrap();
    assert_eq!(settings["hooks"]["PreToolUse"], json!([theirs, our_hook()]));
    assert_eq!(settings["hooks"]["Stop"], json!([]));
}

#[test]
fn a_user_memory_dir_in_flag_settings_is_kept() {
    let l = layout(true);
    let a = apply(&l, &["--settings", "{\"autoMemoryDirectory\":\"/mine\"}"]);
    let settings = a.settings.unwrap();
    assert_eq!(settings["autoMemoryDirectory"], json!("/mine"));
    assert_eq!(settings["hooks"]["PreToolUse"], json!([our_hook()]));
}

#[test]
fn a_null_user_memory_dir_counts_as_unset() {
    let l = layout(true);
    let a = apply(&l, &["--settings", "{\"autoMemoryDirectory\":null}"]);
    assert_eq!(
        a.settings.unwrap()["autoMemoryDirectory"],
        json!(memory_dir(&l))
    );
}

#[test]
fn a_memory_dir_in_any_settings_file_suppresses_ours() {
    for file in [
        ("runtime", "settings.json"),
        ("project", "settings.json"),
        ("project", "settings.local.json"),
    ] {
        let l = layout(true);
        let path = match file.0 {
            "runtime" => l.runtime.join(file.1),
            _ => {
                fs::create_dir_all(l.cwd.join(".claude")).unwrap();
                l.cwd.join(".claude").join(file.1)
            }
        };
        fs::write(&path, "{\"autoMemoryDirectory\":\"/elsewhere\"}").unwrap();
        let a = apply(&l, &[]);
        assert_eq!(
            a.settings,
            Some(json!({ "hooks": { "PreToolUse": [our_hook()] } })),
            "{file:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_memory_dir_in_the_git_root_local_tier_suppresses_ours() {
    // On unix Claude Code reads the local tier at the git root, with the
    // launch dir's file merged beneath it as legacy local settings.
    let l = layout(true);
    let dir = l.root.join("work/repo/.claude");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("settings.local.json"),
        "{\"autoMemoryDirectory\":\"/elsewhere\"}",
    )
    .unwrap();
    let a = apply(&l, &[]);
    assert_eq!(
        a.settings,
        Some(json!({ "hooks": { "PreToolUse": [our_hook()] } }))
    );
}

#[cfg(windows)]
#[test]
fn a_memory_dir_in_the_git_root_local_tier_does_not_suppress_ours_on_windows() {
    // Windows has no uids for Claude Code's ownership check, so its local tier
    // stays at the launch dir and a root file is never read from `sub`.
    let l = layout(true);
    let dir = l.root.join("work/repo/.claude");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("settings.local.json"),
        "{\"autoMemoryDirectory\":\"/elsewhere\"}",
    )
    .unwrap();
    let a = apply(&l, &[]);
    assert_eq!(
        a.settings.unwrap()["autoMemoryDirectory"],
        json!(memory_dir(&l))
    );
}

#[test]
fn a_memory_dir_in_the_git_root_project_tier_does_not_suppress_ours() {
    // Only the local tier moves to the git root; the project tier stays at
    // the launch dir, so a root `settings.json` is never read from `sub`.
    let l = layout(true);
    let dir = l.root.join("work/repo/.claude");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("settings.json"),
        "{\"autoMemoryDirectory\":\"/elsewhere\"}",
    )
    .unwrap();
    let a = apply(&l, &[]);
    assert_eq!(
        a.settings.unwrap()["autoMemoryDirectory"],
        json!(memory_dir(&l))
    );
}

#[cfg(unix)]
#[test]
fn a_memory_dir_in_a_worktrees_main_checkout_local_tier_suppresses_ours() {
    // A linked worktree at `work/wt` of `work/repo`: `memory_base` resolves it
    // to the main checkout, whose local tier is the one Claude Code keys.
    let l = layout(true);
    worktree(
        &l.root.join("work"),
        "wt",
        "repo/.git",
        &l.root.join("work/wt/.git"),
    );
    fs::create_dir_all(l.root.join("work/repo/.claude")).unwrap();
    fs::write(
        l.root.join("work/repo/.claude/settings.local.json"),
        "{\"autoMemoryDirectory\":\"/elsewhere\"}",
    )
    .unwrap();
    let mut command = Command::new("claude");
    apply_spawn_settings(
        &mut command,
        &l.runtime,
        &l.root.join("work/wt"),
        &[],
        HOOK,
        &l.settings_file,
    );
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(&l.settings_file).unwrap()).unwrap();
    assert_eq!(settings, json!({ "hooks": { "PreToolUse": [our_hook()] } }));
}

#[test]
fn a_memory_dir_in_a_worktrees_own_root_local_tier_does_not_suppress_ours() {
    // From `work/wt/sub` Claude Code reads the main checkout's local tier and
    // the launch dir's legacy one, never the worktree root's.
    let l = layout(true);
    worktree(
        &l.root.join("work"),
        "wt",
        "repo/.git",
        &l.root.join("work/wt/.git"),
    );
    fs::create_dir_all(l.root.join("work/wt/.claude")).unwrap();
    fs::write(
        l.root.join("work/wt/.claude/settings.local.json"),
        "{\"autoMemoryDirectory\":\"/elsewhere\"}",
    )
    .unwrap();
    let cwd = l.root.join("work/wt/sub");
    fs::create_dir_all(&cwd).unwrap();
    let mut command = Command::new("claude");
    apply_spawn_settings(&mut command, &l.runtime, &cwd, &[], HOOK, &l.settings_file);
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(&l.settings_file).unwrap()).unwrap();
    assert_eq!(settings["autoMemoryDirectory"], json!(memory_dir(&l)));
}

#[test]
fn a_memory_dir_in_the_home_local_tier_does_not_suppress_ours_from_a_subdir() {
    // Claude Code never moves the local tier to a git root that is the real
    // `$HOME`, so `<home>/.claude/settings.local.json` is not read from a
    // subdir of home.
    let l = layout(true);
    let home = crate::testutil::node_path(l.home.home());
    fs::create_dir_all(home.join(".git")).unwrap();
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::write(
        home.join(".claude/settings.local.json"),
        "{\"autoMemoryDirectory\":\"/elsewhere\"}",
    )
    .unwrap();
    let cwd = home.join("sub");
    fs::create_dir_all(&cwd).unwrap();
    let mut command = Command::new("claude");
    apply_spawn_settings(&mut command, &l.runtime, &cwd, &[], HOOK, &l.settings_file);
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(&l.settings_file).unwrap()).unwrap();
    let key = home
        .to_string_lossy()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "-");
    assert_eq!(
        settings["autoMemoryDirectory"],
        json!(l.store.join(key).join("memory").to_string_lossy())
    );
}

#[test]
fn a_project_tier_claude_code_skips_at_home_does_not_suppress_ours() {
    // At the real `$HOME` the spawn gets `--setting-sources user`, so the
    // `<cwd>/.claude` project tier is never read; its key cannot count.
    let l = layout(true);
    let home = crate::testutil::node_path(l.home.home());
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::write(
        home.join(".claude/settings.json"),
        "{\"autoMemoryDirectory\":\"/elsewhere\"}",
    )
    .unwrap();
    let mut command = Command::new("claude");
    apply_spawn_settings(&mut command, &l.runtime, &home, &[], HOOK, &l.settings_file);
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(&l.settings_file).unwrap()).unwrap();
    let key = home
        .to_string_lossy()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "-");
    assert_eq!(
        settings["autoMemoryDirectory"],
        json!(l.store.join(key).join("memory").to_string_lossy())
    );
}

#[test]
fn an_env_override_of_the_memory_location_suppresses_ours() {
    for var in [
        "CLAUDE_CODE_PROJECT_DIR_NAME",
        "CLAUDE_CODE_REMOTE_MEMORY_DIR",
    ] {
        let l = layout(true);
        let mut command = Command::new("claude");
        command.env(var, "pinned");
        let a = apply_with(&l, &mut command, &[]);
        assert_eq!(
            a.settings,
            Some(json!({ "hooks": { "PreToolUse": [our_hook()] } })),
            "{var}"
        );
    }
}

#[test]
fn a_project_dir_name_claude_code_would_reject_does_not_suppress_ours() {
    // Claude Code honors it only on `^[A-Za-z0-9_-]{1,64}$` and never a
    // Windows reserved device name; anything else and it keys normally.
    let long = "a".repeat(65);
    for value in ["", "a/b", "a.b", long.as_str(), "con", "LPT1"] {
        let l = layout(true);
        let mut command = Command::new("claude");
        command.env("CLAUDE_CODE_PROJECT_DIR_NAME", value);
        let a = apply_with(&l, &mut command, &[]);
        assert_eq!(
            a.settings.unwrap()["autoMemoryDirectory"],
            json!(memory_dir(&l)),
            "{value:?}"
        );
    }
    let l = layout(true);
    let mut command = Command::new("claude");
    command.env("CLAUDE_CODE_PROJECT_DIR_NAME", "pinned-1_x");
    let a = apply_with(&l, &mut command, &[]);
    assert_eq!(a.settings.unwrap().get("autoMemoryDirectory"), None);
}

#[test]
fn an_empty_remote_memory_dir_does_not_suppress_ours() {
    // Claude Code tests it for truthiness: an empty value is unset to it.
    let l = layout(true);
    let mut command = Command::new("claude");
    command.env("CLAUDE_CODE_REMOTE_MEMORY_DIR", "");
    let a = apply_with(&l, &mut command, &[]);
    assert_eq!(
        a.settings.unwrap()["autoMemoryDirectory"],
        json!(memory_dir(&l))
    );
}

/// Sets a process env var for one test and restores it on drop, under the
/// `HOME_TEST_LOCK` the layout's sandbox holds.
struct EnvVar(&'static str, Option<std::ffi::OsString>);

impl EnvVar {
    #[expect(
        unsafe_code,
        reason = "env mutation is unsafe in Rust 2024; serialized by HOME_TEST_LOCK, held by the layout's sandbox"
    )]
    fn set(name: &'static str, value: &str) -> Self {
        let prev = std::env::var_os(name);
        // SAFETY: test-only, serialized by `HOME_TEST_LOCK`, restored on drop.
        unsafe { std::env::set_var(name, value) };
        Self(name, prev)
    }
}

impl Drop for EnvVar {
    #[expect(
        unsafe_code,
        reason = "env mutation is unsafe in Rust 2024; serialized by HOME_TEST_LOCK, held by the layout's sandbox"
    )]
    fn drop(&mut self) {
        // SAFETY: as in `set`.
        unsafe {
            match &self.1 {
                Some(prev) => std::env::set_var(self.0, prev),
                None => std::env::remove_var(self.0),
            }
        }
    }
}

#[test]
fn an_inherited_env_override_suppresses_ours_unless_the_spawn_removes_it() {
    for var in [
        "CLAUDE_CODE_PROJECT_DIR_NAME",
        "CLAUDE_CODE_REMOTE_MEMORY_DIR",
    ] {
        let l = layout(true);
        let _env = EnvVar::set(var, "pinned");

        let inherited = apply(&l, &[]);
        assert_eq!(
            inherited.settings.unwrap().get("autoMemoryDirectory"),
            None,
            "{var} inherited from this process reaches the child"
        );

        let mut command = Command::new("claude");
        command.env_remove(var);
        let removed = apply_with(&l, &mut command, &[]);
        assert_eq!(
            removed.settings.unwrap()["autoMemoryDirectory"],
            json!(memory_dir(&l)),
            "{var} removed from the child's env"
        );
    }
}

#[cfg(unix)]
#[test]
fn an_unreadable_user_settings_file_warns_and_passes_through() {
    let l = layout(true);
    let passthrough = ["--settings", "missing-file.json", "-p", "hi"];
    let a = apply(&l, &passthrough);
    assert_eq!(a.settings, None);
    assert_eq!(a.argv, strings(&passthrough));
    assert_eq!(
        a.warning.as_deref(),
        Some(
            format!(
                "clauth: --settings \"missing-file.json\": cannot read it (No such file or directory); {TAIL}"
            )
            .as_str()
        )
    );
}

#[test]
fn a_user_settings_file_that_is_not_an_object_warns_and_passes_through() {
    let l = layout(true);
    fs::write(l.cwd.join("list.json"), "[1,2]").unwrap();
    let passthrough = ["--settings", "list.json"];
    let a = apply(&l, &passthrough);
    assert_eq!(a.argv, strings(&passthrough));
    assert_eq!(
        a.warning.as_deref(),
        Some(
            format!(
                "clauth: --settings \"list.json\": not a settings object (not a JSON object); {TAIL}"
            )
            .as_str()
        )
    );
}

/// A settings file that does not parse names the position.
#[cfg(unix)]
#[test]
fn a_user_settings_file_that_does_not_parse_names_the_position() {
    let l = layout(true);
    fs::write(l.cwd.join("torn.json"), "{\"env\": \"sk-c2probe-NOTAKEY").unwrap();
    let passthrough = ["--settings", "torn.json"];
    let a = apply(&l, &passthrough);
    assert_eq!(a.argv, strings(&passthrough));
    assert_eq!(
        a.warning.as_deref(),
        Some(
            format!(
                "clauth: --settings \"torn.json\": not a settings object (line 1, column 27); {TAIL}"
            )
            .as_str()
        )
    );
}

#[test]
fn inline_settings_json_that_does_not_parse_warns_without_echoing_it() {
    let l = layout(true);
    let bad = "{\"token\":\"folded-marker\" oops}";
    let passthrough = ["--settings", bad];
    let a = apply(&l, &passthrough);
    assert_eq!(a.argv, strings(&passthrough));
    assert_eq!(
        a.warning.as_deref(),
        Some(
            format!("clauth: --settings JSON is not a settings object (line 1, column 26); {TAIL}")
                .as_str()
        )
    );
    assert!(!a.warning.unwrap().contains("folded-marker"));
}

#[cfg(unix)]
#[test]
fn a_brace_without_a_closing_brace_is_a_path_like_claude_code_reads_it() {
    let l = layout(true);
    let a = apply(&l, &["--settings", "{half"]);
    assert_eq!(
        a.warning.as_deref(),
        Some(
            format!(
                "clauth: --settings \"{{half\": cannot read it (No such file or directory); {TAIL}"
            )
            .as_str()
        )
    );
}

#[test]
fn malformed_user_hooks_warn_and_pass_through() {
    for (user, field) in [
        ("{\"hooks\":[1]}", "hooks is not an object"),
        (
            "{\"hooks\":{\"PreToolUse\":{\"matcher\":\"x\"}}}",
            "hooks.PreToolUse is not an array",
        ),
    ] {
        let l = layout(true);
        let passthrough = ["--settings", user];
        let a = apply(&l, &passthrough);
        assert_eq!(a.argv, strings(&passthrough), "{user}");
        assert_eq!(
            a.warning.as_deref(),
            Some(format!("clauth: --settings JSON: {field}; {TAIL}").as_str()),
            "{user}"
        );

        fs::write(l.cwd.join("extra.json"), user).unwrap();
        let passthrough = ["--settings", "extra.json"];
        let a = apply(&l, &passthrough);
        assert_eq!(a.argv, strings(&passthrough), "{user}");
        assert_eq!(
            a.warning.as_deref(),
            Some(format!("clauth: --settings \"extra.json\": {field}; {TAIL}").as_str()),
            "{user} from a file"
        );
    }
}

#[test]
fn a_bare_trailing_settings_flag_passes_through_untouched() {
    let l = layout(true);
    let passthrough = ["-p", "hi", "--settings"];
    let a = apply(&l, &passthrough);
    assert_eq!(a.argv, strings(&passthrough));
    assert_eq!(a.warning, None);
}

#[cfg(unix)]
#[test]
fn a_fifo_or_oversize_settings_file_never_blocks_or_balloons() {
    let l = layout(true);
    let fifo = l.cwd.join("pipe.json");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo runs");
    assert!(status.success());
    fs::write(l.cwd.join("huge.json"), " ".repeat(2 << 20)).unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    let runtime = l.runtime.clone();
    let cwd = l.cwd.clone();
    let settings_file = l.settings_file.clone();
    std::thread::spawn(move || {
        for user in ["pipe.json", "huge.json"] {
            let mut command = Command::new("claude");
            let warning = apply_spawn_settings(
                &mut command,
                &runtime,
                &cwd,
                &strings(&["--settings", user]),
                HOOK,
                &settings_file,
            );
            tx.send((user, warning)).unwrap();
        }
    });
    for _ in 0..2 {
        let (user, warning) = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a special or oversize file must be refused, not read");
        assert!(warning.is_some(), "{user}");
    }

    // The project-tier files are probed too; a repo can commit a FIFO there.
    drop(l);
    let l = layout(true);
    fs::create_dir_all(l.cwd.join(".claude")).unwrap();
    let status = std::process::Command::new("mkfifo")
        .arg(l.cwd.join(".claude/settings.json"))
        .status()
        .expect("mkfifo runs");
    assert!(status.success());
    let (tx, rx) = std::sync::mpsc::channel();
    let (runtime, cwd, settings_file) = (l.runtime.clone(), l.cwd.clone(), l.settings_file.clone());
    std::thread::spawn(move || {
        let mut command = Command::new("claude");
        apply_spawn_settings(&mut command, &runtime, &cwd, &[], HOOK, &settings_file);
        tx.send(()).unwrap();
    });
    rx.recv_timeout(std::time::Duration::from_secs(5))
        .expect("a FIFO project settings file must not block the spawn");
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(&l.settings_file).unwrap()).unwrap();
    assert!(settings["autoMemoryDirectory"].is_string());
}

#[test]
fn a_settings_word_after_the_end_of_options_marker_is_left_alone() {
    let l = layout(true);
    let a = apply(&l, &["-p", "--", "--settings", "{\"theme\":\"x\"}"]);
    let settings = a.settings.unwrap();
    assert_eq!(settings.get("theme"), None);
    assert_eq!(
        a.rest,
        strings(&["-p", "--", "--settings", "{\"theme\":\"x\"}"])
    );
}

// ── claude's own argv parse: what counts as a `--settings` flag ──────────────
//
// claude 2.1.311 (measured in cc-system-prompts' test_shim.py): a required-
// value flag takes the next word whatever that word holds, so that word is
// never a flag or the separator; joined `--name=x` / `-nVALUE` take none;
// `-n` is the only value-taking short, taking the next word only as a
// cluster's last letter after boolean shorts; optional-value flags never take
// a `-`-led word.

#[test]
fn a_required_value_flag_takes_the_next_word_so_it_is_no_settings_flag() {
    // `--name --settings x`: claude names the session `--settings` and reads
    // `x` as the prompt, so no user settings exist and no word is stripped
    let l = layout(true);
    let a = apply(&l, &["--name", "--settings", "x"]);
    assert_eq!(
        a.settings,
        Some(json!({
            "autoMemoryDirectory": memory_dir(&l),
            "hooks": { "PreToolUse": [our_hook()] }
        }))
    );
    assert_eq!(a.rest, strings(&["--name", "--settings", "x"]));
}

#[test]
fn a_required_value_flag_takes_a_joined_settings_word_whole() {
    // `--model --settings={...}`: the whole word is the model's value
    let l = layout(true);
    let a = apply(&l, &["--model", "--settings={\"theme\":\"x\"}"]);
    assert_eq!(
        a.settings,
        Some(json!({
            "autoMemoryDirectory": memory_dir(&l),
            "hooks": { "PreToolUse": [our_hook()] }
        }))
    );
    assert_eq!(
        a.rest,
        strings(&["--model", "--settings={\"theme\":\"x\"}"])
    );
}

#[test]
fn a_separator_given_as_a_value_flag_s_value_is_no_marker() {
    // `--model -- --settings {...}`: the model's value is `--`, so the
    // settings flag after it is real
    let l = layout(true);
    let a = apply(&l, &["--model", "--", "--settings", "{\"theme\":\"x\"}"]);
    assert_eq!(a.settings.unwrap()["theme"], json!("x"));
    assert_eq!(a.rest, strings(&["--model", "--"]));
}

#[test]
fn a_short_cluster_ending_in_n_takes_the_next_word() {
    // `-pn --settings x`: claude names the session `--settings`, `x` is the
    // prompt; any run of boolean shorts before the `n` behaves the same
    for cluster in ["-pn", "-ppn", "-cvn", "-hn"] {
        let l = layout(true);
        let a = apply(&l, &[cluster, "--settings", "x"]);
        assert_eq!(
            a.settings,
            Some(json!({
                "autoMemoryDirectory": memory_dir(&l),
                "hooks": { "PreToolUse": [our_hook()] }
            })),
            "{cluster}"
        );
        assert_eq!(a.rest, strings(&[cluster, "--settings", "x"]), "{cluster}");
    }
}

#[test]
fn a_short_n_not_last_in_its_cluster_takes_no_next_word() {
    // `-np` / `-pnn` / `-nfoo`: the first `n`'s value is the rest of the
    // cluster; `-pdn` / `-rn`: an optional-value short takes the rest. either
    // way the next word is never taken and the settings flag is real
    for cluster in [
        "-np", "-pnn", "-nfoo", "-nn", "-nmain", "-non", "-pnon", "-nxn", "-pdn", "-dn", "-rn",
        "-wn",
    ] {
        let l = layout(true);
        let a = apply(&l, &[cluster, "--settings", "{\"theme\":\"x\"}"]);
        assert_eq!(a.settings.unwrap()["theme"], json!("x"), "{cluster}");
        assert_eq!(a.rest, strings(&[cluster]), "{cluster}");
    }
}

#[test]
fn an_optional_value_flag_never_takes_a_dash_led_word() {
    for flag in [
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
    ] {
        let l = layout(true);
        let a = apply(&l, &[flag, "--settings", "{\"theme\":\"x\"}"]);
        assert_eq!(a.settings.unwrap()["theme"], json!("x"), "{flag}");
        assert_eq!(a.rest, strings(&[flag]), "{flag}");
    }
}

#[test]
fn an_optional_value_flag_takes_a_word_without_a_leading_dash() {
    // `--worktree feature --settings {...}`: `feature` is the worktree's
    // value, the settings flag after it is real
    let l = layout(true);
    let a = apply(
        &l,
        &["--worktree", "feature", "--settings", "{\"theme\":\"x\"}"],
    );
    assert_eq!(a.settings.unwrap()["theme"], json!("x"));
    assert_eq!(a.rest, strings(&["--worktree", "feature"]));
}

#[test]
fn a_settings_flag_takes_the_separator_as_its_value() {
    // `--settings --`: claude hands `--settings` the next word whatever it
    // holds, so the value is `--`: unreadable as a settings file, passed
    // through with a warning, nothing injected
    let l = layout(true);
    let a = apply(&l, &["--settings", "--", "x"]);
    assert_eq!(a.settings, None);
    assert_eq!(a.argv, strings(&["--settings", "--", "x"]));
    assert!(a.warning.is_some(), "{:?}", a.warning);
}

fn help_value_flags(help: &str) -> Vec<&str> {
    // claude's help lists an option as `  -n, --name <name>  ...`; a value
    // option's declaration is followed by exactly one space and `<`
    let mut flags = Vec::new();
    for line in help.lines() {
        let Some(rest) = line.strip_prefix("  ") else {
            continue;
        };
        let mut names: Vec<&str> = Vec::new();
        let mut cursor = rest;
        if cursor.len() > 3
            && cursor.as_bytes()[0] == b'-'
            && cursor.as_bytes()[1].is_ascii_alphabetic()
            && &cursor[2..4] == ", "
        {
            names.push(&cursor[..2]);
            cursor = &cursor[4..];
        }
        while let Some(after) = cursor.strip_prefix("--") {
            let end = after
                .find(|c: char| !(c.is_ascii_alphabetic() || c == '-'))
                .unwrap_or(after.len());
            if end == 0 {
                break;
            }
            names.push(&cursor[..end + 2]);
            cursor = &cursor[end + 2..];
            match cursor.strip_prefix(", ") {
                Some(next) => cursor = next,
                None => break,
            }
        }
        if !names.is_empty() && cursor.starts_with(" <") {
            flags.append(&mut names);
        }
    }
    flags
}

#[test]
fn help_value_flags_reads_a_value_option_s_names_off_one_line() {
    let listed = help_value_flags(
        "  -p, --print                     print\n  -n, --name <name>   the name\n  --model <model>    the model\n  -w, --worktree [value]  optional\n  --bare             boolean\n",
    );
    assert_eq!(listed, vec!["-n", "--name", "--model"]);
}

#[test]
#[ignore = "needs a claude binary on PATH; run with --ignored where one is installed"]
fn the_value_flags_cover_every_help_listed_value_option() {
    // drift guard: a value option a new claude release lists fails here until
    // VALUE_FLAGS carries it. Reads `claude --help` and nothing else: never a
    // session, and never the real HOME. Ignored by default because the regular
    // CI legs install no claude, where a skip would read as drift coverage; a
    // run that asks for it fails without one.
    let home = crate::testutil::HomeSandbox::new();
    let output = Command::new("claude")
        .arg("--help")
        .env("HOME", home.home())
        .output()
        .unwrap_or_else(|e| {
            panic!("`claude --help` did not run ({e}): the drift check needs claude on PATH")
        });
    assert!(
        output.status.success(),
        "`claude --help` exited {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let help = String::from_utf8_lossy(&output.stdout);
    let listed = help_value_flags(&help);
    // first, middle and last value option of the 2.1.311 help: the parser
    // found them
    for flag in ["--add-dir", "--model", "--tools", "-n"] {
        assert!(listed.contains(&flag), "`{flag}` missing from {listed:?}");
    }
    let uncovered: Vec<&&str> = listed
        .iter()
        .filter(|flag| !VALUE_FLAGS.contains(flag))
        .collect();
    assert_eq!(
        uncovered,
        Vec::<&&str>::new(),
        "help lists value options VALUE_FLAGS lacks"
    );
}

// ── call sites ───────────────────────────────────────────────────────────────
//
// Neither spawn site can run without a real `claude` child, so the wiring is
// pinned by source scan, the mechanism `mcp_delegate_args.rs` uses.

fn body_of<'a>(src: &'a str, signature: &str) -> &'a str {
    let rest = src.split_once(signature).expect("function exists").1;
    match rest
        .split_once("\nfn ")
        .or_else(|| rest.split_once("\npub(crate) fn "))
    {
        Some((body, _)) => body,
        None => rest,
    }
}

#[test]
fn clauth_start_routes_its_claude_args_through_the_exemption_settings() {
    let body = body_of(include_str!("../../src/start.rs"), "pub(crate) fn run(");
    assert!(
        body.contains("cc_exempt::apply_spawn_settings("),
        "clauth start must build its claude argv through apply_spawn_settings"
    );
}

#[test]
fn a_delegate_routes_its_extra_args_through_the_exemption_settings() {
    let body = body_of(include_str!("../../src/mcp/mod.rs"), "fn run_delegate(");
    assert!(
        body.contains("cc_exempt::apply_spawn_settings("),
        "run_delegate must build its claude argv through apply_spawn_settings"
    );
}

// ── read hook ────────────────────────────────────────────────────────────────

const SID: &str = "0b5c7a9e-1111-4222-8333-944445555666";
const OTHER_SID: &str = "77777777-1111-4222-8333-944445555666";

/// A tool result persisted by session `SID`, reachable through the runtime's
/// `projects` link, plus the payload paths Claude Code would hand the hook.
struct Session {
    l: Layout,
    transcript: PathBuf,
    result_via_link: PathBuf,
}

fn session() -> Session {
    let l = layout(true);
    let key = "-work-plain";
    let results = l.store.join(key).join(SID).join("tool-results");
    fs::create_dir_all(&results).unwrap();
    fs::write(results.join("b1.txt"), "big output").unwrap();
    let other = l.store.join(key).join(OTHER_SID).join("tool-results");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("b2.txt"), "someone else's").unwrap();
    let linked = l.runtime.join("projects").join(key);
    Session {
        transcript: linked.join(format!("{SID}.jsonl")),
        result_via_link: linked.join(SID).join("tool-results").join("b1.txt"),
        l,
    }
}

fn payload(tool: &str, file: &Path, transcript: &Path, sid: &str) -> String {
    json!({
        "hook_event_name": "PreToolUse",
        "session_id": sid,
        "transcript_path": transcript,
        "cwd": "/work/plain",
        "permission_mode": "default",
        "tool_name": tool,
        "tool_input": { "file_path": file },
        "tool_use_id": "toolu_1"
    })
    .to_string()
}

const ALLOW: &str = r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","permissionDecisionReason":"clauth: this session's own tool result, read through the runtime's projects link"}}"#;

#[test]
fn a_read_of_this_sessions_tool_result_through_the_link_is_allowed() {
    let s = session();
    let verdict = read_hook_verdict(&payload("Read", &s.result_via_link, &s.transcript, SID));
    assert_eq!(verdict.as_deref(), Some(ALLOW));
}

#[test]
fn a_read_of_another_sessions_tool_result_is_not_allowed() {
    let s = session();
    let other =
        s.l.runtime
            .join("projects")
            .join("-work-plain")
            .join(OTHER_SID)
            .join("tool-results")
            .join("b2.txt");
    assert_eq!(
        read_hook_verdict(&payload("Read", &other, &s.transcript, SID)),
        None
    );
}

#[test]
fn a_dot_dot_escape_out_of_tool_results_is_not_allowed() {
    let s = session();
    fs::write(s.l.store.join("secret.txt"), "x").unwrap();
    let escape = s
        .result_via_link
        .parent()
        .unwrap()
        .join("../../../secret.txt");
    assert_eq!(
        read_hook_verdict(&payload("Read", &escape, &s.transcript, SID)),
        None
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_inside_tool_results_pointing_out_is_not_allowed() {
    let s = session();
    let outside = s.l.cwd.join("outside.txt");
    fs::write(&outside, "x").unwrap();
    let planted = s.result_via_link.parent().unwrap().join("planted.txt");
    std::os::unix::fs::symlink(&outside, &planted).unwrap();
    assert_eq!(
        read_hook_verdict(&payload("Read", &planted, &s.transcript, SID)),
        None
    );
}

#[test]
fn a_non_read_tool_is_not_allowed() {
    let s = session();
    for tool in ["Write", "Edit", "Bash"] {
        assert_eq!(
            read_hook_verdict(&payload(tool, &s.result_via_link, &s.transcript, SID)),
            None,
            "{tool}"
        );
    }
}

#[test]
fn a_session_id_that_is_not_a_plain_token_is_not_allowed() {
    let s = session();
    for sid in ["../x", "a/b", "", ".."] {
        assert_eq!(
            read_hook_verdict(&payload("Read", &s.result_via_link, &s.transcript, sid)),
            None,
            "{sid:?}"
        );
    }
}

#[test]
fn a_dot_session_id_cannot_name_the_project_dir_as_its_results() {
    // sid `.` with a transcript named `..jsonl` passes the name check; only
    // the token guard stops `<project>/./tool-results` from being "its own".
    let s = session();
    let project = s.transcript.parent().unwrap().to_path_buf();
    let stray = s.l.store.join("-work-plain").join("tool-results");
    fs::create_dir_all(&stray).unwrap();
    fs::write(stray.join("x.txt"), "x").unwrap();
    assert_eq!(
        read_hook_verdict(&payload(
            "Read",
            &project.join("tool-results").join("x.txt"),
            &project.join("..jsonl"),
            "."
        )),
        None
    );
}

#[test]
fn a_transcript_not_named_for_the_session_is_not_allowed() {
    let s = session();
    let wrong = s.transcript.with_file_name(format!("{OTHER_SID}.jsonl"));
    assert_eq!(
        read_hook_verdict(&payload("Read", &s.result_via_link, &wrong, SID)),
        None
    );
}

#[cfg(unix)]
#[test]
fn a_relative_file_path_is_not_allowed_even_when_it_resolves_inside() {
    // Built from the test process's cwd so it DOES resolve to this session's
    // tool result: only the absolute-path guard refuses it.
    let s = session();
    let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
    let up = "../".repeat(cwd.components().count() - 1);
    let target = s.result_via_link.canonicalize().unwrap();
    let relative = PathBuf::from(format!(
        "{up}{}",
        target.strip_prefix("/").unwrap().display()
    ));
    assert!(relative.canonicalize().is_ok(), "{}", relative.display());
    assert_eq!(
        read_hook_verdict(&payload("Read", &relative, &s.transcript, SID)),
        None
    );
}

#[test]
fn a_missing_file_path_is_not_allowed() {
    let s = session();
    let missing = s.result_via_link.with_file_name("nope.txt");
    assert_eq!(
        read_hook_verdict(&payload("Read", &missing, &s.transcript, SID)),
        None
    );
}

#[test]
fn a_malformed_payload_is_not_allowed() {
    for bad in ["", "not json", "{}", "[]", "{\"tool_name\":\"Read\"}"] {
        assert_eq!(read_hook_verdict(bad), None, "{bad:?}");
    }
}

// ── hook command ─────────────────────────────────────────────────────────────

#[test]
fn posix_quote_escapes_an_embedded_single_quote() {
    assert_eq!(posix_quote("/a b/it's"), r"'/a b/it'\''s'");
}

#[test]
fn hook_command_names_the_installed_binary_and_fails_open() {
    // A self-update leaves the running server's `current_exe` reading
    // `<path> (deleted)`; an exit 2 from a binary that lacks the subcommand
    // would block every Read, so the command can never exit non-zero.
    assert_eq!(
        hook_command_for(Path::new("/opt/clauth bin/clauth (deleted)")),
        "'/opt/clauth bin/clauth' hook-read-exempt || true"
    );
}
