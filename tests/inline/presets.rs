//! Preset store: the shipped table, the on-disk round-trip, and the two
//! refusals that keep a built-in's slot the binary's alone.

use super::{
    Binds, Builtin, RowMove, base_url_matches, builtin_binds_digest, delete_preset,
    digest_of_builtins, group_offers, is_builtin, list_presets, load_preset, load_saved_preset,
    preset_exists, presets_dir, rebind_offers, save_preset,
};
use crate::profile::{AppConfig, AppState, ModelSettings, Profile};

fn models(default: &str) -> ModelSettings {
    ModelSettings {
        default: Some(default.to_string()),
        ..ModelSettings::default()
    }
}

/// The shipped names in menu order, so a table change lands in one place.
const BUILTIN_NAMES: [&str; 5] = ["DeepSeek", "Z.ai", "OpenRouter", "MiniMax", "Qwen"];

fn names(presets: &[super::Preset]) -> Vec<&str> {
    presets.iter().map(|p| p.name.as_str()).collect()
}

/// A built-in's whole model block: `default` is the `opus` alias, so the main
/// model follows the opus bind; `subagent` stays unset, so each subagent's own
/// tier pick lands on that tier's bind.
fn binds(opus: &str, sonnet: &str, haiku: &str, fable: &str) -> ModelSettings {
    ModelSettings {
        default: Some("opus".to_string()),
        opus: Some(opus.to_string()),
        sonnet: Some(sonnet.to_string()),
        haiku: Some(haiku.to_string()),
        fable: Some(fable.to_string()),
        subagent: None,
        advisor: None,
    }
}

/// With nothing on disk the list is exactly the built-ins, in menu order, each
/// carrying its endpoint and a bind on every tier.
#[test]
fn the_builtins_ship_with_their_endpoint_and_every_tier_bound() {
    let _home = crate::testutil::HomeSandbox::new();

    let listed = list_presets();
    assert_eq!(names(&listed), BUILTIN_NAMES);
    assert!(listed.iter().all(|p| p.builtin));

    for (preset, base_url, models) in [
        (
            &listed[0],
            "https://api.deepseek.com/anthropic",
            binds(
                "deepseek-v4-pro[1m]",
                "deepseek-flash[1m]",
                "deepseek-flash[1m]",
                "deepseek-v4-pro[1m]",
            ),
        ),
        (
            &listed[1],
            "https://api.z.ai/api/anthropic",
            binds(
                "glm-5.3[1m]",
                "glm-5.3-flash[1m]",
                "glm-5.3-flash[1m]",
                "glm-5.3[1m]",
            ),
        ),
        (
            &listed[2],
            "https://openrouter.ai/api",
            binds(
                "~anthropic/claude-opus-latest[1m]",
                "~anthropic/claude-sonnet-latest[1m]",
                "~anthropic/claude-haiku-latest",
                "~anthropic/claude-fable-latest[1m]",
            ),
        ),
        (
            &listed[3],
            "https://api.minimax.io/anthropic",
            binds(
                "MiniMax-M3[1m]",
                "MiniMax-M3.1-Flash-Preview[1m]",
                "MiniMax-M3.1-Flash-Preview[1m]",
                "MiniMax-M3[1m]",
            ),
        ),
        (
            &listed[4],
            "https://token-plan.maas.qwencloudapi.com/apps/anthropic",
            binds(
                "qwen3.8-max[1m]",
                "qwen3.8-flash[1m]",
                "qwen3.8-flash[1m]",
                "qwen3.8-max[1m]",
            ),
        ),
    ] {
        assert_eq!(
            preset.base_url.as_deref(),
            Some(base_url),
            "{}",
            preset.name
        );
        assert_eq!(preset.models, models, "{}", preset.name);
    }
}

#[test]
fn a_saved_preset_round_trips_and_joins_the_list_after_the_builtins() {
    let _home = crate::testutil::HomeSandbox::new();

    let mut m = models("my-model");
    m.fable = Some("claude-fable-5".to_string());
    save_preset(
        "mine",
        &Some("https://api.example/anthropic".to_string()),
        &m,
    )
    .expect("save");

    let loaded = load_preset("mine").expect("the saved preset loads back");
    assert_eq!(loaded.name, "mine");
    assert_eq!(
        loaded.base_url.as_deref(),
        Some("https://api.example/anthropic")
    );
    assert_eq!(loaded.models, m, "every tier survives the json round-trip");
    assert!(!loaded.builtin);

    assert_eq!(
        names(&list_presets()),
        [BUILTIN_NAMES.as_slice(), ["mine"].as_slice()].concat(),
        "built-ins keep the head of the list",
    );
}

