//! The shunt config editor's engine: a read of the adopted config into the
//! editable view (server `bind` + `default_provider`, `[[upstreams]]`,
//! `[[models]]`, `[[routes]]`) and one edit at a time landed as a minimal
//! text splice on the file's own bytes, located from `toml_edit`'s spans and
//! written through [`write_checked`], the path every config edit takes.
//!
//! Before the write every candidate re-parses, its TOML value equals the
//! original's value with the edit applied (computed apart from the splice, on
//! `toml`'s own tree), and every comment of the original survives with the
//! owner it had but those a removed entry owned, and one glued beside a
//! single-key unset's cut lines, which may change owner. Any miss is an
//! [`EditBug`], never a bad-input [`EditRefusal`].
//!
//! Comment ownership: an entry owns a comment on its own line and the
//! full-line block glued directly above it whose top is set off by a blank
//! line, the list's `[`, a section header or the file's start; an `[[x]]`
//! entry also owns the block glued under its last line that a blank line or
//! the file's end closes; a block before `]` belongs to the list; every other
//! comment between entries belongs to no single entry, and a remove or reorder
//! whose gap holds one refuses. Only a gap between two entries of one list can
//! hold one: a comment above a list's first entry or below its last, not
//! glued to it, lies in no gap and stays.

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the Services card's config section, the next slice, is this engine's only caller"
    )
)]

use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;

use anyhow::Result;
use toml_edit::{Array, Document, InlineTable, Item, Key, Table, TableLike, Value};

use crate::gateway::{
    BIND_ENV, BindRefusal, ConfigBindRefused, GatewayRecord, StoreMoveRefusal, config_parse_error,
    gateway_env, inherited_env, read_config_for_edit, recorded_env_for_holder_pid, resolve_bind,
    spawned_env_var, write_checked,
};

// ── the view ────────────────────────────────────────────────────────────────

/// A value as the file writes it: `raw` is its TOML text (quotes and all, a
/// `${…}` reference unresolved), `value` the string it holds when it is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Written {
    pub(crate) raw: String,
    pub(crate) value: Option<String>,
}

/// The adopted config at the editor's depth. An absent key reads as `None`,
/// never as shunt's default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigView {
    pub(crate) server: ServerView,
    pub(crate) form: ProviderForm,
    pub(crate) upstreams: Vec<UpstreamView>,
    pub(crate) models: Vec<ModelView>,
    pub(crate) routes: Vec<RouteView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerView {
    pub(crate) bind: Option<Written>,
    /// The variable, as the gateway's spawn env spells it, that overrides
    /// `bind`: the row is read-only while it is set.
    pub(crate) bind_override: Option<String>,
    pub(crate) default_provider: Option<Written>,
}

/// How the config declares its providers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderForm {
    Upstreams,
    /// `[providers.*]`, listed read-only by name: upstream edits refuse.
    Legacy {
        providers: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpstreamView {
    pub(crate) name: Option<Written>,
    pub(crate) provider: Option<Written>,
    pub(crate) kind: Option<Written>,
    pub(crate) base_url: Option<Written>,
    pub(crate) auth: Option<AuthView>,
    pub(crate) effort: Option<Written>,
    pub(crate) service_tier: Option<Written>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthView {
    pub(crate) form: AuthForm,
    pub(crate) mode: Option<Written>,
    pub(crate) account: Option<Written>,
    pub(crate) accounts: Option<Vec<AccountView>>,
    pub(crate) env: Option<Written>,
    pub(crate) header: Option<Written>,
}

/// How `auth` is spelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthForm {
    /// `auth = "claude_oauth"`.
    Shorthand,
    /// `auth = { mode = … }`.
    Inline,
    /// `[upstreams.auth]`.
    Table,
    /// `auth.mode = …` dotted keys, as lines or in an inline entry.
    Dotted,
}

/// One `accounts` entry, in shunt's `AccountSelection` forms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AccountView {
    Name(Written),
    /// A table selection (`{ name = … }` or `[[upstreams.auth.accounts]]`):
    /// its other settings are a hand edit.
    Selection {
        name: Option<Written>,
        form: SelectionForm,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectionForm {
    Inline,
    Table,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelView {
    pub(crate) id: Option<Written>,
    pub(crate) display_name: Option<Written>,
    /// Provider → slug, the provider key decoded.
    pub(crate) upstream_model: Option<Vec<(String, Written)>>,
}

/// A route, identified by its position plus every field it carried: an edit
/// naming it refuses when the file no longer matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RouteView {
    /// 1-based, as the refusals name it.
    pub(crate) position: usize,
    pub(crate) model: Option<Written>,
    pub(crate) provider: Option<Written>,
    pub(crate) upstream_model: Option<Written>,
    pub(crate) effort: Option<Written>,
    pub(crate) service_tier: Option<Written>,
}

impl RouteView {
    fn named(&self) -> String {
        let at = format!("the route at position {}", self.position);
        match self.model.as_ref().and_then(|m| m.value.as_deref()) {
            Some(model) => format!("{at} (model {model:?})"),
            None => at,
        }
    }
}

/// The adopted config's view, `bind`'s override read from the gateway's env
/// file over the env the gateway inherits: a running daemon's record matched
/// by pid as the display reads take it ([`recorded_env_for_holder_pid`]), so
/// no process is spawned and no missing record refuses the read; with no
/// daemon, this process's own env as `start daemon` would pass it. Only a
/// `bind` edit reads the daemon's env strictly.
pub(crate) fn read_view(record: &GatewayRecord) -> Result<ConfigView> {
    let (_, text) = read_config_for_edit(record.config())?;
    let env = gateway_env(record)?;
    let inherited = match crate::daemon::singleton_held() {
        Ok(false) => inherited_env().map(|(pairs, _)| pairs).unwrap_or_default(),
        Ok(true) | Err(_) => recorded_env_for_holder_pid().unwrap_or_default(),
    };
    let bind_override = spawned_env_var(&env, BIND_ENV, inherited)
        .map(|(name, _)| name.to_string_lossy().into_owned());
    view_of(&text, bind_override)
}

fn view_of(text: &str, bind_override: Option<String>) -> Result<ConfigView> {
    let ctx = Ctx::new(text, "cannot read the shunt config for editing".to_string())?;
    Ok(ctx.view(bind_override)?)
}

// ── edits ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServerField {
    Bind,
    DefaultProvider,
}

impl ServerField {
    fn key(self) -> &'static str {
        match self {
            Self::Bind => "bind",
            Self::DefaultProvider => "default_provider",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpstreamField {
    Provider,
    Kind,
    BaseUrl,
    Effort,
    ServiceTier,
    AuthMode,
    AuthAccount,
    AuthEnv,
    AuthHeader,
}

impl UpstreamField {
    fn label(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Kind => "kind",
            Self::BaseUrl => "base_url",
            Self::Effort => "effort",
            Self::ServiceTier => "service_tier",
            Self::AuthMode => "auth.mode",
            Self::AuthAccount => "auth.account",
            Self::AuthEnv => "auth.env",
            Self::AuthHeader => "auth.header",
        }
    }

    /// The key inside `auth`, for an auth field.
    fn auth_key(self) -> Option<&'static str> {
        match self {
            Self::AuthMode => Some("mode"),
            Self::AuthAccount => Some("account"),
            Self::AuthEnv => Some("env"),
            Self::AuthHeader => Some("header"),
            Self::Provider | Self::Kind | Self::BaseUrl | Self::Effort | Self::ServiceTier => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelField {
    DisplayName,
    /// The slug for one provider in `upstream_model`.
    UpstreamModel(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteField {
    Model,
    Provider,
    UpstreamModel,
    Effort,
    ServiceTier,
}

impl RouteField {
    fn key(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Provider => "provider",
            Self::UpstreamModel => "upstream_model",
            Self::Effort => "effort",
            Self::ServiceTier => "service_tier",
        }
    }
}

/// A new upstream's fields, written in this order; `None` leaves a key out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NewUpstream {
    pub(crate) name: String,
    pub(crate) provider: Option<String>,
    pub(crate) kind: Option<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) auth: Option<NewAuth>,
    pub(crate) effort: Option<String>,
    pub(crate) service_tier: Option<String>,
}

/// A mode alone writes the shorthand `auth = "<mode>"`, anything more the
/// inline table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NewAuth {
    pub(crate) mode: Option<String>,
    pub(crate) account: Option<String>,
    pub(crate) accounts: Vec<String>,
    pub(crate) env: Option<String>,
    pub(crate) header: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NewModel {
    pub(crate) id: String,
    pub(crate) display_name: Option<String>,
    pub(crate) upstream_model: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NewRoute {
    pub(crate) model: String,
    pub(crate) provider: String,
    pub(crate) upstream_model: Option<String>,
    pub(crate) effort: Option<String>,
    pub(crate) service_tier: Option<String>,
}

/// One edit. A field `value` of `None` unsets the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Edit {
    Server {
        field: ServerField,
        value: Option<String>,
    },
    Upstream {
        name: String,
        field: UpstreamField,
        value: Option<String>,
    },
    Model {
        id: String,
        field: ModelField,
        value: Option<String>,
    },
    Route {
        route: RouteView,
        field: RouteField,
        value: Option<String>,
    },
    /// Renames every reference too: `[[routes]]` and `[[route_prefixes]]`
    /// `provider`, `[[models]]` `upstream_model` keys, `default_provider`,
    /// `[server.codex_endpoint]` and its routes.
    RenameUpstream {
        name: String,
        to: String,
    },
    RenameModel {
        id: String,
        to: String,
    },
    AddUpstream(NewUpstream),
    AddModel(NewModel),
    AddRoute(NewRoute),
    RemoveUpstream {
        name: String,
    },
    RemoveModel {
        id: String,
    },
    RemoveRoute {
        route: RouteView,
    },
    /// The upstream's `accounts`, by name, in this order: kept accounts keep
    /// their bytes and comments, missing ones drop, new ones are added.
    SetAccounts {
        name: String,
        accounts: Vec<String>,
    },
}

impl Edit {
    fn op(&self) -> String {
        let verb = |value: &Option<String>| if value.is_some() { "set" } else { "unset" };
        match self {
            Edit::Server { field, value } => {
                format!("cannot {} server.{}", verb(value), field.key())
            }
            Edit::Upstream { name, field, value } => {
                format!(
                    "cannot {} {} of upstream {name:?}",
                    verb(value),
                    field.label()
                )
            }
            Edit::Model { id, field, value } => {
                let field = match field {
                    ModelField::DisplayName => "display_name".to_string(),
                    ModelField::UpstreamModel(provider) => format!("upstream_model {provider:?}"),
                };
                format!("cannot {} {field} of model {id:?}", verb(value))
            }
            Edit::Route {
                route,
                field,
                value,
            } => format!(
                "cannot {} {} of {}",
                verb(value),
                field.key(),
                route.named()
            ),
            Edit::RenameUpstream { name, to } => {
                format!("cannot rename upstream {name:?} to {to:?}")
            }
            Edit::RenameModel { id, to } => format!("cannot rename model {id:?} to {to:?}"),
            Edit::AddUpstream(new) => format!("cannot add upstream {:?}", new.name),
            Edit::AddModel(new) => format!("cannot add model {:?}", new.id),
            Edit::AddRoute(new) => format!("cannot add a route for model {:?}", new.model),
            Edit::RemoveUpstream { name } => format!("cannot remove upstream {name:?}"),
            Edit::RemoveModel { id } => format!("cannot remove model {id:?}"),
            Edit::RemoveRoute { route } => format!("cannot remove {}", route.named()),
            Edit::SetAccounts { name, .. } => {
                format!("cannot set the accounts of upstream {name:?}")
            }
        }
    }

    fn restart_only(&self) -> bool {
        matches!(
            self,
            Edit::Server {
                field: ServerField::Bind,
                ..
            }
        )
    }
}

/// What else a rename changed, per kind of reference: an upstream rename
/// counts the first six, a model rename the last three.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Cascade {
    pub(crate) routes: usize,
    /// `[[models]]` `upstream_model` keys.
    pub(crate) models: usize,
    pub(crate) default_provider: usize,
    pub(crate) route_prefixes: usize,
    pub(crate) codex_endpoint: usize,
    pub(crate) codex_routes: usize,
    /// `[[routes]]` `model`s.
    pub(crate) route_models: usize,
    /// `[models.router]` targets and judges.
    pub(crate) routers: usize,
    /// `[models.subagents]` targets and judges.
    pub(crate) subagents: usize,
}

/// What an applied edit did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Applied {
    /// `false` when the edit's value equals the file's: nothing was written.
    pub(crate) written: bool,
    /// The written edit takes a gateway restart (of the editable fields,
    /// `bind` alone); shunt hot-reloads every other.
    pub(crate) restart_only: bool,
    /// An upstream rename's other changes.
    pub(crate) cascade: Option<Cascade>,
}

/// Apply `edit` to the adopted config: planned on the file's bytes, verified,
/// then landed through [`write_checked`] (`shunt check` under the gateway's
/// env, the original left byte-identical on any refusal).
pub(crate) fn apply_edit(record: &GatewayRecord, edit: &Edit) -> Result<Applied> {
    let (original, text) = read_config_for_edit(record.config())?;
    let Some(planned) = plan(&text, edit)? else {
        return Ok(Applied {
            written: false,
            restart_only: false,
            cascade: None,
        });
    };
    if edit.restart_only() {
        refuse_overridden_bind(record, edit.op())?;
    }
    write_checked(record, &original, &planned.candidate)?;
    Ok(Applied {
        written: true,
        restart_only: edit.restart_only(),
        cascade: planned.cascade,
    })
}

/// A `bind` edit changes nothing while the gateway's spawn env sets
/// `SHUNT_SERVER__BIND`: read from the env file over the daemon's inherited
/// env, a daemon with no record naming it refusing.
fn refuse_overridden_bind(record: &GatewayRecord, op: String) -> Result<()> {
    let env = gateway_env(record)?;
    let inherited = match inherited_env() {
        Ok((pairs, _)) => pairs,
        Err(e) => {
            return Err(match e.downcast_ref::<StoreMoveRefusal>() {
                Some(StoreMoveRefusal::DaemonEnvUnrecorded) => {
                    EditRefusal::BindUnrecorded { op }.into()
                }
                _ => e,
            });
        }
    };
    match spawned_env_var(&env, BIND_ENV, inherited) {
        Some((name, _)) => Err(EditRefusal::BindOverridden {
            op,
            variable: name.to_string_lossy().into_owned(),
        }
        .into()),
        None => Ok(()),
    }
}

struct Planned {
    candidate: String,
    cascade: Option<Cascade>,
}

/// The candidate text for `edit` over `text`, verified; `None` when the
/// edit's value equals the file's.
fn plan(text: &str, edit: &Edit) -> Result<Option<Planned>> {
    plan_with(text, edit, |ctx, edit| ctx.change(edit))
}

/// [`plan`] over the candidate `build` makes, so a test drives the
/// production body with a builder that returns a wrong candidate.
fn plan_with(
    text: &str,
    edit: &Edit,
    build: impl FnOnce(&Ctx<'_>, &Edit) -> Result<Change, Unplanned>,
) -> Result<Option<Planned>> {
    let op = edit.op();
    let ctx = Ctx::new(text, op.clone())?;
    let change = build(&ctx, edit).map_err(|e| match e {
        Unplanned::Refused(refusal) => anyhow::Error::from(refusal),
        Unplanned::Bug(kind) => EditBug {
            op: op.clone(),
            kind,
        }
        .into(),
    })?;
    let original: toml::Table = toml::from_str(text)
        .map_err(|_| anyhow::anyhow!("the shunt config does not parse as TOML"))?;
    let Some(expected) = semantic::apply(&original, edit, ctx.forms(edit)) else {
        return Err(EditBug {
            op,
            kind: BugKind::ValueMismatch,
        }
        .into());
    };
    if expected == original {
        return Ok(None);
    }
    if let Edit::Server {
        field: ServerField::Bind,
        value: Some(_),
    } = edit
        && let Err(e) = resolve_bind(&change.candidate, None)
        && matches!(
            e.downcast_ref::<ConfigBindRefused>(),
            Some(refused) if refused.refusal == BindRefusal::ConfigReference
        )
    {
        return Err(EditRefusal::BindReference.into());
    }
    verify(edit, text, &change, &expected)?;
    Ok(Some(Planned {
        candidate: change.candidate,
        cascade: change.cascade,
    }))
}

/// The candidate re-parses, holds `expected`, and keeps every comment of
/// `text` with the owner the ownership rules give it in `text`, read in the
/// candidate the same way, but those in the change's owned ranges that the
/// edit may take; a single-key unset's comment glued beside its cut lines
/// may read as any owner's.
fn verify(edit: &Edit, text: &str, change: &Change, expected: &toml::Table) -> Result<(), EditBug> {
    let op = edit.op();
    let bug = |kind| EditBug {
        op: op.clone(),
        kind,
    };
    let parsed: toml::Table =
        toml::from_str(&change.candidate).map_err(|_| bug(BugKind::DoesNotParse))?;
    if parsed != *expected {
        return Err(bug(BugKind::ValueMismatch));
    }
    let (Ok(before), Ok(after)) = (
        Ctx::new(text, op.clone()),
        Ctx::new(&change.candidate, op.clone()),
    ) else {
        return Err(bug(BugKind::DoesNotParse));
    };
    if !same_comments(
        before.comments(&change.owned, Some(edit)),
        after.comments(&[], None),
    ) {
        return Err(bug(BugKind::CommentLost));
    }
    Ok(())
}

/// One comment as the guard compares it: its owner (`None`: any owner),
/// whether it fills its line, and its text.
struct Comment {
    owner: Option<Owner>,
    full: bool,
    text: String,
}

/// Whether `after` holds exactly the comments of `before`, each with the
/// owner `before` gives it unless that is any.
fn same_comments(before: Vec<Comment>, mut after: Vec<Comment>) -> bool {
    let mut any = Vec::new();
    for comment in before {
        let Some(owner) = comment.owner else {
            any.push((comment.full, comment.text));
            continue;
        };
        let Some(i) = after.iter().position(|c| {
            c.owner.as_ref() == Some(&owner) && c.full == comment.full && c.text == comment.text
        }) else {
            return false;
        };
        after.swap_remove(i);
    }
    let mut rest: Vec<(bool, String)> = after.into_iter().map(|c| (c.full, c.text)).collect();
    rest.sort();
    any.sort();
    rest == any
}

/// Who a comment belongs to, as the guard compares it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Owner {
    /// No single entry: a comment between entries, or outside every list.
    Free,
    /// The list itself: a block above its first element or before its `]`,
    /// or a comment on its `[` line beside no element.
    List(ListId),
    Entry(ListId, Ident),
    /// The entry the edit removes.
    Removed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ListId {
    Upstreams,
    Models,
    Routes,
    /// The accounts of the upstream at this position.
    Accounts(usize),
}

/// The entry an edit removes or edits: its list, its position and the
/// bytes it spans (one range per run of an `[[x]]` entry's whole lines).
struct Subject {
    list: ListId,
    at: usize,
    span: Vec<Range<usize>>,
}

/// An entry by its position, or an account by its name, which a reorder
/// keeps.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Ident {
    Position(usize),
    Name(String),
}

/// `owner` as it reads once the entry at `removed` is gone: the entries
/// after it move up one.
fn shifted(owner: Owner, removed: Option<(ListId, usize)>) -> Owner {
    let Some((list, at)) = removed else {
        return owner;
    };
    let shift = |i: usize| match i.cmp(&at) {
        std::cmp::Ordering::Less => Some(i),
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Greater => Some(i - 1),
    };
    match owner {
        Owner::Entry(id, Ident::Position(i)) if id == list => {
            shift(i).map_or(Owner::Removed, |i| Owner::Entry(id, Ident::Position(i)))
        }
        Owner::Entry(ListId::Accounts(u), ident) if list == ListId::Upstreams => {
            shift(u).map_or(Owner::Removed, |u| Owner::Entry(ListId::Accounts(u), ident))
        }
        Owner::List(ListId::Accounts(u)) if list == ListId::Upstreams => {
            shift(u).map_or(Owner::Removed, |u| Owner::List(ListId::Accounts(u)))
        }
        other => other,
    }
}

// ── refusals ────────────────────────────────────────────────────────────────

/// Why an edit wrote nothing, on the user's input or the file's shape. `op`
/// is the edit's own `cannot …` phrase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EditRefusal {
    /// The config uses `[providers.*]`: upstream edits are a hand edit.
    Legacy { op: String },
    /// The gateway's env sets `variable`, which overrides `bind`.
    BindOverridden { op: String, variable: String },
    /// A daemon holds the singleton but recorded no env, so the override
    /// cannot be read.
    BindUnrecorded { op: String },
    /// The new `bind` is a `${…}` reference, which clauth's own bind reader
    /// refuses.
    BindReference,
    /// The route the edit names is no longer at its position as it was.
    EntryChanged { op: String },
    /// `what` names the entry the edit looked for.
    Unknown { op: String, what: String },
    /// More than one entry carries the name.
    Ambiguous { op: String, what: String },
    /// The name is taken (`what` says where).
    Duplicate { op: String, what: String },
    /// A reference already names the rename's target.
    ReferenceTaken {
        op: String,
        name: String,
        site: &'static str,
    },
    /// `key`, as the file spells it, has a shape the editor does not edit.
    Unhandled { op: String, key: String },
    /// An auth field other than the mode, set where no mode is.
    NoAuthMode { op: String, name: String },
    /// A names-only accounts edit over a list holding a table selection.
    TableAccounts { name: String },
    /// A comment in a gap the removal touches belongs to no single entry.
    UnownedComment { entry: String },
    /// A comment in a gap the reorder touches belongs to no single entry.
    UnownedCommentReorder { name: String },
}

