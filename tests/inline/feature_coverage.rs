//! Self-checking feature coverage test.
//!
//! Parses the README's `## Features` list, cross-references each feature
//! against `FEATURE_MAP`, and verifies every referenced test function
//! still exists in the test tree. Fails when a feature has no covering
//! test or a referenced test doesn't exist.
//!
//! Run: `cargo test features_have_test_coverage`.

use std::collections::HashSet;

/// (bolded lead of a README `## Features` bullet → test fn name prefixes
/// that cover it)
///
/// One row per README bullet, so each row is a bucket covering everything
/// that bullet claims; the exhaustive per-subsystem reference lives in
/// `wiki/`. A row passes when EVERY prefix matches at least one function in
/// the test tree (substring match on the function name), so a deleted or
/// renamed test still reds here. Add a row when you add a README bullet;
/// add a prefix when a bullet starts claiming something new.
const FEATURE_MAP: &[(&str, &[&str])] = &[
    (
        // switching, login, delete, the non-destructive swap, which-am-i,
        // and the re-login divergence prompt.
        "Switch",
        &[
            "auto_switch",
            "snapshot_chain",
            "resolves_started_profile",
            "authorize_url",
            "pkce_challenge",
            "base64url_nopad",
            "login_route",
            "reauth_confirmed",
            "login_api_mode",
            "delete_takes_yes_and_force",
            "diverged_",
            "classify_link_",
            "first_login_",
            "build_runtime_dir_writes_settings_not_symlink",
            "session_profile_",
            "matches_profile_by_refresh_token",
            "token_match_",
            "relogin_is_diverged",
            "overwrite_confirm",
            "overwrite_cancel",
            // rolling session token (#59): the arm/restore verbs and the
            // sidecar state they manage — what a switch installs.
            "rolling_gate_",
            "stamp_rolling_token_writes",
            "first_stamp_preserves_the_mint",
            "restore_static_mint_round_trip",
        ],
    ),
    (
        // usage bars, plan detection, per-row activity, stale-data cues,
        // the token dashboard + its cost lens, the status feed.
        "Monitor",
        &[
            "parses_",
            "retry_after",
            "cached_fallback_does_not_clobber",
            "mark_window_open",
            "window_lapsed",
            "gap_boundary",
            "steady_linear_drain_exact_rate",
            "oauth_profile",
            "api_profile",
            "failed_profile",
            "all_tabs_render",
            "empty_state_renders",
            "parses_core_fields",
            "collects_components_with_status",
            "component_status_",
            "dedup_keeps_worst_status",
            "status_selected_row_tint",
            "base_stats_parsed",
            "today_bucket_aggregates",
            "top_up_adds_new_day",
            "group_models_keeps",
            "model_display_name",
            "distill_keeps",
            "rate_strips",
            "cost_sums",
            "total_cost_counts_unpriced",
        ],
    ),
    (
        "Auto-switch",
        &[
            "auto_switch_",
            "wrap_off_",
            "find_recovered_",
            "sink_active_",
            // Interleaved auto-start: membership, gap arithmetic, the
            // history-series classifier, the per-tick election, and the chip.
            // ONE prefix, and every test of the feature is rooted at it, so a
            // deleted test reds this row while nothing else in the tree can
            // match by accident — a bare `queue_` would hit `enqueue_refetch`,
            // a bare `election_` hits `selection_caret_…`.
            "auto_start_queue_",
        ],
    ),
    (
        "Run in parallel",
        &[
            "acquire_creates_runtime_and_pid_file",
            "build_runtime_dir_credentials_not_from_claude_home",
            "acquire_isolates_credentials_from_real_home",
        ],
    ),
    (
        // the MCP server's tools, the bundled hooks, plus the Plugin tab that
        // proves the wiring.
        "From inside Claude",
        &[
            "every_bundled_hook_command_parses_as_a_subcommand",
            "the_first_fire_is_a_baseline_and_a_move_is_announced_once",
            "installed_records",
            "marketplace_known",
            "manual_mcp_wiring",
            "wire_mcp_server",
            "global_entry_drifted",
            "session_scope_resolves_the_tier_through_the_which_tiers",
            "valid_switch_repoints_active_through_the_blocking_task",
            "unknown_target_is_rejected_without_stripping_live_creds",
            "divergence_overwrite_captures_relogin_into_outgoing",
        ],
    ),
    (
        // the daemon loop and its status feed, plus the token rotation it
        // drives on every tick.
        "Headless",
        &[
            "build_status",
            "tick_with_empty_queues",
            "drain_pending_switch_executes",
            "drain_pending_switch_skips",
            "reload_if_changed_fires",
            // single-fetcher lease (#27): exactly one instance fetches; every
            // other one stands down and hydrates from the shared cache.
            "standdown_",
            "one_holder_at_a_time",
            // singleton ceiling (#57): one active + one standby, no pile-up.
            "third_instance_is_redundant",
            "no_standby_exits_rather_than",
            "tick_stands_down_when_another",
            "held_lock_with_fresh_status",
            "rotate_one",
            "live_session_included",
            "force_true_bypasses",
            "rotation_guard_is_independent",
            // rolling session token (#59): the daemon leg — the tick that
            // re-stamps the sidecar and the gate it goes through.
            "claude_rolling_tick_",
            "restamp_",
            "rolling_token_forces_the_preemptive_leg",
        ],
    ),
    (
        // session browsing + resume, model routing, completions, and the
        // multi-instance state lock.
        "Quality-of-life",
        &[
            "sessions_json_has_exact_fields_newest_first_with_null_and_redaction",
            "resume_profile_choice_explicit_flag_forces_no_prompt",
            "info_prints_the_resume_command_workspace_and_storage",
            "profile_config_reads_models_table",
            "model_settings_round_trip",
            "build_settings_writes_model_knobs",
            "build_settings_clears_stale_model_knobs",
            "print_script_supports",
            "print_script_rejects",
            "install_bash_writes",
            "install_bash_is_idempotent",
            "install_fish_writes",
            "install_rejects_unsupported",
            "cross_thread_with_state_lock_serializes",
            "same_thread_reentrancy_does_not_deadlock",
            "poison_recovery_after_panicking_closure",
        ],
    ),
];

