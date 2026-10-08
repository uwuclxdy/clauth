#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The config editor's engine: the view, every edit kind on every list shape
//! as a full candidate text, the comment-ownership refusals, the
//! verification, and the landing through the checked write path. Every test
//! holds a `HomeSandbox`; the landing tests drive a stub `shunt` written into
//! it, a `/bin/sh` script, so they are `#[cfg(unix)]`.

use std::fs;
#[cfg(unix)]
use std::path::{Path, PathBuf};

use super::*;
use crate::testutil::HomeSandbox;

fn w(raw: &str, value: &str) -> Written {
    Written {
        raw: raw.to_string(),
        value: Some(value.to_string()),
    }
}

/// The candidate `edit` plans over `text`.
fn planned(text: &str, edit: Edit) -> String {
    match plan(text, &edit) {
        Ok(Some(planned)) => planned.candidate,
        Ok(None) => panic!("the edit changed nothing"),
        Err(e) => panic!("the edit refused: {e:#}"),
    }
}

/// The refusal `edit` meets over `text`, as its message.
fn refused(text: &str, edit: Edit) -> String {
    match plan(text, &edit) {
        Ok(_) => panic!("the edit planned"),
        Err(e) => {
            assert!(
                e.downcast_ref::<EditRefusal>().is_some(),
                "a typed EditRefusal, got: {e:#}"
            );
            e.to_string()
        }
    }
}

fn set_upstream(name: &str, field: UpstreamField, value: Option<&str>) -> Edit {
    Edit::Upstream {
        name: name.to_string(),
        field,
        value: value.map(str::to_string),
    }
}

fn set_model(id: &str, field: ModelField, value: Option<&str>) -> Edit {
    Edit::Model {
        id: id.to_string(),
        field,
        value: value.map(str::to_string),
    }
}

fn accounts(name: &str, names: &[&str]) -> Edit {
    Edit::SetAccounts {
        name: name.to_string(),
        accounts: names.iter().map(|n| n.to_string()).collect(),
    }
}

fn route(position: usize, model: &str, provider: &str) -> RouteView {
    RouteView {
        position,
        model: Some(w(&format!("\"{model}\""), model)),
        provider: Some(w(&format!("\"{provider}\""), provider)),
        upstream_model: None,
        effort: None,
        service_tier: None,
    }
}

// ── the view ────────────────────────────────────────────────────────────────

const VIEW: &str = r#"# gateway
[server]
bind = "127.0.0.1:3067" # port
default_provider = 'anthropic'

[[upstreams]]
name = "anthropic"
provider = "anthropic"

[[upstreams]]
name = "work"
kind = "anthropic"
base_url = "${UPSTREAM_URL}"
auth = { mode = "claude_oauth", accounts = ["a", { name = "b", token_env = "B" }] }
effort = "high"

[[upstreams]]
name = "keyed"
auth = "api_key"

[[upstreams]]
name = "pool"
[upstreams.auth]
mode = "chatgpt_oauth"
account = "main"
[[upstreams.auth.accounts]]
name = "c"

[[upstreams]]
name = "env"
auth.mode = "api_key"
auth.env = "KEY"
auth.header = "x_api_key"

[[models]]
id = "opus-x"
display_name = "Opus X"
[models.upstream_model]
work = "claude-opus"

[[routes]]
model = "m"
provider = "work"
"#;

fn upstream(name: &str) -> UpstreamView {
    UpstreamView {
        name: Some(w(&format!("\"{name}\""), name)),
        provider: None,
        kind: None,
        base_url: None,
        auth: None,
        effort: None,
        service_tier: None,
    }
}

fn auth(form: AuthForm, mode: &str) -> AuthView {
    AuthView {
        form,
        mode: Some(w(&format!("\"{mode}\""), mode)),
        account: None,
        accounts: None,
        env: None,
        header: None,
    }
}

#[test]
fn the_view_reads_every_auth_form_with_each_value_as_written() {
    let _home = HomeSandbox::new();
    let view = view_of(VIEW, None).expect("view");
    let expected = ConfigView {
        server: ServerView {
            bind: Some(w("\"127.0.0.1:3067\"", "127.0.0.1:3067")),
            bind_override: None,
            default_provider: Some(w("'anthropic'", "anthropic")),
        },
        form: ProviderForm::Upstreams,
        upstreams: vec![
            UpstreamView {
                provider: Some(w("\"anthropic\"", "anthropic")),
                ..upstream("anthropic")
            },
            UpstreamView {
                kind: Some(w("\"anthropic\"", "anthropic")),
                base_url: Some(w("\"${UPSTREAM_URL}\"", "${UPSTREAM_URL}")),
                auth: Some(AuthView {
                    accounts: Some(vec![
                        AccountView::Name(w("\"a\"", "a")),
                        AccountView::Selection {
                            name: Some(w("\"b\"", "b")),
                            form: SelectionForm::Inline,
                        },
                    ]),
                    ..auth(AuthForm::Inline, "claude_oauth")
                }),
                effort: Some(w("\"high\"", "high")),
                ..upstream("work")
            },
            UpstreamView {
                auth: Some(auth(AuthForm::Shorthand, "api_key")),
                ..upstream("keyed")
            },
            UpstreamView {
                auth: Some(AuthView {
                    account: Some(w("\"main\"", "main")),
                    accounts: Some(vec![AccountView::Selection {
                        name: Some(w("\"c\"", "c")),
                        form: SelectionForm::Table,
                    }]),
                    ..auth(AuthForm::Table, "chatgpt_oauth")
                }),
                ..upstream("pool")
            },
            UpstreamView {
                auth: Some(AuthView {
                    env: Some(w("\"KEY\"", "KEY")),
                    header: Some(w("\"x_api_key\"", "x_api_key")),
                    ..auth(AuthForm::Dotted, "api_key")
                }),
                ..upstream("env")
            },
        ],
        models: vec![ModelView {
            id: Some(w("\"opus-x\"", "opus-x")),
            display_name: Some(w("\"Opus X\"", "Opus X")),
            upstream_model: Some(vec![(
                "work".to_string(),
                w("\"claude-opus\"", "claude-opus"),
            )]),
        }],
        routes: vec![route(1, "m", "work")],
    };
    assert_eq!(view, expected);
}

#[test]
fn the_view_reads_one_line_and_multi_line_inline_lists() {
    let _home = HomeSandbox::new();
    let text = r#"upstreams = [{ name = "a" }, { name = "b" },]
models = [
  # opus
  { id = "m1" }, # first
  { id = "m2" }
]
routes = [
  { model = "x", provider = "a" },
  # gap
  { model = "y", provider = "b" },
]
"#;
    let view = view_of(text, None).expect("view");
    assert_eq!(view.upstreams, vec![upstream("a"), upstream("b")]);
    let ids: Vec<Option<Written>> = view.models.into_iter().map(|m| m.id).collect();
    assert_eq!(ids, vec![Some(w("\"m1\"", "m1")), Some(w("\"m2\"", "m2"))]);
    assert_eq!(view.routes, vec![route(1, "x", "a"), route(2, "y", "b")]);
}

#[test]
fn a_legacy_config_lists_its_providers_and_refuses_only_upstream_edits() {
    let _home = HomeSandbox::new();
    let text = r#"[server]
default_provider = "anthropic"

[providers.anthropic]
kind = "anthropic"

[providers.openai]
kind = "openai"

[[models]]
id = "m"

[[routes]]
model = "m"
provider = "anthropic"
"#;
    let view = view_of(text, None).expect("view");
    assert_eq!(
        view.form,
        ProviderForm::Legacy {
            providers: vec!["anthropic".to_string(), "openai".to_string()]
        }
    );
    assert_eq!(view.upstreams, Vec::new());

    let tail = "the config declares its providers as [providers.*], which clauth does not edit; convert them to [[upstreams]] by hand";
    for (edit, op) in [
        (
            set_upstream("anthropic", UpstreamField::Effort, Some("high")),
            "cannot set effort of upstream \"anthropic\"",
        ),
        (
            Edit::RenameUpstream {
                name: "anthropic".into(),
                to: "a".into(),
            },
            "cannot rename upstream \"anthropic\" to \"a\"",
        ),
        (
            Edit::AddUpstream(NewUpstream {
                name: "x".into(),
                ..NewUpstream::default()
            }),
            "cannot add upstream \"x\"",
        ),
        (
            Edit::RemoveUpstream {
                name: "anthropic".into(),
            },
            "cannot remove upstream \"anthropic\"",
        ),
        (
            accounts("anthropic", &["a"]),
            "cannot set the accounts of upstream \"anthropic\"",
        ),
    ] {
        assert_eq!(refused(text, edit), format!("{op}: {tail}"));
    }

    assert_eq!(
        planned(text, set_model("m", ModelField::DisplayName, Some("M"))),
        text.replace("id = \"m\"\n", "id = \"m\"\ndisplay_name = \"M\"\n")
    );
    assert_eq!(
        planned(
            text,
            Edit::Route {
                route: route(1, "m", "anthropic"),
                field: RouteField::Effort,
                value: Some("low".into()),
            }
        ),
        r#"[server]
default_provider = "anthropic"

[providers.anthropic]
kind = "anthropic"

[providers.openai]
kind = "openai"

[[models]]
id = "m"

[[routes]]
model = "m"
provider = "anthropic"
effort = "low"
"#
    );
    assert_eq!(
        planned(
            text,
            Edit::Server {
                field: ServerField::DefaultProvider,
                value: Some("openai".into()),
            }
        ),
        text.replace(
            "default_provider = \"anthropic\"",
            "default_provider = \"openai\""
        )
    );
}

/// A record over `text` in the sandbox, its env file holding `env`, its
/// binary a sandbox path that does not exist: an edit these tests expect
/// refused, if it reaches the check, fails as `ShuntMissing` and never runs
/// a `shunt` off PATH.
fn record_with_env(home: &HomeSandbox, text: &str, env: &str) -> GatewayRecord {
    let etc = home.home().join("etc");
    fs::create_dir_all(&etc).expect("etc");
    let config = etc.join("shunt.toml");
    fs::write(&config, text).expect("config");
    let env_file = home.home().join("tokens.env");
    fs::write(&env_file, env).expect("env file");
    let mut record = GatewayRecord::new(config).expect("record");
    record.env_file = Some(env_file);
    record.binary = Some(home.home().join("no-shunt"));
    record
}

#[test]
fn the_view_names_a_bind_override_in_any_case_as_the_env_spells_it() {
    let home = HomeSandbox::new();
    let _pin = crate::testutil::EnvPin::new(&home, &[(BIND_ENV, None)]);
    let record = record_with_env(
        &home,
        "[server]\nbind = \"127.0.0.1:3067\"\n",
        "Shunt_Server__Bind=127.0.0.1:4000\n",
    );
    let view = read_view(&record).expect("view");
    let spelled = if cfg!(windows) {
        "SHUNT_SERVER__BIND"
    } else {
        "Shunt_Server__Bind"
    };
    assert_eq!(
        view.server,
        ServerView {
            bind: Some(w("\"127.0.0.1:3067\"", "127.0.0.1:3067")),
            bind_override: Some(spelled.to_string()),
            default_provider: None,
        }
    );

    let no_override = record_with_env(&home, "[server]\n", "OTHER=1\n");
    assert_eq!(
        read_view(&no_override).expect("view").server.bind_override,
        None
    );
}

/// A daemon holding the singleton with no env record: only a bind edit reads
/// the daemon's env, so the view and every other edit go on.
#[test]
fn a_bind_edit_under_an_unrecorded_daemon_refuses_naming_the_restart() {
    let home = HomeSandbox::new();
    let _pin = crate::testutil::EnvPin::new(&home, &[(BIND_ENV, None)]);
    let record = record_with_env(&home, "[server]\nbind = \"127.0.0.1:3067\"\n", "");
    let _held = crate::daemon::hold_daemon_lock();
    fs::write(
        home.home().join(".clauth").join("clauthd.pid"),
        format!("{}\n", std::process::id()),
    )
    .expect("stamp the holder pid");

    let err = apply_edit(
        &record,
        &Edit::Server {
            field: ServerField::Bind,
            value: Some("127.0.0.1:4000".into()),
        },
    )
    .expect_err("an unrecorded daemon refuses the bind edit");
    assert_eq!(
        err.downcast_ref::<EditRefusal>(),
        Some(&EditRefusal::BindUnrecorded {
            op: "cannot set server.bind".to_string()
        })
    );
    assert_eq!(
        err.to_string(),
        "cannot set server.bind: the running daemon recorded no environment, so clauth cannot tell whether SHUNT_SERVER__BIND overrides the bind; restart the daemon, then save again"
    );
    assert_eq!(
        apply_edit(
            &record,
            &Edit::Server {
                field: ServerField::Bind,
                value: None,
            },
        )
        .expect_err("an unset refuses too")
        .to_string(),
        "cannot unset server.bind: the running daemon recorded no environment, so clauth cannot tell whether SHUNT_SERVER__BIND overrides the bind; restart the daemon, then save again"
    );
    assert_eq!(
        fs::read_to_string(record.config()).expect("read"),
        "[server]\nbind = \"127.0.0.1:3067\"\n"
    );
    assert_eq!(
        read_view(&record)
            .expect("the view reads on")
            .server
            .bind_override,
        None
    );
}

#[test]
fn a_bind_edit_refuses_while_the_env_file_overrides_the_bind() {
    let home = HomeSandbox::new();
    let text = "[server]\nbind = \"127.0.0.1:3067\"\n";
    let record = record_with_env(&home, text, "SHUNT_SERVER__BIND=127.0.0.1:4000\n");
    let err = apply_edit(
        &record,
        &Edit::Server {
            field: ServerField::Bind,
            value: Some("127.0.0.1:4100".into()),
        },
    )
    .expect_err("an overridden bind refuses");
    assert_eq!(
        err.to_string(),
        "cannot set server.bind: the gateway's environment sets \"SHUNT_SERVER__BIND\", which overrides the config's bind; change that variable instead"
    );
    assert_eq!(
        apply_edit(
            &record,
            &Edit::Server {
                field: ServerField::Bind,
                value: None,
            },
        )
        .expect_err("an overridden bind refuses an unset")
        .to_string(),
        "cannot unset server.bind: the gateway's environment sets \"SHUNT_SERVER__BIND\", which overrides the config's bind; change that variable instead"
    );
    assert_eq!(fs::read_to_string(record.config()).expect("read"), text);
}

/// With no daemon the inherited half is this process's own env, as
/// `start daemon` would pass it.
#[test]
fn a_bind_edit_reads_the_inherited_env_with_no_daemon() {
    let home = HomeSandbox::new();
    let text = "[server]\nbind = \"127.0.0.1:3067\"\n";
    let record = record_with_env(&home, text, "");
    let _pin = crate::testutil::EnvPin::new(
        &home,
        &[(BIND_ENV, Some(std::ffi::OsStr::new("127.0.0.1:4000")))],
    );
    let err = apply_edit(
        &record,
        &Edit::Server {
            field: ServerField::Bind,
            value: Some("127.0.0.1:4100".into()),
        },
    )
    .expect_err("an inherited override refuses");
    assert_eq!(
        err.downcast_ref::<EditRefusal>(),
        Some(&EditRefusal::BindOverridden {
            op: "cannot set server.bind".to_string(),
            variable: "SHUNT_SERVER__BIND".to_string()
        })
    );
}

// ── field edits ─────────────────────────────────────────────────────────────

const COMMENTED: &str = r#"# models I route
[[models]]
id = "opus-x" # the big one
display_name = "Opus X"  # shown in the picker
upstream_model = { work = "claude-opus" }

# second
[[models]]
id = "sonnet-x"
"#;

#[test]
fn a_field_set_changes_exactly_that_value() {
    let _home = HomeSandbox::new();
    assert_eq!(
        planned(
            COMMENTED,
            set_model("opus-x", ModelField::DisplayName, Some("Opus 10"))
        ),
        r#"# models I route
[[models]]
id = "opus-x" # the big one
display_name = "Opus 10"  # shown in the picker
upstream_model = { work = "claude-opus" }

# second
[[models]]
id = "sonnet-x"
"#
    );
}

#[test]
fn an_unrelated_edit_leaves_a_reference_value_byte_identical() {
    let _home = HomeSandbox::new();
    assert_eq!(
        planned(
            VIEW,
            set_upstream("work", UpstreamField::Effort, Some("low"))
        ),
        VIEW.replace("effort = \"high\"", "effort = \"low\"")
    );
    assert!(VIEW.contains("base_url = \"${UPSTREAM_URL}\"\n"));
}

#[test]
fn a_set_key_lands_after_the_entrys_own_keys_and_an_unset_takes_its_line() {
    let _home = HomeSandbox::new();
    let text = r#"[[upstreams]]
  name = "a"
  kind="anthropic" # k
[upstreams.auth]
mode = "passthrough"
"#;
    assert_eq!(
        planned(text, set_upstream("a", UpstreamField::Effort, Some("high"))),
        r#"[[upstreams]]
  name = "a"
  kind="anthropic" # k
  effort="high"
[upstreams.auth]
mode = "passthrough"
"#
    );
    assert_eq!(
        planned(text, set_upstream("a", UpstreamField::Kind, None)),
        r#"[[upstreams]]
  name = "a"
[upstreams.auth]
mode = "passthrough"
"#
    );
}

#[test]
fn a_changed_string_keeps_its_quote_style_when_the_value_allows_it() {
    let _home = HomeSandbox::new();
    let text = "[server]\ndefault_provider = 'a'\n";
    let edit = |value: &str| Edit::Server {
        field: ServerField::DefaultProvider,
        value: Some(value.into()),
    };
    assert_eq!(
        planned(text, edit("b")),
        "[server]\ndefault_provider = 'b'\n"
    );
    assert_eq!(
        planned(text, edit("it's")),
        "[server]\ndefault_provider = \"it's\"\n"
    );
}

#[test]
fn an_inline_entry_takes_field_sets_adds_and_unsets() {
    let _home = HomeSandbox::new();
    let text = r#"routes = [{ model = "m", provider = "a" }, {model="n",provider="b"}]
"#;
    let edit = |position, model: &str, provider: &str, field, value: Option<&str>| Edit::Route {
        route: route(position, model, provider),
        field,
        value: value.map(str::to_string),
    };
    assert_eq!(
        planned(text, edit(1, "m", "a", RouteField::Provider, None)),
        "routes = [{ model = \"m\" }, {model=\"n\",provider=\"b\"}]\n"
    );
    assert_eq!(
        planned(text, edit(2, "n", "b", RouteField::Effort, Some("low"))),
        "routes = [{ model = \"m\", provider = \"a\" }, {model=\"n\",provider=\"b\",effort=\"low\"}]\n"
    );
    assert_eq!(
        planned(text, edit(1, "m", "a", RouteField::Model, None)),
        "routes = [{ provider = \"a\" }, {model=\"n\",provider=\"b\"}]\n"
    );
}

#[test]
fn a_multi_line_inline_list_with_comments_takes_a_field_set() {
    let _home = HomeSandbox::new();
    let text = r#"upstreams = [
  # the main one
  { name = "a", effort = "low" }, # main
  # spare
  { name = "b" },
]
"#;
    assert_eq!(
        planned(text, set_upstream("a", UpstreamField::Effort, Some("high"))),
        text.replace("effort = \"low\"", "effort = \"high\"")
    );
    assert_eq!(
        planned(
            text,
            set_upstream("b", UpstreamField::Kind, Some("anthropic"))
        ),
        text.replace("{ name = \"b\" }", "{ name = \"b\", kind = \"anthropic\" }")
    );
}

#[test]
fn the_files_final_newline_state_holds() {
    let _home = HomeSandbox::new();
    let text = "[server]\nbind = \"127.0.0.1:1\"";
    assert_eq!(
        planned(
            text,
            Edit::Server {
                field: ServerField::DefaultProvider,
                value: Some("a".into()),
            }
        ),
        "[server]\nbind = \"127.0.0.1:1\"\ndefault_provider = \"a\""
    );
    assert_eq!(
        planned(
            "[server]\nbind = \"127.0.0.1:1\"\ndefault_provider = \"a\"",
            Edit::Server {
                field: ServerField::DefaultProvider,
                value: None,
            }
        ),
        text
    );
}

#[test]
fn a_server_field_with_no_server_table_adds_one_at_the_end() {
    let _home = HomeSandbox::new();
    assert_eq!(
        planned(
            "[[routes]]\nmodel = \"m\"\nprovider = \"a\"\n",
            Edit::Server {
                field: ServerField::Bind,
                value: Some("127.0.0.1:4000".into()),
            }
        ),
        "[[routes]]\nmodel = \"m\"\nprovider = \"a\"\n\n[server]\nbind = \"127.0.0.1:4000\"\n"
    );
}

// ── auth ────────────────────────────────────────────────────────────────────

#[test]
fn a_shorthand_auth_becomes_an_inline_table_when_it_gains_an_account() {
    let _home = HomeSandbox::new();
    let text = "[[upstreams]]\nname = \"u\"\nauth = \"passthrough\" # open\n";
    let moded = planned(
        text,
        set_upstream("u", UpstreamField::AuthMode, Some("claude_oauth")),
    );
    assert_eq!(
        moded,
        "[[upstreams]]\nname = \"u\"\nauth = \"claude_oauth\" # open\n"
    );
    assert_eq!(
        planned(
            &moded,
            set_upstream("u", UpstreamField::AuthAccount, Some("work"))
        ),
        "[[upstreams]]\nname = \"u\"\nauth = { mode = \"claude_oauth\", account = \"work\" } # open\n"
    );
}

#[test]
fn an_auth_sub_table_is_edited_in_place() {
    let _home = HomeSandbox::new();
    let text = "[[upstreams]]\nname = \"u\"\n\n[upstreams.auth]\nmode = \"claude_oauth\"\n\n[[upstreams]]\nname = \"v\"\n";
    assert_eq!(
        planned(
            text,
            set_upstream("u", UpstreamField::AuthAccount, Some("work"))
        ),
        "[[upstreams]]\nname = \"u\"\n\n[upstreams.auth]\nmode = \"claude_oauth\"\naccount = \"work\"\n\n[[upstreams]]\nname = \"v\"\n"
    );
    assert_eq!(
        planned(text, set_upstream("u", UpstreamField::AuthMode, None)),
        "[[upstreams]]\nname = \"u\"\n\n[[upstreams]]\nname = \"v\"\n"
    );
}

#[test]
fn unsetting_an_inline_auth_mode_removes_the_line_whatever_its_comments() {
    let _home = HomeSandbox::new();
    let text = "[[upstreams]]\nname = \"u\"\n# the key\nauth = { mode = \"api_key\", env = \"K\" } # keyed\n# after\neffort = \"low\"\n";
    assert_eq!(
        planned(text, set_upstream("u", UpstreamField::AuthMode, None)),
        "[[upstreams]]\nname = \"u\"\n# the key\n# after\neffort = \"low\"\n"
    );
}

#[test]
fn a_dotted_auth_gains_a_key_on_its_own_line() {
    let _home = HomeSandbox::new();
    let text = "[[upstreams]]\nname = \"u\"\nauth.mode = \"api_key\"\nauth.env = \"K\"\neffort = \"low\"\n";
    assert_eq!(
        planned(
            text,
            set_upstream("u", UpstreamField::AuthHeader, Some("x_api_key"))
        ),
        "[[upstreams]]\nname = \"u\"\nauth.mode = \"api_key\"\nauth.env = \"K\"\nauth.header = \"x_api_key\"\neffort = \"low\"\n"
    );
}

#[test]
fn an_auth_field_with_no_mode_refuses() {
    let _home = HomeSandbox::new();
    assert_eq!(
        refused(
            "[[upstreams]]\nname = \"u\"\n",
            set_upstream("u", UpstreamField::AuthEnv, Some("K"))
        ),
        "cannot set auth.env of upstream \"u\": upstream \"u\" has no auth mode; set auth.mode first"
    );
    assert_eq!(
        refused(
            "[[upstreams]]\nname = \"u\"\nauth = { env = \"K\" }\n",
            set_upstream("u", UpstreamField::AuthHeader, Some("bearer"))
        ),
        "cannot set auth.header of upstream \"u\": upstream \"u\" has no auth mode; set auth.mode first"
    );
    assert_eq!(
        refused(
            "[[upstreams]]\nname = \"a\"\n",
            Edit::AddUpstream(NewUpstream {
                name: "x".into(),
                auth: Some(NewAuth {
                    account: Some("w".into()),
                    ..NewAuth::default()
                }),
                ..NewUpstream::default()
            })
        ),
        "cannot add upstream \"x\": upstream \"x\" has no auth mode; set auth.mode first"
    );
}

// ── upstream_model ──────────────────────────────────────────────────────────

#[test]
fn an_upstream_model_slug_is_set_added_and_unset() {
    let _home = HomeSandbox::new();
    let text = "[[models]]\nid = \"m\"\nupstream_model = { work = \"a\" }\n";
    assert_eq!(
        planned(
            text,
            set_model("m", ModelField::UpstreamModel("codex".into()), Some("b"))
        ),
        "[[models]]\nid = \"m\"\nupstream_model = { work = \"a\", codex = \"b\" }\n"
    );
    assert_eq!(
        planned(
            text,
            set_model("m", ModelField::UpstreamModel("work".into()), None)
        ),
        "[[models]]\nid = \"m\"\n"
    );
    assert_eq!(
        planned(
            "[[models]]\nid = \"m\"\n",
            set_model("m", ModelField::UpstreamModel("my.pool".into()), Some("s"))
        ),
        "[[models]]\nid = \"m\"\nupstream_model = { \"my.pool\" = \"s\" }\n"
    );
}

#[test]
fn an_emptied_upstream_model_sub_table_is_removed_or_refused() {
    let _home = HomeSandbox::new();
    let edit = || set_model("m", ModelField::UpstreamModel("work".into()), None);
    assert_eq!(
        planned(
            "[[models]]\nid = \"m\"\n\n[models.upstream_model]\nwork = \"slug\"\n\n[[models]]\nid = \"n\"\n",
            edit()
        ),
        "[[models]]\nid = \"m\"\n\n[[models]]\nid = \"n\"\n"
    );
    assert_eq!(
        planned(
            "[[models]]\nid = \"m\"\n# slugs\n[models.upstream_model]\nwork = \"slug\"\n",
            edit()
        ),
        "[[models]]\nid = \"m\"\n# slugs\n"
    );
    assert_eq!(
        refused(
            "[[models]]\nid = \"m\"\n[models.upstream_model]\nwork = \"slug\"\n# slugs\n[[models]]\nid = \"n\"\n",
            edit()
        ),
        "cannot remove the upstream_model table of model \"m\": a comment next to it belongs to no single entry; edit the config by hand"
    );
}