impl std::fmt::Display for EditRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Legacy { op } => write!(
                f,
                "{op}: the config declares its providers as [providers.*], which clauth does not edit; convert them to [[upstreams]] by hand"
            ),
            Self::BindOverridden { op, variable } => write!(
                f,
                "{op}: the gateway's environment sets {variable:?}, which overrides the config's bind; change that variable instead"
            ),
            Self::BindUnrecorded { op } => write!(
                f,
                "{op}: the running daemon recorded no environment, so clauth cannot tell whether SHUNT_SERVER__BIND overrides the bind; restart the daemon, then save again"
            ),
            Self::BindReference => write!(
                f,
                "cannot set server.bind: {}",
                BindRefusal::ConfigReference
            ),
            Self::EntryChanged { op } => write!(
                f,
                "{op}: the config no longer holds that route there; reload the config and try again"
            ),
            Self::Unknown { op, what } => write!(f, "{op}: the config has no {what}"),
            Self::Ambiguous { op, what } => write!(
                f,
                "{op}: the config has more than one {what}; edit the config by hand"
            ),
            Self::Duplicate { op, what } => write!(f, "{op}: {what}"),
            Self::ReferenceTaken { op, name, site } => write!(
                f,
                "{op}: {site} already names {name:?}; change that reference first"
            ),
            Self::Unhandled { op, key } => write!(
                f,
                "{op}: the config spells {key:?} in a shape clauth does not edit; edit the config by hand"
            ),
            Self::NoAuthMode { op, name } => write!(
                f,
                "{op}: upstream {name:?} has no auth mode; set auth.mode first"
            ),
            Self::TableAccounts { name } => write!(
                f,
                "cannot set the accounts of upstream {name:?}: its accounts hold a table selection, whose settings a names-only edit would drop; edit the config by hand"
            ),
            Self::UnownedComment { entry } => write!(
                f,
                "cannot remove {entry}: a comment next to it belongs to no single entry; edit the config by hand"
            ),
            Self::UnownedCommentReorder { name } => write!(
                f,
                "cannot reorder the accounts of upstream {name:?}: a comment between them belongs to no single entry; edit the config by hand"
            ),
        }
    }
}

impl std::error::Error for EditRefusal {}

/// A candidate the editor built that fails its own verification: clauth's
/// bug, never the user's input. Nothing is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditBug {
    op: String,
    kind: BugKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BugKind {
    DoesNotParse,
    ValueMismatch,
    CommentLost,
    /// Two of its splices overlap, or one cuts outside the text or inside a
    /// character.
    Overlap,
}

impl std::fmt::Display for EditBug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let what = match self.kind {
            BugKind::DoesNotParse => "does not parse as TOML",
            BugKind::ValueMismatch => "changes the config's value beyond the edit",
            BugKind::CommentLost => "loses or changes a comment",
            BugKind::Overlap => "overlaps itself",
        };
        write!(
            f,
            "{}: clauth built an edit that {what}; nothing was written; report this as a clauth bug",
            self.op
        )
    }
}

impl std::error::Error for EditBug {}

/// Why the planner built no candidate: the input or the file's shape, or a
/// [`BugKind`] of its own, which [`plan_with`] names as an [`EditBug`].
#[derive(Debug)]
enum Unplanned {
    Refused(EditRefusal),
    Bug(BugKind),
}

impl From<EditRefusal> for Unplanned {
    fn from(refusal: EditRefusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<BugKind> for Unplanned {
    fn from(kind: BugKind) -> Self {
        Self::Bug(kind)
    }
}

// ── the source: lines and comments ──────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Blank,
    Comment,
    Code,
}

#[derive(Debug, Clone, Copy)]
struct Line {
    start: usize,
    /// The content's end, before `\r\n` or `\n`.
    eol: usize,
    /// The next line's start (the text's length on the last line).
    next: usize,
    kind: Kind,
    /// Where a comment starts on this line, outside any string.
    comment: Option<usize>,
}

struct Source<'a> {
    text: &'a str,
    /// Where the first line starts: past a byte-order mark, which no line,
    /// cut or splice holds.
    body: usize,
    lines: Vec<Line>,
}

impl<'a> Source<'a> {
    fn new(text: &'a str) -> Self {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Str {
            Out,
            Basic,
            Literal,
            MlBasic,
            MlLiteral,
        }
        let bytes = text.as_bytes();
        let at = |i: usize| bytes.get(i).copied();
        let mut lines = Vec::new();
        let body = if text.starts_with('\u{feff}') {
            '\u{feff}'.len_utf8()
        } else {
            0
        };
        let (mut start, mut code, mut comment) = (body, false, None);
        let mut state = Str::Out;
        let mut i = body;
        let push = |lines: &mut Vec<Line>,
                    start: usize,
                    end: usize,
                    next: usize,
                    code: bool,
                    comment: Option<usize>| {
            let eol = if end > start && bytes.get(end - 1) == Some(&b'\r') {
                end - 1
            } else {
                end
            };
            let kind = if code {
                Kind::Code
            } else if comment.is_some() {
                Kind::Comment
            } else {
                Kind::Blank
            };
            lines.push(Line {
                start,
                eol,
                next,
                kind,
                comment,
            });
        };
        while let Some(b) = at(i) {
            if b == b'\n' {
                push(&mut lines, start, i, i + 1, code, comment);
                start = i + 1;
                comment = None;
                code = matches!(state, Str::MlBasic | Str::MlLiteral);
                if matches!(state, Str::Basic | Str::Literal) {
                    state = Str::Out;
                }
                i += 1;
                continue;
            }
            let run = |quote: u8| bytes[i..].iter().take_while(|&&c| c == quote).count();
            match state {
                Str::Out => match b {
                    b'#' => {
                        comment = Some(i);
                        while at(i).is_some_and(|c| c != b'\n') {
                            i += 1;
                        }
                        continue;
                    }
                    b'"' | b'\'' => {
                        code = true;
                        let triple = run(b) >= 3;
                        state = match (b, triple) {
                            (b'"', true) => Str::MlBasic,
                            (b'"', false) => Str::Basic,
                            (_, true) => Str::MlLiteral,
                            (_, false) => Str::Literal,
                        };
                        i += if triple { 3 } else { 1 };
                        continue;
                    }
                    b' ' | b'\t' | b'\r' => {}
                    _ => code = true,
                },
                Str::Basic => match b {
                    b'\\' if !matches!(at(i + 1), Some(b'\n' | b'\r')) => i += 1,
                    b'"' => state = Str::Out,
                    _ => {}
                },
                Str::Literal => {
                    if b == b'\'' {
                        state = Str::Out;
                    }
                }
                Str::MlBasic => match b {
                    b'\\' if !matches!(at(i + 1), Some(b'\n' | b'\r')) => i += 1,
                    b'"' if run(b'"') >= 3 => {
                        i += run(b'"');
                        state = Str::Out;
                        continue;
                    }
                    _ => {}
                },
                Str::MlLiteral => {
                    if b == b'\'' && run(b'\'') >= 3 {
                        i += run(b'\'');
                        state = Str::Out;
                        continue;
                    }
                }
            }
            i += 1;
        }
        if start < bytes.len() {
            push(&mut lines, start, bytes.len(), bytes.len(), code, comment);
        }
        Self { text, body, lines }
    }

    fn line_of(&self, pos: usize) -> usize {
        self.lines
            .partition_point(|line| line.next <= pos)
            .min(self.lines.len().saturating_sub(1))
    }

    fn kind(&self, line: usize) -> Kind {
        self.lines.get(line).map_or(Kind::Code, |l| l.kind)
    }

    fn prev_code(&self, line: usize) -> Option<usize> {
        (0..line).rev().find(|&l| self.kind(l) == Kind::Code)
    }

    fn next_code(&self, line: usize) -> Option<usize> {
        (line + 1..self.lines.len()).find(|&l| self.kind(l) == Kind::Code)
    }

    fn indent(&self, line: usize) -> &'a str {
        let Some(l) = self.lines.get(line) else {
            return "";
        };
        let text = &self.text[l.start..l.eol];
        &text[..text.len() - text.trim_start().len()]
    }

    /// Lines `first..=last` whole. On a last line with no newline the cut
    /// takes the newline before `first` instead, so the file's final-newline
    /// state holds.
    fn cut_lines(&self, first: usize, last: usize) -> Range<usize> {
        let (Some(a), Some(b)) = (self.lines.get(first), self.lines.get(last)) else {
            return 0..0;
        };
        if b.next == b.eol && first > 0 {
            return self.lines[first - 1].eol..b.next;
        }
        a.start..b.next
    }

    /// The line ending a new line after `line` takes: that line's own, else
    /// (the last line, ending none) the one of the nearest line above it.
    fn newline_after(&self, line: usize) -> &'static str {
        self.lines
            .iter()
            .take(line.saturating_add(1))
            .rev()
            .find(|l| l.next > l.eol)
            .map_or("\n", |l| if l.next - l.eol == 2 { "\r\n" } else { "\n" })
    }

    /// Where a new line after `line` goes, and whether it must bring its own
    /// leading newline (the last line has none).
    fn after_line(&self, line: usize) -> (usize, bool) {
        match self.lines.get(line) {
            Some(l) if l.next > l.eol => (l.next, false),
            Some(l) => (l.next, true),
            None => (self.text.len(), false),
        }
    }
}

// ── the parsed config ───────────────────────────────────────────────────────

/// One text replacement on the original.
#[derive(Debug, Clone)]
struct Splice {
    at: Range<usize>,
    text: String,
}

/// An edit's outcome before verification: the candidate text and the
/// original's ranges whose comments go with it.
struct Change {
    candidate: String,
    owned: Vec<Range<usize>>,
    cascade: Option<Cascade>,
}

/// `text` with every splice applied; zero-width inserts may share an offset.
fn splice(text: &str, mut splices: Vec<Splice>) -> Result<String, BugKind> {
    splices.sort_by_key(|s| (s.at.start, s.at.end));
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for s in splices {
        let (Some(kept), Some(_)) = (text.get(at..s.at.start), text.get(s.at.clone())) else {
            return Err(BugKind::Overlap);
        };
        out.push_str(kept);
        out.push_str(&s.text);
        at = s.at.end;
    }
    out.push_str(text.get(at..).unwrap_or_default());
    Ok(out)
}

/// A table either way it is written: a header or dotted table, or inline.
#[derive(Clone, Copy)]
enum Tab<'d> {
    Std(&'d Table),
    /// An inline table and the braced one whose pairs spell its keys: itself,
    /// or for a dotted table (`{ auth.mode = … }`) the table holding the
    /// dotted keys.
    Inline(&'d InlineTable, &'d InlineTable),
}

impl<'d> Tab<'d> {
    /// The table `item` is, under a table that is not inline.
    fn of(item: &'d Item) -> Option<Self> {
        match item {
            Item::Table(t) => Some(Tab::Std(t)),
            Item::Value(Value::InlineTable(t)) => Some(Tab::Inline(t, t)),
            Item::None | Item::Value(_) | Item::ArrayOfTables(_) => None,
        }
    }

    /// The table `item`, one of this table's values, is.
    fn child(self, item: &'d Item) -> Option<Self> {
        match (self, item) {
            (Tab::Inline(_, outer), Item::Value(Value::InlineTable(t))) if t.is_dotted() => {
                Some(Tab::Inline(t, outer))
            }
            _ => Tab::of(item),
        }
    }

    fn like(self) -> &'d dyn TableLike {
        match self {
            Tab::Std(t) => t,
            Tab::Inline(t, _) => t,
        }
    }

    fn get(self, key: &str) -> Option<(&'d Key, &'d Item)> {
        self.like().get_key_value(key)
    }

    /// A table only its sub-tables define (`[server.admin]` alone makes
    /// `server` one): it has no header or line of its own to edit. A dotted
    /// table reads as implicit too, but has lines.
    fn headerless(self) -> bool {
        matches!(self, Tab::Std(t) if t.is_implicit() && !t.is_dotted())
    }

    fn str(self, key: &str) -> Option<&'d str> {
        self.get(key).and_then(|(_, item)| item.as_str())
    }

    fn keys(self) -> Vec<(&'d Key, &'d Item)> {
        self.like()
            .iter()
            .filter_map(|(k, _)| self.get(k))
            .collect()
    }
}

/// A list of tables: `[[x]]` entries or an inline array of inline tables.
struct List<'d> {
    path: String,
    form: ListForm<'d>,
    entries: Vec<Entry<'d>>,
}

enum ListForm<'d> {
    Absent,
    Aot,
    Inline(&'d Array),
}

#[derive(Clone, Copy)]
struct Entry<'d> {
    tab: Tab<'d>,
    start: usize,
    end: usize,
}

/// One `key = value` of a braced inline table as its text spells it: for a
/// dotted key (`auth.mode = …`) the leaf, with the whole path.
struct Pair<'d> {
    /// The path's keys, from the braced table's down to the leaf's.
    path: Vec<&'d Key>,
    /// The dotted tables the path passes through.
    via: Vec<&'d InlineTable>,
    item: &'d Item,
    /// Where the path starts.
    start: usize,
    /// Where the value ends.
    end: usize,
}

impl<'d> Pair<'d> {
    fn key(&self) -> Option<&'d Key> {
        self.path.last().copied()
    }

    /// Whether this pair spells a key of `table`, an inline table whose
    /// pairs `outer` holds.
    fn within(&self, table: &InlineTable, outer: &InlineTable) -> bool {
        std::ptr::eq(table, outer) || self.via.iter().any(|v| std::ptr::eq(*v, table))
    }

    /// Whether this pair spells `item` or a key under it.
    fn under(&self, item: &Item) -> bool {
        std::ptr::eq(self.item, item)
            || matches!(item, Item::Value(Value::InlineTable(t))
                if t.is_dotted() && self.via.iter().any(|v| std::ptr::eq(*v, t)))
    }
}

/// An inline array's layout.
struct Elems {
    open: usize,
    close: usize,
    open_line: usize,
    close_line: usize,
    /// The `[` line holds no element, so it sets off a block below it.
    open_alone: bool,
    trailing_comma: bool,
    elems: Vec<Elem>,
}

#[derive(Debug, Clone, Copy)]
struct Elem {
    start: usize,
    end: usize,
    first: usize,
    last: usize,
    /// Nothing before it on its first line but indentation and, in a
    /// comma-first list, the comma after the element before it, so the block
    /// glued above that line is its own.
    leads: bool,
    /// Nothing else on its first and last lines but a comma and a comment.
    exclusive: bool,
    /// The first element, opening on the list's `[` line, with nothing after
    /// it on its last line but a comma and a comment.
    heads: bool,
    /// Spans lines, with nothing after it on its last line but a comma and a
    /// comment, which is its own.
    tail: bool,
}

impl Elem {
    /// Whether the comment on `line` is this element's own: on a line only
    /// it holds.
    fn owns_line(&self, line: usize) -> bool {
        ((self.exclusive || self.heads) && (self.first..=self.last).contains(&line))
            || (self.tail && line == self.last)
    }
}

/// Why a remove or reorder cannot touch an element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Blocked {
    /// A comment in a gap it touches, or on a line it shares with another
    /// element, belongs to no single entry.
    Unowned,
    /// Its comment shares a line with the list's `]`, or it owns a block it
    /// cannot carry: a layout the editor does not cut or permute.
    Shape,
}

/// What a removed table is to its list.
#[derive(Debug, Clone, Copy)]
enum Removal<'r> {
    /// An `[[x]]` entry, with the line ranges of the list's other entries.
    Entry(&'r [Range<usize>]),
    /// A sub-table of an entry, with the header line of the list's next
    /// entry.
    Sub(Option<usize>),
}

struct Ctx<'a> {
    src: Source<'a>,
    doc: Document<&'a str>,
    /// Lines holding a table header.
    header_lines: HashSet<usize>,
    header_starts: Vec<usize>,
    /// See [`Ctx::tail_heads`].
    tail_heads: OnceCell<HashSet<usize>>,
    op: String,
}

impl<'a> Ctx<'a> {
    fn new(text: &'a str, op: String) -> Result<Self> {
        let doc = Document::parse(text).map_err(|e| config_parse_error(text, &e))?;
        let src = Source::new(text);
        let mut sections = Vec::new();
        collect_sections(doc.as_table(), &mut sections);
        let header_starts: Vec<usize> = sections.iter().map(|s| s.start).collect();
        let header_lines = header_starts.iter().map(|&h| src.line_of(h)).collect();
        Ok(Self {
            src,
            doc,
            header_lines,
            header_starts,
            tail_heads: OnceCell::new(),
            op,
        })
    }