#[test]
fn features_have_test_coverage() {
    let readme = include_str!("../../README.md");

    let features = extract_features(readme);
    assert!(
        !features.is_empty(),
        "no `## Features` section or bullet items found in README"
    );

    let test_fns = collect_test_functions();

    let mut uncovered: Vec<String> = Vec::new();
    let mut rows: Vec<String> = Vec::new();

    for feature in &features {
        let entry = lookup(feature);
        match entry {
            Some(prefixes) => {
                let matched = matched_tests(prefixes, &test_fns);
                let unmatched = unmatched_prefixes(prefixes, &test_fns);

                let tests_str = if matched.is_empty() {
                    "—".to_string()
                } else {
                    matched.join(", ")
                };

                if unmatched.is_empty() {
                    rows.push(format!("| {} | {} | ✅ |", feature, tests_str));
                } else {
                    let detail = format!("missing: {}", unmatched.join(", "));
                    rows.push(format!("| {} | {} | ❌ {} |", feature, tests_str, detail));
                    uncovered.push(format!("  {feature}: {detail}"));
                }
            }
            None => {
                rows.push(format!(
                    "| {} | — | ❌ no mapping in FEATURE_MAP |",
                    feature
                ));
                uncovered.push(format!("  {feature}: add an entry to FEATURE_MAP"));
            }
        }
    }

    println!("\nFeature → Test Coverage Table\n");
    println!("| Feature | Tests | Status |");
    println!("|---|---|---|");
    for row in &rows {
        println!("{row}");
    }
    println!();

    assert!(
        uncovered.is_empty(),
        "Features without test coverage:\n{uncovered}",
        uncovered = uncovered.join("\n")
    );
}

/// Extract feature names from the README's `## Features` bullet list.
fn extract_features(readme: &str) -> Vec<String> {
    let mut in_features = false;
    let mut features = Vec::new();

    for line in readme.lines() {
        if line.starts_with("## Features") {
            in_features = true;
            continue;
        }
        if in_features {
            if line.starts_with("## ") {
                break;
            }
            // `- 🔄 **Feature name** description...`; the emoji is optional,
            // so match the first bold run on the bullet rather than its start.
            if let Some(rest) = line.strip_prefix("- ")
                && let Some(open) = rest.find("**")
                && let Some(name) = rest[open + 2..].split("**").next()
            {
                let name = name.trim();
                if !name.is_empty() {
                    features.push(name.to_string());
                }
            }
        }
    }

    features
}

/// Look up the test prefixes for a feature name.
fn lookup(feature: &str) -> Option<&'static [&'static str]> {
    FEATURE_MAP
        .iter()
        .find(|(key, _)| *key == feature)
        .map(|(_, prefixes)| *prefixes)
}

/// Return all test function names that match at least one prefix.
fn matched_tests(prefixes: &[&str], test_fns: &HashSet<String>) -> Vec<String> {
    let mut names: Vec<String> = test_fns
        .iter()
        .filter(|name| prefixes.iter().any(|p| name.contains(p)))
        .cloned()
        .collect();
    names.sort();
    names
}

/// Return prefixes that match zero test functions.
fn unmatched_prefixes<'a>(prefixes: &'a [&str], test_fns: &HashSet<String>) -> Vec<&'a str> {
    prefixes
        .iter()
        .filter(|p| !test_fns.iter().any(|name| name.contains(*p)))
        .copied()
        .collect()
}

/// Scan `tests/inline/*.rs` for function definitions.
fn collect_test_functions() -> HashSet<String> {
    let mut names = HashSet::new();
    let test_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/inline");

    let dir = match std::fs::read_dir(&test_dir) {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "warning: cannot read tests/inline/: {e} — \
                 using empty function set"
            );
            return names;
        }
    };

    for entry in dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        for raw_line in content.lines() {
            let line = raw_line.trim();
            // Match `fn name(`, `fn name <`, or `fn name` at end
            if let Some(rest) = line
                .strip_prefix("fn ")
                .or_else(|| line.strip_prefix("pub fn "))
                .or_else(|| line.strip_prefix("pub(crate) fn "))
            {
                let rest = rest.trim_start();
                let name = rest.split(['(', '<', ' ', '!']).next().unwrap_or("").trim();
                if !name.is_empty() && !name.starts_with('_') {
                    names.insert(name.to_string());
                }
            }
        }
    }

    names
}