// ── renames ─────────────────────────────────────────────────────────────────

const REFERENCES: &str = r#"[server]
default_provider = "work"

[server.codex_endpoint]
provider = "work"

[[server.codex_endpoint.routes]]
model = "gpt"
provider = "work"

[[upstreams]]
name = "work" # main

[[upstreams]]
name = "other"

[[models]]
id = "m"
upstream_model = { work = "slug", other = "x" }

[[models]]
id = "n"
[models.upstream_model]
"work" = "s2"

[[routes]]
model = "m"
provider = 'work'

[[routes]]
model = "z"
provider = "other"

[[route_prefixes]]
prefix = "claude-"
provider = "work"
"#;

#[test]
fn an_upstream_rename_cascades_to_every_reference_and_counts_them() {
    let _home = HomeSandbox::new();
    let planned = plan(
        REFERENCES,
        &Edit::RenameUpstream {
            name: "work".into(),
            to: "main-pool".into(),
        },
    )
    .expect("plans")
    .expect("changes");
    assert_eq!(
        planned.candidate,
        r#"[server]
default_provider = "main-pool"

[server.codex_endpoint]
provider = "main-pool"

[[server.codex_endpoint.routes]]
model = "gpt"
provider = "main-pool"

[[upstreams]]
name = "main-pool" # main

[[upstreams]]
name = "other"

[[models]]
id = "m"
upstream_model = { main-pool = "slug", other = "x" }

[[models]]
id = "n"
[models.upstream_model]
"main-pool" = "s2"

[[routes]]
model = "m"
provider = 'main-pool'

[[routes]]
model = "z"
provider = "other"

[[route_prefixes]]
prefix = "claude-"
provider = "main-pool"
"#
    );
    assert_eq!(
        planned.cascade,
        Some(Cascade {
            routes: 1,
            models: 2,
            default_provider: 1,
            route_prefixes: 1,
            codex_endpoint: 1,
            codex_routes: 1,
            ..Cascade::default()
        })
    );
}

#[test]
fn a_bare_key_reference_is_quoted_when_the_new_name_needs_it() {
    let _home = HomeSandbox::new();
    let text =
        "[[upstreams]]\nname = \"work\"\n\n[[models]]\nid = \"m\"\nupstream_model.work = \"s\"\n";
    assert_eq!(
        planned(
            text,
            Edit::RenameUpstream {
                name: "work".into(),
                to: "pool.v2".into(),
            }
        ),
        "[[upstreams]]\nname = \"pool.v2\"\n\n[[models]]\nid = \"m\"\nupstream_model.\"pool.v2\" = \"s\"\n"
    );
}

#[test]
fn a_rename_onto_a_taken_name_refuses() {
    let _home = HomeSandbox::new();
    assert_eq!(
        refused(
            REFERENCES,
            Edit::RenameUpstream {
                name: "work".into(),
                to: "other".into(),
            }
        ),
        "cannot rename upstream \"work\" to \"other\": the config already has an upstream named \"other\""
    );
    let dangling = REFERENCES.replace("provider = \"other\"", "provider = \"ghost\"");
    assert_eq!(
        refused(
            &dangling,
            Edit::RenameUpstream {
                name: "other".into(),
                to: "ghost".into(),
            }
        ),
        "cannot rename upstream \"other\" to \"ghost\": a [[routes]] provider already names \"ghost\"; change that reference first"
    );
}

#[test]
fn a_model_rename_cascades_to_router_targets_and_refuses_a_taken_name() {
    let _home = HomeSandbox::new();
    let text = "[[models]]\nid = \"m\"\n\n[[routes]]\nmodel = \"m\"\nprovider = \"a\"\n";
    let rename = |to: &str| Edit::RenameModel {
        id: "m".into(),
        to: to.into(),
    };
    assert_eq!(
        planned(text, rename("m2")),
        "[[models]]\nid = \"m2\"\n\n[[routes]]\nmodel = \"m2\"\nprovider = \"a\"\n"
    );
    let routed = "[[models]]\nid = \"m\"\n\n[[models]]\nid = \"auto\"\n[models.router]\ntype = \"random\"\ntargets = [\"m[1m]\", \"n\"]\n";
    assert_eq!(
        planned(routed, rename("m2")),
        "[[models]]\nid = \"m2\"\n\n[[models]]\nid = \"auto\"\n[models.router]\ntype = \"random\"\ntargets = [\"m2[1m]\", \"n\"]\n"
    );
    let routed_elsewhere = routed.replace("\"m[1m]\", ", "");
    assert_eq!(
        refused(&routed_elsewhere, rename("n")),
        "cannot rename model \"m\" to \"n\": a [models.router] or [models.subagents] target already names \"n\"; change that reference first"
    );
    assert_eq!(
        refused(routed, rename("auto")),
        "cannot rename model \"m\" to \"auto\": the config already has a model with id \"auto\""
    );
}

// ── adds ────────────────────────────────────────────────────────────────────

#[test]
fn an_array_of_tables_add_copies_the_gap_before_the_last_entry() {
    let _home = HomeSandbox::new();
    let text = "[[upstreams]]\nname = \"a\"\n\n\n[[upstreams]]\nname = \"b\"\nauth = \"passthrough\"\n\n[server]\nbind = \"127.0.0.1:3067\"\n";
    assert_eq!(
        planned(
            text,
            Edit::AddUpstream(NewUpstream {
                name: "new".into(),
                provider: Some("anthropic".into()),
                ..NewUpstream::default()
            })
        ),
        "[[upstreams]]\nname = \"a\"\n\n\n[[upstreams]]\nname = \"b\"\nauth = \"passthrough\"\n\n\n[[upstreams]]\nname = \"new\"\nprovider = \"anthropic\"\n\n[server]\nbind = \"127.0.0.1:3067\"\n"
    );
}

#[test]
fn an_array_of_tables_add_after_one_entry_takes_one_blank_line() {
    let _home = HomeSandbox::new();
    assert_eq!(
        planned(
            "[[routes]]\nmodel = \"a\"\nprovider = \"x\"\n[server]\nbind = \"127.0.0.1:1\"\n",
            Edit::AddRoute(NewRoute {
                model: "b".into(),
                provider: "y".into(),
                effort: Some("low".into()),
                ..NewRoute::default()
            })
        ),
        "[[routes]]\nmodel = \"a\"\nprovider = \"x\"\n\n[[routes]]\nmodel = \"b\"\nprovider = \"y\"\neffort = \"low\"\n[server]\nbind = \"127.0.0.1:1\"\n"
    );
    assert_eq!(
        planned(
            "[[models]]\nid = \"a\"",
            Edit::AddModel(NewModel {
                id: "b".into(),
                ..NewModel::default()
            })
        ),
        "[[models]]\nid = \"a\"\n\n[[models]]\nid = \"b\""
    );
}

#[test]
fn an_add_to_an_absent_list_lands_at_the_end() {
    let _home = HomeSandbox::new();
    assert_eq!(
        planned(
            "[server]\nbind = \"127.0.0.1:1\"",
            Edit::AddModel(NewModel {
                id: "m".into(),
                display_name: Some("M".into()),
                upstream_model: vec![("work".into(), "slug".into())],
            })
        ),
        "[server]\nbind = \"127.0.0.1:1\"\n\n[[models]]\nid = \"m\"\ndisplay_name = \"M\"\nupstream_model = { work = \"slug\" }"
    );
}

#[test]
fn an_upstream_add_writes_its_auth_in_the_form_its_fields_need() {
    let _home = HomeSandbox::new();
    let text = "[[upstreams]]\nname = \"a\"\n";
    assert_eq!(
        planned(
            text,
            Edit::AddUpstream(NewUpstream {
                name: "pool".into(),
                kind: Some("anthropic".into()),
                base_url: Some("${POOL_URL}".into()),
                auth: Some(NewAuth {
                    mode: Some("claude_oauth".into()),
                    accounts: vec!["x".into(), "y".into()],
                    ..NewAuth::default()
                }),
                effort: Some("high".into()),
                service_tier: Some("fast".into()),
                ..NewUpstream::default()
            })
        ),
        "[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"pool\"\nkind = \"anthropic\"\nbase_url = \"${POOL_URL}\"\nauth = { mode = \"claude_oauth\", accounts = [\"x\", \"y\"] }\neffort = \"high\"\nservice_tier = \"fast\"\n"
    );
    assert_eq!(
        planned(
            text,
            Edit::AddUpstream(NewUpstream {
                name: "open".into(),
                auth: Some(NewAuth {
                    mode: Some("passthrough".into()),
                    ..NewAuth::default()
                }),
                ..NewUpstream::default()
            })
        ),
        "[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"open\"\nauth = \"passthrough\"\n"
    );
}

#[test]
fn an_inline_list_add_copies_the_lists_own_separator() {
    let _home = HomeSandbox::new();
    let add = |model: &str| {
        Edit::AddRoute(NewRoute {
            model: model.into(),
            provider: "z".into(),
            ..NewRoute::default()
        })
    };
    assert_eq!(
        planned(
            "routes = [{ model = \"a\", provider = \"x\" },{model=\"b\",provider=\"y\"}]\n",
            add("c")
        ),
        "routes = [{ model = \"a\", provider = \"x\" },{model=\"b\",provider=\"y\"},{ model = \"c\", provider = \"z\" }]\n"
    );
    assert_eq!(
        planned(
            "routes = [{ model = \"a\", provider = \"x\" },]\n",
            add("c")
        ),
        "routes = [{ model = \"a\", provider = \"x\" }, { model = \"c\", provider = \"z\" },]\n"
    );
    assert_eq!(
        planned("routes = []\n", add("c")),
        "routes = [{ model = \"c\", provider = \"z\" }]\n"
    );
}

#[test]
fn a_multi_line_inline_list_add_takes_its_own_line_and_its_comma_rule() {
    let _home = HomeSandbox::new();
    let add = || {
        Edit::AddUpstream(NewUpstream {
            name: "c".into(),
            ..NewUpstream::default()
        })
    };
    assert_eq!(
        planned(
            "upstreams = [\n  { name = \"a\" }, # main\n  { name = \"b\" } # spare\n]\n",
            add()
        ),
        "upstreams = [\n  { name = \"a\" }, # main\n  { name = \"b\" }, # spare\n  { name = \"c\" }\n]\n"
    );
    assert_eq!(
        planned(
            "upstreams = [\n  { name = \"a\" },\n  { name = \"b\" },\n  # the end\n]\n",
            add()
        ),
        "upstreams = [\n  { name = \"a\" },\n  { name = \"b\" },\n  { name = \"c\" },\n  # the end\n]\n"
    );
}

#[test]
fn a_duplicate_add_refuses() {
    let _home = HomeSandbox::new();
    assert_eq!(
        refused(
            "[[upstreams]]\nname = \"a\"\n",
            Edit::AddUpstream(NewUpstream {
                name: "a".into(),
                ..NewUpstream::default()
            })
        ),
        "cannot add upstream \"a\": the config already has an upstream named \"a\""
    );
    assert_eq!(
        refused(
            "models = [{ id = \"m\" }]\n",
            Edit::AddModel(NewModel {
                id: "m".into(),
                ..NewModel::default()
            })
        ),
        "cannot add model \"m\": the config already has a model with id \"m\""
    );
}

// ── removes ─────────────────────────────────────────────────────────────────

#[test]
fn a_table_remove_takes_its_owned_comment_and_the_blank_line_before_it() {
    let _home = HomeSandbox::new();
    let text = "[[upstreams]]\nname = \"a\"\n\n# b: the backup\n[[upstreams]]\nname = \"b\" # b\n[upstreams.auth]\nmode = \"api_key\"\n\n[[upstreams]]\nname = \"c\"\n";
    assert_eq!(
        planned(text, Edit::RemoveUpstream { name: "b".into() }),
        "[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"c\"\n"
    );
}

#[test]
fn a_file_start_comment_block_lies_in_no_gap() {
    let _home = HomeSandbox::new();
    let text = "# my gateway\n\n[[routes]]\nmodel = \"a\"\nprovider = \"x\"\n\n[[routes]]\nmodel = \"b\"\nprovider = \"y\"\n";
    assert_eq!(
        planned(
            text,
            Edit::RemoveRoute {
                route: route(1, "a", "x")
            }
        ),
        "# my gateway\n\n[[routes]]\nmodel = \"b\"\nprovider = \"y\"\n"
    );
}

#[test]
fn a_one_line_list_cut_takes_the_element_and_its_own_separator() {
    let _home = HomeSandbox::new();
    let text =
        "routes = [{ model = \"a\", provider = \"x\" }, { model = \"b\", provider = \"y\" }]\n";
    let remove = |position, model: &str, provider: &str| Edit::RemoveRoute {
        route: route(position, model, provider),
    };
    assert_eq!(
        planned(text, remove(2, "b", "y")),
        "routes = [{ model = \"a\", provider = \"x\" }]\n"
    );
    assert_eq!(
        planned(text, remove(1, "a", "x")),
        "routes = [{ model = \"b\", provider = \"y\" }]\n"
    );
    assert_eq!(
        planned(
            "routes = [{ model = \"a\", provider = \"x\" }, { model = \"b\", provider = \"y\" },]\n",
            remove(2, "b", "y")
        ),
        "routes = [{ model = \"a\", provider = \"x\" },]\n"
    );
    assert_eq!(
        planned(
            "routes = [{ model = \"a\", provider = \"x\" }]\n",
            remove(1, "a", "x")
        ),
        "routes = []\n"
    );
}

#[test]
fn a_multi_line_list_cut_takes_the_elements_lines_and_leaves_the_commas() {
    let _home = HomeSandbox::new();
    let text = "upstreams = [\n  { name = \"a\" }, # main\n  { name = \"b\" } # spare\n]\n";
    assert_eq!(
        planned(text, Edit::RemoveUpstream { name: "b".into() }),
        "upstreams = [\n  { name = \"a\" }, # main\n]\n"
    );
    assert_eq!(
        planned(text, Edit::RemoveUpstream { name: "a".into() }),
        "upstreams = [\n  { name = \"b\" } # spare\n]\n"
    );
}

#[test]
fn removing_one_of_two_identical_routes_removes_the_one_at_its_position() {
    let _home = HomeSandbox::new();
    let text = "[[routes]]\nmodel = \"m\"\nprovider = \"a\" # first\n\n[[routes]]\nmodel = \"m\"\nprovider = \"a\" # second\n";
    assert_eq!(
        planned(
            text,
            Edit::RemoveRoute {
                route: route(2, "m", "a")
            }
        ),
        "[[routes]]\nmodel = \"m\"\nprovider = \"a\" # first\n"
    );
}

#[test]
fn a_route_that_no_longer_matches_refuses() {
    let _home = HomeSandbox::new();
    let text = "[[routes]]\nmodel = \"m\"\nprovider = \"a\"\n\n[[routes]]\nmodel = \"n\"\nprovider = \"b\"\n";
    assert_eq!(
        refused(
            text,
            Edit::RemoveRoute {
                route: route(2, "m", "a")
            }
        ),
        "cannot remove the route at position 2 (model \"m\"): the config no longer holds that route there; reload the config and try again"
    );
    assert_eq!(
        refused(
            text,
            Edit::Route {
                route: route(3, "n", "b"),
                field: RouteField::Effort,
                value: Some("low".into()),
            }
        ),
        "cannot set effort of the route at position 3 (model \"n\"): the config no longer holds that route there; reload the config and try again"
    );
}

#[test]
fn an_unknown_entry_refuses_by_name() {
    let _home = HomeSandbox::new();
    assert_eq!(
        refused(
            "[[upstreams]]\nname = \"a\"\n",
            set_upstream("nope", UpstreamField::Effort, Some("low"))
        ),
        "cannot set effort of upstream \"nope\": the config has no upstream named \"nope\""
    );
    assert_eq!(
        refused(
            "[[models]]\nid = \"m\"\n",
            Edit::RemoveModel { id: "x".into() }
        ),
        "cannot remove model \"x\": the config has no model with id \"x\""
    );
}

#[test]
fn a_shape_the_editor_does_not_handle_refuses_naming_the_dotted_key() {
    let _home = HomeSandbox::new();
    let text = "[server.bind]\nhost = \"127.0.0.1\"\n";
    assert_eq!(
        refused(
            text,
            Edit::Server {
                field: ServerField::Bind,
                value: Some("127.0.0.1:1".into()),
            }
        ),
        "cannot set server.bind: the config spells \"server.bind\" in a shape clauth does not edit; edit the config by hand"
    );
    assert_eq!(
        view_of(text, None)
            .expect_err("the view refuses")
            .to_string(),
        "cannot read the shunt config for editing: the config spells \"server.bind\" in a shape clauth does not edit; edit the config by hand"
    );
}

// ── comment ownership ───────────────────────────────────────────────────────

#[test]
fn a_remove_next_to_an_unowned_comment_refuses() {
    let _home = HomeSandbox::new();
    let glued = "upstreams = [\n  { name = \"a\" },\n  # b: backup\n  { name = \"b\" },\n  { name = \"c\" },\n]\n";
    for name in ["a", "b"] {
        assert_eq!(
            refused(glued, Edit::RemoveUpstream { name: name.into() }),
            format!(
                "cannot remove upstream {name:?}: a comment next to it belongs to no single entry; edit the config by hand"
            )
        );
    }
    let set_off = "upstreams = [\n  { name = \"a\" },\n\n  # b: backup\n  { name = \"b\" },\n  { name = \"c\" },\n]\n";
    assert_eq!(
        planned(set_off, Edit::RemoveUpstream { name: "a".into() }),
        "upstreams = [\n\n  # b: backup\n  { name = \"b\" },\n  { name = \"c\" },\n]\n"
    );
    assert_eq!(
        planned(set_off, Edit::RemoveUpstream { name: "b".into() }),
        "upstreams = [\n  { name = \"a\" },\n  { name = \"c\" },\n]\n"
    );
}

#[test]
fn a_table_remove_next_to_an_unowned_comment_refuses() {
    let _home = HomeSandbox::new();
    let text = "[[upstreams]]\nname = \"a\"\n# under a\n\n[[upstreams]]\nname = \"b\"\n\n# floating\n\n[[upstreams]]\nname = \"c\"\n";
    assert_eq!(
        planned(text, Edit::RemoveUpstream { name: "a".into() }),
        "\n[[upstreams]]\nname = \"b\"\n\n# floating\n\n[[upstreams]]\nname = \"c\"\n"
    );
    for name in ["b", "c"] {
        assert_eq!(
            refused(text, Edit::RemoveUpstream { name: name.into() }),
            format!(
                "cannot remove upstream {name:?}: a comment next to it belongs to no single entry; edit the config by hand"
            )
        );
    }
}

#[test]
fn a_same_line_comment_on_a_line_of_two_entries_belongs_to_neither() {
    let _home = HomeSandbox::new();
    let text =
        "upstreams = [\n  { name = \"a\" }, { name = \"b\" }, # ab\n  { name = \"c\" },\n]\n";
    for name in ["a", "b"] {
        assert_eq!(
            refused(text, Edit::RemoveUpstream { name: name.into() }),
            format!(
                "cannot remove upstream {name:?}: a comment next to it belongs to no single entry; edit the config by hand"
            )
        );
    }
    assert_eq!(
        planned(text, Edit::RemoveUpstream { name: "c".into() }),
        "upstreams = [\n  { name = \"a\" }, { name = \"b\" }, # ab\n]\n"
    );
}

#[test]
fn a_comment_block_before_the_closing_bracket_belongs_to_the_list() {
    let _home = HomeSandbox::new();
    let text = "upstreams = [\n  { name = \"a\" },\n  { name = \"b\" },\n  # more to come\n]\n";
    assert_eq!(
        planned(text, Edit::RemoveUpstream { name: "b".into() }),
        "upstreams = [\n  { name = \"a\" },\n  # more to come\n]\n"
    );
}

#[test]
fn field_sets_unsets_renames_and_adds_never_refuse_on_a_comment() {
    let _home = HomeSandbox::new();
    let text = "upstreams = [\n  { name = \"a\", effort = \"low\" },\n  # glued under a\n  { name = \"b\" },\n]\n";
    assert_eq!(
        planned(text, set_upstream("a", UpstreamField::Effort, None)),
        "upstreams = [\n  { name = \"a\" },\n  # glued under a\n  { name = \"b\" },\n]\n"
    );
    assert_eq!(
        planned(
            text,
            Edit::RenameUpstream {
                name: "b".into(),
                to: "c".into(),
            }
        ),
        "upstreams = [\n  { name = \"a\", effort = \"low\" },\n  # glued under a\n  { name = \"c\" },\n]\n"
    );
    assert_eq!(
        planned(
            text,
            Edit::AddUpstream(NewUpstream {
                name: "d".into(),
                ..NewUpstream::default()
            })
        ),
        "upstreams = [\n  { name = \"a\", effort = \"low\" },\n  # glued under a\n  { name = \"b\" },\n  { name = \"d\" },\n]\n"
    );
}

// ── accounts ────────────────────────────────────────────────────────────────

const ACCOUNTS: &str = "[[upstreams]]\nname = \"u\"\nauth = { mode = \"claude_oauth\", accounts = [\n  \"a\", # main\n  \"b\" # spare\n] }\n";

#[test]
fn an_accounts_reorder_moves_each_account_with_its_comment_and_keeps_the_commas() {
    let _home = HomeSandbox::new();
    assert_eq!(
        planned(ACCOUNTS, accounts("u", &["b", "a"])),
        "[[upstreams]]\nname = \"u\"\nauth = { mode = \"claude_oauth\", accounts = [\n  \"b\", # spare\n  \"a\" # main\n] }\n"
    );
}

#[test]
fn an_accounts_drop_and_add_follow_the_list_layout() {
    let _home = HomeSandbox::new();
    assert_eq!(
        planned(ACCOUNTS, accounts("u", &["a"])),
        "[[upstreams]]\nname = \"u\"\nauth = { mode = \"claude_oauth\", accounts = [\n  \"a\", # main\n] }\n"
    );
    assert_eq!(
        planned(ACCOUNTS, accounts("u", &["a", "b", "c"])),
        "[[upstreams]]\nname = \"u\"\nauth = { mode = \"claude_oauth\", accounts = [\n  \"a\", # main\n  \"b\", # spare\n  \"c\"\n] }\n"
    );
    let one_line = |list: &str| {
        format!(
            "[[upstreams]]\nname = \"u\"\n[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = {list}\n"
        )
    };
    assert_eq!(
        planned(&one_line("[\"a\", \"b\"]"), accounts("u", &["a"])),
        one_line("[\"a\"]")
    );
    assert_eq!(
        planned(&one_line("[\"a\", \"b\",]"), accounts("u", &["a"])),
        one_line("[\"a\",]")
    );
    assert_eq!(
        planned(
            &one_line("[\"a\",\"b\"]"),
            accounts("u", &["a", "b", "new"])
        ),
        one_line("[\"a\",\"b\",\"new\"]")
    );
    assert_eq!(
        planned(
            &one_line("[\"a\", \"b\", \"c\"]"),
            accounts("u", &["c", "x", "a"])
        ),
        one_line("[\"c\", \"x\", \"a\"]")
    );
}

#[test]
fn accounts_set_where_none_were_listed_adds_the_key() {
    let _home = HomeSandbox::new();
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"u\"\nauth = \"claude_oauth\"\n",
            accounts("u", &["a", "b"])
        ),
        "[[upstreams]]\nname = \"u\"\nauth = { mode = \"claude_oauth\", accounts = [\"a\", \"b\"] }\n"
    );
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"u\"\n[upstreams.auth]\nmode = \"claude_oauth\"\n",
            accounts("u", &["a"])
        ),
        "[[upstreams]]\nname = \"u\"\n[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = [\"a\"]\n"
    );
}

#[test]
fn an_accounts_edit_next_to_an_unowned_comment_refuses() {
    let _home = HomeSandbox::new();
    let text = "[[upstreams]]\nname = \"u\"\n[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = [\n  \"a\",\n  # between\n  \"b\",\n]\n";
    assert_eq!(
        refused(text, accounts("u", &["b", "a"])),
        "cannot reorder the accounts of upstream \"u\": a comment between them belongs to no single entry; edit the config by hand"
    );
    assert_eq!(
        refused(text, accounts("u", &["a"])),
        "cannot remove account \"b\" of upstream \"u\": a comment next to it belongs to no single entry; edit the config by hand"
    );
}

#[test]
fn a_names_only_accounts_set_over_a_table_selection_always_refuses() {
    let _home = HomeSandbox::new();
    let inline = "[[upstreams]]\nname = \"u\"\nauth = { mode = \"claude_oauth\", accounts = [\"a\", { name = \"b\", token_env = \"B\" }] }\n";
    let message = "cannot set the accounts of upstream \"u\": its accounts hold a table selection, whose settings a names-only edit would drop; edit the config by hand";
    assert_eq!(refused(inline, accounts("u", &["b", "a"])), message);
    assert_eq!(refused(inline, accounts("u", &["a", "b"])), message);
    let tables = "[[upstreams]]\nname = \"u\"\n[upstreams.auth]\nmode = \"claude_oauth\"\n[[upstreams.auth.accounts]]\nname = \"a\"\n";
    assert_eq!(refused(tables, accounts("u", &["a"])), message);
}

// ── verification ────────────────────────────────────────────────────────────