    fn text(&self) -> &'a str {
        self.src.text
    }

    fn root(&self) -> Tab<'_> {
        Tab::Std(self.doc.as_table())
    }

    fn spell(&self, key: &Key) -> String {
        key.span()
            .and_then(|span| self.text().get(span))
            .map_or_else(|| key.get().to_string(), str::to_string)
    }

    /// `key` as the file spells it, through the first dotted key it opens
    /// when `item` is a dotted table (`server.bind`, `effort . x`).
    fn spell_item(&self, key: &Key, item: &Item) -> String {
        let mut leaves = Vec::new();
        dotted_leaves(item, &mut leaves);
        leaves
            .iter()
            .filter_map(|(leaf, _)| leaf.span())
            .min_by_key(|span| span.start)
            .zip(key.span())
            .and_then(|(leaf, head)| self.text().get(head.start..leaf.end))
            .map_or_else(|| self.spell(key), str::to_string)
    }

    fn shape(&self, key: impl Into<String>) -> EditRefusal {
        EditRefusal::Unhandled {
            op: self.op.clone(),
            key: key.into(),
        }
    }

    fn refuse_unknown(&self, what: String) -> EditRefusal {
        EditRefusal::Unknown {
            op: self.op.clone(),
            what,
        }
    }

    // ── reading ──

    fn written(&self, item: &Item) -> Option<Written> {
        let value = item.as_value()?;
        let raw = value
            .span()
            .and_then(|span| self.text().get(span))
            .map_or_else(|| value.to_string(), str::to_string);
        Some(Written {
            raw,
            value: value.as_str().map(str::to_string),
        })
    }

    /// A scalar field of `tab` at `path.key`.
    fn scalar(&self, tab: Tab<'_>, key: &str, path: &str) -> Result<Option<Written>, EditRefusal> {
        match tab.get(key) {
            None => Ok(None),
            Some((k, item)) => match item {
                Item::Value(Value::InlineTable(_)) | Item::Table(_) | Item::ArrayOfTables(_) => {
                    Err(self.shape(format!("{path}.{}", self.spell_item(k, item))))
                }
                Item::Value(_) | Item::None => Ok(self.written(item)),
            },
        }
    }

    /// A sub-table of `tab` at `key`: `None` absent, else the table and its
    /// spelled path.
    fn sub<'s>(
        &'s self,
        tab: Tab<'s>,
        key: &str,
        path: &str,
    ) -> Result<Option<(Tab<'s>, String)>, EditRefusal> {
        match tab.get(key) {
            None => Ok(None),
            Some((k, item)) => {
                let spelled = format!("{path}.{}", self.spell(k));
                match tab.child(item) {
                    Some(t) => Ok(Some((t, spelled))),
                    None => Err(self.shape(spelled)),
                }
            }
        }
    }

    fn list<'s>(
        &'s self,
        parent: Tab<'s>,
        key: &str,
        path: Option<&str>,
    ) -> Result<List<'s>, EditRefusal> {
        let Some((k, item)) = parent.get(key) else {
            return Ok(List {
                path: key.to_string(),
                form: ListForm::Absent,
                entries: Vec::new(),
            });
        };
        let under = |spelled: String| match path {
            Some(path) => format!("{path}.{spelled}"),
            None => spelled,
        };
        let path = under(self.spell(k));
        match item {
            Item::ArrayOfTables(aot) => {
                let entries = aot
                    .iter()
                    .map(|t| Entry {
                        tab: Tab::Std(t),
                        start: t.span().map_or(0, |s| s.start),
                        end: table_end(t),
                    })
                    .collect();
                Ok(List {
                    path,
                    form: ListForm::Aot,
                    entries,
                })
            }
            Item::Value(Value::Array(array)) => {
                let mut entries = Vec::new();
                for value in array.iter() {
                    let (Value::InlineTable(t), Some(span)) = (value, value.span()) else {
                        return Err(self.shape(path));
                    };
                    entries.push(Entry {
                        tab: Tab::Inline(t, t),
                        start: span.start,
                        end: span.end,
                    });
                }
                Ok(List {
                    path,
                    form: ListForm::Inline(array),
                    entries,
                })
            }
            Item::None | Item::Value(_) | Item::Table(_) => {
                Err(self.shape(under(self.spell_item(k, item))))
            }
        }
    }

    fn server<'s>(&'s self) -> Result<Option<(Tab<'s>, String)>, EditRefusal> {
        match self.doc.as_table().get_key_value("server") {
            None => Ok(None),
            Some((k, item)) => match Tab::of(item) {
                Some(t) => Ok(Some((t, self.spell(k)))),
                None => Err(self.shape(self.spell(k))),
            },
        }
    }

    fn legacy(&self) -> Result<Option<Vec<String>>, EditRefusal> {
        match self.doc.as_table().get_key_value("providers") {
            None => Ok(None),
            Some((k, item)) => match Tab::of(item) {
                Some(t) => Ok(Some(
                    t.keys().iter().map(|(k, _)| k.get().to_string()).collect(),
                )),
                None => Err(self.shape(self.spell(k))),
            },
        }
    }

    fn view(&self, bind_override: Option<String>) -> Result<ConfigView, EditRefusal> {
        let server = match self.server()? {
            None => ServerView {
                bind: None,
                bind_override,
                default_provider: None,
            },
            Some((tab, path)) => ServerView {
                bind: self.scalar(tab, "bind", &path)?,
                bind_override,
                default_provider: self.scalar(tab, "default_provider", &path)?,
            },
        };
        let form = match self.legacy()? {
            Some(providers) => ProviderForm::Legacy { providers },
            None => ProviderForm::Upstreams,
        };
        let upstreams = self.list(self.root(), "upstreams", None)?;
        let upstreams = upstreams
            .entries
            .iter()
            .map(|e| self.upstream_view(e.tab, &upstreams.path))
            .collect::<Result<_, _>>()?;
        let models = self.list(self.root(), "models", None)?;
        let models =
            models
                .entries
                .iter()
                .map(|e| {
                    Ok(ModelView {
                        id: self.scalar(e.tab, "id", &models.path)?,
                        display_name: self.scalar(e.tab, "display_name", &models.path)?,
                        upstream_model: match self.sub(e.tab, "upstream_model", &models.path)? {
                            None => None,
                            Some((map, path)) => Some(
                                map.keys()
                                    .into_iter()
                                    .map(|(k, item)| match self.written(item) {
                                        Some(w) if !item.is_inline_table() => {
                                            Ok((k.get().to_string(), w))
                                        }
                                        _ => Err(self
                                            .shape(format!("{path}.{}", self.spell_item(k, item)))),
                                    })
                                    .collect::<Result<_, _>>()?,
                            ),
                        },
                    })
                })
                .collect::<Result<_, EditRefusal>>()?;
        let routes = self.list(self.root(), "routes", None)?;
        let routes = routes
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| self.route_view(i, e.tab, &routes.path))
            .collect::<Result<_, _>>()?;
        Ok(ConfigView {
            server,
            form,
            upstreams,
            models,
            routes,
        })
    }

    fn upstream_view<'s>(&'s self, tab: Tab<'s>, path: &str) -> Result<UpstreamView, EditRefusal> {
        Ok(UpstreamView {
            name: self.scalar(tab, "name", path)?,
            provider: self.scalar(tab, "provider", path)?,
            kind: self.scalar(tab, "kind", path)?,
            base_url: self.scalar(tab, "base_url", path)?,
            auth: self.auth_view(tab, path)?,
            effort: self.scalar(tab, "effort", path)?,
            service_tier: self.scalar(tab, "service_tier", path)?,
        })
    }

    fn auth_view<'s>(&'s self, tab: Tab<'s>, path: &str) -> Result<Option<AuthView>, EditRefusal> {
        let auth = match self.auth(tab, path)? {
            Auth::Absent => return Ok(None),
            Auth::Shorthand(item) => {
                return Ok(Some(AuthView {
                    form: AuthForm::Shorthand,
                    mode: self.written(item),
                    account: None,
                    accounts: None,
                    env: None,
                    header: None,
                }));
            }
            Auth::Tab(auth, path, form) => (auth, path, form),
        };
        let (auth, path, form) = auth;
        let accounts = match auth.get("accounts") {
            None => None,
            Some((k, Item::ArrayOfTables(aot))) => Some(
                aot.iter()
                    .map(|t| {
                        Ok(AccountView::Selection {
                            name: self.scalar(
                                Tab::Std(t),
                                "name",
                                &format!("{path}.{}", self.spell(k)),
                            )?,
                            form: SelectionForm::Table,
                        })
                    })
                    .collect::<Result<_, EditRefusal>>()?,
            ),
            Some((k, Item::Value(Value::Array(array)))) => {
                let spelled = format!("{path}.{}", self.spell(k));
                Some(
                    array
                        .iter()
                        .map(|value| match value {
                            Value::String(_) => Ok(AccountView::Name(Written {
                                raw: value
                                    .span()
                                    .and_then(|s| self.text().get(s))
                                    .map_or_else(|| value.to_string(), str::to_string),
                                value: value.as_str().map(str::to_string),
                            })),
                            Value::InlineTable(t) => Ok(AccountView::Selection {
                                name: self.scalar(Tab::Inline(t, t), "name", &spelled)?,
                                form: SelectionForm::Inline,
                            }),
                            _ => Err(self.shape(spelled.clone())),
                        })
                        .collect::<Result<_, EditRefusal>>()?,
                )
            }
            Some((k, _)) => return Err(self.shape(format!("{path}.{}", self.spell(k)))),
        };
        Ok(Some(AuthView {
            form,
            mode: self.scalar(auth, "mode", &path)?,
            account: self.scalar(auth, "account", &path)?,
            accounts,
            env: self.scalar(auth, "env", &path)?,
            header: self.scalar(auth, "header", &path)?,
        }))
    }

    fn auth<'s>(&'s self, tab: Tab<'s>, path: &str) -> Result<Auth<'s>, EditRefusal> {
        let Some((k, item)) = tab.get("auth") else {
            return Ok(Auth::Absent);
        };
        let spelled = format!("{path}.{}", self.spell(k));
        let form = match item {
            Item::Value(Value::String(_)) => return Ok(Auth::Shorthand(item)),
            Item::Value(Value::InlineTable(t)) if t.is_dotted() => AuthForm::Dotted,
            Item::Value(Value::InlineTable(_)) => AuthForm::Inline,
            Item::Table(t) if t.is_dotted() => AuthForm::Dotted,
            Item::Table(t) if !t.is_implicit() => AuthForm::Table,
            Item::Table(_) | Item::Value(_) | Item::ArrayOfTables(_) | Item::None => {
                return Err(self.shape(spelled));
            }
        };
        match tab.child(item) {
            Some(auth) => Ok(Auth::Tab(auth, spelled, form)),
            None => Err(self.shape(spelled)),
        }
    }

    fn route_view<'s>(
        &'s self,
        index: usize,
        tab: Tab<'s>,
        path: &str,
    ) -> Result<RouteView, EditRefusal> {
        Ok(RouteView {
            position: index + 1,
            model: self.scalar(tab, "model", path)?,
            provider: self.scalar(tab, "provider", path)?,
            upstream_model: self.scalar(tab, "upstream_model", path)?,
            effort: self.scalar(tab, "effort", path)?,
            service_tier: self.scalar(tab, "service_tier", path)?,
        })
    }

    /// The entry of `list` whose `key` is `wanted`.
    fn find<'s>(
        &'s self,
        list: &List<'s>,
        key: &str,
        wanted: &str,
        what: String,
    ) -> Result<Entry<'s>, EditRefusal> {
        let mut found = list
            .entries
            .iter()
            .filter(|e| e.tab.str(key) == Some(wanted));
        match (found.next(), found.next()) {
            (Some(entry), None) => Ok(*entry),
            (None, _) => Err(self.refuse_unknown(what)),
            (Some(_), Some(_)) => Err(EditRefusal::Ambiguous {
                op: self.op.clone(),
                what,
            }),
        }
    }

    fn refuse_legacy(&self) -> Result<(), EditRefusal> {
        match self.legacy()? {
            Some(_) => Err(EditRefusal::Legacy {
                op: self.op.clone(),
            }),
            None => Ok(()),
        }
    }

    // ── layout ──

    /// The topmost line of the comment block glued above `first` when the
    /// block's top is set off, else `first`. A blank line or the file's
    /// start sets it off; above an entry (not a `sub`-table) so do `open`, a
    /// list's lone `[` line, and a header whose block below is no `[[x]]`
    /// entry's tail.
    fn owned_top(&self, first: usize, open: Option<usize>, sub: bool) -> usize {
        let mut top = first;
        while top > 0 && self.src.kind(top - 1) == Kind::Comment {
            top -= 1;
        }
        if top == first || top == 0 {
            return top;
        }
        let above = top - 1;
        let header = self.header_lines.contains(&above) && !self.tail_heads().contains(&above);
        if self.src.kind(above) == Kind::Blank || (!sub && (header || open == Some(above))) {
            top
        } else {
            first
        }
    }

    /// The last line of each run of every `[[x]]` entry of the edited lists
    /// that owns the block glued under it ([`Ctx::tail_end`]).
    fn tail_heads(&self) -> &HashSet<usize> {
        self.tail_heads.get_or_init(|| {
            let mut ends = HashSet::new();
            for key in ["upstreams", "models", "routes"] {
                let Ok(list) = self.list(self.root(), key, None) else {
                    continue;
                };
                let lines = self.entry_lines(&list);
                for entry in &list.entries {
                    if let (ListForm::Aot, Tab::Std(table)) = (&list.form, entry.tab) {
                        ends.extend(
                            self.table_runs(table)
                                .into_iter()
                                .filter(|&(first, last)| {
                                    self.tail_end(last, next_entry(&lines, first)) > last
                                })
                                .map(|(_, last)| last),
                        );
                    }
                }
            }
            ends
        })
    }

    /// The last line an `[[x]]` entry ending on line `last` owns: the
    /// comment block glued under it, unless the block is glued to `next`,
    /// the header line of its list's next entry, as well.
    fn tail_end(&self, last: usize, next: Option<usize>) -> usize {
        let mut end = last;
        while self.src.kind(end + 1) == Kind::Comment {
            end += 1;
        }
        if end > last && next == Some(end + 1) {
            last
        } else {
            end
        }
    }

    /// The line range of each entry of the `[[x]]` list `list`, its
    /// sub-tables included.
    fn entry_lines(&self, list: &List<'_>) -> Vec<Range<usize>> {
        list.entries
            .iter()
            .map(|e| self.src.line_of(e.start)..self.src.line_of(e.end.saturating_sub(1)) + 1)
            .collect()
    }

    /// Whether lines `from..to` hold a comment.
    fn comment_on(&self, from: usize, to: usize) -> bool {
        (from..to).any(|l| self.src.kind(l) == Kind::Comment)
    }

    /// The first line a cut of lines `top..=last` takes: the one separating
    /// blank line directly before it (above `floor`, a list's `[` line),
    /// unless the comment block above that blank would then glue onto the
    /// line after the cut and change owner.
    fn cut_from(&self, top: usize, last: usize, floor: Option<usize>) -> usize {
        let lowest = floor.map_or(0, |f| f + 1);
        let after = last + 1;
        let glues = top >= lowest + 2
            && self.src.kind(top - 2) == Kind::Comment
            && after < self.src.lines.len()
            && self.src.kind(after) != Kind::Blank;
        if top > lowest && self.src.kind(top - 1) == Kind::Blank && !glues {
            top - 1
        } else {
            top
        }
    }

    /// Whether removing the entry on lines `first..=last` (its owned block
    /// from `top`) touches a comment in a gap to a sibling entry
    /// (`siblings`, line ranges) that neither owns. A comment with no
    /// sibling across lies in no gap.
    fn table_gaps_unowned(
        &self,
        first: usize,
        top: usize,
        last: usize,
        siblings: &[Range<usize>],
    ) -> bool {
        let sibling = |line: &usize| siblings.iter().any(|r| r.contains(line));
        let above = self
            .src
            .prev_code(top)
            .filter(sibling)
            .is_some_and(|code| self.comment_on(self.tail_end(code, Some(first)) + 1, top));
        let below = self
            .src
            .next_code(last)
            .filter(sibling)
            .is_some_and(|code| self.comment_on(last + 1, self.owned_top(code, None, false)));
        above || below
    }

    /// The line runs `table`'s text takes, each `(first, last)`: its header's
    /// lines with its sub-tables', split where another table's header comes
    /// between (TOML binds `[x.sub]` to the most recent `[[x]]`, wherever
    /// it sits).
    fn table_runs(&self, table: &Table) -> Vec<(usize, usize)> {
        let mut sections = Vec::new();
        collect_sections(table, &mut sections);
        sections.sort_by_key(|s| s.start);
        let inner: Vec<usize> = sections.iter().map(|s| s.start).collect();
        let mut runs: Vec<Range<usize>> = Vec::new();
        for section in sections {
            match runs.last_mut() {
                Some(run)
                    if !self
                        .header_starts
                        .iter()
                        .any(|h| *h > run.start && *h < section.start && !inner.contains(h)) =>
                {
                    run.end = run.end.max(section.end);
                }
                _ => runs.push(section),
            }
        }
        runs.into_iter()
            .map(|r| {
                (
                    self.src.line_of(r.start),
                    self.src.line_of(r.end.saturating_sub(1)),
                )
            })
            .collect()
    }

    /// The cuts removing the whole table `own` (a `[[x]]` entry or a
    /// sub-table, its children included), one per run of its lines, each
    /// pushing onto `owned` the range it owns: its lines and the block glued
    /// above them, and for an entry the block it owns below them. Each cut
    /// takes or keeps the blank line before it so that no comment it glues
    /// onto a new neighbour changes owner, else the table's shape refuses.
    /// `entry` names it in the refusal; `removed` is the entry it removes.
    fn remove_table(
        &self,
        own: &Table,
        path: &str,
        entry: &str,
        removal: Removal<'_>,
        removed: Option<(ListId, usize)>,
        owned: &mut Vec<Range<usize>>,
    ) -> Result<Vec<Splice>, EditRefusal> {
        let runs = self.table_runs(own);
        if runs.is_empty() {
            return Err(self.shape(path));
        }
        let mut cuts = Vec::new();
        for (run, (first, last)) in runs.into_iter().enumerate() {
            let sub = run > 0 || matches!(removal, Removal::Sub(_));
            let top = self.owned_top(first, None, sub);
            let (last, unowned) = match removal {
                Removal::Entry(siblings) => {
                    let last = self.tail_end(last, next_entry(siblings, first));
                    (last, self.table_gaps_unowned(first, top, last, siblings))
                }
                // A block glued under a sub-table is its entry's and stays,
                // unless the next entry's header glues it as well.
                Removal::Sub(next) => (
                    last,
                    self.src.kind(last + 1) == Kind::Comment && self.tail_end(last, next) == last,
                ),
            };
            if unowned {
                return Err(EditRefusal::UnownedComment {
                    entry: entry.to_string(),
                });
            }
            cuts.push((top, self.cut_from(top, last, None), last));
            owned.push(self.src.lines[top].start..self.src.lines[last].eol);
        }
        let mut lines: Vec<(usize, usize)> =
            cuts.iter().map(|&(_, from, last)| (from, last)).collect();
        let mut keeps = self.joins_keep_owners(&lines, removed);
        for (i, &(top, from, _)) in cuts.iter().enumerate() {
            if keeps[i] {
                continue;
            }
            let other = if from < top {
                top
            } else if top > 0 && self.src.kind(top - 1) == Kind::Blank {
                top - 1
            } else {
                return Err(self.shape(path));
            };
            lines[i].0 = other;
            keeps = self.joins_keep_owners(&lines, removed);
            if !keeps[i] {
                return Err(self.shape(path));
            }
        }
        Ok(lines
            .into_iter()
            .map(|(first, last)| Splice {
                at: self.src.cut_lines(first, last),
                text: String::new(),
            })
            .collect())
    }

    /// For each cut of lines `(first, last)`, whether every comment it
    /// glues onto a new neighbour (the block right above `first`, the one
    /// right below `last`) keeps its owner in the text all the cuts leave;
    /// `removed` is the entry they remove. A candidate that does not parse
    /// is [`verify`]'s to refuse.
    fn joins_keep_owners(
        &self,
        cuts: &[(usize, usize)],
        removed: Option<(ListId, usize)>,
    ) -> Vec<bool> {
        let comment = |l: &usize| self.src.kind(*l) == Kind::Comment;
        let neighbours = |&(first, last): &(usize, usize)| {
            let above = (0..first).rev().take_while(comment);
            let below = (last + 1..self.src.lines.len()).take_while(comment);
            above.chain(below).filter_map(|l| self.src.lines[l].comment)
        };
        if cuts.iter().all(|cut| neighbours(cut).next().is_none()) {
            return vec![true; cuts.len()];
        }
        let ranges: Vec<Range<usize>> = cuts
            .iter()
            .map(|&(first, last)| self.src.cut_lines(first, last))
            .collect();
        let splices = ranges
            .iter()
            .map(|at| Splice {
                at: at.clone(),
                text: String::new(),
            })
            .collect();
        let Ok(text) = splice(self.text(), splices) else {
            return vec![true; cuts.len()];
        };
        let Ok(after) = Ctx::new(&text, self.op.clone()) else {
            return vec![true; cuts.len()];
        };
        let (before, now) = (self.owners(), after.owners());
        let moved = |at: usize| {
            at - ranges
                .iter()
                .filter(|r| r.end <= at)
                .map(|r| r.len())
                .sum::<usize>()
        };
        cuts.iter()
            .map(|cut| {
                neighbours(cut).all(|at| {
                    let owner = shifted(before.get(&at).cloned().unwrap_or(Owner::Free), removed);
                    owner == Owner::Removed
                        || now.get(&moved(at)).cloned().unwrap_or(Owner::Free) == owner
                })
            })
            .collect()
    }

    fn elems(&self, array: &Array) -> Elems {
        let span = array.span().unwrap_or(0..0);
        let open = span.start;
        let close = span.end.saturating_sub(1);
        let open_line = self.src.line_of(open);
        let text = self.text();
        let elems: Vec<Elem> = array
            .iter()
            .filter_map(|v| v.span())
            .enumerate()
            .map(|(i, s)| {
                let first = self.src.line_of(s.start);
                let last = self.src.line_of(s.end.saturating_sub(1));
                let before = &text[self.src.lines[first].start..s.start];
                let after = text[s.end..self.src.lines[last].eol].trim_start();
                let after = after.strip_prefix(',').unwrap_or(after).trim_start();
                let leads = matches!(before.trim(), "" | ",");
                let bare = after.is_empty() || after.starts_with('#');
                Elem {
                    start: s.start,
                    end: s.end,
                    first,
                    last,
                    leads,
                    exclusive: leads && bare,
                    heads: i == 0 && first == open_line && bare,
                    tail: last > first && bare,
                }
            })
            .collect();
        Elems {
            open,
            close,
            open_line,
            close_line: self.src.line_of(close),
            open_alone: !elems.iter().any(|e| e.first == open_line),
            trailing_comma: array.trailing_comma(),
            elems,
        }
    }

    /// The top of the block element `i` owns, else its first line.
    fn elem_top(&self, list: &Elems, i: usize) -> usize {
        let e = list.elems[i];
        if !e.leads {
            return e.first;
        }
        self.owned_top(e.first, list.open_alone.then_some(list.open_line), false)
    }

    /// The range element `i` owns: its value, the block it owns above it,
    /// and the comment on its last line when that line is its own (the `[`
    /// line included).
    fn elem_owned(&self, list: &Elems, i: usize) -> Range<usize> {
        let e = list.elems[i];
        let top = self.elem_top(list, i);
        let start = if top < e.first {
            self.src.lines[top].start
        } else {
            e.start
        };
        let end = if e.owns_line(e.last) {
            self.src.lines[e.last].eol
        } else {
            e.end
        };
        start..end
    }

    /// Whether the gap between elements `i - 1` and `i` holds a comment
    /// neither owns. The lines above the first element and below the last
    /// lie in no gap.
    fn gap_above_unowned(&self, list: &Elems, i: usize) -> bool {
        if i == 0 || i >= list.elems.len() {
            return false;
        }
        let prev = list.elems[i - 1].last;
        let e = list.elems[i];
        // A line here holding code holds only the comma between the two.
        prev < e.first
            && (prev + 1..self.elem_top(list, i)).any(|l| self.src.lines[l].comment.is_some())
    }

    /// Whether element `i` shares `line` with another element.
    fn shares_line(&self, list: &Elems, i: usize, line: usize) -> bool {
        (i > 0 && list.elems[i - 1].last == line)
            || list.elems.get(i + 1).is_some_and(|next| next.first == line)
    }

    /// The lines of element `i`'s first and last that hold a comment:
    /// inside the list, or after a `]` that closes a multi-line list there.
    fn elem_comments(&self, list: &Elems, i: usize) -> Vec<usize> {
        let e = list.elems[i];
        let mut lines = vec![e.first, e.last];
        lines.dedup();
        lines.retain(|&line| {
            self.src
                .lines
                .get(line)
                .and_then(|l| l.comment)
                .is_some_and(|at| {
                    (list.open..list.close).contains(&at)
                        || (at > list.close
                            && line == list.close_line
                            && list.open_line < list.close_line)
                })
        });
        lines
    }

    /// Why a remove or reorder cannot touch element `i`, if it cannot.
    fn blocked(&self, list: &Elems, i: usize) -> Option<Blocked> {
        if self.gap_above_unowned(list, i) || self.gap_above_unowned(list, i + 1) {
            return Some(Blocked::Unowned);
        }
        let comments = self.elem_comments(list, i);
        if comments.iter().any(|&line| self.shares_line(list, i, line)) {
            return Some(Blocked::Unowned);
        }
        if comments.iter().any(|&line| !list.elems[i].owns_line(line)) {
            return Some(Blocked::Shape);
        }
        None
    }

    /// The cuts removing element `i` of an inline array at `path`, and the
    /// range it owns. Of the commas on either side of it exactly one goes
    /// (both when it is the last and the second trails): a comma left on
    /// the next element's line (comma-first) or on a line of its own goes
    /// when the cut keeps the one before it too, and of two on its own
    /// lines (`, "b",`) the one after it stays.
    fn remove_elem(
        &self,
        list: &Elems,
        i: usize,
        entry: &str,
        path: &str,
    ) -> Result<(Vec<Range<usize>>, Range<usize>), EditRefusal> {
        match self.blocked(list, i) {
            Some(Blocked::Unowned) => {
                return Err(EditRefusal::UnownedComment {
                    entry: entry.to_string(),
                });
            }
            Some(Blocked::Shape) => return Err(self.shape(path)),
            None => {}
        }
        let e = list.elems[i];
        let top = self.elem_top(list, i);
        let floor = Some(list.open_line);
        let cut = match list.elems.get(i + 1) {
            _ if e.exclusive => self
                .src
                .cut_lines(self.cut_from(top, e.last, floor), e.last),
            Some(next) if next.first == e.last => {
                if top < e.first {
                    return Err(self.shape(path));
                }
                e.start..next.start
            }
            _ if i > 0 && list.elems[i - 1].last == e.first => list.elems[i - 1].end..e.end,
            _ => match (e.first == list.open_line, e.last == list.close_line) {
                (true, true) => list.open + 1..list.close,
                (false, true) => {
                    self.src.lines[self.cut_from(top, e.last, floor)].start..list.close
                }
                (true, false) => list.open + 1..self.src.lines[e.last].eol,
                (false, false) => return Err(self.shape(path)),
            },
        };
        let before = i.checked_sub(1).and_then(|p| self.comma_after(list, p));
        let after = self.comma_after(list, i);
        let held = |comma: Option<usize>| comma.is_some_and(|at| cut.contains(&at));
        let mut cuts = match (before, after) {
            // Its own lines hold a comma on either side (`, "b",`): the one
            // after it stays, on its line.
            (Some(b), Some(a)) if held(before) && held(after) && i + 1 < list.elems.len() => {
                let block = cut.start..self.src.lines[e.first].start;
                if !e.exclusive {
                    return Err(self.shape(path));
                }
                let tail = a + 1..self.src.lines[e.last].eol;
                vec![block, b..a, tail]
            }
            (_, Some(a)) if !held(before) && !held(after) => vec![cut, self.comma_cut(a)],
            _ => vec![cut],
        };
        // Sharing its first line, it takes its own last line's comment apart
        // from the cut, which keeps the comma after it.
        if e.tail
            && !e.exclusive
            && !e.heads
            && let Some(at) = self.src.lines[e.last].comment
        {
            let spaces = self.text()[..at].trim_end_matches([' ', '\t']).len();
            cuts.push(spaces..self.src.lines[e.last].eol);
        }
        Ok((cuts, self.elem_owned(list, i)))
    }

    /// Where the comma after element `i` sits, if one follows it.
    fn comma_after(&self, list: &Elems, i: usize) -> Option<usize> {
        let bytes = self.text().as_bytes();
        let mut at = list.elems[i].end;
        while at < list.close {
            match bytes.get(at)? {
                b',' => return Some(at),
                b'#' => at = self.src.lines[self.src.line_of(at)].eol,
                b' ' | b'\t' | b'\r' | b'\n' => at += 1,
                _ => return None,
            }
        }
        None
    }

    /// The cut taking the comma at `at`: its whole line when nothing else is
    /// on it, else the comma and the spaces after it.
    fn comma_cut(&self, at: usize) -> Range<usize> {
        let line = self.src.line_of(at);
        let l = self.src.lines[line];
        let text = self.text();
        let rest = &text[at + 1..l.eol];
        if text[l.start..at].trim().is_empty() && rest.trim().is_empty() {
            return self.src.cut_lines(line, line);
        }
        at..at + 1 + rest.len() - rest.trim_start().len()
    }

    /// The splices appending `value` to an inline array.
    fn append_elem(&self, list: &Elems, value: &str) -> Vec<Splice> {
        let commented = |line: usize| {
            self.src.lines[line]
                .comment
                .is_some_and(|at| (list.open..list.close).contains(&at))
        };
        // An element's own line under the `[`: the first full-line comment's
        // indent, else the `]` line's plus two spaces.
        let fresh_indent = || {
            (list.open_line + 1..list.close_line)
                .find(|&l| self.src.kind(l) == Kind::Comment)
                .map_or_else(
                    || format!("{}  ", self.src.indent(list.close_line)),
                    |l| self.src.indent(l).to_string(),
                )
        };
        let Some(last) = list.elems.last().copied() else {
            if !(list.open_line..=list.close_line).any(commented) {
                return vec![Splice {
                    at: list.open + 1..list.close,
                    text: value.to_string(),
                }];
            }
            // Under the `[` line, so every comment stays below it, the list's.
            let at = self.src.lines[list.open_line].next;
            return vec![Splice {
                at: at..at,
                text: format!(
                    "{}{value},{}",
                    fresh_indent(),
                    self.src.newline_after(list.open_line)
                ),
            }];
        };
        let lead = &self.text()[self.src.lines[last.first].start..last.start];
        let trailing = self.comma_after(list, list.elems.len() - 1);
        if last.exclusive && lead.trim() == "," {
            // Comma-first: the new element leads with the last one's comma,
            // and a trailing comma on that line moves to the new last.
            let at = self.src.lines[last.last].next;
            let nl = self.src.newline_after(last.last);
            match trailing {
                None => {
                    return vec![Splice {
                        at: at..at,
                        text: format!("{lead}{value}{nl}"),
                    }];
                }
                Some(comma) if self.src.line_of(comma) == last.last => {
                    return vec![
                        Splice {
                            at: comma..comma + 1,
                            text: String::new(),
                        },
                        Splice {
                            at: at..at,
                            text: format!("{lead}{value},{nl}"),
                        },
                    ];
                }
                Some(_) => {}
            }
        }
        // Never onto a `[` line holding a comment, which would then sit
        // beside one more element.
        let beside_open = last.first == list.open_line && commented(last.last);
        if last.exclusive || beside_open || (last.tail && commented(last.last)) {
            let indent = if last.exclusive {
                self.src.indent(last.first).to_string()
            } else {
                fresh_indent()
            };
            let mut out = Vec::new();
            if !list.trailing_comma {
                out.push(Splice {
                    at: last.end..last.end,
                    text: ",".to_string(),
                });
            }
            // Below the trailing comma when it sits on a later line.
            let below = trailing.map_or(last.last, |at| self.src.line_of(at).max(last.last));
            let at = self.src.lines[below].next;
            out.push(Splice {
                at: at..at,
                text: format!(
                    "{indent}{value}{}{}",
                    if list.trailing_comma { "," } else { "" },
                    self.src.newline_after(below)
                ),
            });
            return out;
        }
        let n = list.elems.len();
        let sep = match n {
            0 | 1 => ", ",
            _ => &self.text()[list.elems[n - 2].end..last.start],
        };
        let sep = match sep {
            // Its own line, closed by the `]`: so is the new one's.
            _ if (n < 2 || sep.contains('#')) && last.leads && last.last == list.close_line => {
                format!(
                    ",{}{}",
                    self.src.newline_after(last.last),
                    self.src.indent(last.first)
                )
            }
            _ if sep.contains('#') => ", ".to_string(),
            _ => sep.to_string(),
        };
        vec![Splice {
            at: last.end..last.end,
            text: format!("{sep}{value}"),
        }]
    }

    /// The splices putting element `order[j]` in slot `j` of the inline
    /// array at `path`: a value moves with its owned block and its same-line
    /// comment, and with the blank lines that set its block off unless its
    /// new slot is set off already; commas, indentation and every other blank
    /// line keep their places. A moved block whose blank lines stay behind
    /// gains one in a slot not set off; one moving into the slot of the
    /// element on the `[` line goes on its own lines under the `[`.
    fn permute(
        &self,
        list: &Elems,
        order: &[usize],
        name: &str,
        path: &str,
    ) -> Result<Vec<Splice>, EditRefusal> {
        struct Piece {
            /// The blank lines directly above an owned block.
            blank: Range<usize>,
            block: Range<usize>,
            value: Range<usize>,
            tail: Range<usize>,
        }
        let refuse = |blocked| match blocked {
            Blocked::Unowned => EditRefusal::UnownedCommentReorder {
                name: name.to_string(),
            },
            Blocked::Shape => self.shape(path),
        };
        let text = self.text();
        let pieces: Vec<Piece> = (0..list.elems.len())
            .map(|i| {
                let e = list.elems[i];
                if !e.exclusive && !e.heads && !e.tail {
                    return Piece {
                        blank: e.start..e.start,
                        block: e.start..e.start,
                        value: e.start..e.end,
                        tail: e.end..e.end,
                    };
                }
                let top = self.elem_top(list, i);
                let mut run = top;
                if top < e.first {
                    while run > list.open_line + 1 && self.src.kind(run - 1) == Kind::Blank {
                        run -= 1;
                    }
                    // Blank lines that also set off a comment above them
                    // stay, so that comment glues onto no element.
                    if run > list.open_line + 1 && self.src.kind(run - 1) == Kind::Comment {
                        run = top;
                    }
                }
                let eol = self.src.lines[e.last].eol;
                let after = &text[e.end..eol];
                let ws = after.len() - after.trim_start().len();
                let tail = match after[ws..].strip_prefix(',') {
                    Some(_) => e.end + ws + 1..eol,
                    None => e.end..eol,
                };
                // Only an element leading its line owns a block above it.
                let (blank, block) = if !e.exclusive {
                    (e.start..e.start, e.start..e.start)
                } else {
                    (
                        self.src.lines[run].start..self.src.lines[top].start,
                        self.src.lines[top].start..self.src.lines[e.first].start,
                    )
                };
                Piece {
                    blank,
                    block,
                    value: e.start..e.end,
                    tail,
                }
            })
            .collect();
        // A slot is set off by the list's `[`, alone or once the element on
        // its line leaves it, or by a blank line above it that its own
        // element does not carry away.
        let set_off = |slot: usize, carry: &[bool]| {
            (slot == 0 && (list.open_alone || list.elems[0].heads))
                || (!carry[slot]
                    && self
                        .elem_top(list, slot)
                        .checked_sub(1)
                        .is_some_and(|line| self.src.kind(line) == Kind::Blank))
        };
        let mut carry = vec![false; pieces.len()];
        loop {
            let mut more = false;
            for (slot, &from) in order.iter().enumerate() {
                if slot != from
                    && !carry[from]
                    && !pieces[from].blank.is_empty()
                    && !set_off(slot, &carry)
                {
                    carry[from] = true;
                    more = true;
                }
            }
            if !more {
                break;
            }
        }
        let moved = |i: usize| {
            let piece = &pieces[i];
            if carry[i] {
                piece.blank.start..piece.block.end
            } else {
                piece.block.clone()
            }
        };
        let mut out = Vec::new();
        for (slot, &from) in order.iter().enumerate() {
            if slot == from {
                continue;
            }
            for i in [from, slot] {
                if let Some(blocked) = self.blocked(list, i) {
                    return Err(refuse(blocked));
                }
                if !list.elems[i].exclusive && self.elem_top(list, i) < list.elems[i].first {
                    return Err(refuse(Blocked::Shape));
                }
            }
            let (piece, at) = (&pieces[from], &pieces[slot]);
            let owns_block = !piece.block.is_empty();
            let carries = owns_block || !text[piece.tail.clone()].trim().is_empty();
            if carries && !list.elems[slot].exclusive && !list.elems[slot].heads {
                return Err(refuse(Blocked::Shape));
            }
            out.push(if owns_block && list.elems[slot].heads {
                Splice {
                    at: list.open + 1..list.elems[slot].start,
                    text: format!(
                        "{}{}{}",
                        self.src.newline_after(list.open_line),
                        &text[moved(from)],
                        self.src.indent(list.elems[from].first)
                    ),
                }
            } else {
                // A block whose blank lines stay behind gains one of its own.
                let pad = owns_block && !carry[from] && !set_off(slot, &carry);
                let above = self.src.line_of(moved(slot).start).saturating_sub(1);
                Splice {
                    at: moved(slot),
                    text: format!(
                        "{}{}",
                        if pad {
                            self.src.newline_after(above)
                        } else {
                            ""
                        },
                        &text[moved(from)]
                    ),
                }
            });
            out.push(Splice {
                at: at.value.clone(),
                text: text[piece.value.clone()].to_string(),
            });
            out.push(Splice {
                at: at.tail.clone(),
                text: text[piece.tail.clone()].to_string(),
            });
        }
        Ok(out)
    }

    // ── the guard's reading ──

    /// Every comment with its owner, sorted, but those inside `owned` that
    /// `edit` may take ([`Ctx::may_take`]): a full-line comment as its whole
    /// line, a same-line one as its own text. The owners read as they stand
    /// once the entry `edit` removes is gone.
    fn comments(&self, owned: &[Range<usize>], edit: Option<&Edit>) -> Vec<Comment> {
        let owners = self.owners();
        let removed = edit.and_then(|edit| self.removed(edit));
        let subject = edit.and_then(|edit| self.subject(edit));
        let beside = match edit {
            Some(edit) if self.unsets_one_key(edit) => self.beside(owned),
            _ => HashSet::new(),
        };
        self.src
            .lines
            .iter()
            .filter_map(|line| {
                let at = line.comment?;
                let owner = shifted(owners.get(&at).cloned().unwrap_or(Owner::Free), removed);
                if let Some(edit) = edit
                    && owned.iter().any(|r| r.contains(&at))
                    && Self::may_take(edit, &owner, at, subject.as_ref())
                {
                    return None;
                }
                Some(Comment {
                    owner: (!beside.contains(&at)).then_some(owner),
                    full: line.kind == Kind::Comment,
                    text: match line.kind {
                        Kind::Comment => self.text()[line.start..line.eol].to_string(),
                        Kind::Code | Kind::Blank => self.text()[at..line.eol].to_string(),
                    },
                })
            })
            .collect()
    }

    /// Whether `edit` unsets one key (a field, a whole `auth = …`, a slug,
    /// the emptied `accounts`), never a sub-table or an entry: the one edit
    /// a comment beside the cut may change owner under.
    fn unsets_one_key(&self, edit: &Edit) -> bool {
        match edit {
            Edit::Server { value, .. } | Edit::Route { value, .. } => value.is_none(),
            Edit::SetAccounts { accounts, .. } => accounts.is_empty(),
            Edit::Upstream { name, field, value } => {
                value.is_none()
                    && !(*field == UpstreamField::AuthMode
                        && self.auth_form(name) == Some(AuthForm::Table))
            }
            Edit::Model { id, field, value } => {
                let slug_table = |provider: &str| {
                    self.list(self.root(), "models", None).is_ok_and(|list| {
                        self.find(&list, "id", id, String::new())
                            .and_then(|entry| self.sub(entry.tab, "upstream_model", &list.path))
                            .is_ok_and(|map| {
                                matches!(map, Some((map @ Tab::Std(table), _))
                                    if !table.is_dotted() && holds_only(map, provider))
                            })
                    })
                };
                value.is_none()
                    && !matches!(field, ModelField::UpstreamModel(provider) if slug_table(provider))
            }
            Edit::RenameUpstream { .. }
            | Edit::RenameModel { .. }
            | Edit::AddUpstream(_)
            | Edit::AddModel(_)
            | Edit::AddRoute(_)
            | Edit::RemoveUpstream { .. }
            | Edit::RemoveModel { .. }
            | Edit::RemoveRoute { .. } => false,
        }
    }

    /// Where every comment starts that is glued directly above or below the
    /// whole lines a range of `owned` spans.
    fn beside(&self, owned: &[Range<usize>]) -> HashSet<usize> {
        let comment = |l: &usize| self.src.kind(*l) == Kind::Comment;
        owned
            .iter()
            .filter_map(|r| {
                let (first, last) = (self.src.line_of(r.start), self.src.line_of(r.end));
                (self.src.lines.get(first)?.start == r.start
                    && self.src.lines.get(last)?.eol == r.end)
                    .then_some((first, last))
            })
            .flat_map(|(first, last)| {
                let above = (0..first).rev().take_while(comment);
                let below = (last + 1..self.src.lines.len()).take_while(comment);
                above.chain(below).collect::<Vec<_>>()
            })
            .filter_map(|l| self.src.lines[l].comment)
            .collect()
    }

    /// The list and position of the entry `edit` removes.
    fn removed(&self, edit: &Edit) -> Option<(ListId, usize)> {
        match edit {
            Edit::RemoveUpstream { .. } | Edit::RemoveModel { .. } | Edit::RemoveRoute { .. } => {
                self.subject(edit).map(|subject| (subject.list, subject.at))
            }
            _ => None,
        }
    }

    /// The entry `edit` removes or edits.
    fn subject(&self, edit: &Edit) -> Option<Subject> {
        let (id, key, by) = match edit {
            Edit::RemoveUpstream { name }
            | Edit::Upstream { name, .. }
            | Edit::SetAccounts { name, .. } => {
                (ListId::Upstreams, "upstreams", Err(("name", name)))
            }
            Edit::RemoveModel { id } | Edit::Model { id, .. } => {
                (ListId::Models, "models", Err(("id", id)))
            }
            Edit::RemoveRoute { route } | Edit::Route { route, .. } => {
                (ListId::Routes, "routes", Ok(route.position.checked_sub(1)?))
            }
            _ => return None,
        };
        let list = self.list(self.root(), key, None).ok()?;
        let at = match by {
            Ok(at) => at,
            Err((field, wanted)) => list
                .entries
                .iter()
                .position(|e| e.tab.str(field) == Some(wanted.as_str()))?,
        };
        let entry = list.entries.get(at)?;
        let span = match entry.tab {
            Tab::Std(table) => self
                .table_runs(table)
                .into_iter()
                .map(|(first, last)| self.src.lines[first].start..self.src.lines[last].eol)
                .collect(),
            Tab::Inline(..) => std::iter::once(entry.start..entry.end).collect(),
        };
        Some(Subject { list: id, at, span })
    }

    /// Whether `edit` may take a comment starting at `at` that the owner map
    /// gives `owner` (as read once a removed entry is gone): a removal the
    /// removed entry's or a free one inside it (an unowned comment of its own
    /// `accounts`); an edit of one entry (`subject`) that entry's, its
    /// accounts', or a free one inside it; any other edit a free one.
    fn may_take(edit: &Edit, owner: &Owner, at: usize, subject: Option<&Subject>) -> bool {
        let inside = subject.is_some_and(|s| s.span.iter().any(|r| r.contains(&at)));
        if matches!(
            edit,
            Edit::RemoveUpstream { .. } | Edit::RemoveModel { .. } | Edit::RemoveRoute { .. }
        ) {
            return *owner == Owner::Removed || (*owner == Owner::Free && inside);
        }
        let Some(subject) = subject else {
            return *owner == Owner::Free;
        };
        match owner {
            Owner::Entry(ListId::Accounts(u), _) | Owner::List(ListId::Accounts(u)) => {
                subject.list == ListId::Upstreams && *u == subject.at
            }
            Owner::Entry(list, Ident::Position(i)) => *list == subject.list && *i == subject.at,
            Owner::Free => inside,
            Owner::List(_) | Owner::Entry(..) | Owner::Removed => false,
        }
    }

    /// The owner of each comment of the lists an edit removes from or
    /// reorders, by where the comment starts; any other comment is
    /// [`Owner::Free`].
    fn owners(&self) -> HashMap<usize, Owner> {
        let mut out = HashMap::new();
        for (id, key) in [
            (ListId::Upstreams, "upstreams"),
            (ListId::Models, "models"),
            (ListId::Routes, "routes"),
        ] {
            let Ok(list) = self.list(self.root(), key, None) else {
                continue;
            };
            match list.form {
                ListForm::Absent => {}
                ListForm::Aot => {
                    let lines = self.entry_lines(&list);
                    for (i, entry) in list.entries.iter().enumerate() {
                        let Tab::Std(table) = entry.tab else {
                            continue;
                        };
                        for (run, (first, last)) in self.table_runs(table).into_iter().enumerate() {
                            let last = self.tail_end(last, next_entry(&lines, first));
                            for line in self.owned_top(first, None, run > 0)..=last {
                                if let Some(at) = self.src.lines[line].comment {
                                    out.insert(at, Owner::Entry(id, Ident::Position(i)));
                                }
                            }
                        }
                    }
                }
                ListForm::Inline(array) => self.elem_owners(array, id, &mut out),
            }
            if id == ListId::Upstreams {
                for (u, entry) in list.entries.iter().enumerate() {
                    if let Ok(Auth::Tab(auth, _, _)) = self.auth(entry.tab, &list.path)
                        && let Some((_, Item::Value(Value::Array(array)))) = auth.get("accounts")
                    {
                        self.elem_owners(array, ListId::Accounts(u), &mut out);
                    }
                }
            }
        }
        out
    }

    /// [`Ctx::owners`] for the comments inside an inline array: an element
    /// owns its block and, alone on its line (the `[` line included), that
    /// line's comment; the list owns a comment on a `[` line holding no
    /// element and the blocks above its first element and below its last;
    /// any other comment is free. A comment after `]` is
    /// its line's, so an edit writing or dropping the list keeps its owner.
    fn elem_owners(&self, array: &Array, id: ListId, out: &mut HashMap<usize, Owner>) {
        let list = self.elems(array);
        let ident = |i: usize| match array.get(i).and_then(Value::as_str) {
            Some(name) => Ident::Name(name.to_string()),
            None => Ident::Position(i),
        };
        for line in list.open_line..=list.close_line {
            let Some(at) = self.src.lines[line].comment.filter(|&at| at < list.close) else {
                continue;
            };
            let exclusive = list.elems.iter().position(|e| e.owns_line(line));
            let block = (0..list.elems.len()).find(|&i| {
                let e = list.elems[i];
                e.leads && (self.elem_top(&list, i)..e.first).contains(&line)
            });
            let outside = list.elems.first().is_none_or(|e| line < e.first)
                || list.elems.last().is_some_and(|e| line > e.last);
            let mut beside = (0..list.elems.len()).filter(|&i| list.elems[i].first == line);
            let owner = match (exclusive.or(block), outside) {
                _ if line == list.open_line => match (beside.next(), beside.next()) {
                    (None, _) => Owner::List(id),
                    (Some(i), None) => Owner::Entry(id, ident(i)),
                    (Some(_), Some(_)) => Owner::Free,
                },
                (Some(i), _) => Owner::Entry(id, ident(i)),
                (None, true) => Owner::List(id),
                (None, false) => Owner::Free,
            };
            out.insert(at, owner);
        }
    }

    // ── key edits ──

    /// [`Ctx::set_value`] for a value inside an array or inline table.
    fn set_str(&self, item: &Value, value: &str) -> Splice {
        let span = item.span().unwrap_or(0..0);
        let raw = &self.text()[span.clone()];
        Splice {
            at: span,
            text: encode_like(raw, value),
        }
    }

    /// Replace `item`'s value with the string `value`, keeping its quote
    /// style when `value` allows it.
    fn set_value(&self, item: &Item, value: &str) -> Splice {
        let span = item.span().unwrap_or(0..0);
        let raw = &self.text()[span.clone()];
        Splice {
            at: span,
            text: encode_like(raw, value),
        }
    }

    /// The splice adding `key = <value text>` to `tab`, a header, dotted or
    /// inline table.
    fn add_key<'s>(
        &'s self,
        tab: Tab<'s>,
        key: &str,
        value: &str,
        path: &str,
    ) -> Result<Splice, EditRefusal> {
        let key = key_repr(key);
        match tab {
            Tab::Inline(t, outer) => {
                let pairs = self.pairs(outer);
                let members: Vec<&Pair<'_>> = pairs.iter().filter(|p| p.within(t, outer)).collect();
                let Some(last) = members.last() else {
                    let span = outer.span().unwrap_or(0..0);
                    return Ok(Splice {
                        at: span.start + 1..span.end.saturating_sub(1),
                        text: format!(" {key} = {value} "),
                    });
                };
                // A new key goes after the table's last pair, so a dotted
                // table's pairs stay together, spelled as they are.
                let sep = match pairs.iter().rposition(|p| p.end < last.end) {
                    Some(prev) => self.text()[pairs[prev].end..last.start].to_string(),
                    None => ", ".to_string(),
                };
                let prefix = self.dotted_prefix(t, &members);
                let eq = last
                    .key()
                    .map_or_else(|| " = ".to_string(), |k| self.eq_spelling(k, last.item));
                Ok(Splice {
                    at: last.end..last.end,
                    text: format!("{sep}{prefix}{key}{eq}{value}"),
                })
            }
            Tab::Std(t) if t.is_dotted() => {
                // After the table's last dotted line, its path spelled as the
                // line its key (or its dotted sub-table's) starts on spells it.
                let last = t
                    .iter()
                    .filter_map(|(k, _)| t.get_key_value(k))
                    .filter(|(_, item)| {
                        item.is_value() || matches!(item, Item::Table(sub) if sub.is_dotted())
                    })
                    .max_by_key(|(_, item)| own_end(item));
                let Some((last_key, item)) = last else {
                    return Err(self.shape(path));
                };
                let key_start = last_key.span().map_or(0, |s| s.start);
                let line = self.src.line_of(key_start);
                let prefix = &self.text()[self.src.lines[line].start..key_start];
                let eq = if item.is_value() {
                    self.eq_spelling(last_key, item)
                } else {
                    " = ".to_string()
                };
                let end = self.src.line_of(own_end(item).saturating_sub(1));
                self.key_line_after(end, &format!("{prefix}{key}{eq}{value}"), path)
            }
            Tab::Std(t) => {
                if t.is_implicit() || t.span().is_none() {
                    return Err(self.shape(path));
                }
                let own: Vec<(&Key, &Item)> = t
                    .iter()
                    .filter_map(|(k, _)| t.get_key_value(k))
                    .filter(|(_, item)| match item {
                        Item::Value(_) => true,
                        Item::Table(sub) => sub.is_dotted(),
                        Item::ArrayOfTables(_) | Item::None => false,
                    })
                    .collect();
                let last = own.iter().max_by_key(|(_, item)| own_end(item));
                let (line, indent, eq) = match last {
                    Some((k, item)) => {
                        let line = self.src.line_of(own_end(item).saturating_sub(1));
                        let key_line = self.src.line_of(k.span().map_or(0, |s| s.start));
                        (line, self.src.indent(key_line), self.eq_spelling(k, item))
                    }
                    None => {
                        let header = t.span().unwrap_or(0..0);
                        (
                            self.src.line_of(header.end.saturating_sub(1)),
                            "",
                            " = ".to_string(),
                        )
                    }
                };
                self.key_line_after(line, &format!("{indent}{key}{eq}{value}"), path)
            }
        }
    }

    /// [`Ctx::line_after`] for a new key line, with a blank line under it
    /// where the comment block right below `line` would otherwise change
    /// owner (a header above it set it off for the entry below it); else the
    /// shape of `path` refuses.
    fn key_line_after(
        &self,
        line: usize,
        content: &str,
        path: &str,
    ) -> Result<Splice, EditRefusal> {
        let plain = self.line_after(line, content);
        let block: Vec<usize> = (line + 1..self.src.lines.len())
            .take_while(|&l| self.src.kind(l) == Kind::Comment)
            .filter_map(|l| self.src.lines[l].comment)
            .collect();
        if block.is_empty() || self.insert_keeps_owners(&plain, &block) {
            return Ok(plain);
        }
        let spaced = Splice {
            text: format!("{}{}", plain.text, self.src.newline_after(line)),
            ..plain
        };
        if self.insert_keeps_owners(&spaced, &block) {
            Ok(spaced)
        } else {
            Err(self.shape(path))
        }
    }

    /// Whether the comments starting at `block`, after the insert `added`,
    /// keep their owners in the text it leaves. A candidate that does not
    /// parse is [`verify`]'s to refuse.
    fn insert_keeps_owners(&self, added: &Splice, block: &[usize]) -> bool {
        let Ok(text) = splice(self.text(), vec![added.clone()]) else {
            return true;
        };
        let Ok(after) = Ctx::new(&text, self.op.clone()) else {
            return true;
        };
        let (before, now) = (self.owners(), after.owners());
        block.iter().all(|at| {
            before.get(at).cloned().unwrap_or(Owner::Free)
                == now
                    .get(&(at + added.text.len()))
                    .cloned()
                    .unwrap_or(Owner::Free)
        })
    }

    /// The pairs of the braced inline table `outer` in text order, each
    /// starting past the `{` or the separator before it.
    fn pairs<'d>(&self, outer: &'d InlineTable) -> Vec<Pair<'d>> {
        fn walk<'d>(
            table: &'d InlineTable,
            path: &[&'d Key],
            via: &[&'d InlineTable],
            out: &mut Vec<Pair<'d>>,
        ) {
            for (k, _) in table.iter() {
                let Some((key, item)) = table.get_key_value(k) else {
                    continue;
                };
                let path = [path, &[key]].concat();
                match item {
                    Item::Value(Value::InlineTable(t)) if t.is_dotted() => {
                        walk(t, &path, &[via, &[t]].concat(), out);
                    }
                    _ => out.push(Pair {
                        path,
                        via: via.to_vec(),
                        item,
                        start: 0,
                        end: item_end(item),
                    }),
                }
            }
        }
        let mut out = Vec::new();
        walk(outer, &[], &[], &mut out);
        out.sort_by_key(|p| p.end);
        let bytes = self.text().as_bytes();
        let mut at = outer.span().map_or(0, |s| s.start + 1);
        for pair in &mut out {
            loop {
                match bytes.get(at) {
                    Some(b' ' | b'\t' | b'\r' | b'\n' | b',') => at += 1,
                    Some(b'#') => at = self.src.lines[self.src.line_of(at)].eol,
                    _ => break,
                }
            }
            pair.start = at;
            at = pair.end;
        }
        out
    }

    /// How `members`, the pairs of the dotted inline table `table`, spell
    /// the path to it, the dot after it included (`auth.`, `"auth" . `): as
    /// its last pair does when that pair holds one of its own keys, else as
    /// its first does, the one pair whose every key past `table` is its own.
    /// Empty for a braced table.
    fn dotted_prefix(&self, table: &InlineTable, members: &[&Pair<'_>]) -> String {
        let spelled = |pair: &Pair<'_>| {
            let depth = pair.via.iter().position(|v| std::ptr::eq(*v, table))?;
            let next = pair.path.get(depth + 1)?.span()?;
            self.text().get(pair.start..next.start).map(str::to_string)
        };
        let last = members
            .last()
            .filter(|p| p.via.last().is_some_and(|v| std::ptr::eq(*v, table)));
        last.or(members.first())
            .and_then(|pair| spelled(pair))
            .unwrap_or_default()
    }

    /// The `=` with its spacing as `key = item` spells it, or ` = `.
    fn eq_spelling(&self, key: &Key, item: &Item) -> String {
        let (Some(k), Some(v)) = (key.span(), item.span()) else {
            return " = ".to_string();
        };
        match self.text().get(k.end..v.start) {
            Some(eq) if eq.trim() == "=" => eq.to_string(),
            _ => " = ".to_string(),
        }
    }

    /// A new line holding `content` after line `line`.
    fn line_after(&self, line: usize, content: &str) -> Splice {
        let (at, lead) = self.src.after_line(line);
        let nl = self.src.newline_after(line);
        Splice {
            at: at..at,
            text: if lead {
                format!("{nl}{content}")
            } else {
                format!("{content}{nl}")
            },
        }
    }

    /// The cuts removing `key` of `tab` (at `path`), the ranges it owns
    /// pushed onto `own`: in a header or dotted table its whole lines, each
    /// run of a dotted key's lines as one; in an inline one its pairs, each
    /// run of them with one separator. A dotted table a header also opens a
    /// sub-table of is a shape the editor does not cut.
    fn remove_key<'s>(
        &'s self,
        tab: Tab<'s>,
        key: &str,
        path: &str,
        own: &mut Vec<Range<usize>>,
    ) -> Result<Vec<Range<usize>>, EditRefusal> {
        let Some((k, item)) = tab.get(key) else {
            return Ok(Vec::new());
        };
        let mut cuts = Vec::new();
        match tab {
            Tab::Std(_) => {
                let mut lines = Vec::new();
                self.key_lines(k, item, path, false, &mut lines)?;
                lines.sort_unstable();
                let mut runs: Vec<(usize, usize)> = Vec::new();
                for (first, last) in lines {
                    match runs.last_mut() {
                        Some(run) if first <= run.1 + 1 => run.1 = run.1.max(last),
                        _ => runs.push((first, last)),
                    }
                }
                for (first, last) in runs {
                    cuts.push(self.src.cut_lines(first, last));
                    own.push(self.src.lines[first].start..self.src.lines[last].eol);
                }
            }
            Tab::Inline(_, outer) => {
                let pairs = self.pairs(outer);
                let gone: Vec<bool> = pairs.iter().map(|p| p.under(item)).collect();
                let span = outer.span().unwrap_or(0..0);
                let mut i = 0;
                while i < pairs.len() {
                    if !gone[i] {
                        i += 1;
                        continue;
                    }
                    let first = i;
                    while i < pairs.len() && gone[i] {
                        own.push(pairs[i].start..pairs[i].end);
                        i += 1;
                    }
                    cuts.push(match (pairs.get(i), first.checked_sub(1)) {
                        (Some(next), _) => pairs[first].start..next.start,
                        (None, Some(prev)) => pairs[prev].end..pairs[i - 1].end,
                        (None, None) => span.start + 1..span.end.saturating_sub(1),
                    });
                }
            }
        }
        Ok(cuts)
    }

    /// The first and last line of `key = item` under a header or dotted
    /// table at `path`, each dotted key's under it; `dotted` when that table
    /// is a dotted one.
    fn key_lines(
        &self,
        key: &Key,
        item: &Item,
        path: &str,
        dotted: bool,
        out: &mut Vec<(usize, usize)>,
    ) -> Result<(), EditRefusal> {
        let spelled = format!("{path}.{}", self.spell(key));
        match item {
            Item::Table(t) if t.is_dotted() => {
                for (k, _) in t.iter() {
                    if let Some((k, child)) = t.get_key_value(k) {
                        self.key_lines(k, child, &spelled, true, out)?;
                    }
                }
                Ok(())
            }
            Item::Table(_) | Item::ArrayOfTables(_) if dotted => Err(self.shape(spelled)),
            _ => {
                let start = key.span().map_or(0, |s| s.start);
                out.push((
                    self.src.line_of(start),
                    self.src.line_of(item_end(item).saturating_sub(1)),
                ));
                Ok(())
            }
        }
    }

    // ── the edits ──

    fn change(&self, edit: &Edit) -> Result<Change, Unplanned> {
        let mut owned = Vec::new();
        let mut cascade = None;
        let splices = match edit {
            Edit::Server { field, value } => {
                self.edit_server(*field, value.as_deref(), &mut owned)?
            }
            Edit::Upstream { name, field, value } => {
                self.refuse_legacy()?;
                self.edit_upstream(name, *field, value.as_deref(), &mut owned)?
            }
            Edit::Model { id, field, value } => {
                self.edit_model(id, field, value.as_deref(), &mut owned)?
            }
            Edit::Route {
                route,
                field,
                value,
            } => {
                let routes = self.list(self.root(), "routes", None)?;
                let entry = self.route(&routes, route)?;
                self.edit_key(entry.tab, field.key(), value, &routes.path, &mut owned)?
            }
            Edit::RenameUpstream { name, to } => {
                self.refuse_legacy()?;
                let (splices, counts) = self.rename_upstream(name, to)?;
                cascade = Some(counts);
                splices
            }
            Edit::RenameModel { id, to } => {
                let (splices, counts) = self.rename_model(id, to)?;
                cascade = Some(counts);
                splices
            }
            Edit::AddUpstream(new) => {
                self.refuse_legacy()?;
                self.add_upstream(new)?
            }
            Edit::AddModel(new) => self.add_model(new)?,
            Edit::AddRoute(new) => {
                let mut fields = vec![
                    ("model", basic(&new.model)),
                    ("provider", basic(&new.provider)),
                ];
                push_opt(&mut fields, "upstream_model", &new.upstream_model);
                push_opt(&mut fields, "effort", &new.effort);
                push_opt(&mut fields, "service_tier", &new.service_tier);
                let routes = self.list(self.root(), "routes", None)?;
                self.add_entry(&routes, "routes", &fields)
            }
            Edit::RemoveUpstream { name } => {
                self.refuse_legacy()?;
                let list = self.list(self.root(), "upstreams", None)?;
                let entry = self.find(&list, "name", name, format!("upstream named {name:?}"))?;
                let named = format!("upstream {name:?}");
                self.remove_entry(&list, entry, &named, self.removed(edit), &mut owned)?
            }
            Edit::RemoveModel { id } => {
                let list = self.list(self.root(), "models", None)?;
                let entry = self.find(&list, "id", id, format!("model with id {id:?}"))?;
                let named = format!("model {id:?}");
                self.remove_entry(&list, entry, &named, self.removed(edit), &mut owned)?
            }
            Edit::RemoveRoute { route } => {
                let list = self.list(self.root(), "routes", None)?;
                let entry = self.route(&list, route)?;
                self.remove_entry(&list, entry, &route.named(), self.removed(edit), &mut owned)?
            }
            Edit::SetAccounts { name, accounts } => {
                self.refuse_legacy()?;
                return self.set_accounts(name, accounts);
            }
        };
        Ok(Change {
            candidate: splice(self.text(), splices)?,
            owned,
            cascade,
        })
    }

    /// The spellings [`semantic::apply`] needs to compute `edit`'s value.
    fn forms(&self, edit: &Edit) -> semantic::Forms {
        semantic::Forms {
            tables: self.removes_a_table(edit),
            dotted: self.edits_dotted(edit),
        }
    }

    /// Whether the table `edit` edits a key of is dotted (`server.bind`,
    /// `auth.env`), so it vanishes with its last key.
    fn edits_dotted(&self, edit: &Edit) -> bool {
        let name = match edit {
            Edit::Server { .. } => {
                return matches!(self.server(), Ok(Some((Tab::Std(t), _))) if t.is_dotted());
            }
            Edit::Upstream { name, field, .. } if field.auth_key().is_some() => name,
            Edit::SetAccounts { name, .. } => name,
            _ => return false,
        };
        self.auth_form(name) == Some(AuthForm::Dotted)
    }

    /// How upstream `name` spells its `auth` when it is a table (inline,
    /// dotted or `[upstreams.auth]`): `None` for a shorthand, no `auth`, a
    /// shape clauth does not edit, or no such upstream.
    fn auth_form(&self, name: &str) -> Option<AuthForm> {
        let list = self.list(self.root(), "upstreams", None).ok()?;
        match self
            .find(&list, "name", name, String::new())
            .and_then(|entry| self.auth(entry.tab, &list.path))
        {
            Ok(Auth::Tab(_, _, form)) => Some(form),
            Ok(Auth::Absent | Auth::Shorthand(_)) | Err(_) => None,
        }
    }

    /// Whether `edit` removes an entry of a `[[x]]` list.
    fn removes_a_table(&self, edit: &Edit) -> bool {
        let key = match edit {
            Edit::RemoveUpstream { .. } => "upstreams",
            Edit::RemoveModel { .. } => "models",
            Edit::RemoveRoute { .. } => "routes",
            _ => return false,
        };
        self.list(self.root(), key, None)
            .is_ok_and(|list| matches!(list.form, ListForm::Aot))
    }

    fn route<'s>(&'s self, routes: &List<'s>, route: &RouteView) -> Result<Entry<'s>, EditRefusal> {
        let changed = || EditRefusal::EntryChanged {
            op: self.op.clone(),
        };
        let index = route.position.checked_sub(1).ok_or_else(changed)?;
        let entry = *routes.entries.get(index).ok_or_else(changed)?;
        if self.route_view(index, entry.tab, &routes.path)? != *route {
            return Err(changed());
        }
        Ok(entry)
    }

    /// Set or unset one plain string key of `tab`.
    fn edit_key<'s>(
        &'s self,
        tab: Tab<'s>,
        key: &str,
        value: &Option<String>,
        path: &str,
        owned: &mut Vec<Range<usize>>,
    ) -> Result<Vec<Splice>, EditRefusal> {
        self.scalar(tab, key, path)?;
        match (value, tab.get(key)) {
            (Some(value), Some((_, item))) => Ok(vec![self.set_value(item, value)]),
            (Some(value), None) => Ok(vec![self.add_key(tab, key, &basic(value), path)?]),
            (None, Some(_)) => self.cut(tab, key, path, owned),
            (None, None) => Ok(Vec::new()),
        }
    }

    /// [`Ctx::remove_key`]'s cuts as splices.
    fn cut<'s>(
        &'s self,
        tab: Tab<'s>,
        key: &str,
        path: &str,
        owned: &mut Vec<Range<usize>>,
    ) -> Result<Vec<Splice>, EditRefusal> {
        Ok(self
            .remove_key(tab, key, path, owned)?
            .into_iter()
            .map(|at| Splice {
                at,
                text: String::new(),
            })
            .collect())
    }

    fn edit_server(
        &self,
        field: ServerField,
        value: Option<&str>,
        owned: &mut Vec<Range<usize>>,
    ) -> Result<Vec<Splice>, EditRefusal> {
        let key = field.key();
        match self.server()? {
            Some((tab, path)) if !tab.headerless() => {
                self.edit_key(tab, key, &value.map(str::to_string), &path, owned)
            }
            Some((tab, path)) => {
                self.scalar(tab, key, &path)?;
                Ok(value.map_or_else(Vec::new, |value| {
                    vec![self.new_table_at_end(&[
                        "[server]".to_string(),
                        format!("{key} = {}", basic(value)),
                    ])]
                }))
            }
            None => Ok(value.map_or_else(Vec::new, |value| {
                vec![self.new_table_at_end(&[
                    "[server]".to_string(),
                    format!("{key} = {}", basic(value)),
                ])]
            })),
        }
    }

    /// A splice appending `block` (a header and its lines) at the file's end,
    /// one blank line before it.
    fn new_table_at_end(&self, block: &[String]) -> Splice {
        let text = self.text();
        let nl = self
            .src
            .newline_after(self.src.lines.len().saturating_sub(1));
        let block = block.join(nl);
        let at = text.len();
        let body = if text.len() == self.src.body {
            format!("{block}{nl}")
        } else if !text.ends_with('\n') {
            format!("{nl}{nl}{block}")
        } else if self.src.lines.last().is_some_and(|l| l.kind == Kind::Blank) {
            format!("{block}{nl}")
        } else {
            format!("{nl}{block}{nl}")
        };
        Splice {
            at: at..at,
            text: body,
        }
    }

    fn edit_upstream(
        &self,
        name: &str,
        field: UpstreamField,
        value: Option<&str>,
        owned: &mut Vec<Range<usize>>,
    ) -> Result<Vec<Splice>, EditRefusal> {
        let list = self.list(self.root(), "upstreams", None)?;
        let entry = self.find(&list, "name", name, format!("upstream named {name:?}"))?;
        let Some(auth_key) = field.auth_key() else {
            return self.edit_key(
                entry.tab,
                field.label(),
                &value.map(str::to_string),
                &list.path,
                owned,
            );
        };
        let no_mode = || EditRefusal::NoAuthMode {
            op: self.op.clone(),
            name: name.to_string(),
        };
        match (self.auth(entry.tab, &list.path)?, value) {
            (Auth::Absent, None) => Ok(Vec::new()),
            (Auth::Absent, Some(value)) if auth_key == "mode" => Ok(vec![self.add_key(
                entry.tab,
                "auth",
                &basic(value),
                &list.path,
            )?]),
            (Auth::Absent, Some(_)) => Err(no_mode()),
            (Auth::Shorthand(item), Some(value)) if auth_key == "mode" => {
                Ok(vec![self.set_value(item, value)])
            }
            (Auth::Shorthand(_), None) if auth_key == "mode" => {
                self.cut(entry.tab, "auth", &list.path, owned)
            }
            (Auth::Shorthand(_), None) => Ok(Vec::new()),
            (Auth::Shorthand(item), Some(value)) => Ok(vec![self.shorthand_to_inline(
                item,
                auth_key,
                &basic(value),
            )]),
            (Auth::Tab(auth, path, form), value) => {
                if auth_key != "mode" && value.is_some() && auth.get("mode").is_none() {
                    return Err(no_mode());
                }
                match (auth_key, value, form) {
                    ("mode", None, AuthForm::Inline | AuthForm::Dotted) => {
                        self.cut(entry.tab, "auth", &list.path, owned)
                    }
                    ("mode", None, AuthForm::Table) => {
                        let Tab::Std(table) = auth else {
                            return Err(self.shape(path));
                        };
                        self.remove_table(
                            table,
                            &path,
                            &format!("the auth table of upstream {name:?}"),
                            Removal::Sub(next_entry(
                                &self.entry_lines(&list),
                                self.src.line_of(entry.start),
                            )),
                            None,
                            owned,
                        )
                    }
                    _ => self.edit_key(auth, auth_key, &value.map(str::to_string), &path, owned),
                }
            }
        }
    }

    /// `auth = "<mode>"` rewritten as `{ mode = "<mode>", key = <value> }`,
    /// the mode's own text kept.
    fn shorthand_to_inline(&self, item: &Item, key: &str, value: &str) -> Splice {
        let span = item.span().unwrap_or(0..0);
        let mode = &self.text()[span.clone()];
        Splice {
            at: span,
            text: format!("{{ mode = {mode}, {} = {value} }}", key_repr(key)),
        }
    }

    fn edit_model(
        &self,
        id: &str,
        field: &ModelField,
        value: Option<&str>,
        owned: &mut Vec<Range<usize>>,
    ) -> Result<Vec<Splice>, EditRefusal> {
        let list = self.list(self.root(), "models", None)?;
        let entry = self.find(&list, "id", id, format!("model with id {id:?}"))?;
        let provider = match field {
            ModelField::DisplayName => {
                return self.edit_key(
                    entry.tab,
                    "display_name",
                    &value.map(str::to_string),
                    &list.path,
                    owned,
                );
            }
            ModelField::UpstreamModel(provider) => provider,
        };
        let Some((map, path)) = self.sub(entry.tab, "upstream_model", &list.path)? else {
            return match value {
                None => Ok(Vec::new()),
                Some(value) => Ok(vec![self.add_key(
                    entry.tab,
                    "upstream_model",
                    &format!("{{ {} = {} }}", key_repr(provider), basic(value)),
                    &list.path,
                )?]),
            };
        };
        if map.headerless() {
            return Err(self.shape(path));
        }
        let only = holds_only(map, provider);
        match (value, map) {
            (None, Tab::Inline(..)) if only => {
                self.cut(entry.tab, "upstream_model", &list.path, owned)
            }
            (None, Tab::Std(table)) if only && !table.is_dotted() => self.remove_table(
                table,
                &path,
                &format!("the upstream_model table of model {id:?}"),
                Removal::Sub(next_entry(
                    &self.entry_lines(&list),
                    self.src.line_of(entry.start),
                )),
                None,
                owned,
            ),
            _ => self.edit_key(map, provider, &value.map(str::to_string), &path, owned),
        }
    }

    fn remove_entry<'s>(
        &'s self,
        list: &List<'s>,
        entry: Entry<'s>,
        named: &str,
        removed: Option<(ListId, usize)>,
        owned: &mut Vec<Range<usize>>,
    ) -> Result<Vec<Splice>, EditRefusal> {
        match (&list.form, entry.tab) {
            (ListForm::Inline(array), _) => {
                let elems = self.elems(array);
                let i = elems
                    .elems
                    .iter()
                    .position(|e| e.start == entry.start)
                    .ok_or_else(|| self.shape(list.path.clone()))?;
                let (cuts, own) = self.remove_elem(&elems, i, named, &list.path)?;
                owned.push(own);
                Ok(cuts
                    .into_iter()
                    .map(|at| Splice {
                        at,
                        text: String::new(),
                    })
                    .collect())
            }
            (ListForm::Aot, Tab::Std(table)) => {
                let first = self.src.line_of(entry.start);
                let siblings: Vec<Range<usize>> = self
                    .entry_lines(list)
                    .into_iter()
                    .filter(|other| other.start != first)
                    .collect();
                self.remove_table(
                    table,
                    &list.path,
                    named,
                    Removal::Entry(&siblings),
                    removed,
                    owned,
                )
            }
            (ListForm::Aot | ListForm::Absent, _) => Err(self.shape(list.path.clone())),
        }
    }

    /// Append an entry whose fields are `fields` (key, TOML value text).
    fn add_entry<'s>(
        &'s self,
        list: &List<'s>,
        key: &str,
        fields: &[(&str, String)],
    ) -> Vec<Splice> {
        match &list.form {
            ListForm::Inline(array) => {
                let inline = fields
                    .iter()
                    .map(|(k, v)| format!("{} = {v}", key_repr(k)))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.append_elem(&self.elems(array), &format!("{{ {inline} }}"))
            }
            ListForm::Absent => {
                vec![self.new_table_at_end(&aot_entry(&format!("[[{key}]]"), fields))]
            }
            ListForm::Aot => {
                let Some(last) = list.entries.last() else {
                    return vec![self.new_table_at_end(&aot_entry(&format!("[[{key}]]"), fields))];
                };
                let header = match last.tab {
                    Tab::Std(t) => t
                        .span()
                        .and_then(|s| self.text().get(s))
                        .unwrap_or_default()
                        .to_string(),
                    Tab::Inline(..) => format!("[[{key}]]"),
                };
                // The block the last entry owns below it stays its own, set
                // off from the new entry.
                let last_line = self.src.line_of(last.end.saturating_sub(1));
                let owned_end = self.tail_end(last_line, None);
                let nl = self.src.newline_after(owned_end);
                let top = self.owned_top(self.src.line_of(last.start), None, false);
                let sep = if list.entries.len() == 1 {
                    nl.to_string()
                } else {
                    let blanks = (0..top)
                        .rev()
                        .take_while(|&l| self.src.kind(l) == Kind::Blank)
                        .count();
                    self.text()[self.src.lines[top - blanks].start..self.src.lines[top].start]
                        .to_string()
                };
                let entry = aot_entry(&header, fields).join(nl);
                let sep = if owned_end > last_line && sep.is_empty() {
                    nl.to_string()
                } else {
                    sep
                };
                let (at, lead) = self.src.after_line(owned_end);
                vec![Splice {
                    at: at..at,
                    text: if lead {
                        format!("{nl}{sep}{entry}")
                    } else {
                        format!("{sep}{entry}{nl}")
                    },
                }]
            }
        }
    }

    fn add_upstream(&self, new: &NewUpstream) -> Result<Vec<Splice>, EditRefusal> {
        let list = self.list(self.root(), "upstreams", None)?;
        if list
            .entries
            .iter()
            .any(|e| e.tab.str("name") == Some(new.name.as_str()))
        {
            return Err(EditRefusal::Duplicate {
                op: self.op.clone(),
                what: format!("the config already has an upstream named {:?}", new.name),
            });
        }
        let mut fields = vec![("name", basic(&new.name))];
        push_opt(&mut fields, "provider", &new.provider);
        push_opt(&mut fields, "kind", &new.kind);
        push_opt(&mut fields, "base_url", &new.base_url);
        if let Some(auth) = &new.auth {
            let Some(mode) = &auth.mode else {
                return Err(EditRefusal::NoAuthMode {
                    op: self.op.clone(),
                    name: new.name.clone(),
                });
            };
            let mut parts = vec![("mode", basic(mode))];
            push_opt(&mut parts, "account", &auth.account);
            if !auth.accounts.is_empty() {
                parts.push(("accounts", string_array(&auth.accounts)));
            }
            push_opt(&mut parts, "env", &auth.env);
            push_opt(&mut parts, "header", &auth.header);
            let value = if parts.len() == 1 {
                basic(mode)
            } else {
                inline_table(&parts)
            };
            fields.push(("auth", value));
        }
        push_opt(&mut fields, "effort", &new.effort);
        push_opt(&mut fields, "service_tier", &new.service_tier);
        Ok(self.add_entry(&list, "upstreams", &fields))
    }

    fn add_model(&self, new: &NewModel) -> Result<Vec<Splice>, EditRefusal> {
        let list = self.list(self.root(), "models", None)?;
        if list
            .entries
            .iter()
            .any(|e| e.tab.str("id") == Some(new.id.as_str()))
        {
            return Err(EditRefusal::Duplicate {
                op: self.op.clone(),
                what: format!("the config already has a model with id {:?}", new.id),
            });
        }
        let mut fields = vec![("id", basic(&new.id))];
        push_opt(&mut fields, "display_name", &new.display_name);
        if !new.upstream_model.is_empty() {
            let map: Vec<(&str, String)> = new
                .upstream_model
                .iter()
                .map(|(provider, slug)| (provider.as_str(), basic(slug)))
                .collect();
            fields.push(("upstream_model", inline_table(&map)));
        }
        Ok(self.add_entry(&list, "models", &fields))
    }

    // ── renames ──

    /// Every place an upstream name is referenced.
    fn references<'s>(&'s self) -> Result<Vec<Reference<'s>>, EditRefusal> {
        let mut out = Vec::new();
        let mut values = |list: &List<'s>, site: Site| {
            for entry in &list.entries {
                if let Some((_, item)) = entry.tab.get("provider")
                    && item.is_str()
                {
                    out.push(Reference::Value(site, item));
                }
            }
        };
        let routes = self.list(self.root(), "routes", None)?;
        values(&routes, Site::Routes);
        let prefixes = self.list(self.root(), "route_prefixes", None)?;
        values(&prefixes, Site::RoutePrefixes);
        let server = self.server()?;
        let codex = match &server {
            Some((tab, path)) => self.sub(*tab, "codex_endpoint", path)?,
            None => None,
        };
        if let Some((codex, path)) = &codex {
            let routes = self.list(*codex, "routes", Some(path))?;
            values(&routes, Site::CodexRoutes);
        }
        if let Some((tab, _)) = &server
            && let Some((_, item)) = tab.get("default_provider")
            && item.is_str()
        {
            out.push(Reference::Value(Site::DefaultProvider, item));
        }
        if let Some((codex, _)) = &codex
            && let Some((_, item)) = codex.get("provider")
            && item.is_str()
        {
            out.push(Reference::Value(Site::CodexEndpoint, item));
        }
        let models = self.list(self.root(), "models", None)?;
        for entry in &models.entries {
            if let Some((map, _)) = self.sub(entry.tab, "upstream_model", &models.path)? {
                for (key, _) in map.keys() {
                    out.push(Reference::Key(key));
                }
            }
        }
        Ok(out)
    }

    fn rename_upstream(&self, name: &str, to: &str) -> Result<(Vec<Splice>, Cascade), EditRefusal> {
        let list = self.list(self.root(), "upstreams", None)?;
        let entry = self.find(&list, "name", name, format!("upstream named {name:?}"))?;
        if name == to {
            return Ok((Vec::new(), Cascade::default()));
        }
        if list.entries.iter().any(|e| e.tab.str("name") == Some(to)) {
            return Err(EditRefusal::Duplicate {
                op: self.op.clone(),
                what: format!("the config already has an upstream named {to:?}"),
            });
        }
        let references = self.references()?;
        let server = self.server()?;
        let default_implicit = server
            .as_ref()
            .is_none_or(|(tab, _)| tab.get("default_provider").is_none());
        let codex_implicit = match &server {
            Some((tab, path)) => self
                .sub(*tab, "codex_endpoint", path)?
                .filter(|(codex, _)| codex.get("provider").is_none()),
            None => None,
        };
        let implicit = |upstream: &str| {
            if upstream == DEFAULT_PROVIDER && default_implicit {
                Some(Site::DefaultProvider)
            } else if upstream == CODEX_ENDPOINT_PROVIDER && codex_implicit.is_some() {
                Some(Site::CodexEndpoint)
            } else {
                None
            }
        };
        let taken = references
            .iter()
            .find(|r| r.name() == to)
            .map(Reference::site)
            .or_else(|| implicit(to));
        if let Some(site) = taken {
            return Err(EditRefusal::ReferenceTaken {
                op: self.op.clone(),
                name: to.to_string(),
                site: site.describe(),
            });
        }
        let mut cascade = Cascade::default();
        let mut splices = Vec::new();
        if let Some((_, item)) = entry.tab.get("name") {
            splices.push(self.set_value(item, to));
        }
        match (implicit(name), &codex_implicit) {
            (Some(Site::DefaultProvider), _) => {
                cascade.default_provider += 1;
                splices.extend(self.edit_server(
                    ServerField::DefaultProvider,
                    Some(to),
                    &mut Vec::new(),
                )?);
            }
            (Some(Site::CodexEndpoint), Some((codex, path))) => {
                cascade.codex_endpoint += 1;
                splices.push(if codex.headerless() {
                    self.new_table_at_end(&[
                        format!("[{path}]"),
                        format!("provider = {}", basic(to)),
                    ])
                } else {
                    self.add_key(*codex, "provider", &basic(to), path)?
                });
            }
            _ => {}
        }
        for reference in references.iter().filter(|r| r.name() == name) {
            let count = match reference.site() {
                Site::Routes => &mut cascade.routes,
                Site::RoutePrefixes => &mut cascade.route_prefixes,
                Site::CodexRoutes => &mut cascade.codex_routes,
                Site::DefaultProvider => &mut cascade.default_provider,
                Site::CodexEndpoint => &mut cascade.codex_endpoint,
                Site::ModelKey => &mut cascade.models,
                Site::ModelRouter | Site::RouteModel => continue,
            };
            *count += 1;
            splices.push(match reference {
                Reference::Value(_, item) => self.set_value(item, to),
                Reference::Key(key) => {
                    let span = key.span().unwrap_or(0..0);
                    let raw = &self.text()[span.clone()];
                    Splice {
                        at: span,
                        text: key_like(raw, to),
                    }
                }
            });
        }
        Ok((splices, cascade))
    }

    fn rename_model(&self, id: &str, to: &str) -> Result<(Vec<Splice>, Cascade), EditRefusal> {
        let list = self.list(self.root(), "models", None)?;
        let entry = self.find(&list, "id", id, format!("model with id {id:?}"))?;
        if id == to {
            return Ok((Vec::new(), Cascade::default()));
        }
        if list.entries.iter().any(|e| e.tab.str("id") == Some(to)) {
            return Err(EditRefusal::Duplicate {
                op: self.op.clone(),
                what: format!("the config already has a model with id {to:?}"),
            });
        }
        let targets = self.model_targets(&list)?;
        let names = |wanted: &str| {
            targets
                .iter()
                .any(|(_, v)| v.as_str().is_some_and(|t| split_hint(t).0 == wanted))
        };
        if names(to) {
            return Err(EditRefusal::ReferenceTaken {
                op: self.op.clone(),
                name: to.to_string(),
                site: Site::ModelRouter.describe(),
            });
        }
        let routes = self.list(self.root(), "routes", None)?;
        let route_models: Vec<&Item> = routes
            .entries
            .iter()
            .filter_map(|e| e.tab.get("model").map(|(_, item)| item))
            .collect();
        if route_models.iter().any(|item| item.as_str() == Some(to)) {
            return Err(EditRefusal::ReferenceTaken {
                op: self.op.clone(),
                name: to.to_string(),
                site: Site::RouteModel.describe(),
            });
        }
        let mut cascade = Cascade::default();
        let mut splices: Vec<Splice> = entry
            .tab
            .get("id")
            .map(|(_, item)| vec![self.set_value(item, to)])
            .unwrap_or_default();
        for (overlay, value) in &targets {
            let Some((base, hint)) = value.as_str().map(split_hint) else {
                continue;
            };
            if base != id {
                continue;
            }
            match overlay {
                Overlay::Router => cascade.routers += 1,
                Overlay::Subagents => cascade.subagents += 1,
            }
            splices.push(self.set_str(value, &format!("{to}{hint}")));
        }
        for item in route_models.iter().filter(|item| item.as_str() == Some(id)) {
            cascade.route_models += 1;
            splices.push(self.set_value(item, to));
        }
        Ok((splices, cascade))
    }

    /// Every value naming a model id in a `[models.router]` or
    /// `[models.subagents]` table, by the keys shunt reads ids from
    /// (`named_targets` and `named_judges` of each `type` and `mode`). A
    /// custom classifier's `default_target` and `models` keys name groups.
    fn model_targets<'s>(
        &'s self,
        models: &List<'s>,
    ) -> Result<Vec<(Overlay, &'s Value)>, EditRefusal> {
        let mut out = Vec::new();
        for entry in &models.entries {
            if let Some((router, path)) = self.sub(entry.tab, "router", &models.path)? {
                let mut push = |values: Vec<&'s Value>| {
                    out.extend(values.into_iter().map(|v| (Overlay::Router, v)));
                };
                match (router.str("type"), router.str("mode")) {
                    (Some("stage_router"), _) => {
                        push(self.strings(router, &["capable_target", "efficient_target"]));
                        if let Some((classifier, _)) = self.sub(router, "classifier", &path)? {
                            push(self.strings(classifier, &["target"]));
                        }
                    }
                    (Some("auto"), _) => {
                        push(self.strings(router, &["capable_target", "efficient_target"]));
                    }
                    (Some("random" | "prefill_router"), _) => {
                        push(self.string_array(router, "targets", &path)?);
                    }
                    (Some("llm_classifier"), Some("capability" | "escalation")) => {
                        push(self.strings(
                            router,
                            &["classifier_target", "strong_target", "weak_target"],
                        ))
                    }
                    (Some("llm_classifier"), Some("custom")) => {
                        push(self.groups(router, &path)?);
                    }
                    (Some("composite"), _) => {
                        if let Some((classifier, _)) = self.sub(router, "classifier", &path)? {
                            push(self.strings(classifier, &["target"]));
                        }
                        if let Some((stage, _)) = self.sub(router, "stage", &path)? {
                            push(self.strings(stage, &["capable_target", "efficient_target"]));
                        }
                    }
                    (Some("advisor"), _) => {
                        push(self.strings(router, &["executor_target", "advisor_target"]));
                    }
                    (Some("noop"), _) => {}
                    _ => return Err(self.shape(path)),
                }
            }
            if let Some((overlay, path)) = self.sub(entry.tab, "subagents", &models.path)? {
                let mut push = |values: Vec<&'s Value>| {
                    out.extend(values.into_iter().map(|v| (Overlay::Subagents, v)));
                };
                match (overlay.str("type"), overlay.str("mode")) {
                    (Some("passthrough"), _) => {
                        push(self.strings(overlay, &["target"]));
                        if let Some((by_type, _)) = self.sub(overlay, "by_type", &path)? {
                            push(
                                by_type
                                    .keys()
                                    .into_iter()
                                    .filter_map(|(_, item)| item.as_value().filter(|v| v.is_str()))
                                    .collect(),
                            );
                        }
                    }
                    (Some("llm_classifier"), Some("custom")) => push(self.groups(overlay, &path)?),
                    _ => return Err(self.shape(path)),
                }
            }
        }
        Ok(out)
    }

    /// The string values of `keys` in `tab`.
    fn strings<'s>(&'s self, tab: Tab<'s>, keys: &[&str]) -> Vec<&'s Value> {
        keys.iter()
            .filter_map(|key| tab.get(key))
            .filter_map(|(_, item)| item.as_value().filter(|v| v.is_str()))
            .collect()
    }

    /// The strings of the array at `key` in `tab`.
    fn string_array<'s>(
        &'s self,
        tab: Tab<'s>,
        key: &str,
        path: &str,
    ) -> Result<Vec<&'s Value>, EditRefusal> {
        match tab.get(key) {
            None => Ok(Vec::new()),
            Some((_, Item::Value(Value::Array(array)))) => {
                Ok(array.iter().filter(|v| v.is_str()).collect())
            }
            Some((k, _)) => Err(self.shape(format!("{path}.{}", self.spell(k)))),
        }
    }

    /// The ids of every group of a custom classifier's `models` table.
    fn groups<'s>(&'s self, tab: Tab<'s>, path: &str) -> Result<Vec<&'s Value>, EditRefusal> {
        let Some((groups, path)) = self.sub(tab, "models", path)? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for (key, _) in groups.keys() {
            out.extend(self.string_array(groups, key.get(), &path)?);
        }
        Ok(out)
    }

    // ── accounts ──

    fn set_accounts(&self, name: &str, accounts: &[String]) -> Result<Change, Unplanned> {
        let mut seen = HashSet::new();
        if let Some(twice) = accounts.iter().find(|a| !seen.insert(a.as_str())) {
            return Err(EditRefusal::Duplicate {
                op: self.op.clone(),
                what: format!("the accounts name {twice:?} twice"),
            }
            .into());
        }
        let list = self.list(self.root(), "upstreams", None)?;
        let entry = self.find(&list, "name", name, format!("upstream named {name:?}"))?;
        let no_mode = || EditRefusal::NoAuthMode {
            op: self.op.clone(),
            name: name.to_string(),
        };
        let unchanged = || Change {
            candidate: self.text().to_string(),
            owned: Vec::new(),
            cascade: None,
        };
        let splices = match self.auth(entry.tab, &list.path)? {
            Auth::Absent if accounts.is_empty() => return Ok(unchanged()),
            Auth::Absent => return Err(no_mode().into()),
            Auth::Shorthand(_) if accounts.is_empty() => return Ok(unchanged()),
            Auth::Shorthand(item) => {
                vec![self.shorthand_to_inline(item, "accounts", &string_array(accounts))]
            }
            Auth::Tab(auth, path, _) => {
                if auth.get("mode").is_none() && !accounts.is_empty() {
                    return Err(no_mode().into());
                }
                match auth.get("accounts") {
                    None if accounts.is_empty() => return Ok(unchanged()),
                    None => vec![self.add_key(auth, "accounts", &string_array(accounts), &path)?],
                    Some((_, Item::ArrayOfTables(_))) => {
                        return Err(EditRefusal::TableAccounts {
                            name: name.to_string(),
                        }
                        .into());
                    }
                    Some((k, Item::Value(Value::Array(array)))) => {
                        if array.iter().any(Value::is_inline_table) {
                            return Err(EditRefusal::TableAccounts {
                                name: name.to_string(),
                            }
                            .into());
                        }
                        if !array.iter().all(Value::is_str) {
                            return Err(self.shape(format!("{path}.{}", self.spell(k))).into());
                        }
                        if accounts.is_empty() {
                            let mut owned = Vec::new();
                            let splices = self.cut(auth, "accounts", &path, &mut owned)?;
                            return Ok(Change {
                                candidate: splice(self.text(), splices)?,
                                owned,
                                cascade: None,
                            });
                        }
                        return self.edit_accounts(name, accounts);
                    }
                    Some((k, _)) => {
                        return Err(self.shape(format!("{path}.{}", self.spell(k))).into());
                    }
                }
            }
        };
        Ok(Change {
            candidate: splice(self.text(), splices)?,
            owned: Vec::new(),
            cascade: None,
        })
    }

    /// The accounts list of upstream `name` as an inline array of names,
    /// with its layout. The caller has checked the shape.
    fn accounts_elems(&self, name: &str) -> Result<(Elems, Vec<String>, String), EditRefusal> {
        let list = self.list(self.root(), "upstreams", None)?;
        let entry = self.find(&list, "name", name, format!("upstream named {name:?}"))?;
        let Auth::Tab(auth, path, _) = self.auth(entry.tab, &list.path)? else {
            return Err(self.shape(list.path));
        };
        match auth.get("accounts") {
            Some((k, Item::Value(Value::Array(array)))) => Ok((
                self.elems(array),
                array
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
                format!("{path}.{}", self.spell(k)),
            )),
            _ => Err(self.shape(path)),
        }
    }

    /// Drop, add and reorder over an inline array of names, one step at a
    /// time on the text so each step reads the layout the last one left.
    fn edit_accounts(&self, name: &str, accounts: &[String]) -> Result<Change, Unplanned> {
        let (original, names, _) = self.accounts_elems(name)?;
        let mut seen = HashSet::new();
        if let Some(twice) = names.iter().find(|n| !seen.insert(n.as_str())) {
            return Err(EditRefusal::Duplicate {
                op: self.op.clone(),
                what: format!("the config names account {twice:?} twice in upstream {name:?}"),
            }
            .into());
        }
        let mut owned = Vec::new();
        let mut text = self.text().to_string();
        let broken = |text: String, owned| Change {
            candidate: text,
            owned,
            cascade: None,
        };
        // Drops run from the end, so each step's element `i` is the
        // original's element `i`.
        for (i, dropped) in names.iter().enumerate().rev() {
            if accounts.contains(dropped) {
                continue;
            }
            let Ok(step) = Ctx::new(&text, self.op.clone()) else {
                return Ok(broken(text, owned));
            };
            let (now, _, path) = step.accounts_elems(name)?;
            let (cuts, _) = step.remove_elem(
                &now,
                i,
                &format!("account {dropped:?} of upstream {name:?}"),
                &path,
            )?;
            owned.push(self.elem_owned(&original, i));
            text = splice(
                &text,
                cuts.into_iter()
                    .map(|at| Splice {
                        at,
                        text: String::new(),
                    })
                    .collect(),
            )?;
        }
        for added in accounts.iter().filter(|a| !names.contains(a)) {
            let Ok(step) = Ctx::new(&text, self.op.clone()) else {
                return Ok(broken(text, owned));
            };
            let (now, _, _) = step.accounts_elems(name)?;
            text = splice(&text, step.append_elem(&now, &basic(added)))?;
        }
        let Ok(step) = Ctx::new(&text, self.op.clone()) else {
            return Ok(broken(text, owned));
        };
        let (now, held, path) = step.accounts_elems(name)?;
        let order: Vec<usize> = accounts
            .iter()
            .filter_map(|a| held.iter().position(|n| n == a))
            .collect();
        if order.len() == held.len() {
            text = splice(&text, step.permute(&now, &order, name, &path)?)?;
        }
        Ok(broken(text, owned))
    }
}

