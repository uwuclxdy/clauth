#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Startup-path pin: `mcp::startup` over a broken plugin registration must
//! reach the heal's `claude` spawn. Deleting the `heal_detached` call from
//! `startup` reds here; the daemon twin
//! `tick_heals_a_broken_plugin_registration` pins the tick call site.
//! Unix-only: the fake `claude` is a shell shim.

#[cfg(unix)]
use crate::testutil::{HomeSandbox, git_shim, heal_env, lightweight_tag, stateful_heal_shim};

#[cfg(unix)]
#[test]
fn startup_heals_a_broken_plugin_registration() {
    use crate::testutil::{FakeClaude, HomeSandbox, join_background_tasks};

    let home = HomeSandbox::new();
    let fake = FakeClaude::new(&home);
    crate::plugin_host::reset_heal_throttle_for_test();
    // This test pins the claude heal; the herdr twin shares the startup call
    // site, so its throttle is armed to keep this test spawn-free.
    crate::herdr::arm_heal_throttle_for_test();
    crate::testutil::seed_broken_plugin_registration();

    let _marker = super::startup();
    join_background_tasks();

    assert!(
        !fake.log().is_empty(),
        "startup over a broken registration must reach the heal"
    );
}

/// The herdr heal at startup is a NETWORK update, so it reads the saved
/// `[update]` toggle fresh: a server started with `auto_update = false` on
/// disk reinstalls nothing, and one started with it back on reaches the fake
/// install — proving the saved-off call never claimed the throttle floor.
#[cfg(unix)]
#[test]
fn startup_herdr_heal_follows_the_saved_update_toggle() {
    use std::ffi::OsStr;

    use crate::testutil::join_background_tasks;

    let home = HomeSandbox::new();
    let stale = crate::herdr::plugin_list_json(
        r#"{"enabled":true,"plugin_id":"clauth","source":{"kind":"github","owner":"uwuclxdy","repo":"clauth","resolved_commit":"aaaaaaaaaaaaaaaa"}}"#,
    );
    let shim = stateful_heal_shim(home.home());
    git_shim(home.home());
    let tags = lightweight_tag("v0.15.1", "bbbbbbbbbbbbbbbb");
    let _env = heal_env(
        &home,
        &shim,
        &stale,
        &stale,
        &tags,
        &[("HERDR_SHIM_STATE", OsStr::new("1"))],
    );
    // The claude heal shares the startup call site; its throttle is armed so
    // this test stays spawn-free beside the herdr heal it pins.
    crate::plugin_host::arm_heal_throttle_for_test();
    crate::herdr::reset_heal_throttle_for_test();

    let mut state = crate::profile::load_app_state().expect("load state");
    state.update.auto_update = false;
    crate::profile::save_app_state(&state).expect("persist off toggle");

    let _marker = super::startup();
    join_background_tasks();
    assert!(
        !home.home().join("heal.log").exists(),
        "a server started with the saved toggle off reinstalls nothing"
    );

    // The saved-off call must not have claimed the throttle: back on, a
    // server started fresh reads the new value and installs.
    let mut state = crate::profile::load_app_state().expect("load state");
    state.update.auto_update = true;
    crate::profile::save_app_state(&state).expect("persist on toggle");

    let _marker = super::startup();
    join_background_tasks();
    let log = std::fs::read_to_string(home.home().join("heal.log")).unwrap_or_default();
    assert_eq!(
        log.trim(),
        "plugin install uwuclxdy/clauth/herdr-plugin --ref v0.15.1 --yes",
        "a server started with the saved toggle on reaches the fake install"
    );
}