#[test]
fn a_candidate_failing_verification_is_clauths_bug() {
    let _home = HomeSandbox::new();
    let text = "# keep\n[server]\nbind = \"127.0.0.1:1\" # port\n";
    let expected: toml::Table = toml::from_str("[server]\nbind = \"127.0.0.1:2\"\n").expect("toml");
    let change = |candidate: &str| Change {
        candidate: candidate.to_string(),
        owned: Vec::new(),
        cascade: None,
    };
    let edit = Edit::Server {
        field: ServerField::Bind,
        value: Some("127.0.0.1:2".into()),
    };
    let check = |candidate: &str| {
        verify(&edit, text, &change(candidate), &expected)
            .expect_err("refused")
            .to_string()
    };
    assert_eq!(
        check("# keep\n[server]\nbind = \"127.0.0.1:2 # port\n"),
        "cannot set server.bind: clauth built an edit that does not parse as TOML; nothing was written; report this as a clauth bug"
    );
    assert_eq!(
        check("# keep\n[server]\nbind = \"127.0.0.1:3\" # port\n"),
        "cannot set server.bind: clauth built an edit that changes the config's value beyond the edit; nothing was written; report this as a clauth bug"
    );
    assert_eq!(
        check("[server]\nbind = \"127.0.0.1:2\" # port\n"),
        "cannot set server.bind: clauth built an edit that loses or changes a comment; nothing was written; report this as a clauth bug"
    );
    assert_eq!(
        check("# keep\n[server]\nbind = \"127.0.0.1:2\"\n"),
        "cannot set server.bind: clauth built an edit that loses or changes a comment; nothing was written; report this as a clauth bug"
    );
    verify(
        &edit,
        text,
        &change("# keep\n[server]\nbind = \"127.0.0.1:2\" # port\n"),
        &expected,
    )
    .expect("the true candidate passes");
}

#[test]
fn a_save_equal_to_the_file_plans_nothing() {
    let _home = HomeSandbox::new();
    assert!(
        plan(
            VIEW,
            &set_upstream("work", UpstreamField::Effort, Some("high"))
        )
        .expect("plans")
        .is_none()
    );
    assert!(
        plan(VIEW, &accounts("anthropic", &[]))
            .expect("plans")
            .is_none()
    );
    assert!(
        plan(
            VIEW,
            &Edit::Server {
                field: ServerField::DefaultProvider,
                value: Some("anthropic".into()),
            }
        )
        .expect("plans")
        .is_none(),
        "a value written in another quote style is the same value"
    );
}

// ── landing, over a stub `shunt` ────────────────────────────────────────────

/// A sandbox holding an adopted config and a stub `shunt` that records each
/// `check` call and the candidate it was handed; a `fail` marker in the stub
/// dir makes it refuse.
#[cfg(unix)]
struct Rig {
    home: HomeSandbox,
    config: PathBuf,
    stub: PathBuf,
    record: GatewayRecord,
}

#[cfg(unix)]
fn rig(text: &str) -> Rig {
    use std::os::unix::fs::PermissionsExt as _;
    let home = HomeSandbox::new();
    fs::create_dir_all(home.home().join("etc")).expect("etc");
    let etc = fs::canonicalize(home.home().join("etc")).expect("canonical etc");
    let config = etc.join("shunt.toml");
    fs::write(&config, text).expect("config");
    let stub = home.home().join("stub");
    fs::create_dir_all(&stub).expect("stub dir");
    let binary = stub.join("shunt");
    fs::write(
        &binary,
        format!(
            "#!/bin/sh\nd='{}'\necho call >> \"$d/calls\"\ncp \"$3\" \"$d/candidate\"\nif [ -e \"$d/fail\" ]; then echo 'config error: boom' >&2; exit 1; fi\nexit 0\n",
            stub.display()
        ),
    )
    .expect("stub");
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).expect("chmod");
    let mut record = GatewayRecord::new(config.clone()).expect("record");
    record.binary = Some(binary);
    Rig {
        home,
        config,
        stub,
        record,
    }
}

#[cfg(unix)]
impl Rig {
    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.stub.join(name)).unwrap_or_default()
    }

    fn config_text(&self) -> String {
        fs::read_to_string(&self.config).expect("read config")
    }
}

#[cfg(unix)]
fn assert_only(dir: &Path, names: &[&str]) {
    let mut found: Vec<String> = fs::read_dir(dir)
        .expect("read_dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    found.sort();
    assert_eq!(found, names, "no staging file left");
}

#[cfg(unix)]
#[test]
fn a_bind_edit_lands_through_the_check_and_is_restart_only() {
    let rig = rig("[server]\nbind = \"127.0.0.1:3067\" # port\n");
    let _pin = crate::testutil::EnvPin::new(&rig.home, &[(BIND_ENV, None)]);
    let applied = apply_edit(
        &rig.record,
        &Edit::Server {
            field: ServerField::Bind,
            value: Some("127.0.0.1:4000".into()),
        },
    )
    .expect("lands");
    assert_eq!(
        applied,
        Applied {
            written: true,
            restart_only: true,
            cascade: None,
        }
    );
    let expected = "[server]\nbind = \"127.0.0.1:4000\" # port\n";
    assert_eq!(rig.config_text(), expected);
    assert_eq!(rig.read("candidate"), expected, "the check saw what landed");
    assert_eq!(rig.read("calls"), "call\n");
    assert_only(rig.config.parent().expect("etc"), &["shunt.toml"]);
}

#[cfg(unix)]
#[test]
fn a_hot_reloaded_edit_is_not_restart_only_and_a_rename_reports_its_cascade() {
    let rig = rig("[[upstreams]]\nname = \"a\"\n\n[[routes]]\nmodel = \"m\"\nprovider = \"a\"\n");
    assert_eq!(
        apply_edit(
            &rig.record,
            &Edit::RenameUpstream {
                name: "a".into(),
                to: "b".into(),
            }
        )
        .expect("lands"),
        Applied {
            written: true,
            restart_only: false,
            cascade: Some(Cascade {
                routes: 1,
                ..Cascade::default()
            }),
        }
    );
    assert_eq!(
        rig.config_text(),
        "[[upstreams]]\nname = \"b\"\n\n[[routes]]\nmodel = \"m\"\nprovider = \"b\"\n"
    );
}

#[cfg(unix)]
#[test]
fn a_save_equal_to_the_file_writes_nothing_and_restarts_nothing() {
    let text = "[server]\nbind = '127.0.0.1:3067'\n";
    let rig = rig(text);
    assert_eq!(
        apply_edit(
            &rig.record,
            &Edit::Server {
                field: ServerField::Bind,
                value: Some("127.0.0.1:3067".into()),
            }
        )
        .expect("no-op"),
        Applied {
            written: false,
            restart_only: false,
            cascade: None,
        }
    );
    assert_eq!(rig.config_text(), text);
    assert_eq!(rig.read("calls"), "", "no check ran");
}

#[cfg(unix)]
#[test]
fn a_refused_edit_leaves_the_file_byte_identical() {
    let text = "upstreams = [\n  { name = \"a\" },\n  # b: backup\n  { name = \"b\" },\n]\n";
    let rig = rig(text);
    let err =
        apply_edit(&rig.record, &Edit::RemoveUpstream { name: "a".into() }).expect_err("refused");
    assert_eq!(
        err.downcast_ref::<EditRefusal>(),
        Some(&EditRefusal::UnownedComment {
            entry: "upstream \"a\"".to_string()
        })
    );
    assert_eq!(rig.config_text(), text);
    assert_eq!(rig.read("calls"), "", "no check ran");

    fs::write(rig.stub.join("fail"), "").expect("marker");
    let err = apply_edit(
        &rig.record,
        &set_upstream("b", UpstreamField::Effort, Some("low")),
    )
    .expect_err("the check refuses");
    assert!(
        matches!(
            err.downcast_ref::<crate::gateway::ConfigEditRefusal>(),
            Some(crate::gateway::ConfigEditRefusal::CheckFailed { code: Some(1), .. })
        ),
        "a refused check keeps its meaning: {err:#}"
    );
    assert_eq!(rig.config_text(), text);
    assert_eq!(
        rig.read("candidate"),
        "upstreams = [\n  { name = \"a\" },\n  # b: backup\n  { name = \"b\", effort = \"low\" },\n]\n"
    );
    assert_only(rig.config.parent().expect("etc"), &["shunt.toml"]);
}

/// Only a bind edit reads the daemon's env: a field edit under a daemon with
/// no env record lands.
#[cfg(unix)]
#[test]
fn a_non_bind_edit_under_an_unrecorded_daemon_lands() {
    let rig = rig("[[models]]\nid = \"m\"\n");
    let _held = crate::daemon::hold_daemon_lock();
    fs::write(
        rig.home.home().join(".clauth").join("clauthd.pid"),
        format!("{}\n", std::process::id()),
    )
    .expect("stamp the holder pid");
    apply_edit(
        &rig.record,
        &set_model("m", ModelField::DisplayName, Some("M")),
    )
    .expect("lands");
    assert_eq!(
        rig.config_text(),
        "[[models]]\nid = \"m\"\ndisplay_name = \"M\"\n"
    );
}

#[test]
fn with_no_daemon_the_view_reads_the_override_from_this_processs_env() {
    let home = HomeSandbox::new();
    let record = record_with_env(&home, "[server]\nbind = \"127.0.0.1:3067\"\n", "");
    let _pin = crate::testutil::EnvPin::new(
        &home,
        &[(BIND_ENV, Some(std::ffi::OsStr::new("127.0.0.1:4000")))],
    );
    assert_eq!(
        read_view(&record).expect("view").server.bind_override,
        Some("SHUNT_SERVER__BIND".to_string())
    );
}

// ── inputs from the earlier reviews ─────────────────────────────────────────

#[test]
fn a_removed_first_entry_takes_its_file_start_block_and_leaves_the_rest() {
    let _home = HomeSandbox::new();
    // r1 F1
    assert_eq!(
        planned(
            "# upstreams section\n[[upstreams]]\nname = \"a\"\n\n# backup provider, keep\n[[upstreams]]\nname = \"b\"\n",
            Edit::RemoveUpstream { name: "a".into() }
        ),
        "\n# backup provider, keep\n[[upstreams]]\nname = \"b\"\n"
    );
    // r3 R3
    assert_eq!(
        planned(
            "# a: primary\n[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"b\"\n",
            Edit::RemoveUpstream { name: "a".into() }
        ),
        "\n[[upstreams]]\nname = \"b\"\n"
    );
    // r2 N2 (Q1b)
    assert_eq!(
        planned(
            "[server]\nbind = \"x\"\n\n# a: the primary anthropic account\n[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"b\"\n",
            Edit::RemoveUpstream { name: "a".into() }
        ),
        "[server]\nbind = \"x\"\n\n[[upstreams]]\nname = \"b\"\n"
    );
    // r1 P20: a file-start block set off by a blank line stays
    assert_eq!(
        planned(
            "# my shunt config, do not lose\n\n[[upstreams]]\nname = \"a\"\n\n[[models]]\nid = \"m\"\n",
            Edit::RemoveUpstream { name: "a".into() }
        ),
        "# my shunt config, do not lose\n\n[[models]]\nid = \"m\"\n"
    );
    // r3 m-e: a list split by another table
    assert_eq!(
        planned(
            "# ups\n\n# a own\n[[upstreams]]\nname = \"a\"\n\n[[models]]\nid = \"m\"\n\n[[upstreams]]\nname = \"b\"\n",
            Edit::RemoveUpstream { name: "a".into() }
        ),
        "# ups\n\n[[models]]\nid = \"m\"\n\n[[upstreams]]\nname = \"b\"\n"
    );
}

#[test]
fn a_block_set_off_between_two_entries_refuses_their_removal() {
    let _home = HomeSandbox::new();
    // r6 F6: a block with blank lines on both sides
    assert_eq!(
        refused(
            "[[upstreams]]\nname = \"a\"\n\n# section: backups\n\n# b own\n[[upstreams]]\nname = \"b\"\n",
            Edit::RemoveUpstream { name: "b".into() }
        ),
        "cannot remove upstream \"b\": a comment next to it belongs to no single entry; edit the config by hand"
    );
}

#[test]
fn a_single_key_unset_keeps_every_comment_around_it() {
    let _home = HomeSandbox::new();
    // r4 X1 I23 / I24
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\nkind = \"anthropic\"\n# base_url = \"https://old\"\neffort = \"high\"\n",
            set_upstream("a", UpstreamField::Effort, None)
        ),
        "[[upstreams]]\nname = \"a\"\nkind = \"anthropic\"\n# base_url = \"https://old\"\n"
    );
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\n# --- tuning ---\n\neffort = \"high\"\n",
            set_upstream("a", UpstreamField::Effort, None)
        ),
        "[[upstreams]]\nname = \"a\"\n# --- tuning ---\n\n"
    );
    // r5 M2: an inline map's slug
    assert_eq!(
        planned(
            "[[models]]\nid = \"m1\"\nupstream_model = { claude = \"x\", codex = \"y\" }\n",
            set_model("m1", ModelField::UpstreamModel("claude".into()), None)
        ),
        "[[models]]\nid = \"m1\"\nupstream_model = { codex = \"y\" }\n"
    );
    // r5 m1: dotted keys
    assert_eq!(
        planned(
            "server.bind = \"127.0.0.1:1\"\nserver.default_provider = \"a\"\n",
            Edit::Server {
                field: ServerField::DefaultProvider,
                value: None,
            }
        ),
        "server.bind = \"127.0.0.1:1\"\n"
    );
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"u\"\nauth.mode = \"api_key\"\nauth.env = \"K\"\n",
            set_upstream("u", UpstreamField::AuthEnv, None)
        ),
        "[[upstreams]]\nname = \"u\"\nauth.mode = \"api_key\"\n"
    );
    assert_eq!(
        planned(
            "[[models]]\nid = \"m\"\nupstream_model.a = \"s\"\n",
            set_model("m", ModelField::UpstreamModel("a".into()), None)
        ),
        "[[models]]\nid = \"m\"\n"
    );
}

#[test]
fn a_key_lands_among_its_tables_own_keys_never_in_a_sub_table() {
    let _home = HomeSandbox::new();
    // r4 X3 I2
    assert_eq!(
        planned(
            "[[models]]\nid = \"m\"\n[models.upstream_model]\nwork = \"s\"\n",
            set_model("m", ModelField::DisplayName, Some("M"))
        ),
        "[[models]]\nid = \"m\"\ndisplay_name = \"M\"\n[models.upstream_model]\nwork = \"s\"\n"
    );
    // r4 X3 I3: the shape clauth's own admin edit writes
    assert_eq!(
        planned(
            "[server]\nbind = \"127.0.0.1:1\"\n\n[server.admin]\n\n[[server.admin.write_keys]]\nid = \"clauth\"\nkey = \"${file:/k}\"\n",
            Edit::Server {
                field: ServerField::DefaultProvider,
                value: Some("a".into()),
            }
        ),
        "[server]\nbind = \"127.0.0.1:1\"\ndefault_provider = \"a\"\n\n[server.admin]\n\n[[server.admin.write_keys]]\nid = \"clauth\"\nkey = \"${file:/k}\"\n"
    );
    // r4 X3 I32: `server` implicit under `[server.admin]`
    assert_eq!(
        planned(
            "[server.admin]\ntokens_env = \"T\"\n",
            Edit::Server {
                field: ServerField::Bind,
                value: Some("127.0.0.1:4000".into()),
            }
        ),
        "[server.admin]\ntokens_env = \"T\"\n\n[server]\nbind = \"127.0.0.1:4000\"\n"
    );
    // r5 m1: dotted auth gains accounts
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"u\"\nauth.mode = \"claude_oauth\"\n",
            accounts("u", &["a"])
        ),
        "[[upstreams]]\nname = \"u\"\nauth.mode = \"claude_oauth\"\nauth.accounts = [\"a\"]\n"
    );
}

#[test]
fn a_block_before_the_closing_bracket_stays_with_the_list() {
    let _home = HomeSandbox::new();
    // r5 B1
    assert_eq!(
        planned(
            "upstreams = [\n  { name = \"a\" },\n  { name = \"b\" }\n  # more later\n]\n",
            Edit::RemoveUpstream { name: "b".into() }
        ),
        "upstreams = [\n  { name = \"a\" },\n  # more later\n]\n"
    );
    assert_eq!(
        planned(
            "routes = [\n  { model = \"m\", provider = \"a\" } # the only route\n  # add more routes here\n]\n",
            Edit::RemoveRoute {
                route: route(1, "m", "a")
            }
        ),
        "routes = [\n  # add more routes here\n]\n"
    );
    let pool = |list: &str| {
        format!(
            "[[upstreams]]\nname = \"u\"\n[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = [\n{list}]\n"
        )
    };
    let held = pool("  \"a\", # main\n  \"b\" # spare\n  # more later\n");
    assert_eq!(
        planned(&held, accounts("u", &["a"])),
        pool("  \"a\", # main\n  # more later\n")
    );
    // r6 F1
    assert_eq!(
        planned(&held, accounts("u", &["b", "a"])),
        pool("  \"b\", # spare\n  \"a\" # main\n  # more later\n")
    );
    // r4 n-2: an indented `]` keeps its indent
    assert_eq!(
        planned(&pool("    \"a\",\n    \"b\",\n  "), accounts("u", &["a"])),
        pool("    \"a\",\n  ")
    );
}

#[test]
fn comment_above_lists_follow_the_ownership_rule() {
    let _home = HomeSandbox::new();
    // r3 R1: each block glued under the previous element
    let upstreams = "upstreams = [\n  # a: primary\n  { name = \"a\" },\n  # b: backup\n  { name = \"b\" },\n  # c: spare\n  { name = \"c\" },\n]\n";
    for name in ["a", "b", "c"] {
        assert_eq!(
            refused(upstreams, Edit::RemoveUpstream { name: name.into() }),
            format!(
                "cannot remove upstream {name:?}: a comment next to it belongs to no single entry; edit the config by hand"
            )
        );
    }
    // r3 R2 / r6 F2
    let pool = |list: &str| {
        format!(
            "[[upstreams]]\nname = \"u\"\n[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = [\n{list}]\n"
        )
    };
    let glued = pool("  # main\n  \"a\",\n  # spare\n  \"b\",\n");
    assert_eq!(
        refused(&glued, accounts("u", &["b"])),
        "cannot remove account \"a\" of upstream \"u\": a comment next to it belongs to no single entry; edit the config by hand"
    );
    assert_eq!(
        refused(&glued, accounts("u", &["b", "a"])),
        "cannot reorder the accounts of upstream \"u\": a comment between them belongs to no single entry; edit the config by hand"
    );
    let set_off = pool("  # main\n  \"a\",\n\n  # spare\n  \"b\",\n");
    assert_eq!(
        planned(&set_off, accounts("u", &["b", "a"])),
        pool("  # spare\n  \"b\",\n\n  # main\n  \"a\",\n")
    );
    assert_eq!(
        planned(&set_off, accounts("u", &["b"])),
        pool("\n  # spare\n  \"b\",\n")
    );
}

#[test]
fn list_edits_keep_each_layout_shunt_accepts() {
    let _home = HomeSandbox::new();
    let add = || {
        Edit::AddUpstream(NewUpstream {
            name: "c".into(),
            ..NewUpstream::default()
        })
    };
    // r6 F4
    assert_eq!(
        planned("upstreams = [{ name = \"a\" }, { name = \"b\" },]\n", add()),
        "upstreams = [{ name = \"a\" }, { name = \"b\" }, { name = \"c\" },]\n"
    );
    // r6 F5
    assert_eq!(
        planned(
            "upstreams = [\n  { name = \"a\" },\n  { name = \"b\" }]\n",
            add()
        ),
        "upstreams = [\n  { name = \"a\" },\n  { name = \"b\" },\n  { name = \"c\" }]\n"
    );
    // r2 N1(b) / Q4c: same-line tails
    assert_eq!(
        planned(
            "upstreams = [\n  { name = \"a\" }, # a: primary\n  { name = \"b\" }, # b: backup\n  { name = \"c\" },\n]\n",
            Edit::RemoveUpstream { name: "b".into() }
        ),
        "upstreams = [\n  { name = \"a\" }, # a: primary\n  { name = \"c\" },\n]\n"
    );
    // r4 X2: an add into an inline list keeps its map
    assert_eq!(
        planned(
            "models = [{ id = \"m1\" }]\n",
            Edit::AddModel(NewModel {
                id: "m2".into(),
                upstream_model: vec![("claude".into(), "x".into())],
                ..NewModel::default()
            })
        ),
        "models = [{ id = \"m1\" }, { id = \"m2\", upstream_model = { claude = \"x\" } }]\n"
    );
    // r6 F3: a multi-line accounts list inside an inline auth table
    let inline = |list: &str| {
        format!(
            "[[upstreams]]\nname = \"u\"\nauth = {{ mode = \"claude_oauth\", accounts = [\n{list}] }}\n"
        )
    };
    let held = inline("  # main\n  \"a\", # a tail\n  \"b\" # b tail\n");
    assert_eq!(
        planned(&held, accounts("u", &["a"])),
        inline("  # main\n  \"a\", # a tail\n")
    );
    assert_eq!(
        planned(&held, accounts("u", &["a", "b", "c"])),
        inline("  # main\n  \"a\", # a tail\n  \"b\", # b tail\n  \"c\"\n")
    );
    // r2 N3
    let pool = |list: &str| {
        format!(
            "[[upstreams]]\nname = \"u\"\n[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = {list}\n"
        )
    };
    assert_eq!(
        planned(
            &pool("[\n  \"a\", # main\n  \"b\", # spare, rate-limited\n]"),
            accounts("u", &["a", "c"])
        ),
        pool("[\n  \"a\", # main\n  \"c\",\n]")
    );
    // r2 S6 / r3 R6: the same list in any spelling is a no-op
    assert!(
        plan(&pool("[\"a\",\"b\"]"), &accounts("u", &["a", "b"]))
            .expect("plans")
            .is_none()
    );
    // r1 F8: unsetting accounts where none are listed
    assert!(
        plan(
            "[[upstreams]]\nname = \"u\"\nauth = \"claude_oauth\"\n",
            &accounts("u", &[])
        )
        .expect("plans")
        .is_none()
    );
}

#[test]
fn a_rename_keeps_every_comment_and_never_overwrites_a_key() {
    let _home = HomeSandbox::new();
    let rename = |name: &str, to: &str| Edit::RenameUpstream {
        name: name.into(),
        to: to.into(),
    };
    // r3 R4
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\n\n[[models]]\nid = \"m\"\n[models.upstream_model]\n# the main slug\na = \"x\" # keep\n# the alt slug\nb = \"y\"\n",
            rename("a", "z")
        ),
        "[[upstreams]]\nname = \"z\"\n\n[[models]]\nid = \"m\"\n[models.upstream_model]\n# the main slug\nz = \"x\" # keep\n# the alt slug\nb = \"y\"\n"
    );
    // r3 R5
    assert_eq!(
        refused(
            "[[upstreams]]\nname = \"a\"\n\n[[models]]\nid = \"m\"\n[models.upstream_model]\na = \"x\"\nz = \"y\"\n",
            rename("a", "z")
        ),
        "cannot rename upstream \"a\" to \"z\": a [[models]] upstream_model key already names \"z\"; change that reference first"
    );
    // r4 m-1: inline spellings of `server` and the codex endpoint
    let planned_rename = plan(
        "upstreams = [{ name = \"c\" }]\nserver = { default_provider = \"c\", codex_endpoint = { provider = \"c\", routes = [{ model = \"g\", provider = \"c\" }] } }\n",
        &rename("c", "z"),
    )
    .expect("plans")
    .expect("changes");
    assert_eq!(
        planned_rename.candidate,
        "upstreams = [{ name = \"z\" }]\nserver = { default_provider = \"z\", codex_endpoint = { provider = \"z\", routes = [{ model = \"g\", provider = \"z\" }] } }\n"
    );
    assert_eq!(
        planned_rename.cascade,
        Some(Cascade {
            default_provider: 1,
            codex_endpoint: 1,
            codex_routes: 1,
            ..Cascade::default()
        })
    );
    // r4 n-4: a literal shorthand mode keeps its spelling
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"u\"\nauth = 'passthrough'\n",
            set_upstream("u", UpstreamField::AuthAccount, Some("w"))
        ),
        "[[upstreams]]\nname = \"u\"\nauth = { mode = 'passthrough', account = \"w\" }\n"
    );
}

#[test]
fn line_endings_and_odd_strings_hold() {
    let _home = HomeSandbox::new();
    // r4 n-5
    assert_eq!(
        planned(
            "[[upstreams]]\r\nname = \"a\"\r\n",
            set_upstream("a", UpstreamField::Effort, Some("low"))
        ),
        "[[upstreams]]\r\nname = \"a\"\r\neffort = \"low\"\r\n"
    );
    // r5 m2: a multi-line string ending in quotes, then a `#` inside a string
    let text = "[[upstreams]]\nname = \"a\"\nkind = \"\"\"He said \"hi\"\"\"\"\neffort = \"e\\\"#x\" # real\n";
    assert_eq!(
        planned(text, set_upstream("a", UpstreamField::Effort, Some("low"))),
        "[[upstreams]]\nname = \"a\"\nkind = \"\"\"He said \"hi\"\"\"\"\neffort = \"low\" # real\n"
    );
    // r1 F14: a non-string slug reads as written, never as an invented value
    assert_eq!(
        view_of(
            "[[models]]\nid = \"m\"\n[models.upstream_model]\nwork = 3\n",
            None
        )
        .expect("view")
        .models[0]
            .upstream_model,
        Some(vec![(
            "work".to_string(),
            Written {
                raw: "3".to_string(),
                value: None,
            }
        )])
    );
}

/// Each field lands under its own key: a field written under another key
/// passes shunt's check wherever the table takes unknown keys (`[[routes]]`).
#[test]
fn every_field_lands_under_its_own_key() {
    let _home = HomeSandbox::new();
    let upstream = "[[upstreams]]\nname = \"u\"\n";
    for (field, key) in [
        (UpstreamField::Provider, "provider"),
        (UpstreamField::Kind, "kind"),
        (UpstreamField::BaseUrl, "base_url"),
        (UpstreamField::Effort, "effort"),
        (UpstreamField::ServiceTier, "service_tier"),
    ] {
        assert_eq!(
            planned(upstream, set_upstream("u", field, Some("v"))),
            format!("[[upstreams]]\nname = \"u\"\n{key} = \"v\"\n"),
            "{field:?}"
        );
    }
    let routes = "[[routes]]\nmodel = \"m\"\nprovider = \"a\"\n";
    for (field, expected) in [
        (
            RouteField::Model,
            "[[routes]]\nmodel = \"v\"\nprovider = \"a\"\n",
        ),
        (
            RouteField::Provider,
            "[[routes]]\nmodel = \"m\"\nprovider = \"v\"\n",
        ),
        (
            RouteField::UpstreamModel,
            "[[routes]]\nmodel = \"m\"\nprovider = \"a\"\nupstream_model = \"v\"\n",
        ),
        (
            RouteField::Effort,
            "[[routes]]\nmodel = \"m\"\nprovider = \"a\"\neffort = \"v\"\n",
        ),
        (
            RouteField::ServiceTier,
            "[[routes]]\nmodel = \"m\"\nprovider = \"a\"\nservice_tier = \"v\"\n",
        ),
    ] {
        assert_eq!(
            planned(
                routes,
                Edit::Route {
                    route: route(1, "m", "a"),
                    field,
                    value: Some("v".into()),
                }
            ),
            expected,
            "{field:?}"
        );
    }
    let server = "[server]\nbind = \"127.0.0.1:1\"\n";
    assert_eq!(
        planned(
            server,
            Edit::Server {
                field: ServerField::Bind,
                value: Some("127.0.0.1:2".into()),
            }
        ),
        "[server]\nbind = \"127.0.0.1:2\"\n"
    );
    assert_eq!(
        planned(
            server,
            Edit::Server {
                field: ServerField::DefaultProvider,
                value: Some("v".into()),
            }
        ),
        "[server]\nbind = \"127.0.0.1:1\"\ndefault_provider = \"v\"\n"
    );
}