/// shunt's `[server] default_provider` when the key is absent
/// (`Config::default`, shunt 03d99f1 `src/config.rs`).
const DEFAULT_PROVIDER: &str = "anthropic";

/// shunt's `[server.codex_endpoint] provider` when the key is absent
/// (`default_codex_endpoint_provider`, the same file).
const CODEX_ENDPOINT_PROVIDER: &str = "codex";

/// Which table a model-id reference sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overlay {
    Router,
    Subagents,
}

/// `id` and its context-window hint (`[1m]`/`[1M]`, which shunt strips
/// before matching a `[[models]]` id).
fn split_hint(id: &str) -> (&str, &str) {
    for hint in ["[1m]", "[1M]"] {
        if let Some(base) = id.strip_suffix(hint) {
            return (base, hint);
        }
    }
    (id, "")
}

enum Auth<'d> {
    Absent,
    Shorthand(&'d Item),
    Tab(Tab<'d>, String, AuthForm),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Site {
    Routes,
    ModelKey,
    DefaultProvider,
    RoutePrefixes,
    CodexEndpoint,
    CodexRoutes,
    ModelRouter,
    RouteModel,
}

impl Site {
    fn describe(self) -> &'static str {
        match self {
            Site::Routes => "a [[routes]] provider",
            Site::ModelKey => "a [[models]] upstream_model key",
            Site::DefaultProvider => "server.default_provider",
            Site::RoutePrefixes => "a [[route_prefixes]] provider",
            Site::CodexEndpoint => "[server.codex_endpoint] provider",
            Site::CodexRoutes => "a [[server.codex_endpoint.routes]] provider",
            Site::ModelRouter => "a [models.router] or [models.subagents] target",
            Site::RouteModel => "a [[routes]] model",
        }
    }
}

