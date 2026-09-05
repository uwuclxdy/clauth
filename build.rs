//! agentgear's version guard: `plugins/.claude-plugin/plugin.json` must carry
//! the crate version, or the build fails (CC keys its plugin cache on that
//! version, so a mismatch would ship a silent no-op). Also tracks the tree for
//! rebuilds and emits the marker the `#[derive(PluginHost)]` expansion checks
//! for, so a build without this file is a compile error. The tree lives in
//! `plugins/` rather than the default `plugin/`, so the derive's `tree` attr and
//! this call name it twice. The tree dir is read at runtime, never baked with
//! `env!`: build-script binaries are compiled once into the shared target dir
//! and reused across worktrees, so a baked path from a reaped worktree would
//! panic the next build of an unrelated tree.

use std::path::Path;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .unwrap_or_else(|_| panic!("cargo sets CARGO_MANIFEST_DIR"));
    agentgear::build::assert_plugin_version_at(Path::new(&manifest_dir).join("plugins"));
}