#[test]
fn a_name_held_twice_refuses_instead_of_picking_one() {
    let _home = HomeSandbox::new();
    assert_eq!(
        refused(
            "[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"a\"\n",
            set_upstream("a", UpstreamField::Effort, Some("low"))
        ),
        "cannot set effort of upstream \"a\": the config has more than one upstream named \"a\"; edit the config by hand"
    );
    let pool = "[[upstreams]]\nname = \"u\"\nauth = { mode = \"claude_oauth\", accounts = [\"a\", \"a\"] }\n";
    assert_eq!(
        refused(pool, accounts("u", &["a"])),
        "cannot set the accounts of upstream \"u\": the config names account \"a\" twice in upstream \"u\""
    );
    assert_eq!(
        refused(
            "[[upstreams]]\nname = \"u\"\nauth = \"claude_oauth\"\n",
            accounts("u", &["b", "c", "b"])
        ),
        "cannot set the accounts of upstream \"u\": the accounts name \"b\" twice"
    );
}

// ── round 2 rulings (cloudy, 2026-10-07) ────────────────────────────────────

#[test]
fn a_bind_edit_to_a_reference_refuses_naming_the_env_file_fix() {
    let _home = HomeSandbox::new();
    let text = "[server]\nbind = \"127.0.0.1:3067\"\n";
    for value in ["${GATEWAY_BIND}", "127.0.0.1:${PORT}"] {
        assert_eq!(
            refused(
                text,
                Edit::Server {
                    field: ServerField::Bind,
                    value: Some(value.into()),
                }
            ),
            "cannot set server.bind: [server].bind is a ${...} reference, which clauth does not resolve; set SHUNT_SERVER__BIND in the gateway's env file to the address instead"
        );
    }
}

#[test]
fn emptying_the_accounts_removes_the_key() {
    let _home = HomeSandbox::new();
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"u\"\nauth = { mode = \"claude_oauth\", accounts = [\"a\", \"b\"] }\n",
            accounts("u", &[])
        ),
        "[[upstreams]]\nname = \"u\"\nauth = { mode = \"claude_oauth\" }\n"
    );
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"u\"\n[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = [\n  \"a\", # main\n  # spare below\n  \"b\",\n]\neffort_note = 1\n",
            accounts("u", &[])
        ),
        "[[upstreams]]\nname = \"u\"\n[upstreams.auth]\nmode = \"claude_oauth\"\neffort_note = 1\n"
    );
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"u\"\nauth.mode = \"claude_oauth\"\nauth.accounts = [\"a\"]\n",
            accounts("u", &[])
        ),
        "[[upstreams]]\nname = \"u\"\nauth.mode = \"claude_oauth\"\n"
    );
}

#[test]
fn a_comment_block_at_the_files_end_lies_in_no_gap() {
    let _home = HomeSandbox::new();
    let text = "[[routes]]\nmodel = \"a\"\nprovider = \"x\"\n\n[[routes]]\nmodel = \"b\"\nprovider = \"y\"\n\n# end notes\n# keep these\n";
    assert_eq!(
        planned(
            text,
            Edit::RemoveRoute {
                route: route(2, "b", "y")
            }
        ),
        "[[routes]]\nmodel = \"a\"\nprovider = \"x\"\n\n# end notes\n# keep these\n"
    );
    assert_eq!(
        planned(
            "[[routes]]\nmodel = \"a\"\nprovider = \"x\"\n\n# end notes\n",
            Edit::RemoveRoute {
                route: route(1, "a", "x")
            }
        ),
        "\n# end notes\n"
    );
    // a comment glued under the last entry is the entry's own
    let glued = "[[routes]]\nmodel = \"a\"\nprovider = \"x\"\n\n[[routes]]\nmodel = \"b\"\nprovider = \"y\"\n# under b\n\n# end notes\n";
    assert_eq!(
        planned(
            glued,
            Edit::RemoveRoute {
                route: route(2, "b", "y")
            }
        ),
        "[[routes]]\nmodel = \"a\"\nprovider = \"x\"\n\n# end notes\n"
    );
}

/// Every key shunt 03d99f1 reads a model id from (`named_targets` /
/// `named_judges`, `src/config/router.rs`, `router/*.rs`,
/// `src/config/subagents.rs`, `subagents/classifier.rs`), a `[1m]` or `[1M]`
/// hint kept; a custom classifier's `default_target` and its `models` keys
/// name groups, not ids, and stay.
const ROUTERS: &str = r#"[[models]]
id = "m"
upstream_model = { a = "s" }

[[models]]
id = "stage"
[models.router]
type = "stage_router"
capable_target = "m"
efficient_target = "e"
[models.router.classifier]
target = "m[1m]"

[[models]]
id = "auto"
router = { type = "auto", capable_target = "m", efficient_target = "e" }

[[models]]
id = "rand"
router.type = "random"
router.targets = ["m", "e", 'm[1M]']

[[models]]
id = "pre"
[models.router]
type = "prefill_router"
targets = ["e", "m"]
checkpoint = "/ck"

[[models]]
id = "cap"
[models.router]
type = "llm_classifier"
mode = "capability"
classifier_target = "m"
strong_target = "m"
weak_target = "e"
base_threshold = 0.5

[[models]]
id = "cus"
[models.router]
type = "llm_classifier"
mode = "custom"
default_target = "m"
prompt = "m"
response_schema = "{}"
policy = { type = "target_selector", selector = "/g" }
models = { judge = ["m"], any = ["m", "e"], m = ["e"] }

[[models]]
id = "esc"
[models.router]
type = "llm_classifier"
mode = "escalation"
classifier_target = "e"
strong_target = "m"
weak_target = "e"

[[models]]
id = "comp"
[models.router]
type = "composite"
[models.router.classifier]
target = "m"
base_threshold = 0.5
classify_trigger = "every_request"
[models.router.stage]
capable_target = "m"
efficient_target = "e"
confidence_threshold = 0.5

[[models]]
id = "adv"
[models.router]
type = "advisor"
executor_target = "e"
advisor_target = "m"

[[models]]
id = "noop"
router = { type = "noop" }

[[models]]
id = "sub"
[models.subagents]
type = "passthrough"
target = "m"
by_type = { Explore = "m", Plan = "e" }

[[models]]
id = "subc"
[models.subagents]
type = "llm_classifier"
mode = "custom"
default_target = "m"
prompt = "p"
response_schema = "{}"
policy = { type = "target_selector", selector = "/g" }
models = { judge = ["e"], any = ["m"] }

[[routes]]
model = "m"
provider = "a"
"#;

#[test]
fn a_model_rename_cascades_to_every_router_and_subagents_form() {
    let _home = HomeSandbox::new();
    let planned_rename = plan(
        ROUTERS,
        &Edit::RenameModel {
            id: "m".into(),
            to: "m2".into(),
        },
    )
    .expect("plans")
    .expect("changes");
    assert_eq!(
        planned_rename.candidate,
        r#"[[models]]
id = "m2"
upstream_model = { a = "s" }

[[models]]
id = "stage"
[models.router]
type = "stage_router"
capable_target = "m2"
efficient_target = "e"
[models.router.classifier]
target = "m2[1m]"

[[models]]
id = "auto"
router = { type = "auto", capable_target = "m2", efficient_target = "e" }

[[models]]
id = "rand"
router.type = "random"
router.targets = ["m2", "e", 'm2[1M]']

[[models]]
id = "pre"
[models.router]
type = "prefill_router"
targets = ["e", "m2"]
checkpoint = "/ck"

[[models]]
id = "cap"
[models.router]
type = "llm_classifier"
mode = "capability"
classifier_target = "m2"
strong_target = "m2"
weak_target = "e"
base_threshold = 0.5

[[models]]
id = "cus"
[models.router]
type = "llm_classifier"
mode = "custom"
default_target = "m"
prompt = "m"
response_schema = "{}"
policy = { type = "target_selector", selector = "/g" }
models = { judge = ["m2"], any = ["m2", "e"], m = ["e"] }

[[models]]
id = "esc"
[models.router]
type = "llm_classifier"
mode = "escalation"
classifier_target = "e"
strong_target = "m2"
weak_target = "e"

[[models]]
id = "comp"
[models.router]
type = "composite"
[models.router.classifier]
target = "m2"
base_threshold = 0.5
classify_trigger = "every_request"
[models.router.stage]
capable_target = "m2"
efficient_target = "e"
confidence_threshold = 0.5

[[models]]
id = "adv"
[models.router]
type = "advisor"
executor_target = "e"
advisor_target = "m2"

[[models]]
id = "noop"
router = { type = "noop" }

[[models]]
id = "sub"
[models.subagents]
type = "passthrough"
target = "m2"
by_type = { Explore = "m2", Plan = "e" }

[[models]]
id = "subc"
[models.subagents]
type = "llm_classifier"
mode = "custom"
default_target = "m"
prompt = "p"
response_schema = "{}"
policy = { type = "target_selector", selector = "/g" }
models = { judge = ["e"], any = ["m2"] }

[[routes]]
model = "m2"
provider = "a"
"#
    );
    assert_eq!(
        planned_rename.cascade,
        Some(Cascade {
            route_models: 1,
            routers: 14,
            subagents: 3,
            ..Cascade::default()
        })
    );
    assert_eq!(
        refused(
            ROUTERS,
            Edit::RenameModel {
                id: "m".into(),
                to: "e".into(),
            }
        ),
        "cannot rename model \"m\" to \"e\": a [models.router] or [models.subagents] target already names \"e\"; change that reference first"
    );
}

// ── round 3 (review r2; cloudy's rulings, 2026-10-07) ───────────────────────

/// An upstream `p` whose `[upstreams.auth]` lists `accounts = [\n{list}]`.
fn pool_p(list: &str) -> String {
    format!(
        "[[upstreams]]\nname = \"p\"\n[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = [\n{list}]\n"
    )
}

const POOL_SHAPE: &str = "cannot set the accounts of upstream \"p\": the config spells \"upstreams.auth.accounts\" in a shape clauth does not edit; edit the config by hand";

/// The refusal [`plan_with`] meets when the production builder's candidate
/// is replaced by `candidate(<it>)`, its owned ranges left as built.
fn bug_over(text: &str, edit: Edit, candidate: impl FnOnce(String) -> String) -> String {
    bug_of(plan_with(text, &edit, |ctx, edit| {
        let mut change = ctx.change(edit)?;
        change.candidate = candidate(change.candidate);
        Ok(change)
    }))
}

/// The refusal [`plan_with`] meets over a builder handing back `candidate`
/// with `owned`, each the only place it occurs in `text`, as its exempted
/// ranges.
fn bug_with(text: &str, edit: Edit, owned: &[&str], candidate: &str) -> String {
    let owned = owned
        .iter()
        .map(|own| {
            let at = text.find(own).expect("owned text");
            at..at + own.len()
        })
        .collect();
    bug_of(plan_with(text, &edit, |_, _| {
        Ok(Change {
            candidate: candidate.to_string(),
            owned,
            cascade: None,
        })
    }))
}

fn bug_of(planned: Result<Option<Planned>>) -> String {
    match planned {
        Ok(_) => panic!("the wrong candidate planned"),
        Err(e) => {
            assert!(
                e.downcast_ref::<EditBug>().is_some(),
                "an EditBug, got: {e:#}"
            );
            e.to_string()
        }
    }
}

#[test]
fn a_cut_beside_a_shared_line_keeps_every_other_entrys_comment() {
    let _home = HomeSandbox::new();
    let shared = pool_p("  \"a\", \"b\",\n\n  # c own\n  \"c\",\n");
    assert_eq!(
        planned(&shared, accounts("p", &["a", "c"])),
        pool_p("  \"a\",\n\n  # c own\n  \"c\",\n")
    );
    assert_eq!(
        planned(&shared, accounts("p", &["b", "c"])),
        pool_p("  \"b\",\n\n  # c own\n  \"c\",\n")
    );
    let upstreams = "upstreams = [\n  { name = \"a\" }, { name = \"b\" },\n\n  # c: backup\n  { name = \"c\" },\n]\n";
    assert_eq!(
        planned(upstreams, Edit::RemoveUpstream { name: "b".into() }),
        "upstreams = [\n  { name = \"a\" },\n\n  # c: backup\n  { name = \"c\" },\n]\n"
    );
    assert_eq!(
        planned(upstreams, Edit::RemoveUpstream { name: "a".into() }),
        "upstreams = [\n  { name = \"b\" },\n\n  # c: backup\n  { name = \"c\" },\n]\n"
    );
    // a block above a line of two belongs to the line's first element
    let above = pool_p("  \"x\",\n\n  # ab\n  \"a\", \"b\",\n  \"c\",\n");
    assert_eq!(
        planned(&above, accounts("p", &["x", "a", "c"])),
        pool_p("  \"x\",\n\n  # ab\n  \"a\",\n  \"c\",\n")
    );
    assert_eq!(refused(&above, accounts("p", &["x", "b", "c"])), POOL_SHAPE);
}

#[test]
fn a_last_entry_closing_its_list_leaves_the_previous_entrys_comma_and_comment() {
    let _home = HomeSandbox::new();
    assert_eq!(
        planned(&pool_p("  \"a\", # main\n  \"b\""), accounts("p", &["a"])),
        pool_p("  \"a\", # main\n")
    );
    assert_eq!(
        planned(&pool_p("  \"a\",\n  \"b\""), accounts("p", &["a"])),
        pool_p("  \"a\",\n")
    );
    assert_eq!(
        planned(
            "upstreams = [\n  { name = \"a\" }, # primary\n  { name = \"b\" }]\n",
            Edit::RemoveUpstream { name: "b".into() }
        ),
        "upstreams = [\n  { name = \"a\" }, # primary\n]\n"
    );
    assert_eq!(
        planned(
            "upstreams = [\n  { name = \"a\" },\n  { name = \"b\" }]\n",
            Edit::RemoveUpstream { name: "b".into() }
        ),
        "upstreams = [\n  { name = \"a\" },\n]\n"
    );
    // its own block goes with it, with the blank line before it
    assert_eq!(
        planned(
            &pool_p("  \"a\",\n\n  # b own\n  \"b\""),
            accounts("p", &["a"])
        ),
        pool_p("  \"a\",\n")
    );
    // a comment after its `]` is the list's as much as the entry's
    let tail = "[[upstreams]]\nname = \"p\"\n[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = [\n  \"a\",\n  \"b\"] # spare\n";
    assert_eq!(refused(tail, accounts("p", &["a"])), POOL_SHAPE);
}

#[test]
fn the_guard_sees_a_cut_that_swallows_a_neighbours_comment() {
    let _home = HomeSandbox::new();
    assert_eq!(
        bug_over(
            &pool_p("  \"a\", \"b\",\n\n  # c own\n  \"c\",\n"),
            accounts("p", &["a", "c"]),
            |_| pool_p("  \"a\", \"c\",\n")
        ),
        lost("cannot set the accounts of upstream \"p\"")
    );
    assert_eq!(
        bug_over(
            "upstreams = [\n  { name = \"a\" }, { name = \"b\" },\n\n  # c: backup\n  { name = \"c\" },\n]\n",
            Edit::RemoveUpstream { name: "b".into() },
            |_| "upstreams = [\n  { name = \"a\" }, { name = \"c\" },\n]\n".to_string()
        ),
        lost("cannot remove upstream \"b\"")
    );
    assert_eq!(
        bug_over(
            &pool_p("  \"a\", # main\n  \"b\""),
            accounts("p", &["a"]),
            |_| pool_p("  \"a\"")
        ),
        lost("cannot set the accounts of upstream \"p\"")
    );
    assert_eq!(
        bug_over(
            "[[upstreams]]\nname = \"a\"\n\n# b own\n[[upstreams]]\nname = \"b\"\n",
            Edit::RemoveUpstream { name: "a".into() },
            |_| "[[upstreams]]\nname = \"b\"\n".to_string()
        ),
        lost("cannot remove upstream \"a\"")
    );
    assert_eq!(
        bug_over(
            "[[upstreams]]\nname = \"a\"\n# keep\n\n[upstreams.auth]\nmode = \"api_key\"\n",
            set_upstream("a", UpstreamField::AuthMode, None),
            |_| "[[upstreams]]\nname = \"a\"\n".to_string()
        ),
        lost("cannot unset auth.mode of upstream \"a\"")
    );
    assert_eq!(
        bug_over(
            "[[upstreams]]\nname = \"a\"\neffort = \"high\"\n# keep\n",
            set_upstream("a", UpstreamField::Effort, None),
            |_| "[[upstreams]]\nname = \"a\"\n".to_string()
        ),
        lost("cannot unset effort of upstream \"a\"")
    );
}

#[test]
fn every_removal_kind_takes_exactly_its_owned_comments() {
    let _home = HomeSandbox::new();
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\n\n# b own\n[[upstreams]]\nname = \"b\"\n# b's kind below\nkind = \"anthropic\" # inline\n[upstreams.auth]\n# the mode\nmode = \"api_key\"\n\n[[upstreams]]\nname = \"c\"\n",
            Edit::RemoveUpstream { name: "b".into() }
        ),
        "[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"c\"\n"
    );
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\n\n# auth below\n[upstreams.auth]\n# api key mode\nmode = \"api_key\" # k\nenv = \"K\"\n",
            set_upstream("a", UpstreamField::AuthMode, None)
        ),
        "[[upstreams]]\nname = \"a\"\n"
    );
    assert_eq!(
        planned(
            "[[models]]\nid = \"m\"\n\n[models.upstream_model]\n# the one slug\nclaude = \"x\" # x\n",
            set_model("m", ModelField::UpstreamModel("claude".into()), None)
        ),
        "[[models]]\nid = \"m\"\n"
    );
    assert_eq!(
        planned(
            &pool_p("  # a own\n  \"a\", # a tail\n\n  # b own\n  \"b\", # b tail\n  \"c\",\n"),
            accounts("p", &["c"])
        ),
        pool_p("  \"c\",\n")
    );
}

/// [`plan`] drives the verification: a builder handing back a wrong
/// candidate refuses as clauth's bug, nothing planned.
#[test]
fn a_wrong_candidate_from_the_builder_refuses_as_clauths_bug() {
    let _home = HomeSandbox::new();
    assert_eq!(
        bug_over(
            "[[upstreams]]\nname = \"a\"\n",
            set_upstream("a", UpstreamField::Effort, Some("low")),
            |candidate| candidate.replace("\"low\"", "\"high\"")
        ),
        "cannot set effort of upstream \"a\": clauth built an edit that changes the config's value beyond the edit; nothing was written; report this as a clauth bug"
    );
}

#[test]
fn an_array_of_tables_add_lands_below_a_comment_glued_under_the_last_entry() {
    let _home = HomeSandbox::new();
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"b\"\n# effort = \"high\"\n\n[[models]]\nid = \"m\"\n",
            Edit::AddUpstream(NewUpstream {
                name: "c".into(),
                ..NewUpstream::default()
            })
        ),
        "[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"b\"\n# effort = \"high\"\n\n[[upstreams]]\nname = \"c\"\n\n[[models]]\nid = \"m\"\n"
    );
    let add = || {
        Edit::AddRoute(NewRoute {
            model: "b".into(),
            provider: "y".into(),
            ..NewRoute::default()
        })
    };
    assert_eq!(
        planned(
            "[[routes]]\nmodel = \"a\"\nprovider = \"x\"\n# effort = \"low\"\n",
            add()
        ),
        "[[routes]]\nmodel = \"a\"\nprovider = \"x\"\n# effort = \"low\"\n\n[[routes]]\nmodel = \"b\"\nprovider = \"y\"\n"
    );
    assert_eq!(
        planned(
            "[[routes]]\nmodel = \"a\"\nprovider = \"x\"\n# effort = \"low\"",
            add()
        ),
        "[[routes]]\nmodel = \"a\"\nprovider = \"x\"\n# effort = \"low\"\n\n[[routes]]\nmodel = \"b\"\nprovider = \"y\""
    );
}

#[test]
fn a_reorder_the_engine_cannot_lay_out_names_the_list() {
    let _home = HomeSandbox::new();
    assert_eq!(
        refused(
            &pool_p("  \"a\", # main\n  \"b\""),
            accounts("p", &["b", "a"])
        ),
        POOL_SHAPE
    );
    assert_eq!(
        planned(&pool_p("  \"a\",\n  \"b\""), accounts("p", &["b", "a"])),
        pool_p("  \"b\",\n  \"a\"")
    );
}

#[test]
fn unsetting_the_only_key_of_a_dotted_table_drops_the_table() {
    let _home = HomeSandbox::new();
    assert_eq!(
        planned(
            "server.bind = \"127.0.0.1:1\"\n",
            Edit::Server {
                field: ServerField::Bind,
                value: None,
            }
        ),
        ""
    );
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"u\"\nauth.env = \"K\"\n",
            set_upstream("u", UpstreamField::AuthEnv, None)
        ),
        "[[upstreams]]\nname = \"u\"\n"
    );
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"u\"\nauth.accounts = [\"a\"]\n",
            accounts("u", &[])
        ),
        "[[upstreams]]\nname = \"u\"\n"
    );
}

#[test]
fn only_a_comment_between_two_entries_of_one_list_blocks_a_remove() {
    let _home = HomeSandbox::new();
    let remove = |name: &str| Edit::RemoveUpstream { name: name.into() };
    // a header block preceded by a blank line
    assert_eq!(
        planned(
            "\n# header\n\n[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"b\"\n",
            remove("a")
        ),
        "\n# header\n\n[[upstreams]]\nname = \"b\"\n"
    );
    // two set-off blocks at the start, then at the end
    assert_eq!(
        planned(
            "# one\n\n# two\n\n[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"b\"\n",
            remove("a")
        ),
        "# one\n\n# two\n\n[[upstreams]]\nname = \"b\"\n"
    );
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"b\"\n\n# one\n\n# two\n",
            remove("b")
        ),
        "[[upstreams]]\nname = \"a\"\n\n# one\n\n# two\n"
    );
    // a section banner between [server] and the first entry stays set off,
    // never glued onto the next entry
    assert_eq!(
        planned(
            "[server]\nbind = \"127.0.0.1:1\"\n\n# upstreams\n\n[[upstreams]]\nname = \"a\"\n[[upstreams]]\nname = \"b\"\n",
            remove("a")
        ),
        "[server]\nbind = \"127.0.0.1:1\"\n\n# upstreams\n\n[[upstreams]]\nname = \"b\"\n"
    );
    // a section banner after the last entry
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"b\"\n\n# models\n\n[[models]]\nid = \"m\"\n",
            remove("b")
        ),
        "[[upstreams]]\nname = \"a\"\n\n# models\n\n[[models]]\nid = \"m\"\n"
    );
    // an inline list's block above its first element, set off from it
    assert_eq!(
        planned(
            "upstreams = [\n  # pool\n\n  { name = \"a\" },\n  { name = \"b\" },\n]\n",
            remove("a")
        ),
        "upstreams = [\n  # pool\n\n  { name = \"b\" },\n]\n"
    );
    assert_eq!(
        planned(
            &pool_p("  # pool\n\n  \"a\",\n  \"b\",\n"),
            accounts("p", &["b", "a"])
        ),
        pool_p("  # pool\n\n  \"b\",\n  \"a\",\n")
    );
    // a sub-table has no sibling: a comment set off from it stays
    assert_eq!(
        planned(
            "[[models]]\nid = \"m\"\n# codex = \"old\"\n\n[models.upstream_model]\nclaude = \"x\"\n",
            set_model("m", ModelField::UpstreamModel("claude".into()), None)
        ),
        "[[models]]\nid = \"m\"\n# codex = \"old\"\n"
    );
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\n# effort = \"high\"\n\n[upstreams.auth]\nmode = \"api_key\"\n",
            set_upstream("a", UpstreamField::AuthMode, None)
        ),
        "[[upstreams]]\nname = \"a\"\n# effort = \"high\"\n"
    );
}

#[test]
fn a_model_rename_cascades_to_every_route_serving_it() {
    let _home = HomeSandbox::new();
    let rename = |to: &str| Edit::RenameModel {
        id: "m".into(),
        to: to.into(),
    };
    let text = "[[models]]\nid = \"m\"\n\n[[routes]]\nmodel = \"m\" # main\nprovider = \"a\"\n\n[[routes]]\nmodel = \"m[1m]\"\nprovider = \"b\"\n\n[[routes]]\nmodel = \"x\"\nprovider = \"a\"\n";
    let planned_rename = plan(text, &rename("n")).expect("plans").expect("changes");
    assert_eq!(
        planned_rename.candidate,
        "[[models]]\nid = \"n\"\n\n[[routes]]\nmodel = \"n\" # main\nprovider = \"a\"\n\n[[routes]]\nmodel = \"m[1m]\"\nprovider = \"b\"\n\n[[routes]]\nmodel = \"x\"\nprovider = \"a\"\n"
    );
    assert_eq!(
        planned_rename.cascade,
        Some(Cascade {
            route_models: 1,
            ..Cascade::default()
        })
    );
    assert_eq!(
        planned(
            "routes = [{ model = 'm', provider = \"a\" }]\n\n[[models]]\nid = \"m\"\n",
            rename("n")
        ),
        "routes = [{ model = 'n', provider = \"a\" }]\n\n[[models]]\nid = \"n\"\n"
    );
    assert_eq!(
        refused(text, rename("x")),
        "cannot rename model \"m\" to \"x\": a [[routes]] model already names \"x\"; change that reference first"
    );
}