/// The whole preset tree lives under `~/.clauth`, so it is bound by the
/// owner-only invariant — dir 0o700, file 0o600, both
/// born that way rather than repaired later.
#[cfg(unix)]
#[test]
fn preset_file_and_dir_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let _home = crate::testutil::HomeSandbox::new();

    save_preset("mine", &None, &models("m")).expect("save");

    let dir = presets_dir().expect("presets dir");
    let dir_mode = std::fs::metadata(&dir)
        .expect("dir metadata")
        .permissions()
        .mode();
    assert_eq!(dir_mode & 0o777, 0o700, "presets dir is owner-only");

    let file_mode = std::fs::metadata(dir.join("mine.json"))
        .expect("file metadata")
        .permissions()
        .mode();
    assert_eq!(file_mode & 0o777, 0o600, "a preset file is owner-only");
}

/// A built-in's name is the binary's, whatever the spelling: a case-folding
/// filesystem would otherwise let `deepseek.json` take the slot on one host and
/// not another.
#[test]
fn a_builtin_name_refuses_both_writes_whatever_its_case() {
    let _home = crate::testutil::HomeSandbox::new();

    assert!(is_builtin("DeepSeek") && is_builtin("deepseek") && is_builtin("Z.AI"));

    let err = save_preset("deepseek", &None, &models("m")).expect_err("built-in name refuses");
    assert!(
        err.to_string().contains("built-in"),
        "the refusal names why: {err}",
    );
    assert!(
        delete_preset("DeepSeek").is_err(),
        "a built-in cannot be deleted either",
    );
    assert!(!preset_exists("deepseek"), "the refused save wrote no file",);
}

/// The name is the file NAME, so anything that could climb out of the presets
/// dir is refused before it ever reaches a `join`.
#[test]
fn a_name_that_is_not_a_bare_filename_is_refused() {
    let _home = crate::testutil::HomeSandbox::new();

    for bad in ["../escape", "a/b", "", ".hidden"] {
        assert!(
            save_preset(bad, &None, &models("m")).is_err(),
            "'{bad}' must not reach a path join",
        );
    }
}

#[test]
fn delete_drops_the_file_and_then_reports_the_miss() {
    let _home = crate::testutil::HomeSandbox::new();

    save_preset("mine", &None, &models("m")).expect("save");
    assert!(preset_exists("mine"));
    delete_preset("mine").expect("delete");
    assert!(!preset_exists("mine"));
    assert!(load_preset("mine").is_none());
    assert!(
        delete_preset("mine").is_err(),
        "deleting what is already gone is an error, not a silent success",
    );
}

