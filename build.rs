// agentgear's version guard: `plugins/.claude-plugin/plugin.json` must carry the
// crate version, or the build fails (CC keys its plugin cache on that version, so
// a mismatch would ship a silent no-op). Also tracks the tree for rebuilds and
// emits the marker the `#[derive(PluginHost)]` expansion checks for, so a build
// without this file is a compile error. The tree lives in `plugins/` rather than
// the default `plugin/`, so the derive's `tree` attr and this call name it twice.
fn main() {
    agentgear::build::assert_plugin_version_at(concat!(env!("CARGO_MANIFEST_DIR"), "/plugins"));
    rewrite_herdr_plugin_version();
}

// The version-tie the herdr auto-update heal keys on: `herdr-plugin/herdr-plugin.toml`
// must carry the crate version, or a release ships a binary whose heal reads every
// install as stale (a reinstall can only fetch whatever the repo's HEAD carries).
// Generated here rather than bumped by hand: the file keeps its committed spelling
// when it already matches, so a clean tree stays clean, and a version bump lands
// the rewrite as an unstaged edit to commit alongside the `Cargo.toml` bump.
fn rewrite_herdr_plugin_version() {
    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/herdr-plugin/herdr-plugin.toml"
    );
    let text = read_manifest(manifest);
    let doc: toml::Value = match toml::from_str(&text) {
        Ok(doc) => doc,
        Err(e) => panic!("herdr-plugin/herdr-plugin.toml parses: {e}"),
    };
    let current = doc
        .get("version")
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("herdr-plugin/herdr-plugin.toml carries no version"));
    if current == env!("CARGO_PKG_VERSION") {
        return;
    }
    write_manifest(manifest, &rewrite_version_line(&text, current));
    println!(
        "cargo:warning=herdr-plugin/herdr-plugin.toml version rewritten to {}; \
         commit it with the bump",
        env!("CARGO_PKG_VERSION")
    );
}

// A build script's failure mode is a panicking build: name the file and the
// operation, no `expect`.
fn read_manifest(path: &str) -> String {
    match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => panic!("cannot read {path}: {e}"),
    }
}

fn write_manifest(path: &str, text: &str) {
    if let Err(e) = std::fs::write(path, text) {
        panic!("cannot write {path}: {e}");
    }
}

// Replace the top-level `version = "..."` line in place, leaving every comment
// and other key untouched, so a reserialization cannot reformat the manifest.
fn rewrite_version_line(text: &str, old: &str) -> String {
    let needle = format!("version = \"{old}\"");
    let mut replaced = false;
    let out: Vec<String> = text
        .lines()
        .map(|line| {
            if !replaced && !line.starts_with(' ') && line.trim() == needle {
                replaced = true;
                format!("version = \"{}\"", env!("CARGO_PKG_VERSION"))
            } else {
                line.to_string()
            }
        })
        .collect();
    assert!(
        replaced,
        "herdr-plugin/herdr-plugin.toml has no top-level `{needle}` line"
    );
    let mut out = out.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
}