#[test]
fn an_upstream_rename_writes_the_implicit_default_it_names() {
    let _home = HomeSandbox::new();
    let rename = |name: &str, to: &str| Edit::RenameUpstream {
        name: name.into(),
        to: to.into(),
    };
    let check = |text: &str, edit: Edit, expected: &str, cascade: Cascade| {
        let planned = plan(text, &edit).expect("plans").expect("changes");
        assert_eq!(planned.candidate, expected);
        assert_eq!(planned.cascade, Some(cascade));
    };
    let default = Cascade {
        default_provider: 1,
        ..Cascade::default()
    };
    check(
        "[server]\nbind = \"127.0.0.1:1\"\n\n[[upstreams]]\nname = \"anthropic\"\n",
        rename("anthropic", "main"),
        "[server]\nbind = \"127.0.0.1:1\"\ndefault_provider = \"main\"\n\n[[upstreams]]\nname = \"main\"\n",
        default,
    );
    check(
        "[[upstreams]]\nname = \"anthropic\"\n",
        rename("anthropic", "main"),
        "[[upstreams]]\nname = \"main\"\n\n[server]\ndefault_provider = \"main\"\n",
        default,
    );
    check(
        "server.bind = \"127.0.0.1:1\"\nupstreams = [{ name = \"anthropic\" }]\n",
        rename("anthropic", "main"),
        "server.bind = \"127.0.0.1:1\"\nserver.default_provider = \"main\"\nupstreams = [{ name = \"main\" }]\n",
        default,
    );
    check(
        "[server]\ndefault_provider = \"anthropic\"\n\n[[upstreams]]\nname = \"anthropic\"\n",
        rename("anthropic", "main"),
        "[server]\ndefault_provider = \"main\"\n\n[[upstreams]]\nname = \"main\"\n",
        default,
    );
    check(
        "[server]\ndefault_provider = \"a\"\n\n[server.codex_endpoint]\n\n[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"codex\"\nauth = \"chatgpt_oauth\"\n",
        rename("codex", "pool"),
        "[server]\ndefault_provider = \"a\"\n\n[server.codex_endpoint]\nprovider = \"pool\"\n\n[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"pool\"\nauth = \"chatgpt_oauth\"\n",
        Cascade {
            codex_endpoint: 1,
            ..Cascade::default()
        },
    );
    check(
        "[server]\ndefault_provider = \"a\"\n\n[[server.codex_endpoint.routes]]\nmodel = \"g\"\nprovider = \"codex\"\n\n[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"codex\"\n",
        rename("codex", "pool"),
        "[server]\ndefault_provider = \"a\"\n\n[[server.codex_endpoint.routes]]\nmodel = \"g\"\nprovider = \"pool\"\n\n[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"pool\"\n\n[server.codex_endpoint]\nprovider = \"pool\"\n",
        Cascade {
            codex_endpoint: 1,
            codex_routes: 1,
            ..Cascade::default()
        },
    );
    // a rename onto the name an implicit default holds would hand it that
    // default
    assert_eq!(
        refused("[[upstreams]]\nname = \"x\"\n", rename("x", "anthropic")),
        "cannot rename upstream \"x\" to \"anthropic\": server.default_provider already names \"anthropic\"; change that reference first"
    );
    assert_eq!(
        refused(
            "[server]\ndefault_provider = \"x\"\n\n[server.codex_endpoint]\n\n[[upstreams]]\nname = \"x\"\n",
            rename("x", "codex")
        ),
        "cannot rename upstream \"x\" to \"codex\": [server.codex_endpoint] provider already names \"codex\"; change that reference first"
    );
}

// ── round 4 (review r3; the owner's and the lead's calls, 2026-10-07) ───────

fn lost(op: &str) -> String {
    format!(
        "{op}: clauth built an edit that loses or changes a comment; nothing was written; report this as a clauth bug"
    )
}

/// The text of each range `edit`'s change exempts from the comment guard.
fn owned_text(text: &str, edit: &Edit) -> Vec<String> {
    let ctx = Ctx::new(text, edit.op()).expect("parses");
    let change = ctx
        .change(edit)
        .unwrap_or_else(|e| panic!("the edit refused: {e:?}"));
    change
        .owned
        .iter()
        .map(|r| text[r.clone()].to_string())
        .collect()
}

/// Each removal kind exempts the range it owns, never its cut (which also
/// takes a separator, a blank line or a newline), and the guard refuses a
/// cut that swallows a neighbour's comment.
#[test]
fn every_removal_kind_exempts_its_owned_range_and_never_its_cut() {
    let _home = HomeSandbox::new();
    // an inline element on a line of two
    let inline = "upstreams = [\n  { name = \"a\" }, { name = \"b\" },\n\n  # c: backup\n  { name = \"c\" },\n]\n";
    let remove_b = Edit::RemoveUpstream { name: "b".into() };
    assert_eq!(owned_text(inline, &remove_b), ["{ name = \"b\" }"]);
    assert_eq!(
        bug_over(inline, remove_b, |_| {
            "upstreams = [\n  { name = \"a\" }, { name = \"c\" },\n]\n".to_string()
        }),
        lost("cannot remove upstream \"b\"")
    );
    // an [[x]] entry
    let aot = "[[upstreams]]\nname = \"a\"\n\n# b own\n[[upstreams]]\nname = \"b\" # b\n\n# c own\n[[upstreams]]\nname = \"c\"\n";
    let remove_b = Edit::RemoveUpstream { name: "b".into() };
    assert_eq!(
        owned_text(aot, &remove_b),
        ["# b own\n[[upstreams]]\nname = \"b\" # b"]
    );
    assert_eq!(
        bug_over(aot, remove_b, |_| {
            "[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"c\"\n".to_string()
        }),
        lost("cannot remove upstream \"b\"")
    );
    // the auth table
    let auth = "[[upstreams]]\nname = \"a\" # a\n\n# auth below\n[upstreams.auth]\nmode = \"api_key\" # k\n";
    let unset_mode = set_upstream("a", UpstreamField::AuthMode, None);
    assert_eq!(
        owned_text(auth, &unset_mode),
        ["# auth below\n[upstreams.auth]\nmode = \"api_key\" # k"]
    );
    assert_eq!(
        bug_over(auth, unset_mode, |_| "[[upstreams]]\nname = \"a\"\n"
            .to_string()),
        lost("cannot unset auth.mode of upstream \"a\"")
    );
    // the upstream_model table
    let slugs =
        "[[models]]\nid = \"m\" # m\n\n# slugs\n[models.upstream_model]\nclaude = \"x\" # x\n";
    let unset_slug = set_model("m", ModelField::UpstreamModel("claude".into()), None);
    assert_eq!(
        owned_text(slugs, &unset_slug),
        ["# slugs\n[models.upstream_model]\nclaude = \"x\" # x"]
    );
    assert_eq!(
        bug_over(slugs, unset_slug, |_| "[[models]]\nid = \"m\"\n"
            .to_string()),
        lost("cannot unset upstream_model \"claude\" of model \"m\"")
    );
    // each step of an accounts edit, in the original's bytes
    let shared = pool_p("  \"a\", \"b\",\n\n  # c own\n  \"c\",\n");
    assert_eq!(owned_text(&shared, &accounts("p", &["a", "c"])), ["\"b\""]);
    assert_eq!(
        bug_over(&shared, accounts("p", &["a", "c"]), |_| pool_p(
            "  \"a\", \"c\",\n"
        )),
        lost("cannot set the accounts of upstream \"p\"")
    );
    let two = pool_p("  \"a\", # main\n  \"b\", # spare\n\n  # c own\n  \"c\",\n");
    assert_eq!(
        owned_text(&two, &accounts("p", &["a"])),
        ["  # c own\n  \"c\",", "\"b\", # spare"]
    );
    assert_eq!(
        bug_over(&two, accounts("p", &["a"]), |_| pool_p("  \"a\",\n")),
        lost("cannot set the accounts of upstream \"p\"")
    );
    // a single-key unset
    let key = "[[upstreams]]\nname = \"a\"\neffort = \"high\" # e\n# keep\n";
    let unset_effort = set_upstream("a", UpstreamField::Effort, None);
    assert_eq!(owned_text(key, &unset_effort), ["effort = \"high\" # e"]);
    assert_eq!(
        bug_over(key, unset_effort, |_| "[[upstreams]]\nname = \"a\"\n"
            .to_string()),
        lost("cannot unset effort of upstream \"a\"")
    );
}

/// The review's M-b layout: a reorder moving an element that owns a block
/// off a line it shares.
#[test]
fn a_reorder_moving_a_block_owner_off_a_shared_line_refuses() {
    let _home = HomeSandbox::new();
    assert_eq!(
        refused(
            &pool_p("\n  # a own\n  \"a\", \"b\",\n  \"c\",\n"),
            accounts("p", &["c", "b", "a"])
        ),
        POOL_SHAPE
    );
}

/// The guard compares each surviving comment's owner: a comment that stays
/// but now sits on another entry, or on none, is clauth's bug.
#[test]
fn the_guard_refuses_a_comment_that_changes_owner() {
    let _home = HomeSandbox::new();
    // the review's M-b output, handed to the guard: `# a own` now labels `c`
    let edit = accounts("p", &["c", "b", "a"]);
    let expected: toml::Table =
        toml::from_str(&pool_p("  \"c\", \"b\",\n  \"a\",\n")).expect("toml");
    let relabelled = Change {
        candidate: pool_p("\n  # a own\n  \"c\", \"b\",\n  \"a\",\n"),
        owned: Vec::new(),
        cascade: None,
    };
    assert_eq!(
        verify(
            &edit,
            &pool_p("\n  # a own\n  \"a\", \"b\",\n  \"c\",\n"),
            &relabelled,
            &expected
        )
        .expect_err("refused")
        .to_string(),
        lost("cannot set the accounts of upstream \"p\"")
    );
    // a moved block left glued under the previous account
    assert_eq!(
        bug_over(
            &pool_p("  \"a\",\n\n  # b own\n  \"b\",\n  \"c\",\n"),
            accounts("p", &["a", "c", "b"]),
            |_| pool_p("  \"a\",\n\n  \"c\",\n  # b own\n  \"b\",\n")
        ),
        lost("cannot set the accounts of upstream \"p\"")
    );
    // a banner that lay in no gap, glued onto the next entry
    assert_eq!(
        bug_over(
            "[server]\nbind = \"x\"\n\n# upstreams\n\n[[upstreams]]\nname = \"a\"\n[[upstreams]]\nname = \"b\"\n",
            Edit::RemoveUpstream { name: "a".into() },
            |_| "[server]\nbind = \"x\"\n\n# upstreams\n[[upstreams]]\nname = \"b\"\n".to_string()
        ),
        lost("cannot remove upstream \"a\"")
    );
    // a comment under `a`, glued onto `b` as well
    assert_eq!(
        bug_over(
            "[[upstreams]]\nname = \"a\"\n# effort = \"high\"\n\n[[upstreams]]\nname = \"b\"\n\n[[upstreams]]\nname = \"c\"\n",
            Edit::RemoveUpstream { name: "c".into() },
            |_| "[[upstreams]]\nname = \"a\"\n# effort = \"high\"\n[[upstreams]]\nname = \"b\"\n"
                .to_string()
        ),
        lost("cannot remove upstream \"c\"")
    );
}

/// A reordered element carries the blank line that sets its owned block
/// off, unless its new slot is set off already; a block whose blank line
/// stays with a banner gains a blank line of its own in a slot not set off.
#[test]
fn a_reorder_carries_the_blank_line_that_sets_a_moved_block_off() {
    let _home = HomeSandbox::new();
    // the review's Q5 inputs; the second's `# a own`, set off by the `[`
    // alone, gains a blank line in its new slot
    assert_eq!(
        planned(
            &pool_p("  \"a\",\n\n  # b own\n  \"b\",\n  \"c\",\n"),
            accounts("p", &["a", "c", "b"])
        ),
        pool_p("  \"a\",\n  \"c\",\n\n  # b own\n  \"b\",\n")
    );
    assert_eq!(
        planned(
            &pool_p("  # a own\n  \"a\",\n\n  # b own\n  \"b\",\n  \"c\", # c\n"),
            accounts("p", &["c", "b", "a"])
        ),
        pool_p("  \"c\", # c\n\n  # b own\n  \"b\",\n\n  # a own\n  \"a\",\n")
    );
    // a slot set off by a blank line of its own keeps it
    assert_eq!(
        planned(
            &pool_p("  \"a\",\n\n  \"b\",\n\n  # c own\n  \"c\",\n"),
            accounts("p", &["a", "c", "b"])
        ),
        pool_p("  \"a\",\n\n  # c own\n  \"c\",\n\n  \"b\",\n")
    );
    // a blank line that also sets a banner off stays with the banner
    let banner = pool_p("  # pool\n\n  # a own\n  \"a\",\n  \"b\",\n\n  # c own\n  \"c\",\n");
    assert_eq!(
        planned(&banner, accounts("p", &["b", "a", "c"])),
        pool_p("  # pool\n\n  \"b\",\n\n  # a own\n  \"a\",\n\n  # c own\n  \"c\",\n")
    );
    // the review's Q4f, and CRLF
    let q4f = pool_p("  # banner\n\n  # a own\n  \"a\",\n  \"b\",\n");
    assert_eq!(
        planned(&q4f, accounts("p", &["b", "a"])),
        pool_p("  # banner\n\n  \"b\",\n\n  # a own\n  \"a\",\n")
    );
    let crlf = |text: String| text.replace('\n', "\r\n");
    assert_eq!(
        planned(&crlf(q4f), accounts("p", &["b", "a"])),
        crlf(pool_p("  # banner\n\n  \"b\",\n\n  # a own\n  \"a\",\n"))
    );
    assert_eq!(
        planned(&banner, accounts("p", &["a", "c", "b"])),
        pool_p("  # pool\n\n  # a own\n  \"a\",\n\n  # c own\n  \"c\",\n  \"b\",\n")
    );
}

/// An [[x]] entry owns the comments glued under its last line up to a blank
/// line, the file's end or another table's header; glued to its list's next
/// entry's header as well, they belong to no single entry. An inline-list
/// element owns none below it.
#[test]
fn an_entry_owns_the_comments_glued_under_its_last_line() {
    let _home = HomeSandbox::new();
    let remove = |name: &str| Edit::RemoveUpstream { name: name.into() };
    let message = |name: &str| {
        format!(
            "cannot remove upstream {name:?}: a comment next to it belongs to no single entry; edit the config by hand"
        )
    };
    // the owner's input: a commented-out key under `b`
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"b\"\n# effort = \"high\"\n\n[[models]]\nid = \"m\"\n",
            remove("b")
        ),
        "[[upstreams]]\nname = \"a\"\n\n[[models]]\nid = \"m\"\n"
    );
    // under `b`'s own sub-table
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"b\"\n[upstreams.auth]\nmode = \"api_key\"\n# env = \"OLD\"\n\n[[models]]\nid = \"m\"\n",
            remove("b")
        ),
        "[[upstreams]]\nname = \"a\"\n\n[[models]]\nid = \"m\"\n"
    );
    // at the file's end, with and without a final newline
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"b\"\n# effort = \"high\"\n",
            remove("b")
        ),
        "[[upstreams]]\nname = \"a\"\n"
    );
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"b\"\n# effort = \"high\"",
            remove("b")
        ),
        "[[upstreams]]\nname = \"a\""
    );
    // r2 N1 (S2): the comment under `a` stays with `a`, or goes with it
    let under_a = "[[upstreams]]\nname = \"a\"\nauth = \"passthrough\"\n# effort = \"high\"\n\n[[upstreams]]\nname = \"b\"\n";
    assert_eq!(
        planned(under_a, remove("b")),
        "[[upstreams]]\nname = \"a\"\nauth = \"passthrough\"\n# effort = \"high\"\n"
    );
    assert_eq!(
        planned(under_a, remove("a")),
        "\n[[upstreams]]\nname = \"b\"\n"
    );
    // glued under one entry and above the next one's header
    let glued = "[[upstreams]]\nname = \"a\"\n# x\n[[upstreams]]\nname = \"b\"\n";
    for name in ["a", "b"] {
        assert_eq!(refused(glued, remove(name)), message(name));
    }
    // the review's P2b: glued to another list's header, so `b`'s
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"b\"\n# x\n[[models]]\nid = \"m\"\n",
            remove("b")
        ),
        "[[upstreams]]\nname = \"a\"\n[[models]]\nid = \"m\"\n"
    );
    // an inline-list element owns no block glued under it
    assert_eq!(
        refused(
            "upstreams = [\n  { name = \"a\" },\n  # under a\n\n  { name = \"b\" },\n]\n",
            remove("a")
        ),
        message("a")
    );
}

/// An add leaves every comment under the last entry with its owner: a block
/// it owns, glued to the next table's header or not, stays set off from the
/// new entry.
#[test]
fn an_array_of_tables_add_keeps_the_owner_of_a_comment_under_the_last_entry() {
    let _home = HomeSandbox::new();
    let add = || {
        Edit::AddUpstream(NewUpstream {
            name: "c".into(),
            ..NewUpstream::default()
        })
    };
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"b\"\n# x\n[[models]]\nid = \"m\"\n",
            add()
        ),
        "[[upstreams]]\nname = \"b\"\n# x\n\n[[upstreams]]\nname = \"c\"\n[[models]]\nid = \"m\"\n"
    );
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\n[[upstreams]]\nname = \"b\"\n# effort = \"high\"\n",
            add()
        ),
        "[[upstreams]]\nname = \"a\"\n[[upstreams]]\nname = \"b\"\n# effort = \"high\"\n\n[[upstreams]]\nname = \"c\"\n"
    );
}

/// A comment after a list's `]` is its line's, not the list's: an edit
/// that writes or drops the list keeps it where it was.
#[test]
fn a_comment_after_a_lists_closing_bracket_stays_with_its_line() {
    let _home = HomeSandbox::new();
    let auth = |value: &str| format!("[[upstreams]]\nname = \"u\"\nauth = {value} # pool\n");
    assert_eq!(
        planned(
            &auth("{ mode = \"claude_oauth\", accounts = [\"a\"] }"),
            accounts("u", &[])
        ),
        auth("{ mode = \"claude_oauth\" }")
    );
    assert_eq!(
        planned(&auth("\"claude_oauth\""), accounts("u", &["a"])),
        auth("{ mode = \"claude_oauth\", accounts = [\"a\"] }")
    );
}

// ── round 5 (review r4; the lead's calls, 2026-10-07) ───────────────────────

/// A block glued under an [[x]] entry's last line and above any header but
/// its own list's next entry's is that entry's; one glued under another
/// table's key and above a list's first entry lies in no gap and stays.
#[test]
fn a_block_glued_above_another_header_belongs_to_the_entry_above_it() {
    let _home = HomeSandbox::new();
    let remove = |name: &str| Edit::RemoveUpstream { name: name.into() };
    // the review's P2a: above `[server]`
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"b\"\n# effort = \"high\"\n[server]\nbind = \"x\"\n",
            remove("b")
        ),
        "[[upstreams]]\nname = \"a\"\n[server]\nbind = \"x\"\n"
    );
    // under another list's last entry: that entry's, so it stays or goes
    // with it
    let models = "[[models]]\nid = \"m\"\n# note\n[[upstreams]]\nname = \"a\"\n";
    assert_eq!(
        planned(models, remove("a")),
        "[[models]]\nid = \"m\"\n# note\n"
    );
    assert_eq!(
        planned(models, Edit::RemoveModel { id: "m".into() }),
        "[[upstreams]]\nname = \"a\"\n"
    );
    // under another table's key and above the first entry: no entry's
    assert_eq!(
        planned(
            "[server]\nbind = \"x\"\n# note\n[[upstreams]]\nname = \"a\"\n",
            remove("a")
        ),
        "[server]\nbind = \"x\"\n# note\n"
    );
    assert_eq!(
        planned(
            "[server]\nbind = \"x\"\n# note\n[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"b\"\n",
            remove("a")
        ),
        "[server]\nbind = \"x\"\n# note\n\n[[upstreams]]\nname = \"b\"\n"
    );
    // the guard reads the block as the entry's: an add gluing it onto the
    // new entry's header as well is clauth's bug
    assert_eq!(
        bug_over(
            "[[upstreams]]\nname = \"b\"\n# x\n[[models]]\nid = \"m\"\n",
            Edit::AddUpstream(NewUpstream {
                name: "c".into(),
                ..NewUpstream::default()
            }),
            |_| {
                "[[upstreams]]\nname = \"b\"\n# x\n[[upstreams]]\nname = \"c\"\n[[models]]\nid = \"m\"\n"
                .to_string()
            }
        ),
        lost("cannot add upstream \"c\"")
    );
}

/// A comment inside an entry, glued under a key above its sub-table's
/// header or under the sub-table's last line, is the entry's: removing the
/// sub-table leaves it in the entry. One glued to the next entry's header
/// as well lies between two entries and refuses.
#[test]
fn a_comment_inside_an_entry_stays_when_its_sub_table_goes() {
    let _home = HomeSandbox::new();
    let unset_mode = |name: &str| set_upstream(name, UpstreamField::AuthMode, None);
    let unset_slug = || set_model("m", ModelField::UpstreamModel("claude".into()), None);
    // the review's P1a (r4 X1 I26): the file's end closes it
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\n\n[upstreams.auth]\nmode = \"api_key\"\n# env = \"OLD_KEY\"\n",
            unset_mode("a")
        ),
        "[[upstreams]]\nname = \"a\"\n# env = \"OLD_KEY\"\n"
    );
    // P1b: a blank line closes it
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\n\n[upstreams.auth]\nmode = \"api_key\"\n# env = \"OLD_KEY\"\n\n[[upstreams]]\nname = \"b\"\n",
            unset_mode("a")
        ),
        "[[upstreams]]\nname = \"a\"\n# env = \"OLD_KEY\"\n\n[[upstreams]]\nname = \"b\"\n"
    );
    // P13: the owner's shape, under `b`
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"b\"\n[upstreams.auth]\nmode = \"api_key\"\n# env = \"OLD\"\n",
            unset_mode("b")
        ),
        "[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"b\"\n# env = \"OLD\"\n"
    );
    // P9b: the upstream_model twin
    assert_eq!(
        planned(
            "[[models]]\nid = \"m\"\n\n[models.upstream_model]\nclaude = \"x\"\n# codex = \"old\"\n",
            unset_slug()
        ),
        "[[models]]\nid = \"m\"\n# codex = \"old\"\n"
    );
    // P1c (r4 X1 I25): glued under a key and above the sub-table's header
    assert_eq!(
        planned(
            "[[models]]\nid = \"m\"\n# codex = \"old\"\n[models.upstream_model]\nclaude = \"x\"\n",
            unset_slug()
        ),
        "[[models]]\nid = \"m\"\n# codex = \"old\"\n"
    );
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\n# auth below\n[upstreams.auth]\nmode = \"api_key\"\n",
            unset_mode("a")
        ),
        "[[upstreams]]\nname = \"a\"\n# auth below\n"
    );
    // glued above another table of the same entry
    assert_eq!(
        planned(
            "[[models]]\nid = \"m\"\n[models.upstream_model]\nclaude = \"x\"\n# old\n[models.router]\ntype = \"random\"\ntargets = [\"n\"]\n",
            unset_slug()
        ),
        "[[models]]\nid = \"m\"\n# old\n[models.router]\ntype = \"random\"\ntargets = [\"n\"]\n"
    );
    // glued to the next entry's header as well
    assert_eq!(
        refused(
            "[[upstreams]]\nname = \"a\"\n[upstreams.auth]\nmode = \"api_key\"\n# env = \"OLD\"\n[[upstreams]]\nname = \"b\"\n",
            unset_mode("a")
        ),
        "cannot remove the auth table of upstream \"a\": a comment next to it belongs to no single entry; edit the config by hand"
    );
}

/// A moved element whose block only the list's `[` sets off gains one
/// blank line above the block in its new slot, so the block stays its own.
#[test]
fn a_block_set_off_by_the_open_bracket_moves_with_a_blank_line_above_it() {
    let _home = HomeSandbox::new();
    // the review's P3a
    assert_eq!(
        planned(
            &pool_p("  # primary\n  \"a\",\n  \"b\",\n"),
            accounts("p", &["b", "a"])
        ),
        pool_p("  \"b\",\n\n  # primary\n  \"a\",\n")
    );
    let crlf = |text: String| text.replace('\n', "\r\n");
    assert_eq!(
        planned(
            &crlf(pool_p("  # primary\n  \"a\",\n  \"b\",\n  \"c\",\n")),
            accounts("p", &["b", "c", "a"])
        ),
        crlf(pool_p("  \"b\",\n  \"c\",\n\n  # primary\n  \"a\",\n"))
    );
}

/// An element added to a list holding comments and no element goes on its
/// own line directly under the `[` line, every comment below it.
#[test]
fn an_element_added_to_a_list_of_only_comments_goes_under_the_open_bracket() {
    let _home = HomeSandbox::new();
    // the review's P4a, P4b, P4e
    assert_eq!(
        planned(&pool_p("  # pool\n"), accounts("p", &["x"])),
        pool_p("  \"x\",\n  # pool\n")
    );
    assert_eq!(
        planned(&pool_p("  # pool\n"), accounts("p", &["x", "y"])),
        pool_p("  \"x\",\n  \"y\",\n  # pool\n")
    );
    assert_eq!(
        planned(&pool_p("  # banner\n\n  # tail\n"), accounts("p", &["x"])),
        pool_p("  \"x\",\n  # banner\n\n  # tail\n")
    );
    assert_eq!(
        planned(&pool_p("  \"a\",\n  # tail\n"), accounts("p", &["x"])),
        pool_p("  \"x\",\n  # tail\n")
    );
    // P4d: the `[` line's comment stays the list's
    let open = |list: &str| {
        format!(
            "[[upstreams]]\nname = \"p\"\n[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = [ # pool\n{list}]\n"
        )
    };
    assert_eq!(
        planned(&open(""), accounts("p", &["x"])),
        open("  \"x\",\n")
    );
    // P4g: an inline upstreams list
    assert_eq!(
        planned(
            "upstreams = [\n  # none yet\n]\n",
            Edit::AddUpstream(NewUpstream {
                name: "x".into(),
                ..NewUpstream::default()
            })
        ),
        "upstreams = [\n  { name = \"x\" },\n  # none yet\n]\n"
    );
}