/// A file whose stem no preset name can spell is skipped like an unparseable
/// one: apply and delete validate (and trim) the name, so an inner space would
/// offer a row that only ever answers "gone", and a trailing one (`good .json`)
/// a row that loads and deletes `good.json` instead.
#[test]
fn a_file_no_preset_name_can_spell_is_not_listed() {
    let _home = crate::testutil::HomeSandbox::new();

    save_preset("good", &None, &models("m")).expect("save");
    let dir = presets_dir().expect("presets dir");
    std::fs::write(dir.join("my preset.json"), r#"{"models":{}}"#).expect("hand-named file");
    std::fs::write(dir.join("good .json"), r#"{"models":{}}"#).expect("hand-named file");

    assert_eq!(
        names(&list_presets()),
        [BUILTIN_NAMES.as_slice(), ["good"].as_slice()].concat(),
    );
}

/// One unparseable file must not hide the rest of the store from the picker.
#[test]
fn an_unparseable_preset_is_skipped_not_fatal() {
    let _home = crate::testutil::HomeSandbox::new();

    save_preset("good", &None, &models("m")).expect("save");
    let dir = presets_dir().expect("presets dir");
    std::fs::write(dir.join("broken.json"), "{ not json").expect("write broken preset");

    assert_eq!(
        names(&list_presets()),
        [BUILTIN_NAMES.as_slice(), ["good"].as_slice()].concat(),
    );
}

/// A file whose stem matches a built-in's name (a preset a user saved before a
/// release shipped a built-in under that name, `qwen.json` beside `Qwen`) is
/// still the user's: it lists among the saved presets under its own stem,
/// loads and deletes as a saved preset, and never replaces the built-in, whose
/// entry keeps the shipped endpoint and stays undeletable.
#[test]
fn a_saved_file_in_a_builtins_slot_lists_beside_it_and_never_replaces_it() {
    let _home = crate::testutil::HomeSandbox::new();

    save_preset("anchor", &None, &models("m")).expect("seed the dir");
    let dir = presets_dir().expect("presets dir");
    std::fs::write(
        dir.join("qwen.json"),
        r#"{"base_url":"https://mine.test","models":{"default":"my-model"}}"#,
    )
    .expect("a file saved under the name before the built-in shipped");

    let listed = list_presets();
    assert_eq!(
        listed
            .iter()
            .map(|p| (p.name.as_str(), p.builtin))
            .collect::<Vec<_>>(),
        [
            ("DeepSeek", true),
            ("Z.ai", true),
            ("OpenRouter", true),
            ("MiniMax", true),
            ("Qwen", true),
            ("anchor", false),
            ("qwen", false),
        ],
        "the file lists among the saved presets, after the built-ins",
    );
    let builtin = listed
        .iter()
        .find(|p| p.builtin && p.name == "Qwen")
        .expect("built-in listed");
    assert_eq!(
        builtin.base_url.as_deref(),
        Some("https://token-plan.maas.qwencloudapi.com/apps/anthropic"),
        "the built-in entry reads the binary's copy, not the file",
    );
    let saved = listed
        .iter()
        .find(|p| !p.builtin && p.name == "qwen")
        .expect("saved entry listed");
    assert_eq!(saved.base_url.as_deref(), Some("https://mine.test"));
    assert_eq!(saved.models, models("my-model"));

    assert_eq!(
        load_preset("qwen").expect("loads").base_url.as_deref(),
        Some("https://token-plan.maas.qwencloudapi.com/apps/anthropic"),
        "a load by name still answers with the built-in",
    );
    let file = load_saved_preset("qwen").expect("the saved file loads");
    assert_eq!(file.base_url.as_deref(), Some("https://mine.test"));
    assert!(!file.builtin);

    delete_preset("qwen").expect("the saved file deletes");
    assert!(
        !dir.join("qwen.json").exists(),
        "delete removed the user's file"
    );
    assert_eq!(
        names(&list_presets()),
        [BUILTIN_NAMES.as_slice(), ["anchor"].as_slice()].concat(),
        "the built-in survives the delete",
    );
    let err = delete_preset("Qwen").expect_err("no file left: the built-in refuses");
    assert!(
        err.to_string().contains("built-in"),
        "the refusal names why: {err}"
    );
}

/// `load_saved_preset` reads only the disk: a built-in name with no file is
/// `None`, an ordinary saved preset loads as `load_preset` does.
#[test]
fn load_saved_preset_reads_only_the_file() {
    let _home = crate::testutil::HomeSandbox::new();

    assert!(load_saved_preset("DeepSeek").is_none());
    save_preset("mine", &None, &models("m")).expect("save");
    assert_eq!(
        load_saved_preset("mine"),
        Some(super::Preset {
            name: "mine".to_string(),
            base_url: None,
            models: models("m"),
            builtin: false,
        }),
    );
}

// ── Re-bind offer engine ──────────────────────────────────────────────────────

fn config_with(profiles: Vec<Profile>) -> AppConfig {
    AppConfig {
        state: AppState {
            profiles: profiles.iter().map(|p| p.name.clone()).collect(),
            ..AppState::default()
        },
        profiles,
    }
}

fn account(name: &str, base_url: &str, models: ModelSettings) -> Profile {
    let mut p = Profile::new(name.to_string(), Some(base_url.to_string()), None);
    p.models = models;
    p
}

fn move_(row: &'static str, old: Option<&str>, new: Option<&str>) -> RowMove {
    RowMove {
        row,
        old: old.map(str::to_string),
        new: new.map(str::to_string),
    }
}

/// The `(unset)` arm for a row with no stored id: `default` on a matching
/// account becomes the opus alias, and the four empty tier rows take the new
/// tier ids; `subagent` (whose bind is unset) is no change.
#[test]
fn a_former_id_and_every_empty_row_are_offered_the_new_binds() {
    let m = ModelSettings {
        default: Some("deepseek-chat".to_string()),
        ..ModelSettings::default()
    };
    let config = config_with(vec![account(
        "DS1",
        "https://api.deepseek.com/anthropic",
        m,
    )]);

    let offers = rebind_offers(&config);
    assert_eq!(offers.len(), 1);
    assert_eq!(offers[0].account, "DS1");
    assert_eq!(
        offers[0].rows,
        [
            move_("default", Some("deepseek-chat"), Some("opus")),
            move_("opus", None, Some("deepseek-v4-pro[1m]")),
            move_("sonnet", None, Some("deepseek-flash[1m]")),
            move_("haiku", None, Some("deepseek-flash[1m]")),
            move_("fable", None, Some("deepseek-v4-pro[1m]")),
        ],
    );
}

/// A Qwen Cloud account still holding what the four retired Model Studio
/// presets wrote (one id on every row, the subagent row included) is offered
/// the Qwen preset's tier binds, and its subagent row the preset's unset value;
/// an account whose rows hold ids no earlier release shipped (`qwen3.8-max[1m]`, the
/// `qwen` account's own spelling) keeps them and is offered only its empty rows.
#[test]
fn the_retired_qwen_ids_are_offered_the_qwen_cloud_binds() {
    let url = "https://token-plan.maas.qwencloudapi.com/apps/anthropic";
    let every = |id: &str| ModelSettings {
        default: Some(id.to_string()),
        opus: Some(id.to_string()),
        sonnet: Some(id.to_string()),
        haiku: Some(id.to_string()),
        fable: Some(id.to_string()),
        subagent: Some(id.to_string()),
        advisor: None,
    };
    let own = ModelSettings {
        opus: Some("qwen3.8-max[1m]".to_string()),
        sonnet: Some("qwen3.8-max[1m]".to_string()),
        haiku: Some("qwen3.8-max[1m]".to_string()),
        subagent: Some("qwen3.8-max[1m]".to_string()),
        ..ModelSettings::default()
    };
    let config = config_with(vec![
        account("token", url, every("qwen3.8-max")),
        account("coding", url, every("qwen3-coder-plus")),
        account("own", url, own),
    ]);

    let offers = rebind_offers(&config);
    let moved = |old: &str| {
        [
            move_("default", Some(old), Some("opus")),
            move_("opus", Some(old), Some("qwen3.8-max[1m]")),
            move_("sonnet", Some(old), Some("qwen3.8-flash[1m]")),
            move_("haiku", Some(old), Some("qwen3.8-flash[1m]")),
            move_("fable", Some(old), Some("qwen3.8-max[1m]")),
            move_("subagent", Some(old), None),
        ]
    };
    assert_eq!(offers.len(), 3);
    assert_eq!(offers[0].account, "token");
    assert_eq!(offers[0].rows, moved("qwen3.8-max"));
    assert_eq!(offers[1].account, "coding");
    assert_eq!(offers[1].rows, moved("qwen3-coder-plus"));
    assert_eq!(offers[2].account, "own");
    assert_eq!(
        offers[2].rows,
        [
            move_("default", None, Some("opus")),
            move_("fable", None, Some("qwen3.8-max[1m]")),
        ],
    );
}

/// A fully empty matching account is offered every tier the bind fills, and
/// nothing else.
#[test]
fn a_fully_empty_account_is_offered_every_filled_row() {
    let config = config_with(vec![account(
        "fresh",
        "https://api.z.ai/api/anthropic",
        ModelSettings::default(),
    )]);

    let offers = rebind_offers(&config);
    assert_eq!(offers.len(), 1);
    assert_eq!(
        offers[0].rows,
        [
            move_("default", None, Some("opus")),
            move_("opus", None, Some("glm-5.3[1m]")),
            move_("sonnet", None, Some("glm-5.3-flash[1m]")),
            move_("haiku", None, Some("glm-5.3-flash[1m]")),
            move_("fable", None, Some("glm-5.3[1m]")),
        ],
    );
}

/// A custom id on a row is never touched, whatever it is; only the empty rows
/// are offered.
#[test]
fn a_custom_id_row_is_untouched() {
    let m = ModelSettings {
        default: Some("nvidia/nemotron-3-ultra-550b-a55b:free[1m]".to_string()),
        ..ModelSettings::default()
    };
    let config = config_with(vec![account("openrouter", "https://openrouter.ai/api", m)]);

    let offers = rebind_offers(&config);
    assert_eq!(offers.len(), 1);
    assert_eq!(
        offers[0].rows,
        [
            move_("opus", None, Some("~anthropic/claude-opus-latest[1m]")),
            move_("sonnet", None, Some("~anthropic/claude-sonnet-latest[1m]")),
            move_("haiku", None, Some("~anthropic/claude-haiku-latest")),
            move_("fable", None, Some("~anthropic/claude-fable-latest[1m]")),
        ],
        "the custom default row is not among the moves",
    );
}

/// A row already equal to its new value is no change — nothing to offer.
#[test]
fn a_row_already_on_its_new_value_is_no_change() {
    let m = ModelSettings {
        default: Some("opus".to_string()),
        opus: Some("deepseek-v4-pro[1m]".to_string()),
        sonnet: Some("deepseek-flash[1m]".to_string()),
        haiku: Some("deepseek-flash[1m]".to_string()),
        fable: Some("deepseek-v4-pro[1m]".to_string()),
        ..ModelSettings::default()
    };
    let config = config_with(vec![account(
        "current",
        "https://api.deepseek.com/anthropic",
        m,
    )]);

    assert!(rebind_offers(&config).is_empty());
}

/// A former id on a row the bind leaves unset is offered `(unset)`: the exact
/// string match is per-row, so `deepseek-chat` on the `subagent` row moves.
/// The whole row vec is pinned, so an extra move reds.
#[test]
fn a_former_id_on_a_row_the_bind_leaves_unset_is_offered_unset() {
    let m = ModelSettings {
        subagent: Some("deepseek-chat".to_string()),
        ..ModelSettings::default()
    };
    let config = config_with(vec![account(
        "DS1",
        "https://api.deepseek.com/anthropic",
        m,
    )]);

    let offers = rebind_offers(&config);
    assert_eq!(offers.len(), 1);
    assert_eq!(
        offers[0].rows,
        [
            move_("default", None, Some("opus")),
            move_("opus", None, Some("deepseek-v4-pro[1m]")),
            move_("sonnet", None, Some("deepseek-flash[1m]")),
            move_("haiku", None, Some("deepseek-flash[1m]")),
            move_("fable", None, Some("deepseek-v4-pro[1m]")),
            move_("subagent", Some("deepseek-chat"), None),
        ],
    );
}

/// An endpoint no built-in ships is never offered.
#[test]
fn an_account_on_another_endpoint_is_untouched() {
    let config = config_with(vec![account(
        "anthropic",
        "https://api.anthropic.com",
        ModelSettings::default(),
    )]);
    assert!(rebind_offers(&config).is_empty());
}

/// The endpoint predicate is the shared host matcher: case-insensitive on
/// scheme + host, forgiving a trailing slash, and a different provider's
/// endpoint never matches.
#[test]
fn a_trailing_slash_base_url_matches() {
    assert!(base_url_matches(
        "https://api.deepseek.com/anthropic/",
        "https://api.deepseek.com/anthropic"
    ));
    assert!(
        base_url_matches(
            "https://API.deepseek.com/anthropic",
            "https://api.deepseek.com/anthropic"
        ),
        "a mixed-case host matches"
    );
    assert!(
        !base_url_matches(
            "https://api.anthropic.com",
            "https://api.deepseek.com/anthropic"
        ),
        "another provider never matches"
    );
    assert!(
        !base_url_matches(
            "https://api.deepseek.com",
            "https://api.deepseek.com/anthropic"
        ),
        "a host-only account is a different endpoint"
    );

    let config = config_with(vec![account(
        "DS1",
        "https://api.deepseek.com/anthropic/",
        ModelSettings::default(),
    )]);
    assert_eq!(rebind_offers(&config).len(), 1);
}

/// Accounts whose change sets are byte-identical share a block; a differing
/// one splits into its own, preserving first-seen order.
#[test]
fn identical_change_sets_share_a_block_and_a_differing_one_splits() {
    let ds_a = account(
        "a",
        "https://api.deepseek.com/anthropic",
        ModelSettings::default(),
    );
    let ds_b = account(
        "b",
        "https://api.deepseek.com/anthropic",
        ModelSettings::default(),
    );
    let z = account(
        "z",
        "https://api.z.ai/api/anthropic",
        ModelSettings::default(),
    );
    let blocks = group_offers(rebind_offers(&config_with(vec![ds_a, ds_b, z])));

    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].accounts, ["a", "b"]);
    assert_eq!(blocks[1].accounts, ["z"]);
    assert_ne!(blocks[0].rows, blocks[1].rows);
}