enum Reference<'d> {
    Value(Site, &'d Item),
    Key(&'d Key),
}

impl Reference<'_> {
    fn name(&self) -> &str {
        match self {
            Reference::Value(_, item) => item.as_str().unwrap_or_default(),
            Reference::Key(key) => key.get(),
        }
    }

    fn site(&self) -> Site {
        match self {
            Reference::Value(site, _) => *site,
            Reference::Key(_) => Site::ModelKey,
        }
    }
}

/// Every header (`[x]`, `[[x]]`) under `table`, its own included, as the
/// range from the header's start to where the keys it heads end. The
/// document's root has an empty span and no header.
fn collect_sections(table: &Table, out: &mut Vec<Range<usize>>) {
    if !table.is_dotted()
        && !table.is_implicit()
        && let Some(span) = table.span()
        && !span.is_empty()
    {
        let end = table
            .iter()
            .map(|(_, item)| own_end(item))
            .fold(span.end, usize::max);
        out.push(span.start..end);
    }
    for (_, item) in table.iter() {
        match item {
            Item::Table(t) => collect_sections(t, out),
            Item::ArrayOfTables(aot) => aot.iter().for_each(|t| collect_sections(t, out)),
            Item::Value(_) | Item::None => {}
        }
    }
}

/// The header line of the first entry below line `line` among `entries`, a
/// list's entry line ranges.
fn next_entry(entries: &[Range<usize>], line: usize) -> Option<usize> {
    entries.iter().map(|r| r.start).filter(|&s| s > line).min()
}