/// A comment on the `[` line beside an element is that element's, so the
/// guard refuses a candidate handing it to another element, or writing an
/// element beside the list's own `[`-line comment.
#[test]
fn a_comment_on_the_open_bracket_line_belongs_to_the_element_beside_it() {
    let _home = HomeSandbox::new();
    let auth = |list: &str| {
        format!(
            "[[upstreams]]\nname = \"p\"\n[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = {list}]\n"
        )
    };
    assert_eq!(
        bug_with(
            &auth("[\"a\", # main\n  \"b\",\n"),
            accounts("p", &["b", "a"]),
            &[],
            &auth("[\"b\", # main\n  \"a\",\n")
        ),
        lost("cannot set the accounts of upstream \"p\"")
    );
    // the review's P11
    assert_eq!(
        bug_over(&auth("[ # pool\n"), accounts("p", &["x"]), |_| auth(
            "[\"x\", # pool\n"
        )),
        lost("cannot set the accounts of upstream \"p\"")
    );
}

/// The guard reads a list's own block, before its `]` or set off above its
/// first element, as the list's: a candidate moving it into a gap between
/// elements is clauth's bug (the review's P10, P10b).
#[test]
fn the_guard_refuses_a_list_block_moved_into_a_gap() {
    let _home = HomeSandbox::new();
    assert_eq!(
        bug_over(
            &pool_p("  \"a\",\n  \"b\",\n  # tail\n"),
            accounts("p", &["a", "b", "c"]),
            |_| pool_p("  \"a\",\n  \"b\",\n  # tail\n  \"c\",\n")
        ),
        lost("cannot set the accounts of upstream \"p\"")
    );
    assert_eq!(
        bug_over(
            &pool_p("  # pool\n\n  \"a\",\n  \"b\",\n"),
            accounts("p", &["b", "a"]),
            |_| pool_p("  \"b\",\n  # pool\n\n  \"a\",\n")
        ),
        lost("cannot set the accounts of upstream \"p\"")
    );
}

/// A removal whose exempted range misses the removed entry's own block
/// leaves that block on the entry that moves up into its place: the guard
/// refuses it (the review's P8).
#[test]
fn the_guard_refuses_a_removed_entrys_block_left_on_the_next_entry() {
    let _home = HomeSandbox::new();
    assert_eq!(
        bug_with(
            "[[upstreams]]\nname = \"a\"\n\n# b own\n[[upstreams]]\nname = \"b\"\n\n[[upstreams]]\nname = \"c\"\n",
            Edit::RemoveUpstream { name: "b".into() },
            &["[[upstreams]]\nname = \"b\"\n"],
            "[[upstreams]]\nname = \"a\"\n\n# b own\n[[upstreams]]\nname = \"c\"\n"
        ),
        lost("cannot remove upstream \"b\"")
    );
}

/// Removing an upstream moves the owners of a later upstream's accounts
/// comments, the accounts' own and the list's, up with it, so the removal
/// lands.
#[test]
fn removing_an_upstream_keeps_the_owners_of_a_later_upstreams_accounts_comments() {
    let _home = HomeSandbox::new();
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"b\"\n[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = [\n  # main\n  \"x\",\n  # tail\n]\n",
            Edit::RemoveUpstream { name: "a".into() }
        ),
        "\n[[upstreams]]\nname = \"b\"\n[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = [\n  # main\n  \"x\",\n  # tail\n]\n"
    );
}

// ── round 6 (review r5; the head rule, 2026-10-07) ──────────────────────────

/// An upstream `p` whose `[upstreams.auth]` lists `accounts = [{list}]`, the
/// list's first line being the `[` line.
fn bracket_p(list: &str) -> String {
    format!(
        "[[upstreams]]\nname = \"p\"\n[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = [{list}]\n"
    )
}

/// An element added to a list whose `[` line holds an element and a comment
/// goes on its own line after the last element, so that comment keeps its
/// owner; a list without a trailing comma gains none.
#[test]
fn an_add_beside_an_open_bracket_comment_goes_on_its_own_line() {
    let _home = HomeSandbox::new();
    let add_b = || {
        Edit::AddUpstream(NewUpstream {
            name: "b".into(),
            ..NewUpstream::default()
        })
    };
    // the review's B-up-add and its twin without a trailing comma
    assert_eq!(
        planned("upstreams = [{ name = \"a\" }, # main\n]\n", add_b()),
        "upstreams = [{ name = \"a\" }, # main\n  { name = \"b\" },\n]\n"
    );
    assert_eq!(
        planned("upstreams = [{ name = \"a\" } # main\n]\n", add_b()),
        "upstreams = [{ name = \"a\" }, # main\n  { name = \"b\" }\n]\n"
    );
    // Q1-add-b and B-acc-nocomma
    assert_eq!(
        planned(&bracket_p("\"a\", # main\n"), accounts("p", &["a", "b"])),
        bracket_p("\"a\", # main\n  \"b\",\n")
    );
    assert_eq!(
        planned(&bracket_p("\"a\" # main\n"), accounts("p", &["a", "b"])),
        bracket_p("\"a\", # main\n  \"b\"\n")
    );
    // the add-plus-reorder: the comment stays `a`'s
    assert_eq!(
        planned(&bracket_p("\"a\", # main\n"), accounts("p", &["b", "a"])),
        bracket_p("\"b\",\n  \"a\", # main\n")
    );
    // two elements share the `[` line's comment; a comment line below the
    // last element gives its indent and stays below
    assert_eq!(
        planned(
            &bracket_p("\"a\", \"b\", # ab\n"),
            accounts("p", &["a", "b", "c"])
        ),
        bracket_p("\"a\", \"b\", # ab\n  \"c\",\n")
    );
    assert_eq!(
        planned(
            &bracket_p("\"a\", # main\n    # tail\n"),
            accounts("p", &["a", "b"])
        ),
        bracket_p("\"a\", # main\n    \"b\",\n    # tail\n")
    );
    // inline routes, and CRLF
    assert_eq!(
        planned(
            "routes = [{ model = \"m\", provider = \"x\" }, # main\n]\n",
            Edit::AddRoute(NewRoute {
                model: "c".into(),
                provider: "z".into(),
                ..NewRoute::default()
            })
        ),
        "routes = [{ model = \"m\", provider = \"x\" }, # main\n  { model = \"c\", provider = \"z\" },\n]\n"
    );
    let crlf = |text: String| text.replace('\n', "\r\n");
    assert_eq!(
        planned(
            &crlf(bracket_p("\"a\", # main\n")),
            accounts("p", &["a", "b"])
        ),
        crlf(bracket_p("\"a\", # main\n  \"b\",\n"))
    );
}

/// The element alone on the `[` line owns that line's comment: removing it
/// takes the comment, and a reorder moves the comment with it, touching only
/// the moved elements' bytes. Only a comment between two entries refuses.
#[test]
fn the_open_bracket_element_goes_and_moves_with_its_comment() {
    let _home = HomeSandbox::new();
    // the review's Q3-drop-a (P12-drop-a) and Q3-swap
    let held = bracket_p("\"a\", # main\n  \"b\",\n");
    assert_eq!(
        planned(&held, accounts("p", &["b"])),
        bracket_p("\n  \"b\",\n")
    );
    assert_eq!(
        planned(&held, accounts("p", &["b", "a"])),
        bracket_p("\"b\",\n  \"a\", # main\n")
    );
    // without a trailing comma
    let bare = bracket_p("\"a\", # main\n  \"b\"\n");
    assert_eq!(
        planned(&bare, accounts("p", &["b"])),
        bracket_p("\n  \"b\"\n")
    );
    assert_eq!(
        planned(&bare, accounts("p", &["b", "a"])),
        bracket_p("\"b\",\n  \"a\" # main\n")
    );
    // the review's B-up-rm: inline upstreams
    let upstreams = "upstreams = [{ name = \"a\" }, # main\n  { name = \"b\" },\n]\n";
    assert_eq!(
        planned(upstreams, Edit::RemoveUpstream { name: "a".into() }),
        "upstreams = [\n  { name = \"b\" },\n]\n"
    );
    assert_eq!(
        planned(upstreams, Edit::RemoveUpstream { name: "b".into() }),
        "upstreams = [{ name = \"a\" }, # main\n]\n"
    );
    // an element with its own comment moves onto the `[` line; one owning a
    // block moves under it, the `[` setting the block off
    assert_eq!(
        planned(
            &bracket_p("\"a\", # main\n  \"b\", # spare\n"),
            accounts("p", &["b", "a"])
        ),
        bracket_p("\"b\", # spare\n  \"a\", # main\n")
    );
    assert_eq!(
        planned(
            &bracket_p("\"a\", # main\n\n  # b own\n  \"b\",\n"),
            accounts("p", &["b", "a"])
        ),
        bracket_p("\n  # b own\n  \"b\",\n\n  \"a\", # main\n")
    );
    // a rotation, and CRLF
    assert_eq!(
        planned(
            &bracket_p("\"a\", # main\n  \"b\",\n  \"c\",\n"),
            accounts("p", &["b", "c", "a"])
        ),
        bracket_p("\"b\",\n  \"c\",\n  \"a\", # main\n")
    );
    let crlf = |text: String| text.replace('\n', "\r\n");
    assert_eq!(
        planned(&crlf(held.clone()), accounts("p", &["b", "a"])),
        crlf(bracket_p("\"b\",\n  \"a\", # main\n"))
    );
    // a comment between two entries refuses, and so does a `[` line whose
    // comment two elements share
    let unowned = |name: &str| {
        format!(
            "cannot remove account {name:?} of upstream \"p\": a comment next to it belongs to no single entry; edit the config by hand"
        )
    };
    assert_eq!(
        refused(
            &bracket_p("\"a\", # main\n  # between\n  \"b\",\n"),
            accounts("p", &["b"])
        ),
        unowned("a")
    );
    assert_eq!(
        refused(
            &bracket_p("\"a\", \"b\", # ab\n  \"c\",\n"),
            accounts("p", &["b", "c"])
        ),
        unowned("a")
    );
    // `]` on the last element's line: the comment would have to follow `]`
    assert_eq!(
        refused(
            &bracket_p("\"a\", # main\n  \"b\""),
            accounts("p", &["b", "a"])
        ),
        POOL_SHAPE
    );
}

/// A comment on a `[` line holding two elements belongs to neither, so a
/// candidate leaving it on the `[` line as the list's is clauth's bug.
#[test]
fn the_guard_refuses_a_shared_open_bracket_comment_handed_to_the_list() {
    let _home = HomeSandbox::new();
    assert_eq!(
        bug_with(
            &bracket_p("\"a\", \"b\", # ab\n  \"c\",\n"),
            accounts("p", &["c"]),
            &["\"a\", \"b\", "],
            &bracket_p(" # ab\n  \"c\",\n")
        ),
        lost("cannot set the accounts of upstream \"p\"")
    );
}

// ── round 7 (reviews r6: the BOM, comma-first lists, a multi-line `[` element)

fn bom(text: &str) -> String {
    format!("\u{feff}{text}")
}

/// A config starting with a byte-order mark edits exactly as the same file
/// without it, and keeps the mark: one twin per edit kind.
#[test]
fn a_byte_order_mark_leaves_every_edit_as_it_is_without_the_mark() {
    let _home = HomeSandbox::new();
    let remove_a = || Edit::RemoveUpstream { name: "a".into() };
    // the review's BOM-rm-a: the block set off by the file's start is `a`'s
    assert_eq!(
        planned(
            &bom("# a own\n[[upstreams]]\nname = \"a\"\n[[upstreams]]\nname = \"b\"\n"),
            remove_a()
        ),
        bom("[[upstreams]]\nname = \"b\"\n")
    );
    // a blank first line sets the block off; CRLF; a header on the first line
    assert_eq!(
        planned(
            &bom("\n# a own\n[[upstreams]]\nname = \"a\"\n[[upstreams]]\nname = \"b\"\n"),
            remove_a()
        ),
        bom("[[upstreams]]\nname = \"b\"\n")
    );
    assert_eq!(
        planned(
            &bom("# a own\r\n[[upstreams]]\r\nname = \"a\"\r\n[[upstreams]]\r\nname = \"b\"\r\n"),
            remove_a()
        ),
        bom("[[upstreams]]\r\nname = \"b\"\r\n")
    );
    assert_eq!(
        planned(
            &bom("[[upstreams]]\nname = \"a\"\n\n[[upstreams]]\nname = \"b\"\n"),
            remove_a()
        ),
        bom("\n[[upstreams]]\nname = \"b\"\n")
    );
    // r1 F1's twin
    assert_eq!(
        planned(
            &bom(
                "# upstreams section\n[[upstreams]]\nname = \"a\"\n\n# backup provider, keep\n[[upstreams]]\nname = \"b\"\n"
            ),
            remove_a()
        ),
        bom("\n# backup provider, keep\n[[upstreams]]\nname = \"b\"\n")
    );
    // field set
    assert_eq!(
        planned(
            &bom(COMMENTED),
            set_model("opus-x", ModelField::DisplayName, Some("Opus 10"))
        ),
        bom(r#"# models I route
[[models]]
id = "opus-x" # the big one
display_name = "Opus 10"  # shown in the picker
upstream_model = { work = "claude-opus" }

# second
[[models]]
id = "sonnet-x"
"#)
    );
    // rename (r3 R4)
    assert_eq!(
        planned(
            &bom(
                "[[upstreams]]\nname = \"a\"\n\n[[models]]\nid = \"m\"\n[models.upstream_model]\n# the main slug\na = \"x\" # keep\n# the alt slug\nb = \"y\"\n"
            ),
            Edit::RenameUpstream {
                name: "a".into(),
                to: "z".into()
            }
        ),
        bom(
            "[[upstreams]]\nname = \"z\"\n\n[[models]]\nid = \"m\"\n[models.upstream_model]\n# the main slug\nz = \"x\" # keep\n# the alt slug\nb = \"y\"\n"
        )
    );
    // add: after one entry with no final newline, and to a file holding
    // only the mark, as to an empty file
    let add_b = || {
        Edit::AddModel(NewModel {
            id: "b".into(),
            ..NewModel::default()
        })
    };
    assert_eq!(
        planned(&bom("[[models]]\nid = \"a\""), add_b()),
        bom("[[models]]\nid = \"a\"\n\n[[models]]\nid = \"b\"")
    );
    assert_eq!(planned(&bom(""), add_b()), bom("[[models]]\nid = \"b\"\n"));
    // accounts reorder
    assert_eq!(
        planned(&bom(ACCOUNTS), accounts("u", &["b", "a"])),
        bom(
            "[[upstreams]]\nname = \"u\"\nauth = { mode = \"claude_oauth\", accounts = [\n  \"b\", # spare\n  \"a\" # main\n] }\n"
        )
    );
}

/// In a comma-first list (`"a"\n  , "b"`) the comma at the start of an
/// element's line separates it from the element before: a remove takes that
/// line whole, and removing the first element takes the next one's leading
/// comma too, so every remaining pair keeps one comma.
#[test]
fn a_comma_first_list_keeps_one_comma_between_every_two_elements() {
    let _home = HomeSandbox::new();
    // the review's CF1 (the `[`-line element with its comment)
    let cf1 = bracket_p("\"a\" # main\n  , \"b\"\n");
    assert_eq!(
        planned(&cf1, accounts("p", &["b"])),
        bracket_p("\n  \"b\"\n")
    );
    assert_eq!(
        planned(&cf1, accounts("p", &["a"])),
        bracket_p("\"a\" # main\n")
    );
    assert_eq!(
        planned(&cf1, accounts("p", &["b", "a"])),
        bracket_p("\"b\"\n  , \"a\" # main\n")
    );
    assert_eq!(
        planned(&cf1, accounts("p", &["a", "b", "c"])),
        bracket_p("\"a\" # main\n  , \"b\"\n  , \"c\"\n")
    );
    // the review's CF3 (the m1 input: the later element)
    let cf3 = pool_p("  \"a\"\n  , \"b\"\n");
    assert_eq!(planned(&cf3, accounts("p", &["a"])), pool_p("  \"a\"\n"));
    assert_eq!(planned(&cf3, accounts("p", &["b"])), pool_p("  \"b\"\n"));
    // the review's CF4 and CF5 (inline upstreams)
    let cf4 = "upstreams = [{ name = \"a\" } # main\n  , { name = \"b\" }\n]\n";
    assert_eq!(
        planned(cf4, Edit::RemoveUpstream { name: "a".into() }),
        "upstreams = [\n  { name = \"b\" }\n]\n"
    );
    assert_eq!(
        planned(cf4, Edit::RemoveUpstream { name: "b".into() }),
        "upstreams = [{ name = \"a\" } # main\n]\n"
    );
    assert_eq!(
        planned(
            "upstreams = [\n  { name = \"a\" }\n  , { name = \"b\" }\n]\n",
            Edit::RemoveUpstream { name: "a".into() }
        ),
        "upstreams = [\n  { name = \"b\" }\n]\n"
    );
    // a middle element, and one owning a block set off above its line
    assert_eq!(
        planned(
            &pool_p("  \"a\"\n  , \"b\" # spare\n  , \"c\"\n"),
            accounts("p", &["a", "c"])
        ),
        pool_p("  \"a\"\n  , \"c\"\n")
    );
    assert_eq!(
        planned(
            &pool_p("  \"a\"\n\n  # b own\n  , \"b\"\n"),
            accounts("p", &["a"])
        ),
        pool_p("  \"a\"\n")
    );
    // a comment between two entries refuses; a `]` on the last element's
    // line leaves the `[`-line comment no slot (S1's shape)
    assert_eq!(
        refused(
            &pool_p("  \"a\"\n  # between\n  , \"b\"\n"),
            accounts("p", &["b"])
        ),
        "cannot remove account \"a\" of upstream \"p\": a comment next to it belongs to no single entry; edit the config by hand"
    );
    assert_eq!(
        refused(
            &bracket_p("\"a\" # main\n  , \"b\""),
            accounts("p", &["b", "a"])
        ),
        POOL_SHAPE
    );
}

/// A multi-line element opening on the `[` line owns the comment after its
/// end on its own last line: removing it takes that comment.
#[test]
fn removing_a_multi_line_open_bracket_element_takes_its_comment() {
    let _home = HomeSandbox::new();
    // DS1's probe, then with a second element
    assert_eq!(
        planned(
            "upstreams = [{ name = \"a\",\n  kind = \"x\" }, # main\n]\n",
            Edit::RemoveUpstream { name: "a".into() }
        ),
        "upstreams = [\n]\n"
    );
    let two = "upstreams = [{ name = \"a\",\n  kind = \"x\" }, # main\n  { name = \"b\" },\n]\n";
    assert_eq!(
        planned(two, Edit::RemoveUpstream { name: "a".into() }),
        "upstreams = [\n  { name = \"b\" },\n]\n"
    );
    // the TOML 1.0 forms: a nested multi-line array, a multi-line string
    assert_eq!(
        planned(
            "upstreams = [{ name = \"a\", auth = { mode = \"claude_oauth\", accounts = [\n  \"x\",\n] } }, # main\n  { name = \"b\" },\n]\n",
            Edit::RemoveUpstream { name: "a".into() }
        ),
        "upstreams = [\n  { name = \"b\" },\n]\n"
    );
    assert_eq!(
        planned(
            &bracket_p("'''\na''', # main\n  \"b\",\n"),
            accounts("p", &["b"])
        ),
        bracket_p("\n  \"b\",\n")
    );
}

/// DS1's D1b: the multi-line `[`-line element moves with the comment on its
/// last line. Only accounts reorder, so the element is a multi-line string.
#[test]
fn reordering_a_multi_line_open_bracket_element_moves_its_comment() {
    let _home = HomeSandbox::new();
    assert_eq!(
        planned(
            &bracket_p("'''\na''', # main\n  \"b\",\n"),
            accounts("p", &["b", "a"])
        ),
        bracket_p("\"b\",\n  '''\na''', # main\n")
    );
    assert_eq!(
        planned(
            &bracket_p("'''\na''', # main\n  \"b\", # spare\n"),
            accounts("p", &["b", "a"])
        ),
        bracket_p("\"b\", # spare\n  '''\na''', # main\n")
    );
}

/// A comma alone on its line, or a line holding a comma on each side of its
/// element (`, "b",`), still leaves one comma between every two elements.
#[test]
fn a_lone_or_doubled_comma_line_keeps_one_comma_between_every_two_elements() {
    let _home = HomeSandbox::new();
    let lone = pool_p("  \"a\"\n  ,\n  \"b\"\n");
    assert_eq!(planned(&lone, accounts("p", &["b"])), pool_p("  \"b\"\n"));
    assert_eq!(
        planned(&lone, accounts("p", &["a"])),
        pool_p("  \"a\"\n  ,\n")
    );
    // the drop leaves the comma trailing; the add goes below it
    assert_eq!(
        planned(&lone, accounts("p", &["a", "n"])),
        pool_p("  \"a\"\n  ,\n  \"n\",\n")
    );
    let doubled = pool_p("  \"a\"\n  , \"b\", # spare\n  \"c\"\n");
    assert_eq!(
        planned(&doubled, accounts("p", &["a", "c"])),
        pool_p("  \"a\"\n  ,\n  \"c\"\n")
    );
    // a comment beside a lone comma lies between two entries
    assert_eq!(
        refused(
            &pool_p("  \"a\"\n  , # c\n  \"b\"\n"),
            accounts("p", &["b"])
        ),
        "cannot remove account \"a\" of upstream \"p\": a comment next to it belongs to no single entry; edit the config by hand"
    );
}

/// The document's first line is no header: a comment glued under it sets
/// off nothing, so between two elements of a list opening there it belongs
/// to neither.
#[test]
fn a_comment_under_the_files_first_line_is_set_off_by_no_header() {
    let _home = HomeSandbox::new();
    let text = "upstreams = [{ name = \"a\" }, # c\n  # between\n  { name = \"b\" },\n]\n";
    for name in ["a", "b"] {
        assert_eq!(
            refused(text, Edit::RemoveUpstream { name: name.into() }),
            format!(
                "cannot remove upstream {name:?}: a comment next to it belongs to no single entry; edit the config by hand"
            )
        );
    }
    // a field set on a first element spanning lines keeps every comment
    // where it was
    assert_eq!(
        planned(
            "models = [{ id = \"a\", display_name = \"\"\"\nDa\"\"\" }, # c\n  # between\n  { id = \"b\" },\n]\n",
            set_model("a", ModelField::DisplayName, Some("E"))
        ),
        "models = [{ id = \"a\", display_name = \"\"\"\nE\"\"\" }, # c\n  # between\n  { id = \"b\" },\n]\n"
    );
}

// ── round 8 (reviews r7: dotted keys in inline entries, a multi-line string's own comment)

#[test]
fn a_dotted_key_inside_an_inline_entry_is_edited_at_its_full_span() {
    let _home = HomeSandbox::new();
    let up = "upstreams = [{ name = \"a\", auth.mode = \"claude_oauth\" }]\n";
    assert_eq!(
        planned(up, set_upstream("a", UpstreamField::Effort, Some("low"))),
        "upstreams = [{ name = \"a\", auth.mode = \"claude_oauth\", effort = \"low\" }]\n"
    );
    assert_eq!(
        planned(up, set_upstream("a", UpstreamField::AuthAccount, Some("w"))),
        "upstreams = [{ name = \"a\", auth.mode = \"claude_oauth\", auth.account = \"w\" }]\n"
    );
    assert_eq!(
        planned(up, accounts("a", &["x"])),
        "upstreams = [{ name = \"a\", auth.mode = \"claude_oauth\", auth.accounts = [\"x\"] }]\n"
    );
    assert_eq!(
        planned(up, set_upstream("a", UpstreamField::AuthMode, None)),
        "upstreams = [{ name = \"a\" }]\n"
    );
    let mid =
        "upstreams = [{ name = \"a\", auth.mode = \"claude_oauth\", provider = \"anthropic\" }]\n";
    assert_eq!(
        planned(mid, set_upstream("a", UpstreamField::Effort, Some("low"))),
        "upstreams = [{ name = \"a\", auth.mode = \"claude_oauth\", provider = \"anthropic\", effort = \"low\" }]\n"
    );
    assert_eq!(
        planned(mid, set_upstream("a", UpstreamField::Provider, None)),
        "upstreams = [{ name = \"a\", auth.mode = \"claude_oauth\" }]\n"
    );
    assert_eq!(
        planned(
            mid,
            set_upstream("a", UpstreamField::AuthAccount, Some("w"))
        ),
        "upstreams = [{ name = \"a\", auth.mode = \"claude_oauth\", auth.account = \"w\", provider = \"anthropic\" }]\n"
    );
    assert_eq!(
        planned(mid, set_upstream("a", UpstreamField::AuthMode, None)),
        "upstreams = [{ name = \"a\", provider = \"anthropic\" }]\n"
    );
    let models = "models = [{ id = \"m\", upstream_model.a = \"s\" }]\n";
    assert_eq!(
        planned(
            models,
            set_model("m", ModelField::UpstreamModel("a".into()), None)
        ),
        "models = [{ id = \"m\" }]\n"
    );
    assert_eq!(
        planned(
            models,
            set_model("m", ModelField::UpstreamModel("b".into()), Some("t"))
        ),
        "models = [{ id = \"m\", upstream_model.a = \"s\", upstream_model.b = \"t\" }]\n"
    );
    assert_eq!(
        planned(models, set_model("m", ModelField::DisplayName, Some("M"))),
        "models = [{ id = \"m\", upstream_model.a = \"s\", display_name = \"M\" }]\n"
    );
    assert_eq!(
        planned(
            "models = [{ id = \"m\", upstream_model.a = \"s\", display_name = \"D\" }]\n",
            set_model("m", ModelField::DisplayName, None)
        ),
        "models = [{ id = \"m\", upstream_model.a = \"s\" }]\n"
    );
}

#[test]
fn every_key_spelling_takes_its_edit_in_its_own_spelling() {
    let _home = HomeSandbox::new();
    // quoted and spaced around the dot, inline
    let spaced = "upstreams = [{ name = \"a\", \"auth\" . mode = \"claude_oauth\", provider = \"anthropic\" }]\n";
    assert_eq!(
        planned(
            spaced,
            set_upstream("a", UpstreamField::AuthAccount, Some("w"))
        ),
        "upstreams = [{ name = \"a\", \"auth\" . mode = \"claude_oauth\", \"auth\" . account = \"w\", provider = \"anthropic\" }]\n"
    );
    assert_eq!(
        planned(
            spaced,
            set_upstream("a", UpstreamField::AuthMode, Some("passthrough"))
        ),
        "upstreams = [{ name = \"a\", \"auth\" . mode = \"passthrough\", provider = \"anthropic\" }]\n"
    );
    assert_eq!(
        planned(spaced, set_upstream("a", UpstreamField::AuthMode, None)),
        "upstreams = [{ name = \"a\", provider = \"anthropic\" }]\n"
    );
    assert_eq!(
        view_of(spaced, None).unwrap().upstreams[0]
            .auth
            .as_ref()
            .map(|auth| auth.form),
        Some(AuthForm::Dotted)
    );
    // one dotted table split around another key, literal-quoted
    let split =
        "upstreams = [{ auth.mode = \"claude_oauth\", name = \"a\", 'auth'.account = \"w\" }]\n";
    assert_eq!(
        planned(split, set_upstream("a", UpstreamField::AuthEnv, Some("K"))),
        "upstreams = [{ auth.mode = \"claude_oauth\", name = \"a\", 'auth'.account = \"w\", 'auth'.env = \"K\" }]\n"
    );
    assert_eq!(
        planned(split, set_upstream("a", UpstreamField::AuthAccount, None)),
        "upstreams = [{ auth.mode = \"claude_oauth\", name = \"a\" }]\n"
    );
    assert_eq!(
        planned(split, set_upstream("a", UpstreamField::AuthMode, None)),
        "upstreams = [{ name = \"a\" }]\n"
    );
    assert_eq!(
        planned(
            split,
            Edit::RenameUpstream {
                name: "a".into(),
                to: "r".into()
            }
        ),
        "upstreams = [{ auth.mode = \"claude_oauth\", name = \"r\", 'auth'.account = \"w\" }]\n"
    );
    // three segments deep, inline: a key set lands after the deepest pair
    let deep = "models = [{ id = \"m\" }, { id = \"k\", router.type = \"stage_router\", router.classifier.target = \"m\", router.capable_target = \"m\" }]\n";
    assert_eq!(
        planned(deep, set_model("k", ModelField::DisplayName, Some("K"))),
        "models = [{ id = \"m\" }, { id = \"k\", router.type = \"stage_router\", router.classifier.target = \"m\", router.capable_target = \"m\", display_name = \"K\" }]\n"
    );
    assert_eq!(
        planned(
            deep,
            Edit::RenameModel {
                id: "m".into(),
                to: "z".into()
            }
        ),
        "models = [{ id = \"z\" }, { id = \"k\", router.type = \"stage_router\", router.classifier.target = \"z\", router.capable_target = \"z\" }]\n"
    );
    // dotted at the root, quoted and spaced
    assert_eq!(
        planned(
            "\"server\" . bind = \"127.0.0.1:1\"\n[[upstreams]]\nname = \"a\"\n",
            Edit::Server {
                field: ServerField::DefaultProvider,
                value: Some("a".into()),
            }
        ),
        "\"server\" . bind = \"127.0.0.1:1\"\n\"server\" . default_provider = \"a\"\n[[upstreams]]\nname = \"a\"\n"
    );
    // a dotted root table that a header also opens a sub-table of
    let opened = "server.bind = \"127.0.0.1:1\"\n\n[server.codex_endpoint]\nenabled = true\n\n[[upstreams]]\nname = \"codex\"\n";
    assert_eq!(
        planned(
            opened,
            Edit::Server {
                field: ServerField::DefaultProvider,
                value: Some("codex".into()),
            }
        ),
        "server.bind = \"127.0.0.1:1\"\nserver.default_provider = \"codex\"\n\n[server.codex_endpoint]\nenabled = true\n\n[[upstreams]]\nname = \"codex\"\n"
    );
    assert_eq!(
        planned(
            opened,
            Edit::RenameUpstream {
                name: "codex".into(),
                to: "r".into()
            }
        ),
        "server.bind = \"127.0.0.1:1\"\n\n[server.codex_endpoint]\nenabled = true\nprovider = \"r\"\n\n[[upstreams]]\nname = \"r\"\n"
    );
    // a dotted root table holding only a deeper dotted table
    assert_eq!(
        planned(
            "server.codex_endpoint.enabled = true\n[[upstreams]]\nname = \"codex\"\n",
            Edit::Server {
                field: ServerField::Bind,
                value: Some("127.0.0.1:9".into()),
            }
        ),
        "server.codex_endpoint.enabled = true\nserver.bind = \"127.0.0.1:9\"\n[[upstreams]]\nname = \"codex\"\n"
    );
    // dotted in an `[[x]]` body, quoted and spaced
    let aot = "[[upstreams]]\nname = \"a\"\n\"auth\" . mode = \"claude_oauth\"\neffort = \"low\"\n";
    assert_eq!(
        planned(
            aot,
            set_upstream("a", UpstreamField::AuthAccount, Some("w"))
        ),
        "[[upstreams]]\nname = \"a\"\n\"auth\" . mode = \"claude_oauth\"\n\"auth\" . account = \"w\"\neffort = \"low\"\n"
    );
    assert_eq!(
        planned(aot, set_upstream("a", UpstreamField::AuthMode, None)),
        "[[upstreams]]\nname = \"a\"\neffort = \"low\"\n"
    );
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\nauth.mode = \"api_key\"\nauth.env = \"K\"",
            set_upstream("a", UpstreamField::AuthMode, None)
        ),
        "[[upstreams]]\nname = \"a\""
    );
    // a dotted key whose table a header opens a sub-table of: new keys go
    // with the entry's own lines, never under that header
    let sub = "[[upstreams]]\nname = \"a\"\nauth.mode = \"claude_oauth\"\n\n[[upstreams.auth.accounts]]\nname = \"x\"\n";
    assert_eq!(
        planned(sub, set_upstream("a", UpstreamField::Effort, Some("low"))),
        "[[upstreams]]\nname = \"a\"\nauth.mode = \"claude_oauth\"\neffort = \"low\"\n\n[[upstreams.auth.accounts]]\nname = \"x\"\n"
    );
    assert_eq!(
        planned(
            sub,
            set_upstream("a", UpstreamField::AuthAccount, Some("w"))
        ),
        "[[upstreams]]\nname = \"a\"\nauth.mode = \"claude_oauth\"\nauth.account = \"w\"\n\n[[upstreams.auth.accounts]]\nname = \"x\"\n"
    );
    assert_eq!(
        refused(sub, set_upstream("a", UpstreamField::AuthMode, None)),
        "cannot unset auth.mode of upstream \"a\": the config spells \"upstreams.auth.accounts\" in a shape clauth does not edit; edit the config by hand"
    );
    let router = "[[models]]\nid = \"m\"\nrouter.type = \"stage_router\"\n[models.router.classifier]\ntarget = \"k\"\n";
    assert_eq!(
        planned(router, set_model("m", ModelField::DisplayName, Some("M"))),
        "[[models]]\nid = \"m\"\nrouter.type = \"stage_router\"\ndisplay_name = \"M\"\n[models.router.classifier]\ntarget = \"k\"\n"
    );
    // a scalar spelled as a dotted key refuses naming it as the file does
    assert_eq!(
        view_of("upstreams = [{ name = \"a\", effort . x = \"y\" }]\n", None)
            .expect_err("the view refuses")
            .to_string(),
        "cannot read the shunt config for editing: the config spells \"upstreams.effort . x\" in a shape clauth does not edit; edit the config by hand"
    );
}