/// Two literal tables hash differently, and the shipped table's digest is a
/// stable 64-hex pin — never recomputed from the code under test.
#[test]
fn the_digest_changes_when_a_bind_changes() {
    let with = |opus: &'static str| {
        digest_of_builtins(&[Builtin {
            name: "X",
            base_url: "https://a",
            former_ids: &[],
            binds: Binds {
                opus,
                sonnet: "fast",
                haiku: "fast",
                fable: "big",
            },
        }])
    };
    let one = with("m1");
    let two = with("m2");
    assert_ne!(one, two, "a bind change must change the digest");
    assert_eq!(one.len(), 64);
}

/// A release that only adds a former id must reopen the confirm for an
/// account that dismissed it, so `former_ids` feeds the digest too.
#[test]
fn the_digest_changes_when_only_a_former_id_changes() {
    let with = |former_ids: &'static [&'static str]| {
        digest_of_builtins(&[Builtin {
            name: "X",
            base_url: "https://a",
            former_ids,
            binds: Binds {
                opus: "big",
                sonnet: "fast",
                haiku: "fast",
                fable: "big",
            },
        }])
    };
    assert_ne!(
        with(&[]),
        with(&["old"]),
        "adding a former id must change the digest"
    );
    assert_ne!(
        with(&["old"]),
        with(&["old", "older"]),
        "a second former id must change the digest"
    );
    assert_ne!(
        with(&["old"]),
        with(&["new"]),
        "replacing a former id must change the digest"
    );
    assert_ne!(
        with(&["ab", "c"]),
        with(&["a", "bc"]),
        "moving an id boundary must change the digest"
    );
}

