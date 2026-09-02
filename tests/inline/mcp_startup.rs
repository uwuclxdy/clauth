#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Startup-path pin: `mcp::startup` over a broken plugin registration must
//! reach the heal's `claude` spawn. Deleting the `heal_detached` call from
//! `startup` reds here; the daemon twin
//! `tick_heals_a_broken_plugin_registration` pins the tick call site.
//! Unix-only: the fake `claude` is a shell shim.

#[cfg(unix)]
#[test]
fn startup_heals_a_broken_plugin_registration() {
    use crate::testutil::{FakeClaude, HomeSandbox, join_background_tasks};

    let home = HomeSandbox::new();
    let fake = FakeClaude::new(&home);
    crate::plugin_host::reset_heal_throttle_for_test();
    crate::testutil::seed_broken_plugin_registration();

    let _marker = super::startup();
    join_background_tasks();

    assert!(
        !fake.log().is_empty(),
        "startup over a broken registration must reach the heal"
    );
}