#[test]
fn a_multi_line_string_sharing_its_first_line_owns_its_last_line_comment() {
    let _home = HomeSandbox::new();
    let p = |list: &str| {
        format!(
            "[[upstreams]]\nname = \"p\"\n[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = [{list}]\n"
        )
    };
    let text = p("\"a\", '''b\nb''' # spare\n");
    assert_eq!(planned(&text, accounts("p", &["a"])), p("\"a\"\n"));
    assert_eq!(
        planned(&text, accounts("p", &["b\nb"])),
        p("'''b\nb''' # spare\n")
    );
    // the same with a name shunt takes, the string's first newline trimmed
    let named = p("\"a\", '''\nb''' # spare\n");
    assert_eq!(planned(&named, accounts("p", &["a"])), p("\"a\"\n"));
    assert_eq!(
        planned(&named, accounts("p", &["b"])),
        p("'''\nb''' # spare\n")
    );
    assert_eq!(
        planned(&p("\"a\", '''b\nb''', # spare\n"), accounts("p", &["a"])),
        p("\"a\",\n")
    );
    let three = p("\"a\", '''b\nb''', # spare\n  \"c\",\n");
    assert_eq!(
        planned(&three, accounts("p", &["a", "c"])),
        p("\"a\",\n  \"c\",\n")
    );
    assert_eq!(
        planned(&three, accounts("p", &["a", "b\nb"])),
        p("\"a\", '''b\nb''', # spare\n")
    );
    assert_eq!(
        planned(&text, accounts("p", &["a", "b\nb", "n"])),
        p("\"a\", '''b\nb''', # spare\n  \"n\"\n")
    );
    // moved into a line of its own, it takes its comment along
    assert_eq!(
        planned(
            &p("\"a\", '''\nb''', '''\nc''' # c\n"),
            accounts("p", &["n", "b", "c"])
        ),
        p("\"n\", '''\nb''',\n  '''\nc''' # c\n")
    );
    assert_eq!(
        planned(
            &p("\"a\", '''b\nb''' # spare\n").replace('\n', "\r\n"),
            accounts("p", &["a"])
        ),
        p("\"a\"\n").replace('\n', "\r\n")
    );
    // a comment on a line the element shares, or between two entries,
    // still blocks
    assert_eq!(
        refused(
            &p("\"a\", '''b\nb''', \"c\" # spare\n"),
            accounts("p", &["a", "c"])
        ),
        "cannot remove account \"b\\nb\" of upstream \"p\": a comment next to it belongs to no single entry; edit the config by hand"
    );
    let between = p("\"a\", '''b\nb''',\n  # between\n  \"c\",\n");
    assert_eq!(
        refused(&between, accounts("p", &["a", "b\nb"])),
        "cannot remove account \"c\" of upstream \"p\": a comment next to it belongs to no single entry; edit the config by hand"
    );
    assert_eq!(
        refused(&between, accounts("p", &["a", "c"])),
        "cannot remove account \"b\\nb\" of upstream \"p\": a comment next to it belongs to no single entry; edit the config by hand"
    );
    // a field set keeps such an element spanning its lines, so its comment
    // stays on a line of its own
    assert_eq!(
        planned(
            "models = [{ id = \"a\" }, { id = \"b\", display_name = \"\"\"\nDb\"\"\" } # c\n]\n",
            set_model("b", ModelField::DisplayName, Some("E"))
        ),
        "models = [{ id = \"a\" }, { id = \"b\", display_name = \"\"\"\nE\"\"\" } # c\n]\n"
    );
    assert_eq!(
        planned(
            "upstreams = [{ name = \"a\" }, { name = \"b\", base_url = '''\r\nhttp://b''' }, # c\r\n]\r\n",
            set_upstream("b", UpstreamField::BaseUrl, Some("http://x"))
        ),
        "upstreams = [{ name = \"a\" }, { name = \"b\", base_url = '''\r\nhttp://x''' }, # c\r\n]\r\n"
    );
}

#[test]
fn a_file_mixing_line_endings_gives_a_new_line_the_ending_of_the_line_before_it() {
    let _home = HomeSandbox::new();
    assert_eq!(
        planned(
            "[[upstreams]]\r\nname = \"a\"\n\n[[upstreams]]\nname = \"b\"\n",
            Edit::AddUpstream(NewUpstream {
                name: "n".into(),
                ..NewUpstream::default()
            })
        ),
        "[[upstreams]]\r\nname = \"a\"\n\n[[upstreams]]\nname = \"b\"\n\n[[upstreams]]\nname = \"n\"\n"
    );
    let mixed = "[[upstreams]]\nname = \"a\"\n[[upstreams]]\r\nname = \"b\"\r\n";
    assert_eq!(
        planned(mixed, set_upstream("a", UpstreamField::Effort, Some("low"))),
        "[[upstreams]]\nname = \"a\"\neffort = \"low\"\n[[upstreams]]\r\nname = \"b\"\r\n"
    );
    assert_eq!(
        planned(mixed, set_upstream("b", UpstreamField::Effort, Some("low"))),
        "[[upstreams]]\nname = \"a\"\n[[upstreams]]\r\nname = \"b\"\r\neffort = \"low\"\r\n"
    );
    assert_eq!(
        planned(
            "[[upstreams]]\r\nname = \"p\"\r\n[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = [\n  \"a\",\n]\n",
            accounts("p", &["a", "n"])
        ),
        "[[upstreams]]\r\nname = \"p\"\r\n[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = [\n  \"a\",\n  \"n\",\n]\n"
    );
}

#[test]
fn an_append_keeps_a_comma_first_or_closing_bracket_layout() {
    let _home = HomeSandbox::new();
    assert_eq!(
        planned(
            &pool_p("  \"a\"\n  , \"b\",\n"),
            accounts("p", &["a", "b", "c"])
        ),
        pool_p("  \"a\"\n  , \"b\"\n  , \"c\",\n")
    );
    let closed = |list: &str| {
        format!(
            "[[upstreams]]\nname = \"p\"\n[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = [{list}]\n"
        )
    };
    assert_eq!(
        planned(&closed("\n  \"a\""), accounts("p", &["a", "b"])),
        closed("\n  \"a\",\n  \"b\"")
    );
    assert_eq!(
        planned(
            &closed("\"a\", # main\n  \"b\""),
            accounts("p", &["a", "b", "c"])
        ),
        closed("\"a\", # main\n  \"b\",\n  \"c\"")
    );
}

// ── round 9 (sign-off reviews r8) ───────────────────────────────────────────

#[test]
fn splices_that_overlap_refuse_as_clauths_bug() {
    let _home = HomeSandbox::new();
    let text = "[server]\nbind = \"127.0.0.1:1\" # pört\n";
    let edit = Edit::Server {
        field: ServerField::Bind,
        value: Some("127.0.0.1:2".into()),
    };
    let cut = |at: std::ops::Range<usize>, text: &str| Splice {
        at,
        text: text.to_string(),
    };
    let planned_over = |splices: Vec<Splice>| {
        plan_with(text, &edit, |ctx, _| {
            Ok(Change {
                candidate: splice(ctx.text(), splices)?,
                owned: Vec::new(),
                cascade: None,
            })
        })
    };
    let overlap = "cannot set server.bind: clauth built an edit that overlaps itself; nothing was written; report this as a clauth bug";
    assert_eq!(
        bug_of(planned_over(vec![
            cut(16..29, "\"127.0.0.1:2\""),
            cut(20..25, "")
        ])),
        overlap
    );
    assert_eq!(
        bug_of(planned_over(vec![cut(27..28, "2"), cut(40..40, "")])),
        overlap
    );
    assert_eq!(bug_of(planned_over(vec![cut(27..50, "")])), overlap);
    assert_eq!(bug_of(planned_over(vec![cut(27..34, "")])), overlap);
    assert_eq!(
        bug_of(planned_over(vec![
            cut(27..28, "2"),
            cut(std::ops::Range { start: 28, end: 27 }, "")
        ])),
        overlap
    );
    assert_eq!(
        bug_of(planned_over(vec![cut(27..28, "2"), cut(34..35, "")])),
        overlap
    );
    let shared = planned_over(vec![cut(27..27, ""), cut(27..27, "2"), cut(27..28, "")])
        .expect("inserts sharing an offset plan")
        .expect("the edit changes the value");
    assert_eq!(
        shared.candidate,
        "[server]\nbind = \"127.0.0.1:2\" # pört\n"
    );
}

#[test]
fn removing_an_entry_whose_sub_table_sits_past_another_table_cuts_each_run() {
    let _home = HomeSandbox::new();
    let remove_a = || Edit::RemoveUpstream { name: "a".into() };
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\nprovider = \"anthropic\"\n\n[[models]]\nid = \"m\"\n\n[upstreams.auth]\nmode = \"claude_oauth\"\n\n[server]\ndefault_provider = \"a\"\n",
            remove_a()
        ),
        "\n[[models]]\nid = \"m\"\n\n[server]\ndefault_provider = \"a\"\n"
    );
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"a\"\nprovider = \"anthropic\"\n\n[server]\ndefault_provider = \"a\"\n\n[upstreams.auth]\nmode = \"claude_oauth\"\n",
            remove_a()
        ),
        "\n[server]\ndefault_provider = \"a\"\n"
    );
    assert_eq!(
        planned(
            "[[models]]\nid = \"m\"\n\n[[upstreams]]\nname = \"a\"\nprovider = \"anthropic\"\nauth = \"passthrough\"\n\n[models.upstream_model]\na = \"s\"\n\n[server]\ndefault_provider = \"a\"\n",
            Edit::RemoveModel { id: "m".into() }
        ),
        "\n[[upstreams]]\nname = \"a\"\nprovider = \"anthropic\"\nauth = \"passthrough\"\n\n[server]\ndefault_provider = \"a\"\n"
    );
    // Each run takes the blocks it owns; the comments of the tables between
    // the runs keep their owners.
    let commented = r#"# file header

[[upstreams]]
name = "a" # a's name
provider = "anthropic"
# a's tail
[[models]]
# m's key
id = "m"

# auth of a
[upstreams.auth]
mode = "claude_oauth" # mode
# under auth

[[upstreams]]
name = "b"
provider = "anthropic"

[server]
# server note
default_provider = "b"
"#;
    assert_eq!(
        planned(commented, remove_a()),
        r#"# file header

[[models]]
# m's key
id = "m"

[[upstreams]]
name = "b"
provider = "anthropic"

[server]
# server note
default_provider = "b"
"#
    );
    let server_between = r#"[[upstreams]]
name = "a"
provider = "anthropic"

[server]
# server note
default_provider = "b"

[upstreams.auth]
mode = "claude_oauth"

[[upstreams]]
name = "b"
provider = "anthropic"
"#;
    assert_eq!(
        planned(server_between, remove_a()),
        "\n[server]\n# server note\ndefault_provider = \"b\"\n\n[[upstreams]]\nname = \"b\"\nprovider = \"anthropic\"\n"
    );
    let between = r#"[[upstreams]]
name = "a"

[server]
default_provider = "b"

[upstreams.auth]
mode = "claude_oauth"
# between
[[upstreams]]
name = "b"
"#;
    assert_eq!(
        refused(between, remove_a()),
        "cannot remove upstream \"a\": a comment next to it belongs to no single entry; edit the config by hand"
    );
}

#[test]
fn a_tab_spaced_dotted_auth_key_keeps_its_spelling() {
    let _home = HomeSandbox::new();
    let text = "upstreams = [{ name = \"a\", auth\t.\tmode = \"claude_oauth\" }]\n";
    assert_eq!(
        planned(
            text,
            set_upstream("a", UpstreamField::AuthAccount, Some("w"))
        ),
        "upstreams = [{ name = \"a\", auth\t.\tmode = \"claude_oauth\", auth\t.\taccount = \"w\" }]\n"
    );
    assert_eq!(
        planned(text, set_upstream("a", UpstreamField::AuthMode, None)),
        "upstreams = [{ name = \"a\" }]\n"
    );
}

#[test]
fn a_quoted_key_holding_a_dot_is_one_key_and_no_auth() {
    let _home = HomeSandbox::new();
    let text = "upstreams = [{ name = \"a\", \"auth.mode\" = \"claude_oauth\" }]\n";
    let view = view_of(text, None).expect("view");
    assert_eq!(view.upstreams[0].auth, None);
    assert_eq!(
        planned(text, set_upstream("a", UpstreamField::AuthMode, Some("x"))),
        "upstreams = [{ name = \"a\", \"auth.mode\" = \"claude_oauth\", auth = \"x\" }]\n"
    );
}

#[test]
fn an_auth_key_added_after_a_multi_line_dotted_value_takes_its_prefix() {
    let _home = HomeSandbox::new();
    assert_eq!(
        refused(
            "upstreams = [{ name = \"a\", auth.env = \"\"\"\nK\"\"\" }]\n",
            set_upstream("a", UpstreamField::AuthAccount, Some("w"))
        ),
        "cannot set auth.account of upstream \"a\": upstream \"a\" has no auth mode; set auth.mode first"
    );
    assert_eq!(
        planned(
            "upstreams = [{ name = \"a\", auth.mode = \"api_key\", auth.env = \"\"\"\nK\"\"\" }]\n",
            set_upstream("a", UpstreamField::AuthAccount, Some("w"))
        ),
        "upstreams = [{ name = \"a\", auth.mode = \"api_key\", auth.env = \"\"\"\nK\"\"\", auth.account = \"w\" }]\n"
    );
}

#[test]
fn a_key_added_inside_an_inline_entry_keeps_the_crlf_line() {
    let _home = HomeSandbox::new();
    assert_eq!(
        planned(
            "upstreams = [{ name = \"a\", auth.mode = \"x\" }]\r\n",
            set_upstream("a", UpstreamField::Effort, Some("low"))
        ),
        "upstreams = [{ name = \"a\", auth.mode = \"x\", effort = \"low\" }]\r\n"
    );
}

#[test]
fn a_four_account_reorder_moves_a_tail_element_with_its_comment() {
    let _home = HomeSandbox::new();
    let pool = |list: &str| {
        format!(
            "[[upstreams]]\nname = \"p\"\n[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = {list}\n"
        )
    };
    assert_eq!(
        planned(
            &pool("[\"a\", '''\nb''', # spare\n  \"c\",\n  \"d\"\n]"),
            accounts("p", &["c", "d", "b", "a"])
        ),
        pool("[\"c\", \"d\",\n  '''\nb''', # spare\n  \"a\"\n]")
    );
}

#[test]
fn a_value_set_with_a_newline_keeps_the_first_elements_comment() {
    let _home = HomeSandbox::new();
    assert_eq!(
        planned(
            "models = [{ id = \"m\", display_name = '''D''' }, # c\n]\n",
            set_model("m", ModelField::DisplayName, Some("E\nF"))
        ),
        "models = [{ id = \"m\", display_name = '''E\nF''' }, # c\n]\n"
    );
}

#[test]
fn a_new_last_line_in_a_crlf_file_with_no_final_newline_takes_crlf() {
    let _home = HomeSandbox::new();
    assert_eq!(
        planned(
            "[[upstreams]]\r\nname = \"a\"\r\nprovider = \"anthropic\"\r\nauth = \"passthrough\"",
            set_upstream("a", UpstreamField::Effort, Some("low"))
        ),
        "[[upstreams]]\r\nname = \"a\"\r\nprovider = \"anthropic\"\r\nauth = \"passthrough\"\r\neffort = \"low\""
    );
}

#[test]
fn a_new_dotted_root_key_lands_after_the_last_dotted_line() {
    let _home = HomeSandbox::new();
    let bind = || Edit::Server {
        field: ServerField::Bind,
        value: Some("127.0.0.1:9".into()),
    };
    assert_eq!(
        planned(
            "server.codex_endpoint.enabled = true\nserver.default_provider = \"codex\"\n\n[[server.codex_endpoint.routes]]\nmodel = \"x\"\nprovider = \"codex\"\n\n[[upstreams]]\nname = \"codex\"\nprovider = \"openai\"\nbase_url = \"https://chatgpt.com/backend-api/codex\"\nauth = \"chatgpt_oauth\"\n",
            bind()
        ),
        "server.codex_endpoint.enabled = true\nserver.default_provider = \"codex\"\nserver.bind = \"127.0.0.1:9\"\n\n[[server.codex_endpoint.routes]]\nmodel = \"x\"\nprovider = \"codex\"\n\n[[upstreams]]\nname = \"codex\"\nprovider = \"openai\"\nbase_url = \"https://chatgpt.com/backend-api/codex\"\nauth = \"chatgpt_oauth\"\n"
    );
    assert_eq!(
        planned(
            "server.codex_endpoint.enabled = true\nserver.default_provider = \"codex\"\n\n[server.codex_endpoint.x]\ny = 1\n\n[[upstreams]]\nname = \"codex\"\nprovider = \"openai\"\nauth = \"chatgpt_oauth\"\n",
            bind()
        ),
        "server.codex_endpoint.enabled = true\nserver.default_provider = \"codex\"\nserver.bind = \"127.0.0.1:9\"\n\n[server.codex_endpoint.x]\ny = 1\n\n[[upstreams]]\nname = \"codex\"\nprovider = \"openai\"\nauth = \"chatgpt_oauth\"\n"
    );
}

// ── round 10 (sign-off reviews r9) ──────────────────────────────────────────

/// A comment glued under another table's lines (its header or its last key)
/// and above a split entry's sub-table header is never the sub-table's:
/// removing the entry or the sub-table leaves it in place, byte for byte.
#[test]
fn a_comment_glued_under_another_tables_lines_stays_when_a_split_entry_goes() {
    let _home = HomeSandbox::new();
    let remove_a = || Edit::RemoveUpstream { name: "a".into() };
    let unset_mode = || set_upstream("a", UpstreamField::AuthMode, None);
    // the review's CO_headers: under the model's empty sub-table
    let under_sub = r#"[[upstreams]]
name = "anthropic"
provider = "anthropic"

[[upstreams]]
name = "a"
provider = "anthropic"

[[models]]
id = "m"
[models.headers]
# under m sub
[upstreams.auth]
mode = "claude_oauth"
"#;
    assert_eq!(
        planned(under_sub, remove_a()),
        r#"[[upstreams]]
name = "anthropic"
provider = "anthropic"

[[models]]
id = "m"
[models.headers]
# under m sub
"#
    );
    assert_eq!(
        planned(under_sub, unset_mode()),
        r#"[[upstreams]]
name = "anthropic"
provider = "anthropic"

[[upstreams]]
name = "a"
provider = "anthropic"

[[models]]
id = "m"
[models.headers]
# under m sub
"#
    );
    // the review's SV1v: under another table's header
    let under_server = r#"[[upstreams]]
name = "a"
provider = "anthropic"

[server]
# server note
[upstreams.auth]
mode = "claude_oauth"

[[upstreams]]
name = "anthropic"
provider = "anthropic"
"#;
    assert_eq!(
        planned(under_server, remove_a()),
        r#"
[server]
# server note

[[upstreams]]
name = "anthropic"
provider = "anthropic"
"#
    );
    assert_eq!(
        planned(under_server, unset_mode()),
        r#"[[upstreams]]
name = "a"
provider = "anthropic"

[server]
# server note

[[upstreams]]
name = "anthropic"
provider = "anthropic"
"#
    );
    // glued under the model's empty sub-table, the last line of the model,
    // and above an entry's own header: the model's, so it stays
    assert_eq!(
        planned(
            r#"[[upstreams]]
name = "anthropic"
provider = "anthropic"

[[models]]
id = "m"
[models.headers]
# under m sub
[[upstreams]]
name = "a"
provider = "anthropic"
"#,
            remove_a()
        ),
        r#"[[upstreams]]
name = "anthropic"
provider = "anthropic"

[[models]]
id = "m"
[models.headers]
# under m sub
"#
    );
}