/// Whether `tab` holds `key` and nothing else.
fn holds_only(tab: Tab<'_>, key: &str) -> bool {
    tab.keys().len() == 1 && tab.get(key).is_some()
}

/// Where `item`'s text ends.
fn item_end(item: &Item) -> usize {
    match item {
        Item::Value(v) => v.span().map_or(0, |s| s.end),
        Item::Table(t) => table_end(t),
        Item::ArrayOfTables(aot) => aot.iter().map(table_end).max().unwrap_or(0),
        Item::None => 0,
    }
}

/// Where the lines `item` writes under its own table's header end: a dotted
/// table's sub-tables, under headers of their own, left out.
fn own_end(item: &Item) -> usize {
    match item {
        Item::Table(t) if t.is_dotted() => t.iter().map(|(_, i)| own_end(i)).max().unwrap_or(0),
        Item::Table(_) | Item::ArrayOfTables(_) => 0,
        Item::Value(_) | Item::None => item_end(item),
    }
}

/// Every key under the dotted table `item` reached through dotted tables
/// alone, with its item; nothing when `item` is no dotted table.
fn dotted_leaves<'d>(item: &'d Item, out: &mut Vec<(&'d Key, &'d Item)>) {
    let table: &dyn TableLike = match item {
        Item::Table(t) if t.is_dotted() => t,
        Item::Value(Value::InlineTable(t)) if t.is_dotted() => t,
        Item::Table(_) | Item::Value(_) | Item::ArrayOfTables(_) | Item::None => return,
    };
    for (k, _) in table.iter() {
        if let Some((key, child)) = table.get_key_value(k) {
            match child {
                Item::Table(t) if t.is_dotted() => dotted_leaves(child, out),
                Item::Value(Value::InlineTable(t)) if t.is_dotted() => dotted_leaves(child, out),
                _ => out.push((key, child)),
            }
        }
    }
}