#[test]
fn the_shipped_binds_digest_is_a_stable_literal() {
    assert_eq!(
        builtin_binds_digest(),
        "2903a49cd2ef4476f104b8bd169e0a29475727ce7505914b8bd7f1e4f3de3585"
    );
}

#[test]
fn the_advisor_is_a_model_row_no_built_in_binds() {
    assert!(super::MODEL_ROWS.contains(&"advisor"));
    for builtin in super::BUILTINS {
        assert!(
            builtin.binds.models().advisor.is_none(),
            "{} must leave the advisor unset",
            builtin.name
        );
    }
}

/// A custom advisor id on a built-in's endpoint is never an offered move: the
/// built-in leaves the row unset, and an account's own id is only ever replaced
/// when it is a former id of that built-in.
#[test]
fn an_advisor_id_on_a_built_in_endpoint_gets_no_rebind_move() {
    let ds = super::BUILTINS
        .iter()
        .find(|b| b.name == "DeepSeek")
        .expect("DeepSeek built-in");
    let mut held = ds.binds.models();
    held.advisor = Some("my-id".to_string());
    let config = config_with(vec![account("ds", ds.base_url, held)]);
    assert!(
        rebind_offers(&config).is_empty(),
        "nothing to offer: every bound row is current and the advisor is the account's own"
    );
}

#[test]
fn apply_diff_and_applied_to_carry_the_advisor_row() {
    let preset = super::Preset {
        name: "p".to_string(),
        base_url: None,
        models: ModelSettings::default(),
        builtin: false,
    };
    let held = ModelSettings {
        advisor: Some("opus".to_string()),
        ..ModelSettings::default()
    };
    let (replaces, leaves) = preset.apply_diff(None, &held);
    assert!(replaces.is_empty());
    assert_eq!(leaves, vec!["advisor"]);

    let (_, kept) = preset.applied_to(&None, &held, super::Leftover::Keep);
    assert_eq!(kept.advisor.as_deref(), Some("opus"));
    let (_, cleared) = preset.applied_to(&None, &held, super::Leftover::Clear);
    assert_eq!(cleared.advisor, None);

    let setting = super::Preset {
        models: ModelSettings {
            advisor: Some("sonnet".to_string()),
            ..ModelSettings::default()
        },
        ..preset
    };
    let (replaces, _) = setting.apply_diff(None, &held);
    assert_eq!(replaces, vec!["advisor"]);
    let (_, replaced) = setting.applied_to(&None, &held, super::Leftover::Keep);
    assert_eq!(replaced.advisor.as_deref(), Some("sonnet"));
}