/// A removal's cut that would glue a surviving comment onto a new neighbour
/// that changes its owner takes or keeps the blank line before it, whichever
/// keeps every owner, and refuses with the shape refusal where neither does.
#[test]
fn a_cut_keeps_the_blank_or_refuses_where_a_comment_would_change_owner() {
    let _home = HomeSandbox::new();
    let remove_a = || Edit::RemoveUpstream { name: "a".into() };
    let unset_mode = |name: &str| set_upstream(name, UpstreamField::AuthMode, None);
    let entry_shape = "cannot remove upstream \"a\": the config spells \"upstreams\" in a shape clauth does not edit; edit the config by hand";
    let auth_shape = "cannot unset auth.mode of upstream \"a\": the config spells \"upstreams.auth\" in a shape clauth does not edit; edit the config by hand";
    // the review's CL1v: `# m note` would glue onto the next model's header
    let cl1v = r#"[[upstreams]]
name = "anthropic"
provider = "anthropic"

[[upstreams]]
name = "a"
provider = "anthropic"

[[models]]
id = "m"
# m note
[upstreams.auth]
mode = "claude_oauth"
[[models]]
id = "m2"
"#;
    assert_eq!(refused(cl1v, remove_a()), entry_shape);
    assert_eq!(refused(cl1v, unset_mode("a")), auth_shape);
    // the review's CL0v: the same under the entry's own header
    let cl0v = r#"[[upstreams]]
name = "anthropic"
provider = "anthropic"

[[models]]
id = "m"
# m note
[[upstreams]]
name = "a"
provider = "anthropic"
[[models]]
id = "m2"
"#;
    assert_eq!(refused(cl0v, remove_a()), entry_shape);
    // the review's S6 (CRLF, no final newline): the remove keeps the blank
    // under `# m tail`; the unset leaves `# under auth` above the next model
    // whichever way it cuts
    let s6 = "[[upstreams]]\r\nname = \"a\"\r\nprovider = \"anthropic\"\r\n\r\n[[models]]\r\nid = \"m\"\r\n# m tail\r\n\r\n# auth doc\r\n[upstreams.auth]\r\nmode = \"claude_oauth\"\r\n# under auth\r\n[[models]]\r\nid = \"m2\"\r\n\r\n[[upstreams]]\r\nname = \"anthropic\"\r\nprovider = \"anthropic\"";
    assert_eq!(
        planned(s6, remove_a()),
        "\r\n[[models]]\r\nid = \"m\"\r\n# m tail\r\n\r\n[[models]]\r\nid = \"m2\"\r\n\r\n[[upstreams]]\r\nname = \"anthropic\"\r\nprovider = \"anthropic\""
    );
    assert_eq!(refused(s6, unset_mode("a")), auth_shape);
    // keeping the blank would set `# y` off on its own; taking it keeps
    // both comments the entry's
    assert_eq!(
        planned(
            r#"[[upstreams]]
name = "anthropic"
provider = "anthropic"
# x

# auth doc
[upstreams.auth]
mode = "claude_oauth"
# y
"#,
            unset_mode("anthropic")
        ),
        r#"[[upstreams]]
name = "anthropic"
provider = "anthropic"
# x
# y
"#
    );
}

/// A key added under a header with no keys leaves a blank line under it
/// where the block below would otherwise lose the entry that owns it; a
/// block no entry owns stays glued under the new key.
#[test]
fn a_key_added_under_a_bare_header_keeps_the_block_below_its_owner() {
    let _home = HomeSandbox::new();
    let bind = || Edit::Server {
        field: ServerField::Bind,
        value: Some("127.0.0.1:2".into()),
    };
    assert_eq!(
        planned(
            r#"[[upstreams]]
name = "anthropic"
provider = "anthropic"

[server]
# a own
[[upstreams]]
name = "a"
provider = "anthropic"
"#,
            bind()
        ),
        r#"[[upstreams]]
name = "anthropic"
provider = "anthropic"

[server]
bind = "127.0.0.1:2"

# a own
[[upstreams]]
name = "a"
provider = "anthropic"
"#
    );
    assert_eq!(
        planned(
            r#"[[upstreams]]
name = "a"
provider = "anthropic"

[server]
# server note
[upstreams.auth]
mode = "claude_oauth"

[[upstreams]]
name = "anthropic"
provider = "anthropic"
"#,
            bind()
        ),
        r#"[[upstreams]]
name = "a"
provider = "anthropic"

[server]
bind = "127.0.0.1:2"
# server note
[upstreams.auth]
mode = "claude_oauth"

[[upstreams]]
name = "anthropic"
provider = "anthropic"
"#
    );
}

/// The guard lets an edit take only the comments its own entry owns, read
/// by the guard's own rules, whatever range the planner claims.
#[test]
fn the_guard_refuses_a_claimed_range_holding_a_comment_the_edit_does_not_own() {
    let _home = HomeSandbox::new();
    let claimed = |text: &str, edit: Edit, candidate: &str, comment: &str| {
        let at = text.find(comment).unwrap();
        let owned = vec![std::ops::Range {
            start: at,
            end: at + comment.len(),
        }];
        bug_of(plan_with(text, &edit, |_, _| {
            Ok(Change {
                candidate: candidate.to_string(),
                owned,
                cascade: None,
            })
        }))
    };
    let lost = |op: &str| {
        format!(
            "{op}: clauth built an edit that loses or changes a comment; nothing was written; report this as a clauth bug"
        )
    };
    let under_sub = r#"[[upstreams]]
name = "anthropic"
provider = "anthropic"

[[upstreams]]
name = "a"
provider = "anthropic"

[[models]]
id = "m"
[models.headers]
# under m sub
[upstreams.auth]
mode = "claude_oauth"
"#;
    // a removal takes only the removed entry's: `# under m sub` is m's
    assert_eq!(
        claimed(
            under_sub,
            Edit::RemoveUpstream { name: "a".into() },
            "[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n\n[[models]]\nid = \"m\"\n[models.headers]\n",
            "# under m sub"
        ),
        lost("cannot remove upstream \"a\"")
    );
    // an edit of `a` takes none of another entry's
    assert_eq!(
        claimed(
            under_sub,
            set_upstream("a", UpstreamField::AuthMode, None),
            "[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n\n[[upstreams]]\nname = \"a\"\nprovider = \"anthropic\"\n\n[[models]]\nid = \"m\"\n[models.headers]\n",
            "# under m sub"
        ),
        lost("cannot unset auth.mode of upstream \"a\"")
    );
    // nor a free comment outside its own lines
    assert_eq!(
        claimed(
            r#"[[upstreams]]
name = "a"
provider = "anthropic"

[server]
# server note
[upstreams.auth]
mode = "claude_oauth"

[[upstreams]]
name = "anthropic"
provider = "anthropic"
"#,
            set_upstream("a", UpstreamField::AuthMode, None),
            "[[upstreams]]\nname = \"a\"\nprovider = \"anthropic\"\n\n[server]\n\n[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n",
            "# server note"
        ),
        lost("cannot unset auth.mode of upstream \"a\"")
    );
    // nor another upstream's account's
    assert_eq!(
        claimed(
            r#"[[upstreams]]
name = "p"
[upstreams.auth]
mode = "claude_oauth"
accounts = ["x", "y"]

[[upstreams]]
name = "q"
[upstreams.auth]
mode = "claude_oauth"
accounts = [
  "z", # z own
]
"#,
            accounts("p", &["x"]),
            "[[upstreams]]\nname = \"p\"\n[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = [\"x\"]\n\n[[upstreams]]\nname = \"q\"\n[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = [\n  \"z\",\n]\n",
            "# z own"
        ),
        lost("cannot set the accounts of upstream \"p\"")
    );
}

/// An entry's own sub-tables are one run with it, so removing the entry
/// takes a comment set off by blank lines between two of them.
#[test]
fn removing_an_entry_takes_a_comment_set_off_between_its_own_sub_tables() {
    let _home = HomeSandbox::new();
    assert_eq!(
        planned(
            r#"[[upstreams]]
name = "anthropic"
provider = "anthropic"

[[models]]
id = "m"
[models.upstream_model]
anthropic = "s"

# between

[models.headers]
x-team = "a"
"#,
            Edit::RemoveModel { id: "m".into() }
        ),
        "[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n"
    );
}

// ── round 11 (the unset ruling, review r10) ─────────────────────────────────

/// A single-key unset cuts the key's lines and nothing else: every comment
/// keeps its bytes and its place, even where the ownership rules then read
/// one beside the cut as another entry's or no entry's.
#[test]
fn a_single_key_unset_cuts_its_lines_and_never_refuses_on_a_comment() {
    let _home = HomeSandbox::new();
    // `# use oauth` ends glued under anthropic's last line and above b's header
    assert_eq!(
        planned(
            r#"[[upstreams]]
name = "anthropic"
provider = "anthropic"
# use oauth
auth = "claude_oauth"
[[upstreams]]
name = "b"
provider = "anthropic"
"#,
            set_upstream("anthropic", UpstreamField::AuthMode, None)
        ),
        r#"[[upstreams]]
name = "anthropic"
provider = "anthropic"
# use oauth
[[upstreams]]
name = "b"
provider = "anthropic"
"#
    );
    // the free note ends under the bare `[server]`, which sets it off for `a`
    assert_eq!(
        planned(
            r#"[[upstreams]]
name = "anthropic"
provider = "anthropic"

[server]
bind = "127.0.0.1:1"
# note
[[upstreams]]
name = "a"
provider = "anthropic"
"#,
            Edit::Server {
                field: ServerField::Bind,
                value: None
            }
        ),
        r#"[[upstreams]]
name = "anthropic"
provider = "anthropic"

[server]
# note
[[upstreams]]
name = "a"
provider = "anthropic"
"#
    );
    // the review's U1a, U1b, U1c
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"x\"\nprovider = \"anthropic\"\n# about effort\neffort = \"high\"\n[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n",
            set_upstream("x", UpstreamField::Effort, None)
        ),
        "[[upstreams]]\nname = \"x\"\nprovider = \"anthropic\"\n# about effort\n[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n"
    );
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"x\"\nprovider = \"anthropic\"\n\n# about effort\neffort = \"high\"\n[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n",
            set_upstream("x", UpstreamField::Effort, None)
        ),
        "[[upstreams]]\nname = \"x\"\nprovider = \"anthropic\"\n\n# about effort\n[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n"
    );
    assert_eq!(
        planned(
            "[server]\nbind = \"127.0.0.1:3067\"\n# note\n[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n",
            Edit::Server {
                field: ServerField::Bind,
                value: None
            }
        ),
        "[server]\n# note\n[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n"
    );
    // a dotted auth cut as one key over two runs, the comment between them
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"u\"\nprovider = \"anthropic\"\nauth.mode = \"claude_oauth\"\n# between\nauth.account = \"w\"\n[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n",
            set_upstream("u", UpstreamField::AuthMode, None)
        ),
        "[[upstreams]]\nname = \"u\"\nprovider = \"anthropic\"\n# between\n[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n"
    );
    // emptying the accounts cuts the key the same way
    assert_eq!(
        planned(
            "[[upstreams]]\nname = \"u\"\nprovider = \"anthropic\"\n[upstreams.auth]\nmode = \"claude_oauth\"\n# the pool\naccounts = [\"a\"]\n[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n",
            accounts("u", &[])
        ),
        "[[upstreams]]\nname = \"u\"\nprovider = \"anthropic\"\n[upstreams.auth]\nmode = \"claude_oauth\"\n# the pool\n[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n"
    );
    // an inline `upstream_model` cut as one key
    assert_eq!(
        planned(
            "[[models]]\nid = \"m\"\n# slugs\nupstream_model = { anthropic = \"s\" }\n[[models]]\nid = \"m2\"\n",
            set_model("m", ModelField::UpstreamModel("anthropic".into()), None)
        ),
        "[[models]]\nid = \"m\"\n# slugs\n[[models]]\nid = \"m2\"\n"
    );
    // a route's field
    let routes = "[[routes]]\nmodel = \"m\"\nprovider = \"anthropic\"\n# tier\neffort = \"high\"\n[[routes]]\nmodel = \"n\"\nprovider = \"anthropic\"\n";
    let first = view_of(routes, None).unwrap().routes[0].clone();
    assert_eq!(
        planned(
            routes,
            Edit::Route {
                route: first,
                field: RouteField::Effort,
                value: None
            }
        ),
        "[[routes]]\nmodel = \"m\"\nprovider = \"anthropic\"\n# tier\n[[routes]]\nmodel = \"n\"\nprovider = \"anthropic\"\n"
    );
}

/// The guard lets a comment change owner only when it sits glued directly
/// beside a single-key unset's cut lines, its bytes kept: never one further
/// off or past a code line, never on a sub-table or entry removal, a set, a
/// rename or an add.
#[test]
fn the_guard_lets_only_a_comment_beside_a_single_key_unset_change_owner() {
    let _home = HomeSandbox::new();
    let lost = |op: &str| {
        format!(
            "{op}: clauth built an edit that loses or changes a comment; nothing was written; report this as a clauth bug"
        )
    };
    // `# far` sits a blank line off the cut: taking that blank relabels it
    assert_eq!(
        bug_with(
            "[[upstreams]]\nname = \"x\"\nprovider = \"anthropic\"\neffort = \"high\"\n\n# far\n[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n",
            set_upstream("x", UpstreamField::Effort, None),
            &["effort = \"high\""],
            "[[upstreams]]\nname = \"x\"\nprovider = \"anthropic\"\n# far\n[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n"
        ),
        lost("cannot unset effort of upstream \"x\"")
    );
    // the same a blank line above the cut
    assert_eq!(
        bug_with(
            "[[upstreams]]\nname = \"x\"\n# far\n\neffort = \"high\"\n[[upstreams]]\nname = \"y\"\n",
            set_upstream("x", UpstreamField::Effort, None),
            &["effort = \"high\""],
            "[[upstreams]]\nname = \"x\"\n# far\n[[upstreams]]\nname = \"y\"\n"
        ),
        lost("cannot unset effort of upstream \"x\"")
    );
    // a key cut inside a line is no line cut: the comment above that line
    // keeps its owner
    assert_eq!(
        bug_with(
            "upstreams = [\n  { name = \"x\" },\n  # between\n  { name = \"y\", effort = \"high\" },\n]\n",
            set_upstream("y", UpstreamField::Effort, None),
            &["effort = \"high\""],
            "upstreams = [\n  { name = \"x\" },\n\n  # between\n  { name = \"y\" },\n]\n"
        ),
        lost("cannot unset effort of upstream \"y\"")
    );
    // the auth table's removal is a remove: `# m note` may not leave m
    let cl1v = "[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n\n[[upstreams]]\nname = \"a\"\nprovider = \"anthropic\"\n\n[[models]]\nid = \"m\"\n# m note\n[upstreams.auth]\nmode = \"claude_oauth\"\n[[models]]\nid = \"m2\"\n";
    assert_eq!(
        bug_with(
            cl1v,
            set_upstream("a", UpstreamField::AuthMode, None),
            &["[upstreams.auth]\nmode = \"claude_oauth\""],
            &cl1v.replace("[upstreams.auth]\nmode = \"claude_oauth\"\n", "")
        ),
        lost("cannot unset auth.mode of upstream \"a\"")
    );
    // nor the upstream_model table's
    let slugs = "[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n\n[[models]]\nid = \"m\"\n# m note\n[models.upstream_model]\nanthropic = \"s\"\n[[models]]\nid = \"m2\"\n";
    assert_eq!(
        bug_with(
            slugs,
            set_model("m", ModelField::UpstreamModel("anthropic".into()), None),
            &["[models.upstream_model]\nanthropic = \"s\""],
            &slugs.replace("[models.upstream_model]\nanthropic = \"s\"\n", "")
        ),
        lost("cannot unset upstream_model \"anthropic\" of model \"m\"")
    );
    // nor an entry's removal
    let cl0v = "[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n\n[[models]]\nid = \"m\"\n# m note\n[[upstreams]]\nname = \"a\"\nprovider = \"anthropic\"\n[[models]]\nid = \"m2\"\n";
    assert_eq!(
        bug_with(
            cl0v,
            Edit::RemoveUpstream { name: "a".into() },
            &["[[upstreams]]\nname = \"a\"\nprovider = \"anthropic\""],
            &cl0v.replace(
                "[[upstreams]]\nname = \"a\"\nprovider = \"anthropic\"\n",
                ""
            )
        ),
        lost("cannot remove upstream \"a\"")
    );
    // nor a set, whatever range it claims
    let bound = "[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n\n[server]\nbind = \"127.0.0.1:1\"\n# note\n[[upstreams]]\nname = \"a\"\nprovider = \"anthropic\"\n";
    assert_eq!(
        bug_with(
            bound,
            Edit::Server {
                field: ServerField::Bind,
                value: Some("127.0.0.1:2".into())
            },
            &["bind = \"127.0.0.1:1\""],
            &bound.replace("bind = \"127.0.0.1:1\"\n", "bind = \"127.0.0.1:2\"\n\n")
        ),
        lost("cannot set server.bind")
    );
    let effort = "[[upstreams]]\nname = \"x\"\nprovider = \"anthropic\"\neffort = \"high\"\n# under effort\n\n[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n";
    assert_eq!(
        bug_with(
            effort,
            set_upstream("x", UpstreamField::Effort, Some("low")),
            &["effort = \"high\""],
            &effort.replace("\"high\"\n# under effort\n\n", "\"low\"\n# under effort\n")
        ),
        lost("cannot set effort of upstream \"x\"")
    );
    let named =
        "[[models]]\nid = \"m\"\ndisplay_name = \"D\"\n# under name\n\n[[models]]\nid = \"m2\"\n";
    assert_eq!(
        bug_with(
            named,
            set_model("m", ModelField::DisplayName, Some("E")),
            &["display_name = \"D\""],
            &named.replace("\"D\"\n# under name\n\n", "\"E\"\n# under name\n")
        ),
        lost("cannot set display_name of model \"m\"")
    );
    // nor an accounts set that keeps a name
    assert_eq!(
        bug_with(
            "[[upstreams]]\nname = \"p\"\n[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = [\n  \"x\",\n  # between\n  \"y\",\n]\n",
            accounts("p", &["x"]),
            &["  \"y\","],
            "[[upstreams]]\nname = \"p\"\n[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = [\n  \"x\",\n  # between\n]\n"
        ),
        lost("cannot set the accounts of upstream \"p\"")
    );
    // an edit of no entry takes no entry's comment, whatever range it claims
    assert_eq!(
        bug_with(
            "[server]\nbind = \"127.0.0.1:1\"\n\n# a own\n[[upstreams]]\nname = \"a\"\nprovider = \"anthropic\"\n",
            Edit::Server {
                field: ServerField::Bind,
                value: None
            },
            &["bind = \"127.0.0.1:1\"", "# a own"],
            "[server]\n\n[[upstreams]]\nname = \"a\"\nprovider = \"anthropic\"\n"
        ),
        lost("cannot unset server.bind")
    );
    // a code line ends the glued block: `# x tail` sits past `provider`
    assert_eq!(
        bug_with(
            "[[upstreams]]\nname = \"x\"\neffort = \"high\"\nprovider = \"anthropic\"\n# x tail\n\n[[upstreams]]\nname = \"y\"\n",
            set_upstream("x", UpstreamField::Effort, None),
            &["effort = \"high\""],
            "[[upstreams]]\nname = \"x\"\nprovider = \"anthropic\"\n\n# x tail\n[[upstreams]]\nname = \"y\"\n"
        ),
        lost("cannot unset effort of upstream \"x\"")
    );
    // a beside comment keeps its bytes
    assert_eq!(
        bug_with(
            "[[upstreams]]\nname = \"x\"\n# about effort\neffort = \"high\"\n[[upstreams]]\nname = \"y\"\n",
            set_upstream("x", UpstreamField::Effort, None),
            &["effort = \"high\""],
            "[[upstreams]]\nname = \"x\"\n# about EFFORT\n[[upstreams]]\nname = \"y\"\n"
        ),
        lost("cannot unset effort of upstream \"x\"")
    );
    // nor a rename or an add, whatever whole line it claims
    let noted = "[[upstreams]]\nname = \"x\"\n# x note\nprovider = \"anthropic\"\n\n[[upstreams]]\nname = \"y\"\n";
    assert_eq!(
        bug_with(
            noted,
            Edit::RenameUpstream {
                name: "x".into(),
                to: "z".into()
            },
            &["name = \"x\""],
            "[[upstreams]]\nname = \"z\"\nprovider = \"anthropic\"\n\n# x note\n[[upstreams]]\nname = \"y\"\n"
        ),
        lost("cannot rename upstream \"x\" to \"z\"")
    );
    assert_eq!(
        bug_with(
            noted,
            Edit::AddUpstream(NewUpstream {
                name: "z".into(),
                ..NewUpstream::default()
            }),
            &["provider = \"anthropic\""],
            "[[upstreams]]\nname = \"x\"\nprovider = \"anthropic\"\n\n# x note\n[[upstreams]]\nname = \"y\"\n\n[[upstreams]]\nname = \"z\"\n"
        ),
        lost("cannot add upstream \"z\"")
    );
}

/// An unowned comment inside a removed upstream's own `accounts` list lies
/// inside the entry: removing the upstream takes it.
#[test]
fn removing_an_upstream_takes_the_unowned_comments_inside_its_accounts() {
    let _home = HomeSandbox::new();
    let remove_u = || Edit::RemoveUpstream { name: "u".into() };
    let rest = "\n[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n";
    let with = |auth: &str| {
        format!(
            "[[upstreams]]\nname = \"u\"\nprovider = \"anthropic\"\n{auth}\n[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n"
        )
    };
    // the review's ACC1: glued under an element
    assert_eq!(
        planned(
            &with(
                "[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = [\n  \"a\",\n  # between\n  \"b\",\n]\n"
            ),
            remove_u()
        ),
        rest
    );
    // ACC2: the same in an inline `auth`
    assert_eq!(
        planned(
            &with(
                "auth = { mode = \"claude_oauth\", accounts = [\n  \"a\",\n  # between\n  \"b\",\n] }\n"
            ),
            remove_u()
        ),
        rest
    );
    // ACC3: on the `[` line beside two elements
    assert_eq!(
        planned(
            &with(
                "[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = [\"a\", \"b\", # two on the open line\n]\n"
            ),
            remove_u()
        ),
        rest
    );
    // ACC4: between blank lines
    assert_eq!(
        planned(
            &with(
                "[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = [\n  \"a\",\n\n  # apart\n\n  \"b\",\n]\n"
            ),
            remove_u()
        ),
        rest
    );
    // a dotted `auth.accounts`
    assert_eq!(
        planned(
            &with(
                "auth.mode = \"claude_oauth\"\nauth.accounts = [\n  \"a\",\n  # between\n  \"b\",\n]\n"
            ),
            remove_u()
        ),
        rest
    );
    // ACC1 in CRLF
    assert_eq!(
        planned(
            &with("[upstreams.auth]\nmode = \"claude_oauth\"\naccounts = [\n  \"a\",\n  # between\n  \"b\",\n]\n")
                .replace('\n', "\r\n"),
            remove_u()
        ),
        rest.replace('\n', "\r\n")
    );
    // an element of an inline `upstreams` list
    assert_eq!(
        planned(
            "upstreams = [\n  { name = \"u\", provider = \"anthropic\", auth = { mode = \"claude_oauth\", accounts = [\n    \"a\",\n    # between\n    \"b\",\n  ] } },\n  { name = \"anthropic\", provider = \"anthropic\" },\n]\n",
            remove_u()
        ),
        "upstreams = [\n  { name = \"anthropic\", provider = \"anthropic\" },\n]\n"
    );
    // the guard lets a removal take no free comment outside the entry
    let under_server = "[[upstreams]]\nname = \"a\"\nprovider = \"anthropic\"\n\n[server]\n# server note\n[upstreams.auth]\nmode = \"claude_oauth\"\n\n[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n";
    assert_eq!(
        bug_with(
            under_server,
            Edit::RemoveUpstream { name: "a".into() },
            &["# server note"],
            "\n[server]\n\n[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n"
        ),
        "cannot remove upstream \"a\": clauth built an edit that loses or changes a comment; nothing was written; report this as a clauth bug"
    );
}

/// A block glued under an entry's sub-table with no keys and above the same
/// list's next entry is that next entry's: removing either entry lands.
#[test]
fn a_block_under_a_bare_sub_table_glued_to_the_next_entry_is_that_entrys() {
    let _home = HomeSandbox::new();
    let head = "[[upstreams]]\nname = \"anthropic\"\nprovider = \"anthropic\"\n\n";
    // the review's SL1
    let sl1 =
        format!("{head}[[models]]\nid = \"m\"\n[models.headers]\n# c\n[[models]]\nid = \"m2\"\n");
    assert_eq!(
        planned(&sl1, Edit::RemoveModel { id: "m2".into() }),
        format!("{head}[[models]]\nid = \"m\"\n[models.headers]\n")
    );
    assert_eq!(
        planned(&sl1, Edit::RemoveModel { id: "m".into() }),
        format!("{head}# c\n[[models]]\nid = \"m2\"\n")
    );
    assert_eq!(
        planned(
            &sl1.replace('\n', "\r\n"),
            Edit::RemoveModel { id: "m2".into() }
        ),
        format!("{head}[[models]]\nid = \"m\"\n[models.headers]\n").replace('\n', "\r\n")
    );
    // SL3: m2 has an empty sub-table of its own
    let sl3 = format!("{sl1}[models.headers]\n");
    assert_eq!(
        planned(&sl3, Edit::RemoveModel { id: "m2".into() }),
        format!("{head}[[models]]\nid = \"m\"\n[models.headers]\n")
    );
    assert_eq!(
        planned(&sl3, Edit::RemoveModel { id: "m".into() }),
        format!("{head}# c\n[[models]]\nid = \"m2\"\n[models.headers]\n")
    );
}