/// Where a table's text ends: its header, its keys and its sub-tables.
fn table_end(table: &Table) -> usize {
    table
        .iter()
        .map(|(_, item)| item_end(item))
        .chain(table.span().map(|s| s.end))
        .max()
        .unwrap_or(0)
}

/// An `[[x]]` entry's lines: `header`, then each field's.
fn aot_entry(header: &str, fields: &[(&str, String)]) -> Vec<String> {
    std::iter::once(header.to_string())
        .chain(fields.iter().map(|(k, v)| format!("{} = {v}", key_repr(k))))
        .collect()
}

fn push_opt<'k>(fields: &mut Vec<(&'k str, String)>, key: &'k str, value: &Option<String>) {
    if let Some(value) = value {
        fields.push((key, basic(value)));
    }
}

fn inline_table(fields: &[(&str, String)]) -> String {
    let body = fields
        .iter()
        .map(|(k, v)| format!("{} = {v}", key_repr(k)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{{ {body} }}")
}

fn string_array(names: &[String]) -> String {
    format!(
        "[{}]",
        names
            .iter()
            .map(|n| basic(n))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

// ── TOML spelling ───────────────────────────────────────────────────────────

/// `s` as a TOML basic string.
fn basic(s: &str) -> String {
    format!("\"{}\"", escaped(s))
}

/// `s` escaped for a TOML basic string, quotes left out.
fn escaped(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", u32::from(c))),
            c => out.push(c),
        }
    }
    out
}

fn literal_allows(s: &str) -> bool {
    !s.contains('\'') && !s.chars().any(|c| c.is_control() && c != '\t')
}

/// `value` as a string spelled like `raw`: literal stays literal, a
/// multi-line string stays multi-line, when `value` allows it; else basic. A
/// multi-line string spanning lines keeps spanning them (from a newline
/// right after its opening quotes, which TOML trims), so no comment after it
/// moves onto another line.
fn encode_like(raw: &str, value: &str) -> String {
    let spans = raw.contains('\n');
    let nl = if raw.contains("\r\n") { "\r\n" } else { "\n" };
    if raw.starts_with("'''") {
        let allowed = !value.contains("'''")
            && !value
                .chars()
                .any(|c| c.is_control() && c != '\t' && c != '\n');
        if allowed {
            let lead = if spans || value.starts_with('\n') {
                nl
            } else {
                ""
            };
            return format!("'''{lead}{value}'''");
        }
    } else if raw.starts_with("\"\"\"") {
        let lead = if spans { nl } else { "" };
        return format!("\"\"\"{lead}{}\"\"\"", escaped(value));
    } else if raw.starts_with('\'') && literal_allows(value) {
        return format!("'{value}'");
    }
    basic(value)
}

fn bare_allows(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn key_repr(key: &str) -> String {
    if bare_allows(key) {
        key.to_string()
    } else {
        basic(key)
    }
}

/// `key` spelled like the key `raw`: a bare key stays bare when `key` allows
/// it, a literal key literal, a quoted one quoted.
fn key_like(raw: &str, key: &str) -> String {
    if raw.starts_with('"') {
        basic(key)
    } else if raw.starts_with('\'') && literal_allows(key) {
        format!("'{key}'")
    } else {
        key_repr(key)
    }
}

// ── the expected value ──────────────────────────────────────────────────────

/// The config's TOML value with an edit applied, computed on `toml`'s tree
/// apart from the splice, so a splice that changes anything else is caught.
mod semantic {
    use toml::{Table, Value};

    use super::{Edit, ModelField, ServerField};

    fn entries<'t>(table: &'t mut Table, key: &str) -> Option<&'t mut Vec<Value>> {
        table.get_mut(key)?.as_array_mut()
    }

    fn named<'t>(list: &'t mut [Value], key: &str, wanted: &str) -> Option<&'t mut Table> {
        list.iter_mut()
            .filter_map(Value::as_table_mut)
            .find(|t| t.get(key).and_then(Value::as_str) == Some(wanted))
    }

    fn set(table: &mut Table, key: &str, value: &Option<String>) {
        match value {
            Some(value) => {
                table.insert(key.to_string(), Value::String(value.clone()));
            }
            None => {
                table.remove(key);
            }
        }
    }

    fn table_at<'t>(table: &'t mut Table, key: &str) -> Option<&'t mut Table> {
        table
            .entry(key.to_string())
            .or_insert_with(|| Value::Table(Table::new()))
            .as_table_mut()
    }

    /// How the file spells what an edit touches, where the value depends on
    /// it.
    #[derive(Debug, Clone, Copy)]
    pub(super) struct Forms {
        /// The edited list is `[[x]]` tables, so removing its last entry
        /// leaves no key at all where an inline array leaves `[]`.
        pub(super) tables: bool,
        /// The edited table is dotted, so unsetting its last key leaves no
        /// table at all where a header or inline table stays empty.
        pub(super) dotted: bool,
    }

    pub(super) fn apply(original: &Table, edit: &Edit, forms: Forms) -> Option<Table> {
        let mut doc = original.clone();
        let tables = forms.tables;
        match edit {
            Edit::Server { field, value } => {
                let key = match field {
                    ServerField::Bind => "bind",
                    ServerField::DefaultProvider => "default_provider",
                };
                if value.is_some() || doc.contains_key("server") {
                    let server = table_at(&mut doc, "server")?;
                    set(server, key, value);
                    if forms.dotted && server.is_empty() {
                        doc.remove("server");
                    }
                }
            }
            Edit::Upstream { name, field, value } => {
                let upstream = named(entries(&mut doc, "upstreams")?, "name", name)?;
                let Some(key) = field.auth_key() else {
                    set(upstream, field.label(), value);
                    return Some(doc);
                };
                match (upstream.get("auth").cloned(), value) {
                    (None, None) => {}
                    (None, Some(value)) => {
                        upstream.insert("auth".into(), Value::String(value.clone()));
                    }
                    (Some(Value::String(_)), None) if key == "mode" => {
                        upstream.remove("auth");
                    }
                    (Some(Value::String(_)), None) => {}
                    (Some(Value::String(_)), Some(value)) if key == "mode" => {
                        upstream.insert("auth".into(), Value::String(value.clone()));
                    }
                    (Some(Value::String(mode)), Some(value)) => {
                        let mut auth = Table::new();
                        auth.insert("mode".into(), Value::String(mode));
                        auth.insert(key.into(), Value::String(value.clone()));
                        upstream.insert("auth".into(), Value::Table(auth));
                    }
                    (Some(Value::Table(_)), None) if key == "mode" => {
                        upstream.remove("auth");
                    }
                    (Some(Value::Table(mut auth)), value) => {
                        set(&mut auth, key, value);
                        if forms.dotted && auth.is_empty() {
                            upstream.remove("auth");
                        } else {
                            upstream.insert("auth".into(), Value::Table(auth));
                        }
                    }
                    (Some(_), _) => return None,
                }
            }
            Edit::Model { id, field, value } => {
                let model = named(entries(&mut doc, "models")?, "id", id)?;
                match field {
                    ModelField::DisplayName => set(model, "display_name", value),
                    ModelField::UpstreamModel(provider) => {
                        if value.is_none() && !model.contains_key("upstream_model") {
                            return Some(doc);
                        }
                        let map = table_at(model, "upstream_model")?;
                        set(map, provider, value);
                        if map.is_empty() {
                            model.remove("upstream_model");
                        }
                    }
                }
            }
            Edit::Route {
                route,
                field,
                value,
            } => {
                let routes = entries(&mut doc, "routes")?;
                let index = route.position.checked_sub(1)?;
                let route = routes.get_mut(index)?.as_table_mut()?;
                set(route, field.key(), value);
            }
            Edit::RenameUpstream { name, to } => rename_upstream(&mut doc, name, to)?,
            Edit::RenameModel { id, to } => rename_model(&mut doc, id, to)?,
            Edit::AddUpstream(new) => {
                let mut t = Table::new();
                t.insert("name".into(), Value::String(new.name.clone()));
                opt(&mut t, "provider", &new.provider);
                opt(&mut t, "kind", &new.kind);
                opt(&mut t, "base_url", &new.base_url);
                if let Some(auth) = &new.auth {
                    let mode = auth.mode.clone()?;
                    let only_mode = auth.account.is_none()
                        && auth.accounts.is_empty()
                        && auth.env.is_none()
                        && auth.header.is_none();
                    if only_mode {
                        t.insert("auth".into(), Value::String(mode));
                    } else {
                        let mut a = Table::new();
                        a.insert("mode".into(), Value::String(mode));
                        opt(&mut a, "account", &auth.account);
                        if !auth.accounts.is_empty() {
                            a.insert("accounts".into(), strings(&auth.accounts));
                        }
                        opt(&mut a, "env", &auth.env);
                        opt(&mut a, "header", &auth.header);
                        t.insert("auth".into(), Value::Table(a));
                    }
                }
                opt(&mut t, "effort", &new.effort);
                opt(&mut t, "service_tier", &new.service_tier);
                push(&mut doc, "upstreams", t);
            }
            Edit::AddModel(new) => {
                let mut t = Table::new();
                t.insert("id".into(), Value::String(new.id.clone()));
                opt(&mut t, "display_name", &new.display_name);
                if !new.upstream_model.is_empty() {
                    let map = new
                        .upstream_model
                        .iter()
                        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                        .collect();
                    t.insert("upstream_model".into(), Value::Table(map));
                }
                push(&mut doc, "models", t);
            }
            Edit::AddRoute(new) => {
                let mut t = Table::new();
                t.insert("model".into(), Value::String(new.model.clone()));
                t.insert("provider".into(), Value::String(new.provider.clone()));
                opt(&mut t, "upstream_model", &new.upstream_model);
                opt(&mut t, "effort", &new.effort);
                opt(&mut t, "service_tier", &new.service_tier);
                push(&mut doc, "routes", t);
            }
            Edit::RemoveUpstream { name } => remove(&mut doc, "upstreams", tables, |list| {
                list.iter()
                    .position(|v| v.get("name").and_then(Value::as_str) == Some(name.as_str()))
            })?,
            Edit::RemoveModel { id } => remove(&mut doc, "models", tables, |list| {
                list.iter()
                    .position(|v| v.get("id").and_then(Value::as_str) == Some(id.as_str()))
            })?,
            Edit::RemoveRoute { route } => remove(&mut doc, "routes", tables, |list| {
                route.position.checked_sub(1).filter(|&i| i < list.len())
            })?,
            Edit::SetAccounts { name, accounts } => {
                let upstream = named(entries(&mut doc, "upstreams")?, "name", name)?;
                match upstream.get("auth").cloned() {
                    None | Some(Value::String(_)) if accounts.is_empty() => {}
                    Some(Value::String(mode)) => {
                        let mut a = Table::new();
                        a.insert("mode".into(), Value::String(mode));
                        a.insert("accounts".into(), strings(accounts));
                        upstream.insert("auth".into(), Value::Table(a));
                    }
                    Some(Value::Table(mut a)) => {
                        if accounts.is_empty() {
                            a.remove("accounts");
                        } else {
                            a.insert("accounts".into(), strings(accounts));
                        }
                        if forms.dotted && a.is_empty() {
                            upstream.remove("auth");
                        } else {
                            upstream.insert("auth".into(), Value::Table(a));
                        }
                    }
                    None | Some(_) => return None,
                }
            }
        }
        Some(doc)
    }

    fn opt(table: &mut Table, key: &str, value: &Option<String>) {
        if let Some(value) = value {
            table.insert(key.to_string(), Value::String(value.clone()));
        }
    }

    fn strings(names: &[String]) -> Value {
        Value::Array(names.iter().cloned().map(Value::String).collect())
    }

    fn push(doc: &mut Table, key: &str, entry: Table) {
        match doc.get_mut(key).and_then(Value::as_array_mut) {
            Some(list) => list.push(Value::Table(entry)),
            None => {
                doc.insert(key.to_string(), Value::Array(vec![Value::Table(entry)]));
            }
        }
    }

    fn remove(
        doc: &mut Table,
        list: &str,
        tables: bool,
        at: impl FnOnce(&[Value]) -> Option<usize>,
    ) -> Option<()> {
        let entries = entries(doc, list)?;
        let index = at(entries)?;
        entries.remove(index);
        if tables && entries.is_empty() {
            doc.remove(list);
        }
        Some(())
    }

    /// The id and every `[models.router]` / `[models.subagents]` key shunt
    /// reads a model id from, per `type` and `mode`.
    fn rename_model(doc: &mut Table, id: &str, to: &str) -> Option<()> {
        named(entries(doc, "models")?, "id", id)?.insert("id".into(), Value::String(to.into()));
        if let Some(routes) = entries(doc, "routes") {
            for route in routes.iter_mut().filter_map(Value::as_table_mut) {
                if route.get("model").and_then(Value::as_str) == Some(id) {
                    route.insert("model".into(), Value::String(to.into()));
                }
            }
        }
        let rename = |value: &mut Value| {
            if let Some((base, hint)) = value.as_str().map(super::split_hint)
                && base == id
            {
                *value = Value::String(format!("{to}{hint}"));
            }
        };
        let keys = |table: &mut Table, keys: &[&str]| {
            for key in keys {
                if let Some(value) = table.get_mut(*key) {
                    rename(value);
                }
            }
        };
        let list = |value: Option<&mut Value>| {
            if let Some(items) = value.and_then(Value::as_array_mut) {
                items.iter_mut().for_each(rename);
            }
        };
        let groups = |table: &mut Table| {
            if let Some(groups) = table.get_mut("models").and_then(Value::as_table_mut) {
                groups.iter_mut().for_each(|(_, ids)| list(Some(ids)));
            }
        };
        for model in entries(doc, "models")?
            .iter_mut()
            .filter_map(Value::as_table_mut)
        {
            if let Some(router) = model.get_mut("router").and_then(Value::as_table_mut) {
                let kind = router
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let mode = router
                    .get("mode")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                match (kind.as_deref(), mode.as_deref()) {
                    (Some("stage_router" | "auto"), _) => {
                        keys(router, &["capable_target", "efficient_target"]);
                        if let Some(classifier) =
                            router.get_mut("classifier").and_then(Value::as_table_mut)
                        {
                            keys(classifier, &["target"]);
                        }
                    }
                    (Some("random" | "prefill_router"), _) => list(router.get_mut("targets")),
                    (Some("llm_classifier"), Some("custom")) => groups(router),
                    (Some("llm_classifier"), _) => {
                        keys(
                            router,
                            &["classifier_target", "strong_target", "weak_target"],
                        );
                    }
                    (Some("composite"), _) => {
                        if let Some(classifier) =
                            router.get_mut("classifier").and_then(Value::as_table_mut)
                        {
                            keys(classifier, &["target"]);
                        }
                        if let Some(stage) = router.get_mut("stage").and_then(Value::as_table_mut) {
                            keys(stage, &["capable_target", "efficient_target"]);
                        }
                    }
                    (Some("advisor"), _) => keys(router, &["executor_target", "advisor_target"]),
                    _ => {}
                }
            }
            if let Some(overlay) = model.get_mut("subagents").and_then(Value::as_table_mut) {
                match overlay.get("type").and_then(Value::as_str) {
                    Some("passthrough") => {
                        keys(overlay, &["target"]);
                        if let Some(by_type) =
                            overlay.get_mut("by_type").and_then(Value::as_table_mut)
                        {
                            by_type.iter_mut().for_each(|(_, target)| rename(target));
                        }
                    }
                    Some("llm_classifier") => groups(overlay),
                    _ => {}
                }
            }
        }
        Some(())
    }

    fn rename_upstream(doc: &mut Table, name: &str, to: &str) -> Option<()> {
        named(entries(doc, "upstreams")?, "name", name)?
            .insert("name".into(), Value::String(to.into()));
        let rename = |value: &mut Value| {
            if value.as_str() == Some(name) {
                *value = Value::String(to.into());
            }
        };
        for list in ["routes", "route_prefixes"] {
            if let Some(entries) = entries(doc, list) {
                for entry in entries.iter_mut().filter_map(Value::as_table_mut) {
                    if let Some(provider) = entry.get_mut("provider") {
                        rename(provider);
                    }
                }
            }
        }
        if let Some(models) = entries(doc, "models") {
            for model in models.iter_mut().filter_map(Value::as_table_mut) {
                if let Some(map) = model
                    .get_mut("upstream_model")
                    .and_then(Value::as_table_mut)
                    && let Some(slug) = map.remove(name)
                {
                    map.insert(to.into(), slug);
                }
            }
        }
        if let Some(server) = doc.get_mut("server").and_then(Value::as_table_mut) {
            if let Some(provider) = server.get_mut("default_provider") {
                rename(provider);
            }
            if let Some(codex) = server
                .get_mut("codex_endpoint")
                .and_then(Value::as_table_mut)
            {
                if let Some(provider) = codex.get_mut("provider") {
                    rename(provider);
                }
                if let Some(routes) = codex.get_mut("routes").and_then(Value::as_array_mut) {
                    for route in routes.iter_mut().filter_map(Value::as_table_mut) {
                        if let Some(provider) = route.get_mut("provider") {
                            rename(provider);
                        }
                    }
                }
            }
        }
        if name == super::DEFAULT_PROVIDER {
            table_at(doc, "server")?
                .entry("default_provider")
                .or_insert_with(|| Value::String(to.into()));
        }
        if name == super::CODEX_ENDPOINT_PROVIDER
            && let Some(codex) = doc
                .get_mut("server")
                .and_then(Value::as_table_mut)
                .and_then(|server| server.get_mut("codex_endpoint"))
                .and_then(Value::as_table_mut)
        {
            codex
                .entry("provider")
                .or_insert_with(|| Value::String(to.into()));
        }
        Some(())
    }
}

#[cfg(test)]
#[path = "../tests/inline/gateway_edit.rs"]
mod tests;
