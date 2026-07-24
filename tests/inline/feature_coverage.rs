//! Self-checking feature coverage test.
//!
//! Parses the README's `## Features` list, cross-references each feature
//! against `FEATURE_MAP`, and verifies every referenced test function
//! still exists in the test tree. Fails when a feature has no covering
//! test or a referenced test doesn't exist.
//!
//! Run: `cargo test features_have_test_coverage`.

use std::collections::HashSet;

/// (feature name in README → test fn name prefixes that cover it)
///
/// A feature passes if each prefix matches at least one function in the
/// test tree (substring match on function name).  Add a new row here
/// when you add a feature to the README's `## Features` list.
const FEATURE_MAP: &[(&str, &[&str])] = &[
    (
        "One-key switching",
        &["auto_switch", "snapshot_chain", "resolves_started_profile"],
    ),
    (
        "Codex accounts too (CDX-1)",
        &[
            "capture_creates_an_active_codex_profile",
            "switch_installs_the_target_chain",
            "switch_adopts_back_a_rotated_outgoing_chain",
            "switch_over_a_foreign_login_refuses_or_archives_by_policy",
            "codex_follow_adopts_a_rotated_live_chain",
            "codex_profiles_are_excluded_from_both_fetch_legs",
            "cross_harness_switches_are_refused",
            "login_codex_flag_and_its_browser_modifier",
            "tui_switch_dispatches_codex_targets_to_the_codex_slot",
            "build_status_publishes_codex_fields",
        ],
    ),
    (
        "Codex, the rest of the ladder (CDX-3/1b/4/5)",
        &[
            // CDX-3 standby refresh + PKCE login
            "refresh_failure_truth_table",
            "apply_refresh_overwrites_only_present_fields_and_stamps_last_refresh",
            "codex_standby_tick_refreshes_a_due_parked_profile",
            "codex_standby_tick_never_spends_the_live_owner_chain",
            "build_auth_json_writes_the_codex_shape_with_explicit_auth_mode",
            "browser_login_store_never_touches_live_or_the_active_slot",
            // CDX-1b isolated start
            "acquire_builds_the_isolated_home_and_holds_a_lease",
            "acquire_refuses_the_live_owner_and_loginless_profiles",
            // CDX-4 codex chain + per-harness independence
            "codex_walk_fires_only_on_an_exhausted_active",
            "codex_limiter_verdict_drives_the_switch_and_clears_on_reset",
            "membership_edits_route_by_harness",
            "pending_switch_gates_are_harness_scoped",
            "scan_codex_auto_switch_enqueues_past_a_pending_claude_entry",
            // CDX-5 injection proxy
            "e2e_injects_identity_and_relays_the_sse_response",
            "e2e_429_rotates_to_the_next_account_and_replays",
            "codex_passive_tick_stands_down_while_the_proxy_is_active",
        ],
    ),
    (
        "Log in an account",
        &[
            "authorize_url",
            "pkce_challenge",
            "base64url_nopad",
            "login_route",
            "reauth_confirmed",
            "login_api_mode",
        ],
    ),
    ("Delete an account", &["delete_takes_yes_and_force"]),
    (
        "Automatic token refresh",
        &[
            "rotate_one",
            "live_session_excluded",
            "force_true_bypasses",
            "rotation_guard_is_independent",
            // AUTH-3: a dead refresh token flags auth_broken; a transient one doesn't.
            "dead_refresh_token_is_terminal",
            "transient_refresh_failure_is_not_terminal",
        ],
    ),
    (
        "Live usage bars",
        &[
            "parses_",
            "retry_after",
            "cached_fallback_does_not_clobber",
            "mark_window_open",
            "window_lapsed",
        ],
    ),
    (
        "Per-row activity",
        &["gap_boundary", "steady_linear_drain_exact_rate"],
    ),
    (
        "Plan detection",
        &["oauth_profile", "api_profile", "failed_profile"],
    ),
    (
        "Per-account breakdown",
        &["all_tabs_render", "empty_state_renders"],
    ),
    (
        "Auto-switch on exhaustion",
        &[
            "auto_switch_",
            "wrap_off_",
            "find_recovered_",
            "sink_active_",
        ],
    ),
    (
        "Headless daemon + status feed",
        &[
            "build_status",
            "switch_valid_profile",
            "snapshot_reply_is_one_line",
            // fallback configuration over the socket (CBAR-2)
            "fallback_add_remove",
            "set_threshold_validates",
            "add_appends",
            "set_wrap_off_toggles",
            // Daemon::tick loop body characterization (TECH-5)
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
        ],
    ),
    ("Stale-data cues", &["all_tabs_render"]),
    (
        "Account-change detection",
        &[
            "relogin_is_diverged",
            "overwrite_confirm",
            "overwrite_cancel",
        ],
    ),
    (
        "Multi-instance safe",
        &[
            "cross_thread_with_state_lock_serializes",
            "same_thread_reentrancy_does_not_deadlock",
            "poison_recovery_after_panicking_closure",
        ],
    ),
    (
        "Non-destructive",
        &[
            "diverged_",
            "classify_link_",
            "first_login_",
            "build_runtime_dir_writes_settings_not_symlink",
        ],
    ),
    (
        "Isolated launch",
        &[
            "acquire_creates_runtime_and_pid_file",
            "build_runtime_dir_credentials_not_from_claude_home",
            "acquire_isolates_credentials_from_real_home",
        ],
    ),
    (
        "Status-line aware",
        &[
            "resolves_started_profile",
            "session_profile_",
            "matches_profile_by_refresh_token",
            "token_match_",
        ],
    ),
    (
        "Per-profile model routing",
        &[
            "profile_config_reads_models_table",
            "model_settings_round_trip",
            "build_settings_writes_model_knobs",
            "build_settings_clears_stale_model_knobs",
        ],
    ),
    (
        "Shell completions",
        &[
            "print_script_supports",
            "print_script_rejects",
            "install_bash_writes",
            "install_bash_is_idempotent",
            "install_fish_writes",
            "install_rejects_unsupported",
        ],
    ),
    ("In-app help", &["all_tabs_render"]),
    (
        "Claude status feed",
        &[
            "parses_core_fields",
            "collects_components_with_status",
            "component_status_",
            "dedup_keeps_worst_status",
            "status_selected_row_tint",
        ],
    ),
    (
        "Token usage dashboard",
        &[
            "base_stats_parsed",
            "today_bucket_aggregates",
            "top_up_adds_new_day",
            "group_models_keeps",
            "model_display_name",
        ],
    ),
    (
        "API-equivalent cost",
        &[
            "distill_keeps",
            "rate_strips",
            "cost_sums",
            "total_cost_counts_unpriced",
        ],
    ),
    (
        "Plugin wiring check",
        &[
            "installed_records",
            "marketplace_known",
            "manual_mcp_wiring",
            "wire_mcp_server",
            "global_entry_drifted",
            "all_tabs_render",
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
            // `- **Feature name** — description...`
            if let Some(content) = line.strip_prefix("- **")
                && let Some(name) = content.split("**").next()
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
