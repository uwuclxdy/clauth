//! Pure formatters for the MCP layer: init instructions block, third-party
//! headline, and the prose spellings of each tool's JSON payload, the
//! folded-in live-usage clause included. No I/O, no locks — callers pass in
//! already-loaded cache data so these stay unit-testable.

use std::net::{IpAddr, Ipv6Addr};

use serde_json::Value;

use crate::format::{format_pct, humanize_span, local_stamp};
use crate::providers::ThirdPartyStats;
use crate::runtime::LinkProbe;
use crate::usage::{humanize_duration, iso_to_epoch_secs, now_epoch_secs};
use crate::which::SessionAuth;

/// Per-profile snapshot fed to [`instructions_block`]: stable identity only (name,
/// provider, tier, base url). Volatile usage figures rot within a turn, so they are
/// served fresh per call by `profiles`, never baked into the boot-time block.
pub(crate) struct ProfileSnapshot {
    pub(crate) name: String,
    pub(crate) active: bool,
    pub(crate) provider: String,
    pub(crate) base_url: Option<String>,
    pub(crate) sub_type: Option<String>,
    /// Where this profile sorts in the roster. See [`RosterRank`].
    pub(crate) rank: RosterRank,
}

/// A profile's roster sort key, for ordering only.
///
/// The variants never interleave: every windowed profile outranks every wallet
/// one, which outranks every profile clauth holds no figure for. That last step
/// is what keeps "no figure" from reading as "full".
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RosterRank {
    /// Percent of this profile's best-known window still FREE.
    Window(f64),
    /// A provider reporting a wallet rather than a window. Amounts are compared
    /// only within one `currency`: ordering 1117 CNY against 31 USD needs an
    /// exchange rate clauth does not have and could not keep fresh.
    Balance { currency: String, amount: f64 },
    /// Nothing cached, or nothing a wallet could be read out of.
    Unknown,
}

/// Host (and port) of a base url — the HOST, never the whole authority. Every
/// profile of one provider carries the same endpoint path, so both the roster and
/// `profiles` print the identifying half only. Shared so the two can never
/// disagree on what a profile's endpoint is called.
///
/// Userinfo is dropped, and dropping it is the whole point rather than tidying:
/// per RFC 3986 an authority is `[ userinfo "@" ] host [ ":" port ]`, so
/// `https://api.deepseek.com:443@evil.tld` has host `evil.tld` and an authority
/// that READS as DeepSeek. Returning the authority named the wrong host on every
/// consumer and printed any basic-auth credentials in it onto two model-facing
/// surfaces. [`crate::providers::url_matches_host`] carries the incident that
/// taught this repo the same lesson at the fetch layer, where it cost an api key;
/// this is that fix arriving at the render layer.
///
/// Split at the LAST `@`, not the first. Neither `userinfo` nor `host` admits a
/// bare `@` (userinfo wants `%40`, and `host` is `IP-literal / IPv4address /
/// reg-name`, none of which allow it), so a well-formed authority holds at most
/// one and the two directions agree. They differ only on malformed input, and
/// there the first-`@` answer still contains an `@` and so cannot be a host at
/// all, while the last-`@` answer at least can be.
///
/// The authority ends at the FIRST of `/`, `?` or `#`, and it is cut BEFORE the
/// userinfo. Both halves of that are load-bearing and each was wrong once. Cut
/// after the userinfo and `http://evil.tld/a@b` reports host `b`, since a path
/// may legally hold an `@`. Cut on `/` alone and query or fragment text stays
/// inside the authority, so `http://evil.tld?x=a@127.0.0.1` reports host
/// `127.0.0.1` and renders a PUBLIC endpoint as local — the same
/// discard-before-validating defect [`authority_host`] closes downstream,
/// reintroduced one function upstream. `url`'s own authority scan breaks on all
/// three delimiters.
pub(super) fn base_url_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    authority
        .rsplit_once('@')
        .map_or(authority, |(_userinfo, host)| host)
}

/// The word a host [`host_locality`] places earns. Returned by that function
/// rather than read beside it: the word rides the decision, so a carrier holds
/// no spelling of its own that could drift from the other's.
const LOCAL_ENDPOINT: &str = "local endpoint";

/// A port is digits, and ZERO digits is the defaulted port of a legal
/// `http://host:/path` — the same spelling
/// [`crate::providers::url_matches_host`] already accepts, so the two layers do
/// not disagree about one string.
fn is_port(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_digit())
}

/// The address inside a well-formed authority host, or `None` when the string is
/// not one. Accepted: `[IPv6]`, `[IPv6%zone]`, an IPv4 literal, or a name — each
/// optionally followed by `:` and a port.
///
/// DISCARD NOTHING UNACCOUNTED FOR. `IpAddr::from_str` already does total
/// accounting on an address, so every bug this function has had was bytes thrown
/// away BEFORE that parse ran: the bracket arm dropped whatever followed `]`, and
/// a zone cut dropped whatever followed `%`, neither validating what it dropped.
/// Each made a PREFIX of a hostile host answer for the whole of it —
/// `[::1]@evil.com` and `127.0.0.1%2Eevil.com` both read as loopback while naming
/// `evil.com`. That is the defect [`crate::providers::url_matches_host`] already
/// documents one module over, where a bare `starts_with` would have claimed
/// `api.deepseek.com.evil.tld`. So every byte here is consumed as host, port or
/// zone, or the whole string is refused.
///
/// A zone id rides INSIDE the brackets and only there (RFC 6874), which is what
/// makes cutting it safe: no "looks like a zone" test could do this work, since
/// `.` is `unreserved` and `2Eevil.com` is a syntactically legal zone. A zone on
/// an IPv4-mapped address is refused rather than cut — that is IPv6 syntax naming
/// a v4 endpoint, so it has no interface to name.
fn authority_host(host: &str) -> Option<&str> {
    if let Some(rest) = host.strip_prefix('[') {
        let (inner, tail) = rest.split_once(']')?;
        if !(tail.is_empty() || tail.strip_prefix(':').is_some_and(is_port)) {
            return None;
        }
        let (addr, zoned) = match inner.split_once('%') {
            None => (inner, false),
            Some((addr, zone)) => {
                if zone.is_empty() {
                    return None;
                }
                (addr, true)
            }
        };
        let v6 = addr.parse::<Ipv6Addr>().ok()?;
        if zoned && v6.to_ipv4_mapped().is_some() {
            return None;
        }
        return Some(addr);
    }
    // Unbracketed, so a surviving colon can only be the port separator, and a
    // bare IPv6 literal is refused. That input is REACHABLE, not impossible:
    // `base_url_host` splits without validating, `Profile::base_url` is raw config
    // text, and nothing in the crate parses a url — so `http://::1/v1` arrives
    // here as `::1` from the commonest IPv6-url typo there is. Refusing it is a
    // deliberate cut: clauth does not guess which authority a malformed one meant.
    match host.rsplit_once(':') {
        Some((h, port)) => (!h.contains(':') && is_port(port)).then_some(h),
        None => Some(host),
    }
}

/// The locality marker a base-url host earns, or `None` for a host clauth
/// cannot place. Both roster carriers ([`roster_bracket`] and [`profile_line`])
/// call this on the host string each already holds, so one predicate answers for
/// both surfaces.
///
/// The claim is about WHERE the endpoint is, and stops there. An address that
/// names a machine on this box or this network is not a hosted vendor endpoint,
/// which is what makes it the cheap target on a roster — but the box answering
/// may be someone else's, or a proxy fronting a metered API, so the marker says
/// where the endpoint lives and leaves the bill to the reader. Which is also why
/// the host is the only field read: it is the one that says where.
///
/// Placed: an IP literal that is loopback, private (`10/8`, `172.16/12`,
/// `192.168/16`), link-local (`169.254/16`, `fe80::/10`), unique-local
/// (`fc00::/7`) or unspecified (`0.0.0.0`, `::`), or the exact name `localhost`.
/// The last two sit here on the same argument as the rest: an unspecified address
/// names this box, and a link-local one reaches no further than a link, so
/// neither can be a hosted vendor endpoint.
///
/// A link-local address is placed in the one spelling a url authority can carry
/// it in — `[fe80::1%25eth0]` — because [`authority_host`] cuts the zone before
/// parsing. `IpAddr::from_str` rejects a zone, and the zone-less form is the one
/// a multi-interface box cannot connect to, so matching only that form would fire
/// on every spelling except the working one. The bare `fe80::1%eth0` is refused
/// with every other unbracketed IPv6, which RFC 3986 has no authority syntax for
/// — a reachable config typo that clauth declines to guess at, not an impossible
/// input. See [`authority_host`].
///
/// NOT placed, each for its own reason. Any other NAME, because clauth resolves
/// nothing — `ollama`, but equally `foo.localhost` and `localhost.`, which RFC
/// 6761 does guarantee as loopback; widening to those is a live option rather
/// than an oversight. And `100.64/10`, carrier-grade NAT space a mesh VPN happens
/// to use, so the block itself says nothing about who runs the box.
fn host_locality(host: &str) -> Option<&'static str> {
    let addr = authority_host(host)?;
    if addr.eq_ignore_ascii_case("localhost") {
        return Some(LOCAL_ENDPOINT);
    }
    // Canonicalised first: `::ffff:0:0/96` carries no verdict of its own, so
    // folding a mapped address to its IPv4 twin adds no reading the address did
    // not already have. Only a MAPPED address folds — measured, `::` stays V6 —
    // which is what keeps the two `is_unspecified` terms on separate arms.
    let placed = match addr.parse::<IpAddr>().ok()?.to_canonical() {
        IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6.is_unspecified()
        }
    };
    placed.then_some(LOCAL_ENDPOINT)
}

/// The `[provider, tier, host]` bracket a roster line ends in, plus the
/// [`host_locality`] marker when the host earns one. Profiles sharing one
/// bracket share a line, and the marker adds no grouping distinction of its own:
/// it is a pure function of the host, which the key already carries ahead of it.
fn roster_bracket(p: &ProfileSnapshot) -> String {
    let mut parts = vec![p.provider.clone()];
    if let Some(s) = &p.sub_type {
        parts.push(s.clone());
    }
    if let Some(b) = &p.base_url {
        let host = base_url_host(b);
        parts.push(host.to_string());
        if let Some(marker) = host_locality(host) {
            parts.push(marker.to_string());
        }
    }
    format!("[{}]", parts.join(", "))
}

/// Currencies in the order the roster first meets them. Two currencies carry no
/// comparable magnitude, so their groups fall back to the order the operator's
/// own config lists them in.
fn currency_order(profiles: &[ProfileSnapshot]) -> Vec<&str> {
    let mut seen: Vec<&str> = Vec::new();
    for p in profiles {
        if let RosterRank::Balance { currency, .. } = &p.rank
            && !seen.contains(&currency.as_str())
        {
            seen.push(currency);
        }
    }
    seen
}

/// Total order over [`RosterRank`] as `(tier, currency group, negated
/// magnitude)`. Sorting ascending on it puts the freest window first and every
/// unknown last, and negating the magnitude is what makes "more left" sort
/// earlier without a second comparator.
fn sort_key(p: &ProfileSnapshot, currencies: &[&str]) -> (u8, usize, f64) {
    match &p.rank {
        RosterRank::Window(free) => (0, 0, -free),
        RosterRank::Balance { currency, amount } => (
            1,
            currencies
                .iter()
                .position(|c| *c == currency.as_str())
                .unwrap_or(usize::MAX),
            -amount,
        ),
        RosterRank::Unknown => (2, 0, 0.0),
    }
}

fn cmp_key(a: (u8, usize, f64), b: (u8, usize, f64)) -> std::cmp::Ordering {
    a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.total_cmp(&b.2))
}

/// The roster marker a profile earns in THIS session's block. A `clauth start`
/// runtime marks the profile the session is pinned to and the account the global
/// link points at separately; a global session IS the global link, so the active
/// profile carries both names; a custom config dir holds no clauth session
/// profile, so only the global link gets a name. The old bare `(active)` read as
/// "this session's account" where a `clauth start` session does not spend the
/// active profile at all.
fn marker(p: &ProfileSnapshot, auth: &SessionAuth) -> Option<&'static str> {
    match auth {
        SessionAuth::IsolatedRuntime(runtime) if p.name == *runtime && p.active => {
            Some(" (global active, this session)")
        }
        SessionAuth::IsolatedRuntime(runtime) if p.name == *runtime => Some(" (this session)"),
        SessionAuth::IsolatedRuntime(_) | SessionAuth::IsolatedCustom if p.active => {
            Some(" (global active)")
        }
        SessionAuth::Global if p.active => Some(" (global active, this session)"),
        SessionAuth::Global | SessionAuth::IsolatedRuntime(_) | SessionAuth::IsolatedCustom => None,
    }
}

/// Roster body: one line per distinct bracket, names joined, most headroom first.
/// A fleet of same-provider profiles otherwise repeats one identical endpoint on
/// every line, which is pure token cost in a block every session loads. The
/// ordering is a hint rather than a claim — it freezes at server start like the
/// rest of the roster, which is why the header calls it a snapshot.
fn roster_lines(profiles: &[ProfileSnapshot], auth: &SessionAuth) -> String {
    let currencies = currency_order(profiles);
    let mut groups: Vec<(String, Vec<&ProfileSnapshot>)> = Vec::new();
    for p in profiles {
        let bracket = roster_bracket(p);
        match groups.iter_mut().find(|(b, _)| *b == bracket) {
            Some((_, members)) => members.push(p),
            None => groups.push((bracket, vec![p])),
        }
    }

    // Stable sorts throughout, so config order breaks every tie. Members first,
    // then groups by their best member — which is `first()` once members are
    // sorted.
    fn best(members: &[&ProfileSnapshot], currencies: &[&str]) -> (u8, usize, f64) {
        members
            .first()
            .map_or((2, 0, 0.0), |p| sort_key(p, currencies))
    }
    for (_, members) in &mut groups {
        members.sort_by(|a, b| cmp_key(sort_key(a, &currencies), sort_key(b, &currencies)));
    }
    groups.sort_by(|a, b| cmp_key(best(&a.1, &currencies), best(&b.1, &currencies)));

    let mut out = String::new();
    for (bracket, members) in &groups {
        let names: Vec<String> = members
            .iter()
            .map(|p| marker(p, auth).map_or_else(|| p.name.clone(), |m| format!("{}{m}", p.name)))
            .collect();
        out.push_str("- ");
        out.push_str(&names.join(", "));
        out.push(' ');
        out.push_str(bracket);
        out.push('\n');
    }
    out
}

/// One-line cached headline for a third-party profile from
/// `third_party_cache.json`: non-empty bars join as `label pct%`, else the first
/// funded wallet row (an empty wallet a two-wallet provider lists first must not
/// win the headline over the funded one), else the first stat row that carries a
/// value; the plan label prefixes the line when present. Value-less rows (e.g.
/// DeepSeek's `USD balance` heading) are skipped so the headline never renders a
/// dangling `label:` with nothing after it.
pub(crate) fn third_party_headline(s: &ThirdPartyStats) -> String {
    let body = if !s.bars.is_empty() {
        s.bars
            .iter()
            .map(|b| format!("{} {}", b.label, format_pct(b.pct)))
            .collect::<Vec<_>>()
            .join(", ")
    } else if let Some(wallet) = crate::providers::funded_wallets(&s.rows).into_iter().next() {
        format!("{}: {}", wallet.label, wallet.value)
    } else if let Some(row) = s.rows.iter().find(|r| !r.value.is_empty()) {
        if row.label.is_empty() {
            row.value.clone()
        } else {
            format!("{}: {}", row.label, row.value)
        }
    } else if !s.is_available {
        "unavailable".to_string()
    } else {
        String::new()
    };

    match (&s.plan, body.is_empty()) {
        (Some(plan), false) => format!("{plan}: {body}"),
        (Some(plan), true) => plan.clone(),
        (None, _) => body,
    }
}

/// What a `switch_profile` does to *this* session, keyed on how it reads its
/// credentials. A global session reads the exact file `switch_profile`
/// repoints; an isolated session (a `clauth start` runtime or a custom
/// `CLAUDE_CONFIG_DIR`) reads its own, so a switch can't disturb it. The
/// subject is the lead-in [`switch_effect_note`] adds — a client that shows
/// tool names only never sees a bare `switch`. Pure mapping — the caller
/// resolves the [`SessionAuth`].
pub(crate) fn switch_effect(auth: &SessionAuth) -> String {
    match auth {
        SessionAuth::Global => "repoints the global `~/.claude` credentials THIS \
session reads; Claude Code reloads them on its next token refresh, so this session would \
start acting as the switched profile mid-task. To use another account \
without disturbing this one, use the `delegate` tool."
            .to_string(),
        SessionAuth::IsolatedRuntime(name) => format!(
            "repoints the global `~/.claude` credentials, but THIS session runs in an \
isolated `clauth start` runtime pinned to `{name}` and is unaffected. Only a later session on \
the global credentials adopts the change."
        ),
        SessionAuth::IsolatedCustom => "repoints the global `~/.claude` credentials, but \
THIS session uses a custom `CLAUDE_CONFIG_DIR` and reads its own credentials, so it is \
unaffected. Only a later session on the global credentials adopts the change."
            .to_string(),
    }
}

/// [`switch_effect`] with its lead, carried by the `switch_profile` /
/// session-scope replies. The block no longer holds the full sentence: its tool
/// router carries the per-tier consequence clause alone
/// ([`switch_router_clause`]), so a client that drops the block still gets the
/// whole note from every reply.
pub(crate) fn switch_effect_note(auth: &SessionAuth) -> String {
    format!("switch_profile & this session: {}", switch_effect(auth))
}

/// How this session's runtime tree maps onto the real global one, for the only
/// tier that has such a tree. A `clauth start` runtime looks per-profile, so a
/// model editing `CLAUDE.md` or `skills/…` under it may believe the edit is
/// scoped. The note frames the consequence, and now states the transport too:
/// the caller probes the tree once (`runtime::link_mode_of`) and the note names
/// the answer. Pinning one mechanism is where the old note went false: a copy
/// host (Windows without symlink privilege) builds the tree by recursive copy,
/// so "mostly symlinks" was false there and a `readlink -f` nudge had nothing to
/// resolve. The two transports differ on new files too: a fresh file under a
/// symlink host's tree stays local and dies with the session, while the copy
/// mirror propagates one-sided files, so each transport arm states its own rule.
///
/// One arm per probe verdict: `Real` states the symlink transport, `Fake` the
/// copy transport, `Mixed` — the entries disagree — names both transports
/// (true under either), and `NothingShared` renders no note at all: a tree
/// sharing no entry has no mirror paths to describe, and the hedge would state
/// a rule for a layout the tree does not carry.
///
/// The note names `$CLAUDE_CONFIG_DIR` rather than a constructed path: the real
/// dir carries a per-session suffix (`runtime-<sid>`, the sid being `<pid>-<seq>`),
/// so any literal spelled here would point at a directory that does not exist.
/// It also names no destination past `~/.claude/`. Whether an entry there chains
/// on somewhere else is the operator's own layout rather than anything clauth
/// builds: this box reaches `~/.agents/skills` through a `~/.claude/skills`
/// symlink the operator made, and a box without it would be told a falsehood.
///
/// `Global` has no runtime dir, and `IsolatedCustom` is a foreign
/// `CLAUDE_CONFIG_DIR` whose layout clauth does not own, so neither may claim
/// this layout. Pure mapping; the caller resolves the [`SessionAuth`] and probes
/// the verdict.
pub(crate) fn runtime_paths_note(auth: &SessionAuth, probe: LinkProbe) -> Option<String> {
    let SessionAuth::IsolatedRuntime(name) = auth else {
        return None;
    };
    let transport = match probe {
        LinkProbe::Real => {
            "this host symlinks, so an edit to an existing file reaches the global file every \
profile loads. files you create here stay here and die with the session."
        }
        LinkProbe::Fake => {
            "this host keeps a copy, so an edit reaches the global file at the watchdog's sync \
cadence."
        }
        LinkProbe::Mixed => {
            "symlinks where the host allows them, a recursive copy the watchdog reconciles where \
it does not. so an edit reaches the global file every profile loads, instantly on a symlink \
host, at the watchdog's cadence on a copy host."
        }
        LinkProbe::NothingShared => return None,
    };
    Some(format!(
        "runtime paths: `$CLAUDE_CONFIG_DIR` (profile `{name}`) mirrors the global `~/.claude`. \
{transport} only `.claude.json`, `settings.json` and `.credentials.json` are this profile's own."
    ))
}

/// The block's first line: who this session is, resolved per tier. A `clauth
/// start` runtime names the profile it is pinned to and the account the global
/// link points at; a global session IS the global link, so the active profile
/// is the session's own account; a custom config dir names only the fact of
/// itself. The profile's provider comes off the snapshot the roster already
/// carries, so a reader meets one spelling per provider.
fn identity_line(profiles: &[ProfileSnapshot], auth: &SessionAuth) -> Option<String> {
    let active = profiles.iter().find(|p| p.active).map(|p| p.name.as_str());
    match auth {
        SessionAuth::IsolatedRuntime(name) => {
            let provider = profiles
                .iter()
                .find(|p| p.name == *name)
                .map(|p| p.provider.as_str());
            let mut line = match provider {
                Some(p) => format!("runtime profile: `{name}` ({p})"),
                None => format!("runtime profile: `{name}`"),
            };
            if let Some(active) = active {
                line.push_str(&format!(" · global active: `{active}`"));
            }
            Some(line)
        }
        SessionAuth::Global => active.map(|a| format!("global active: `{a}`")),
        SessionAuth::IsolatedCustom => Some("custom `CLAUDE_CONFIG_DIR`".to_string()),
    }
}

/// The `switch_profile` parenthetical in the block's tool router, resolved per
/// tier. A global session reads the very file the switch repoints, so it
/// follows on its next token refresh; an isolated runtime and a custom config
/// dir read their own credentials and are unaffected. The full consequence
/// sentence still rides the replies through [`switch_effect_note`]; the router
/// holds only this one clause.
fn switch_router_clause(auth: &SessionAuth) -> &'static str {
    match auth {
        SessionAuth::Global => {
            "repoints the global `~/.claude` credentials; this session follows on its next token \
refresh"
        }
        SessionAuth::IsolatedRuntime(_) | SessionAuth::IsolatedCustom => {
            "repoints the global `~/.claude` credentials; this session is unaffected"
        }
    }
}

/// One generic warning, every block: some providers answer a claude model name
/// with their own model, so `opus` is a tier alias rather than a guarantee. The
/// per-provider mapping is the operator's own workflow and stays out of the
/// block (owner ruling 2026-08-24).
const MODELS_NOTE: &str = "some providers alias claude model names to their own models \
(deepseek: `opus` -> `deepseek-v4-pro`).";

/// Init-time `instructions` block: identity intro, the session-resolved identity
/// line, a generic model-alias note, the runtime-path note that tier earns, a
/// one-line tool router, then the grouped roster. This block is the only clauth
/// text a session is guaranteed to hold: tool descriptions are deferred in some
/// harnesses and unloaded until searched for, so the router line stays even
/// though every tool carries its own description. Per-tool mechanics do NOT
/// stay — they live in that tool's description, which is loaded by the time
/// anyone can call it, and so does the `delegate` cost model. The full
/// session-effect sentence behind `switch_profile` rides the replies, not here:
/// the router carries the per-tier consequence clause alone. No usage
/// percentage or reset timer is baked in; those rot within a turn, so they live
/// in `profiles`.
pub(crate) fn instructions_block(
    profiles: &[ProfileSnapshot],
    auth: &SessionAuth,
    probe: LinkProbe,
) -> String {
    let mut out = String::new();
    out.push_str(
        "clauth manages multiple Claude Code accounts (\"profiles\"): each an isolated \
credential set / subscription. Use its tools to compare usage headroom across accounts, relink \
the active account, or delegate a task to another account without spending this session's \
window.\n\n",
    );
    if let Some(line) = identity_line(profiles, auth) {
        out.push_str(&line);
        out.push_str("\n\n");
    }
    out.push_str(MODELS_NOTE);
    out.push_str("\n\n");
    if let Some(note) = runtime_paths_note(auth, probe) {
        out.push_str(&note);
        out.push_str("\n\n");
    }
    out.push_str(&format!(
        "Tools: `profiles` (accounts + cached usage, zero quota; `scope:\"session\"` for this \
session's own), `switch_profile` ({}), `delegate` (run a task on another account; the only tool \
that spends), `monitor` (check, collect or stop a backgrounded delegate, or wait on clauth's \
state).\n\n",
        switch_router_clause(auth),
    ));
    out.push_str(
        "Profiles, most headroom first (session-start snapshot; call `profiles` for live usage \
and anything added since):\n",
    );
    out.push_str(&roster_lines(profiles, auth));
    out
}

// ── prose spellings (`format: "prose"` is the default) ──────────────────────
//
// Each tool's JSON payload has exactly one prose spelling, produced here. The
// contract: prose names what carries news. A boolean flag appears as a word
// only when true (a spelled-out `false` costs tokens for nothing), telemetry a
// reader cannot act on (a sample count) stays in the JSON spelling, a null
// number reads as `unknown` (never `0%` or an omission a reader takes for
// `none`), and no figure appears that the payload did not have. Raw timestamps
// are named, not re-derived into `resets in N` — a derived figure is one the
// JSON did not carry.

/// A window's share as a prose clause: `12% used` for a number, `unknown` for
/// `None` (so a null reads as unknown, never as `unknown used`).
fn pct_clause(v: Option<f64>) -> String {
    v.map_or_else(
        || "unknown".to_string(),
        |p| format!("{} used", format_pct(p)),
    )
}

/// The folded-in `live_usage` object as a sentence clause. `lead` is the noun
/// for the profile it names: `active profile` for the session-scope roster and
/// `switch_profile`, `target` for `delegate`.
///
/// Three readings the clause keeps apart, because collapsing any two of them
/// tells the reader clauth lost something it holds: no profile at all reads
/// `none` and names no window (there is no account whose windows could be
/// reported); a third-party account with a figure reads whichever headroom
/// [`windows_prose`] renders for it; an account with nothing cached — OAuth or
/// third-party alike — reads `unknown`.
pub(crate) fn live_usage_prose(lu: &Value, lead: &str) -> String {
    let Some(name) = lu.get("profile").and_then(Value::as_str) else {
        return format!("{lead} none");
    };
    let mut out = format!("{lead} `{name}`: ");
    if lu.get("kind").and_then(Value::as_str) == Some("third_party") {
        // Same payload keys `windows_prose` reads, so the two surfaces cannot
        // spell one account's headroom two ways.
        out.push_str(&windows_prose(lu));
    } else {
        let five = lu.get("5h_used_pct").and_then(Value::as_f64);
        let seven = lu.get("7d_used_pct").and_then(Value::as_f64);
        out.push_str(&format!(
            "5h {}, 7d {}",
            pct_clause(five),
            pct_clause(seven)
        ));
        // An age dates a FIGURE. With neither window cached there is no figure
        // to date, and stamping the cache's age onto two `unknown`s would read
        // as a measurement clauth does not have.
        if five.is_some() || seven.is_some() {
            out.push_str(&freshness_clause(lu));
        }
    }
    if let Some(w) = lu.get("throughput_warning").and_then(Value::as_str) {
        out.push_str("; ");
        out.push_str(w);
    }
    out
}

/// One profile name in the digest's from/to pair: backticked for a name,
/// `none` for a null (no active profile configured — the same read
/// [`live_usage_prose`] gives a null profile).
fn digest_name(v: Option<&Value>) -> String {
    v.and_then(Value::as_str)
        .map_or_else(|| "none".to_string(), |n| format!("`{n}`"))
}

/// The folded-in `since_your_last_call` object as a sentence clause: one part
/// per observable that carries news, exactly the keys the JSON spelling kept.
/// The two mtime observables have no figure a reader acts on, so their part
/// names what happened (`refreshed` / `rewritten`), never the timestamp.
pub(crate) fn digest_prose(d: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(ap) = d.get("active_profile") {
        parts.push(format!(
            "active profile {} → {}",
            digest_name(ap.get("from")),
            digest_name(ap.get("to"))
        ));
    }
    if d.get("usage_cache")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        parts.push("usage cache refreshed".to_string());
    }
    if d.get("credentials")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        parts.push("credentials file rewritten".to_string());
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("since your last call: {}", parts.join("; "))
}

/// The state-waiting mode's reply: the change it caught, the baseline it
/// armed, or the wait that found nothing, then the delegates clauth is holding.
/// Self-labels `monitor` — the reply names the tool that can be called again,
/// and a label naming a tool the handshake does not list sends the model
/// searching for one.
pub(crate) fn monitor_state_prose(p: &Value) -> String {
    let state = match p.get("status").and_then(Value::as_str) {
        Some("changed") => format!("monitor: {}", digest_prose(&p["since_your_last_call"])),
        Some("armed") => {
            "monitor armed: baseline set on this first digest call, nothing to compare against yet"
                .to_string()
        }
        _ => {
            let waited = p.get("waited_secs").and_then(Value::as_u64).unwrap_or(0);
            format!("monitor: no change after {waited}s")
        }
    };
    let listing = jobs_listing_prose(p);
    if listing.is_empty() {
        return state;
    }
    format!("{state}\n{listing}")
}

/// The delegate jobs clauth is holding, one line each, or nothing at all when
/// it holds none.
///
/// Empty rather than a "no jobs" line: a session that never delegated should
/// pay nothing for a listing it has no use for, which is the only-when-true rule
/// `profile_line`'s own flags render by. Each line opens with ``job `<id>` `` —
/// the same opener `monitor_batch_prose` uses — so the id a caller has
/// to copy out is always in the same place.
///
/// One age per row and no tail, no quota and no deadline countdown: this
/// enumerates so a caller can NAME a job, and `monitor({job_ids})` is the check
/// that reports what one is doing. Ten rows of the full running line would cost
/// more than the reply it rides on.
fn jobs_listing_prose(p: &Value) -> String {
    let Some(rows) = p.get("jobs").and_then(Value::as_array) else {
        return String::new();
    };
    if rows.is_empty() {
        return String::new();
    }
    let mut out = String::from("delegates clauth holds:");
    for row in rows {
        let job_id = row
            .get("job_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let state = row
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        out.push_str(&format!("\n  job `{job_id}` {state}"));
        if let Some(profile) = row.get("profile").and_then(Value::as_str) {
            out.push_str(&format!(" on `{profile}`"));
        }
        // Said here rather than left to the collect refusal, because the refusal
        // costs the caller a whole turn to learn a fact this row can carry in
        // five words.
        if state == "blocking" {
            out.push_str(" (its own caller takes the result)");
        }
        out.push_str(&age_phrase(row));
    }
    // Guarded here as well as at the producer, because the only-when-true rule
    // belongs to each layer: a `+0 older not listed` line is a claim about
    // nothing, and this renderer answers for whatever payload it is handed.
    match p.get("jobs_not_listed").and_then(Value::as_u64) {
        Some(rest) if rest > 0 => out.push_str(&format!("\n  +{rest} older not listed")),
        _ => {}
    }
    out
}

/// The one age a listing row carries, named for the question that row's state
/// makes worth asking: a live run's is how long it has been going, a finished
/// one's how long its result has been sitting there, an orphan's how long ago
/// anything last wrote to it.
fn age_phrase(row: &Value) -> String {
    // `humanize_span`, never `humanize_duration`: every figure here is a SPAN,
    // and at zero that function spells `now`, which renders `elapsed now` and
    // `finished now ago`. A job that finished under a second ago and a fan-out
    // member that just launched are both routine.
    if let Some(secs) = row.get("elapsed_secs").and_then(Value::as_u64) {
        return format!(", elapsed {}", humanize_span(secs));
    }
    let Some(secs) = row.get("since_secs").and_then(Value::as_u64) else {
        return String::new();
    };
    let when = humanize_span(secs);
    match row.get("state").and_then(Value::as_str) {
        Some("orphaned") => format!(", last seen {when} ago"),
        _ => format!(", finished {when} ago"),
    }
}

/// How old the figures in a headroom payload are, and whether that is past
/// anything a live scheduler produces. Dating a figure is what lets a reader
/// discount it; suppressing a stale one would turn a known-old number into no
/// number, which reads as clauth having lost track of the account. A payload
/// carrying no age at all (the roster, which spends no tokens dating rows that
/// are current) still says `stale` when it is.
fn freshness_clause(v: &Value) -> String {
    let stale = v.get("stale").and_then(Value::as_bool).unwrap_or(false);
    let Some(secs) = v.get("fetched_secs_ago").and_then(Value::as_u64) else {
        return if stale {
            " (stale)".to_string()
        } else {
            String::new()
        };
    };
    let when = if secs == 0 {
        "just now".to_string()
    } else {
        format!("{} ago", humanize_duration(secs as i64))
    };
    if stale {
        format!(" (cached {when}, stale)")
    } else {
        format!(" (cached {when})")
    }
}

/// The age half of [`freshness_clause`] alone, for a line whose `stale` marker
/// already renders beside the figure it dates: the age rides, the stale word
/// does not repeat.
fn age_clause(v: &Value) -> String {
    let Some(secs) = v.get("fetched_secs_ago").and_then(Value::as_u64) else {
        return String::new();
    };
    let when = if secs == 0 {
        "just now".to_string()
    } else {
        format!("{} ago", humanize_duration(secs as i64))
    };
    format!(" (cached {when})")
}

/// The headroom clause, off the discriminated payload
/// [`crate::profile_json::ProfileWindows`] produces: an OAuth account's windows,
/// or a third-party account's own figures in place of a pool it does not draw
/// on. `unknown` answers for an empty cache on either side and for nothing
/// else — no cache is not a zero. A third-party account with no figure yet says
/// only that, no denial: what clauth cannot answer for is the provider's own
/// limits, which is exactly what the denial would be claiming to know.
///
/// A third-party account is told it has no 5h/7d limit only when clauth knows
/// it has none. A provider that publishes usage windows of its own (z.ai,
/// Alibaba) HAS the limits whether or not this one response carried any, so a
/// denial beside its figure is false; a provider answering with a wallet or a
/// counter (DeepSeek, ollama, a generic endpoint) has none, and saying so is
/// what stops its figure reading as one more window someone can wait out. The
/// split is the payload's `provider_windows` flag, decided from the provider
/// plus the response's bars at the source — matching the rendered figure for a
/// `5h` substring would make the copy decide its own meaning.
///
/// A freshness clause rides the FIGURE it dates and nothing else: stamping a
/// cache's age onto `unknown` asserts a measurement clauth does not have.
fn windows_prose(windows: &Value) -> String {
    match windows.get("kind").and_then(Value::as_str) {
        Some("third_party") => {
            let Some(figure) = windows
                .get("balance")
                .and_then(Value::as_str)
                .filter(|b| !b.is_empty())
            else {
                return "usage unknown".to_string();
            };
            let mut out = if windows
                .get("provider_windows")
                .and_then(Value::as_bool)
                .unwrap_or(true)
            {
                figure.to_string()
            } else {
                format!("no 5h/7d limits; {figure}")
            };
            out.push_str(&freshness_clause(windows));
            out
        }
        Some("oauth") => {
            let ws = windows
                .get("windows")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            if ws.is_empty() {
                return "usage unknown".to_string();
            }
            let mut out = ws
                .iter()
                .map(|w| {
                    let label = w.get("label").and_then(Value::as_str).unwrap_or("unknown");
                    let pct = w.get("utilization_pct").and_then(Value::as_f64);
                    let mut s = format!("{label} {}", pct_clause(pct));
                    if let Some(epoch) = w
                        .get("resets_at")
                        .and_then(Value::as_str)
                        .and_then(iso_to_epoch_secs)
                    {
                        let remaining = epoch - now_epoch_secs();
                        // A reset already past is a stale reading `freshness_clause`
                        // marks; `resets at <past> · now` would claim a reset that
                        // already happened and a false countdown, so drop it.
                        if remaining > 0
                            && let Some(stamp) = local_stamp(epoch)
                        {
                            let countdown = humanize_duration(remaining);
                            s.push_str(&format!(" (resets at {stamp} · {countdown})"));
                        }
                    }
                    s
                })
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&freshness_clause(windows));
            out
        }
        _ => "usage unknown".to_string(),
    }
}

/// Per-model throughput rows (`which`'s full summary or a roster's warnings).
/// A healthy row is the model's name and rate; `degraded` and the rate-limit
/// flag appear as words only when true, the retry delay with them. The sample
/// count is clauth's own confidence telemetry, not a figure a reader acts on,
/// so it stays in the JSON spelling. A row whose store key was the `default`
/// placeholder carries no `model` field at all (`throughput_row` omits it) and
/// renders the rate alone — the same nameless reading the delegate warning
/// gives.
fn throughput_prose(rows: &[Value]) -> String {
    rows.iter()
        .map(|m| {
            let named = m.get("model").and_then(Value::as_str);
            let tok_s = m
                .get("tok_s")
                .and_then(Value::as_f64)
                .map_or_else(|| "unknown".to_string(), |v| v.to_string());
            let mut flags: Vec<String> = Vec::new();
            if m.get("degraded").and_then(Value::as_bool).unwrap_or(false) {
                flags.push("degraded".to_string());
            }
            if m.get("rate_limited_recent")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                flags.push("rate-limited recently".to_string());
                if let Some(r) = m.get("retry_after_s").and_then(Value::as_u64) {
                    flags.push(format!("retry in {r}s"));
                }
            }
            let mut s = match named {
                Some(model) => format!("`{model}` {tok_s} tok/s"),
                None => format!("{tok_s} tok/s"),
            };
            if !flags.is_empty() {
                s.push_str(&format!(" ({})", flags.join(", ")));
            }
            s
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// One roster row as a prose line: name + active marker, the
/// `[provider, tier, host]` bracket with whatever [`host_locality`] marker its
/// host earns, this account's own headroom, then the quiet flags. A null tier
/// reads `unknown` on an account that HAS a plan tier and drops out on one that
/// structurally has none.
///
/// The tier guard asks the headroom payload's `kind`, never the display
/// `provider`: [`crate::profile_json::provider_label`] renders every
/// unrecognised endpoint as `anthropic`, so a generic api-key account (a local
/// llama, an aggregator) would be told its Anthropic plan tier is unknown when
/// it has no Anthropic plan at all.
fn profile_line(row: &Value) -> String {
    let name = row.get("name").and_then(Value::as_str).unwrap_or("unknown");
    let active = row.get("active").and_then(Value::as_bool).unwrap_or(false);
    let provider = row
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let tier = row.get("tier").and_then(Value::as_str);
    let host = row.get("host").and_then(Value::as_str);

    let mut bracket = vec![provider.to_string()];
    if let Some(t) = tier {
        bracket.push(t.to_string());
    }
    if let Some(h) = host {
        bracket.push(h.to_string());
        if let Some(marker) = host_locality(h) {
            bracket.push(marker.to_string());
        }
    }

    let mut out = format!(
        "- {}{} [{}]: {}",
        name,
        if active { " (global active)" } else { "" },
        bracket.join(", "),
        windows_prose(&row["windows"]),
    );

    // A null tier is structural for an account whose usage lives in the
    // third-party cache; on a subscription account it means the plan is unknown.
    let api_key_account = row["windows"].get("kind").and_then(Value::as_str) == Some("third_party");
    if tier.is_none() && !api_key_account {
        out.push_str("; tier unknown");
    }
    if row
        .get("has_live_session")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        out.push_str("; live session");
    }
    // The three account-state markers render as one contiguous run, so a
    // reader meets one group rather than three scattered through the line.
    // Which of them refuses a delegate, and on which exemption, is
    // `preflight_target`'s rule. `canceled` follows them because clauth has no
    // cancel gate: it informs the pick, it does not block it.
    if row
        .get("disabled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        out.push_str("; disabled");
    }
    if row
        .get("auth_broken")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        out.push_str("; login expired");
    }
    if row.get("keyless").and_then(Value::as_bool).unwrap_or(false) {
        out.push_str("; no api key");
    }
    if row
        .get("canceled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        out.push_str("; subscription canceled");
    }
    if let Some(rows) = row.get("throughput").and_then(Value::as_array)
        && !rows.is_empty()
    {
        out.push_str("; throughput: ");
        out.push_str(&throughput_prose(rows));
    }
    out
}

/// Prose for `profiles`. The all-scope roster is one `profile_line` per
/// profile; the session-scope arm is the folded-in former `which`: the one row
/// THIS session resolves to, rendered through the same `profile_line` (so it
/// inherits the roster's own guards), then how it resolved, then live usage and
/// the digest.
pub(crate) fn profiles_prose(p: &Value) -> String {
    if p.get("ok").and_then(Value::as_bool) == Some(false) {
        return format!(
            "error: {}",
            p.get("reason").and_then(Value::as_str).unwrap_or("unknown")
        );
    }
    let Some(rows) = p.get("profiles").and_then(Value::as_array) else {
        return "unknown".to_string();
    };
    if p.get("scope").and_then(Value::as_str) == Some("session") {
        // One row at most, with how it resolved, then the folded live usage
        // and digest (the roster arm carries neither).
        let row = rows.first();
        let (mut out, source) = match row {
            Some(row) => {
                let line = profile_line(row);
                let source = row.get("source").and_then(Value::as_str);
                (line, source)
            }
            // No row: `resolve_active` found nothing, which is an unresolved
            // session rather than an empty roster.
            None => ("session profile unknown, source unknown".to_string(), None),
        };
        // The live-usage fold names the CONFIGURED active profile, which this
        // scope's row need not be. When they are the same account the clause
        // restates the row's own headroom word for word, and the row already
        // marks it `(global active)`, so the second copy is dropped rather than
        // rendered twice on one line. What must not drop with it is the age:
        // the row's figures are the ones the clause would date, and the row
        // renders `stale` itself, so the age rides the row BEFORE the source
        // clause, whose text it would otherwise read as dating.
        let lu = p.get("live_usage");
        let same_account = matches!(
            (
                row.and_then(|r| r.get("name")).and_then(Value::as_str),
                lu.and_then(|lu| lu.get("profile")).and_then(Value::as_str),
            ),
            (Some(row_name), Some(active)) if row_name == active
        );
        if same_account && let Some(lu) = lu {
            out.push_str(&age_clause(lu));
        }
        if let Some(source) = source {
            out.push_str(&format!("; source `{source}`"));
        }
        if let Some(lu) = lu.filter(|_| !same_account) {
            out.push_str("; ");
            out.push_str(&live_usage_prose(lu, "active profile"));
        }
        let digest = digest_prose(&p["since_your_last_call"]);
        if !digest.is_empty() {
            out.push_str("; ");
            out.push_str(&digest);
        }
        return out;
    }
    if rows.is_empty() {
        return "no profiles".to_string();
    }
    rows.iter().map(profile_line).collect::<Vec<_>>().join("\n")
}

/// Prose for `switch_profile`: the outcome, then the active profile's live
/// usage, then the digest clause when the payload carries one.
pub(crate) fn switch_profile_prose(p: &Value) -> String {
    let live = live_usage_prose(&p["live_usage"], "active profile");
    let digest = digest_prose(&p["since_your_last_call"]);
    let digest = if digest.is_empty() {
        String::new()
    } else {
        format!("; {digest}")
    };
    match p.get("ok").and_then(Value::as_bool) {
        Some(true) => {
            // A null `previous` is the logged-out state the switch started from
            // (clauth knows there was none), not a figure clauth lost.
            let previous = p
                .get("previous")
                .and_then(Value::as_str)
                .map_or_else(|| "none".to_string(), |v| format!("`{v}`"));
            let active = p
                .get("active")
                .and_then(Value::as_str)
                .map_or_else(|| "unknown".to_string(), |v| format!("`{v}`"));
            format!(
                "switched the global active profile from {previous} to {active}; {live}{digest}"
            )
        }
        _ => {
            let reason = p.get("reason").and_then(Value::as_str).unwrap_or("unknown");
            format!("switch failed: {reason}; {live}{digest}")
        }
    }
}

/// The whole usage clause renders within this many characters. The real
/// envelope renders 117 and the composite-heavy one 254, while the pre-change
/// line ran ~700 characters of mostly zeros; 320 sits far above every
/// envelope this project has observed and cuts only pathological ones.
const USAGE_BUDGET: usize = 320;

/// The token-usage object of a delegate envelope as one clause. The two fields
/// clauth's envelope contract documents always render and read as English
/// (`input N tokens`), or `input unknown tokens` when the wire carries no
/// number, because a run that produced no output is real signal and the
/// clause never drops. Every other key renders one clause per surviving
/// figure, named by its dotted path from the top-level key, recursing into
/// objects and arrays alike (`output_tokens_details.thinking_tokens`,
/// `iterations.0.tokens`). The rule is "no FIGURE vanishes": a zero number, a
/// string that parses as zero, an empty string, a null, a `false` flag, and a
/// composite whose leaves all carry no figure drop, so no raw JSON reaches the
/// reply. The one deliberate omission is the cache total: Anthropic's
/// `cache_creation_input_tokens` is the SUM of the `cache_creation.*`
/// breakdown leaves, so when the total equals its breakdown the same figure
/// would print twice, and the total drops, leaving the leaves; a total that
/// disagrees with its breakdown renders alongside it, because dropping it
/// would hide a figure. A string that parses as a number IS the figure,
/// because clauth fronts third-party proxies that stringify numerics. The
/// dotted path locates a figure for reading, never for round-tripping: a
/// dotted key and a nesting render the same (`{"a.b":1}` and `{"a":{"b":1}}`),
/// and an array index joins the path the same way, so `{"a":[1]}` and
/// `{"a":{"0":1}}` collide too; an empty or all-whitespace key segment reads
/// `(unnamed)` rather than a blank span, so a figure whose key is blank still
/// renders a name a reader can act on. Survivor order is claude's wire
/// order, which `serde_json`'s `preserve_order` feature keeps in the object
/// map. The joined clause is then cut to `USAGE_BUDGET` characters, ending
/// with `…` only on overflow.
fn usage_prose(u: &Value) -> String {
    let Some(obj) = u.as_object() else {
        return "unknown".to_string();
    };
    let cache_total_is_sum = cache_total_equals_breakdown(obj);
    let mut clauses: Vec<String> = Vec::new();
    for (k, v) in obj {
        match k.as_str() {
            // A run that produced no output is a real run: the two documented
            // fields render even at zero, and say `unknown` rather than drop
            // when the wire carries no number at all.
            "input_tokens" | "output_tokens" => {
                let noun = if k == "input_tokens" {
                    "input"
                } else {
                    "output"
                };
                let figure = match v {
                    Value::Number(n) => Some(n.to_string()),
                    Value::String(s) => string_figure(s).map(|(num, _)| num),
                    _ => None,
                };
                match figure {
                    Some(n) => clauses.push(format!("{noun} {n} tokens")),
                    None => clauses.push(format!("{noun} unknown tokens")),
                }
            }
            // The total equals its breakdown: the leaves carry the figure.
            "cache_creation_input_tokens" if cache_total_is_sum => {}
            _ => leaf_clauses(path_segment(k), v, &mut clauses),
        }
    }
    truncate_clause(clauses.join(", "), USAGE_BUDGET)
}

/// Whether the usage object's `cache_creation_input_tokens` total equals the
/// sum of the numeric leaves of its `cache_creation` breakdown, so the same
/// figure would print twice. A stringified figure counts as its numeric twin
/// (proxies stringify numerics), and a breakdown with no parseable leaf is not
/// a match, so a missing or partial breakdown never hides the total.
fn cache_total_equals_breakdown(obj: &serde_json::Map<String, Value>) -> bool {
    let Some(total) = obj
        .get("cache_creation_input_tokens")
        .and_then(value_as_number)
    else {
        return false;
    };
    let Some(breakdown) = obj.get("cache_creation").and_then(Value::as_object) else {
        return false;
    };
    let mut sum = 0.0;
    let mut any = false;
    for v in breakdown.values() {
        if let Some(n) = value_as_number(v) {
            sum += n;
            any = true;
        }
    }
    any && sum == total
}

/// A JSON number or a string that parses as a finite one, as a figure. The
/// string arm is [`string_figure`]'s own, so a `"26800"` reads as its numeric
/// twin.
fn value_as_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => string_figure(s).map(|(_, n)| n),
        _ => None,
    }
}

/// One dotted-path segment. A key that is empty or all whitespace names
/// nothing, so the segment that would render as a blank span reads `(unnamed)`
/// instead — the figure stays visible with a name a reader can act on. A key
/// with real content keeps its own spelling, edge whitespace included. A
/// literal key spelled `(unnamed)` collides with the sentinel, and any
/// borrowed name collides with some spellable key, so no figure vanishes
/// either way.
fn path_segment(seg: &str) -> &str {
    if seg.trim().is_empty() {
        "(unnamed)"
    } else {
        seg
    }
}

/// A string that spells a finite number, or `None`. `"83930"`, `" 83930 "`
/// and `"0"` all parse; the returned string is the trimmed spelling, so a
/// stringified figure reads exactly as its numeric twin would. `"NaN"` and
/// `"inf"` parse as numbers Rust can hold but not show, so both are `None`.
fn string_figure(s: &str) -> Option<(String, f64)> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let n = t.parse::<f64>().ok()?;
    if !n.is_finite() {
        return None;
    }
    Some((t.to_string(), n))
}

/// Walk one usage value to its surviving figures, pushing one clause each. An
/// object key or array index joins the path with a dot; arrays recurse, so a
/// figure inside an array keeps its own path. An empty or all-whitespace key
/// segment reads `(unnamed)` rather than a blank span. A composite with no
/// surviving figure pushes nothing, and its top-level key drops.
fn leaf_clauses(path: &str, v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Array(a) => {
            for (i, child) in a.iter().enumerate() {
                leaf_clauses(&format!("{path}.{i}"), child, out);
            }
        }
        Value::Object(o) => {
            for (k, child) in o {
                leaf_clauses(&format!("{path}.{}", path_segment(k)), child, out);
            }
        }
        Value::Number(n) => {
            if n.as_f64() != Some(0.0) {
                out.push(format!("`{path}` {n}"));
            }
        }
        Value::String(s) => {
            if let Some((num, val)) = string_figure(s) {
                if val != 0.0 {
                    out.push(format!("`{path}` {num}"));
                }
            } else if !s.is_empty() {
                out.push(format!("`{path}` {s}"));
            }
        }
        // A set flag is signal; an unset one is the boolean twin of the zero
        // this function drops, so `false` drops and only `true` reads `set`.
        Value::Bool(true) => out.push(format!("`{path}` set")),
        Value::Bool(false) | Value::Null => {}
    }
}

/// Cut the clause to `budget` characters, ending a cut clause with a single
/// `…`. No marker and no count: this is a one-line summary, so a pathological
/// usage object is cut rather than allowed to dominate the reply. The JSON
/// spelling is internal-only, so a cut figure is not recoverable by the
/// caller.
///
/// The cut walks Unicode SCALAR values, so it never lands mid-scalar and the
/// output is always valid UTF-8; it can split a grapheme cluster (a combining
/// sequence or a ZWJ emoji can be cut between its scalars), because the crate
/// carries no grapheme segmentation. A budget of 0 cuts every non-empty
/// clause to the marker alone.
fn truncate_clause(clause: String, budget: usize) -> String {
    if clause.chars().count() <= budget {
        clause
    } else {
        let mut cut: String = clause.chars().take(budget.saturating_sub(1)).collect();
        cut.push('…');
        cut
    }
}

/// A cost in dollars with the raw f64 tail trimmed: four decimals, trailing
/// zeros dropped. A value that rounds to `0` reads `0`, except a positive one,
/// which reads `<0.0001` because a cheap run is not a free one.
fn fmt_cost(cost: f64) -> String {
    let rounded = (cost * 10_000.0).round() / 10_000.0;
    if rounded == 0.0 {
        return if cost > 0.0 {
            "<0.0001".to_string()
        } else {
            "0".to_string()
        };
    }
    format!("{rounded:.4}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

/// The `permission_denials` field as a clause body, or `None` when the clause
/// drops (absent, null, an empty list, or an empty string). A string renders
/// as its own text. A list renders named tools once in first-seen order with
/// a `N times` count when repeated, then any nameless entries as `N unnamed
/// entries` after the named ones — `unnamed` is a spellable tool name, so the
/// count keeps the synthetic group out of that namespace, and `entry` /
/// `entries` pluralizes the group's own count. A present value of any other
/// shape reads `(unreadable)`, so a denial the envelope carried is never
/// invisible.
fn denial_names(denials: Option<&Value>) -> Option<String> {
    let value = denials?;
    if value.is_null() {
        return None;
    }
    if let Value::String(s) = value {
        return (!s.is_empty()).then(|| s.clone());
    }
    let Some(arr) = value.as_array() else {
        return Some("(unreadable)".to_string());
    };
    if arr.is_empty() {
        return None;
    }
    let mut named: Vec<(String, usize)> = Vec::new();
    let mut unnamed = 0usize;
    for entry in arr {
        let name = entry
            .get("tool_name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match name {
            Some(n) => match named.iter_mut().find(|(x, _)| x == n) {
                Some((_, count)) => *count += 1,
                None => named.push((n.to_string(), 1)),
            },
            None => unnamed += 1,
        }
    }
    let mut parts: Vec<String> = named
        .into_iter()
        .map(|(n, c)| if c > 1 { format!("{n} {c} times") } else { n })
        .collect();
    if unnamed > 0 {
        parts.push(if unnamed == 1 {
            "1 unnamed entry".to_string()
        } else {
            format!("{unnamed} unnamed entries")
        });
    }
    Some(parts.join(", "))
}

/// Prose for a delegate envelope: the verdict (`finished` / `failed` / `timed
/// out`), the self-report, cost and tokens, then the kill/resume markers. The
/// raw envelope may carry more of claude's own fields; those stay in the JSON
/// spelling, and this names the fields clauth documents.
pub(crate) fn envelope_prose(e: &Value) -> String {
    let mut out = String::new();
    let ran_for = || {
        e.get("elapsed_secs")
            .and_then(Value::as_u64)
            .map_or_else(String::new, |el| format!(" after {el}s"))
    };
    // A cancel is read first and on its own key: it is a decision rather than a
    // deadline, so a cancelled envelope carries no `timed_out` for the arm below
    // to find, and "failed" would be the wrong word for a stop the caller asked
    // for.
    if e.get("cancelled").and_then(Value::as_bool).unwrap_or(false) {
        out.push_str("cancelled");
        out.push_str(&ran_for());
    } else if let Some(t) = e.get("timed_out").and_then(Value::as_str) {
        out.push_str(&format!("timed out ({t})"));
        out.push_str(&ran_for());
    } else if e.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
        out.push_str("failed");
    } else {
        out.push_str("finished");
    }
    out.push_str(": ");
    // A bare scalar self-report (a non-object envelope the fold wrapped under
    // `result`) arrives as its own type; read it as its literal so a number or
    // bool never drops to `unknown`.
    out.push_str(&match e.get("result") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => "unknown".to_string(),
    });

    if let Some(cost) = e.get("total_cost_usd").and_then(Value::as_f64) {
        let cost_s = fmt_cost(cost);
        // `total_cost_usd` is the CHILD CLI's own figure, priced against
        // Anthropic's card whatever endpoint served the call, so a DeepSeek or
        // z.ai target's number is a wrong-basis figure a caller reads as the
        // bill. Which endpoint answered is `delegate_call_endpoint`'s answer,
        // arriving as data through the fold — this file derives no figure the
        // JSON did not carry.
        //
        // Three readings, kept apart for the same reason `live_usage_prose`
        // keeps its three: only a POSITIVE `anthropic` earns the bare clause;
        // a named other endpoint earns the equivalence clause, which states the
        // figure is the Anthropic-card price; an unfolded envelope, or a target
        // clauth could not classify, knows no name, so it keeps the clause and
        // adds `endpoint unknown`.
        match e
            .get("live_usage")
            .and_then(|lu| lu.get("endpoint"))
            .and_then(Value::as_str)
        {
            Some("anthropic") => out.push_str(&format!(" (cost ${cost_s})")),
            Some(_) => out.push_str(&format!(" (equivalent Anthropic API rate cost: ${cost_s})")),
            None => out.push_str(&format!(
                " (equivalent Anthropic API rate cost: ${cost_s}, endpoint unknown)"
            )),
        }
    }
    if let Some(u) = e.get("usage") {
        let tokens = usage_prose(u);
        if !tokens.is_empty() {
            out.push_str(&format!(", usage: {tokens}"));
            // The child's token counts are its own self-report. The tokenizer
            // is whichever model actually ran. `live_usage.provider`, the
            // call-resolved label, names who served it. A non-anthropic label
            // qualifies the bytes so the count is not read as Anthropic's
            // tokenization.
            if let Some(p) = e
                .get("live_usage")
                .and_then(|lu| lu.get("provider"))
                .and_then(Value::as_str)
                && !p.is_empty()
                && p != "anthropic"
            {
                out.push_str(&format!(" (served by {p})"));
            }
        }
    }
    if let Some(p) = e.get("partial_result").and_then(Value::as_str) {
        out.push_str(&format!("; partial result: {p}"));
    }
    if let Some(sid) = e.get("session_id").and_then(Value::as_str) {
        out.push_str(&format!("; resume with session id `{sid}`"));
    }
    if let Some(names) = denial_names(e.get("permission_denials")) {
        out.push_str(&format!("; permission denials: {names}"));
    }
    out
}

/// One `delegate` sync-envelope row: the target account and the envelope prose,
/// then that account's live-usage clause. No digest — the digest is the reply's
/// own, folded once by the caller that owns the whole reply.
fn delegate_row(p: &Value) -> String {
    let target = p
        .get("live_usage")
        .and_then(|lu| lu.get("profile"))
        .and_then(Value::as_str);
    let mut out = match target {
        Some(t) => format!("delegate to `{t}` {}", envelope_prose(p)),
        None => {
            let profile = p
                .get("profile")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            format!("delegate to `{profile}` {}", envelope_prose(p))
        }
    };
    if let Some(lu) = p.get("live_usage") {
        out.push_str("; ");
        out.push_str(&live_usage_prose(lu, "target"));
    }
    out
}

/// Prose for `delegate`: the background handle or the sync envelope. Both carry
/// the folded live-usage footer and the digest — a handle is a reply about a
/// spend that just started, and the caller's next routing decision needs the
/// same headroom the blocking reply hands back.
pub(crate) fn delegate_prose(p: &Value) -> String {
    if let Some(job_id) = p.get("job_id").and_then(Value::as_str) {
        let profile = p
            .get("profile")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let status = p.get("status").and_then(Value::as_str).unwrap_or("unknown");
        // A raw start epoch carries no news a reader acts on; the JSON spelling
        // keeps it. The handle's own spelling is unchanged: the bundled
        // `asyncRewake` hook scans this prose for `d-<base36-ms>-<n>` tokens.
        let mut out = format!("delegate to `{profile}` {status}, job `{job_id}`");
        if let Some(lu) = p.get("live_usage") {
            out.push_str("; ");
            out.push_str(&live_usage_prose(lu, "target"));
        }
        let digest = digest_prose(&p["since_your_last_call"]);
        if !digest.is_empty() {
            out.push_str("; ");
            out.push_str(&digest);
        }
        return out;
    }
    let mut out = delegate_row(p);
    let digest = digest_prose(&p["since_your_last_call"]);
    if !digest.is_empty() {
        out.push_str("; ");
        out.push_str(&digest);
    }
    out
}

/// Prose for a `delegate` argument/validation refusal. A refusal that fired
/// before target resolution carries the targets the caller spelled, so the
/// sentence names them; an envelope with no `profiles` (a refusal before any
/// target was named) reads plainly.
pub(crate) fn delegate_refusal_prose(p: &Value) -> String {
    let reason = p.get("result").and_then(Value::as_str).unwrap_or("unknown");
    match p.get("profiles").and_then(Value::as_array) {
        Some(names) => {
            let list = names
                .iter()
                .filter_map(Value::as_str)
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("delegate to {list} failed: {reason}")
        }
        None => format!("delegate failed: {reason}"),
    }
}

/// Prose for a `delegate` `profiles` fan-out: one job per named account, echoing
/// the resolved target list so the caller sees what it just spent, then each
/// target's own headroom.
///
/// The headroom clauses follow the id list rather than sitting inside each
/// parenthesis: the ids and the account names are what the caller (and the
/// `asyncRewake` hook) reads first, and a footer spliced between them would
/// bury the handles. The digest is the reply's, not a row's: it is folded once
/// at the top level, on `DigestMode`'s reporting rule.
pub(crate) fn delegate_fanout_prose(p: &Value) -> String {
    let jobs = p
        .get("jobs")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut out = String::from("delegated to ");
    for (i, job) in jobs.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let profile = job
            .get("profile")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let job_id = job
            .get("job_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        out.push_str(&format!("`{profile}` (job `{job_id}`)"));
    }
    for job in jobs {
        if let Some(lu) = job.get("live_usage") {
            out.push_str("; ");
            out.push_str(&live_usage_prose(lu, "target"));
        }
    }
    let digest = digest_prose(&p["since_your_last_call"]);
    if !digest.is_empty() {
        out.push_str("; ");
        out.push_str(&digest);
    }
    out
}

/// Prose for a blocking fan-out's results: one [`delegate_row`] per account,
/// one per line, then the reply's own digest on a last line when it carries one.
/// The rows keep caller order; each names the account it spent.
pub(crate) fn delegate_fanout_results_prose(p: &Value) -> String {
    let results = p
        .get("results")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut out = results
        .iter()
        .map(delegate_row)
        .collect::<Vec<_>>()
        .join("\n");
    let digest = digest_prose(&p["since_your_last_call"]);
    if !digest.is_empty() {
        out.push('\n');
        out.push_str(&digest);
    }
    out
}

/// Prose for `monitor`'s one-id mode: the running status, the done envelope, or
/// an invalid/unknown job_id refusal.
pub(crate) fn monitor_job_prose(p: &Value) -> String {
    // A crashed tombstone's owner copy is the whole line: no `delegate to`,
    // `finished`/`failed`, or target footer may qualify it.
    if p.get("crashed").and_then(Value::as_bool).unwrap_or(false) {
        return p
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
    }
    if p.get("job_id").and_then(Value::as_str).is_some()
        && p.get("status").and_then(Value::as_str).is_some()
    {
        return running_status_prose(p);
    }
    if let Some(lu) = p.get("live_usage") {
        let target = lu
            .get("profile")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let mut out = format!("delegate to `{target}` {}", envelope_prose(p));
        out.push_str("; ");
        out.push_str(&live_usage_prose(lu, "target"));
        let digest = digest_prose(&p["since_your_last_call"]);
        if !digest.is_empty() {
            out.push_str("; ");
            out.push_str(&digest);
        }
        return out;
    }
    let result = p.get("result").and_then(Value::as_str).unwrap_or("unknown");
    if p.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
        format!("error: {result}")
    } else {
        result.to_string()
    }
}

/// The running-check line shared by `monitor`'s one-id status and each running
/// line of its several-ids reply, so the two spellings cannot drift: the job,
/// the account it spends, how long it has run, when it last said anything, how
/// far each deadline still is, that account's headroom, and — on its own
/// indented line — the newest thing the delegate wrote.
///
/// A run can be missing either deadline and still be perfectly healthy — which
/// shape drops which key is `running_payload`'s rule — so each absence is NAMED
/// rather than left to read as a lost figure. Missing BOTH is the only case
/// that means clauth is short a fact rather than reporting one: every deadline
/// is recorded together at reserve time, so that job was started by a clauth
/// which recorded neither.
pub(super) fn running_status_prose(p: &Value) -> String {
    let job_id = p.get("job_id").and_then(Value::as_str).unwrap_or("unknown");
    let status = p.get("status").and_then(Value::as_str).unwrap_or("unknown");
    let elapsed = p
        .get("elapsed_secs")
        .and_then(Value::as_u64)
        .map_or_else(|| "unknown".to_string(), |v| format!("{v}s"));
    let mut out = format!("job `{job_id}` {status}");
    if let Some(profile) = p.get("profile").and_then(Value::as_str) {
        out.push_str(&format!(" on `{profile}`"));
    }
    out.push_str(&format!(", elapsed {elapsed}"));
    let wall = p.get("wall_kill_in_secs").and_then(Value::as_u64);
    let idle = p.get("idle_kill_in_secs").and_then(Value::as_u64);
    if wall.is_none() && idle.is_none() {
        out.push_str(", liveness not recorded (started under an older clauth)");
    } else {
        match p.get("last_output_secs_ago").and_then(Value::as_u64) {
            Some(secs) => out.push_str(&format!(", last output {secs}s ago")),
            None => out.push_str(", no output yet"),
        }
        match idle {
            Some(secs) => out.push_str(&format!(", idle-kill in {secs}s")),
            None => out.push_str(", no idle deadline"),
        }
        match wall {
            Some(secs) => out.push_str(&format!(", wall-kill in {secs}s")),
            None => out.push_str(", no wall clock"),
        }
    }
    if let Some(q) = p.get("quota") {
        out.push_str(&format!("; quota: {}", windows_prose(q)));
    }
    // Its own line, quoted: this is the delegate's words rather than clauth's
    // report about it. Escaped, because those words are ANOTHER account's model
    // output arriving verbatim in a model-facing reply, and a bare `"` in them
    // would close the span early and let the rest read as clauth's own prose.
    if let Some(tail) = p.get("tail").and_then(Value::as_str) {
        out.push_str(&format!("\n    \"{}\"", escape_quoted(tail)));
    }
    out
}

/// Escape a delegate's own text for the quoted span it lands in: backslashes
/// first, so an escape already in the text cannot consume the one added after
/// it, then the delimiter. `tail_line` has already collapsed every whitespace
/// run, so no newline can break the block shape either.
fn escape_quoted(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// One asked job's verdict for a cancelling `monitor`'s opening line: what the
/// wait OBSERVED, with the seconds that call actually waited for that job —
/// never the grace constant. The id takes the module's backtick convention;
/// the owner record fixes the words. Both spellings render here and nowhere
/// else, so the fixed pair cannot drift.
pub(super) fn kill_verdict(job_id: &str, killed: bool, waited_secs: u64) -> String {
    if killed {
        format!("killed `{job_id}` after {waited_secs}s")
    } else {
        format!("failed to kill `{job_id}` after {waited_secs}s")
    }
}

/// Prose for a `monitor` several-ids reply: one BLOCK per requested id, naming
/// its id and state, then the batch's own digest clause when it carries one,
/// then ONE unknown-count clause on the tail when the payload carries a
/// positive `unknown_job_id_count`. A done line reuses the envelope spelling,
/// a running line the shared running spelling, an absent id reads `unknown`.
///
/// A block is usually one line but is not guaranteed to be: a done envelope's
/// `result` carries the delegate's own newlines, and a running job with a tail
/// puts that tail on its own indented line. What every block does guarantee is
/// that it OPENS with ``job `<id>` ``, which is what maps a wrapped line back to
/// the id that produced it.
///
/// The per-result live-usage fold stays out of the prose, so a batch of many
/// jobs does not repeat one account's percentages per line. The running blocks
/// do carry a quota clause, because a running check's whole job is to say
/// whether the account it is spending still has headroom.
pub(crate) fn monitor_batch_prose(p: &Value) -> String {
    let results = p
        .get("results")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut out = results
        .iter()
        .map(|r| {
            // The same owner copy, rendered raw: the batch row prefix and
            // `finished`/`failed` qualify a result, and this line IS the whole
            // result.
            if r.get("crashed").and_then(Value::as_bool).unwrap_or(false) {
                return r
                    .get("result")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
            }
            let job_id = r.get("job_id").and_then(Value::as_str).unwrap_or("unknown");
            match r.get("status").and_then(Value::as_str) {
                Some("done") => format!("job `{job_id}` {}", envelope_prose(r)),
                Some("running") => running_status_prose(r),
                _ => format!("job `{job_id}` unknown"),
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let digest = digest_prose(&p["since_your_last_call"]);
    if !digest.is_empty() {
        push_tail_clause(&mut out, &digest);
    }
    // The count is the payload's figure, never a recount of the rows: that
    // contract is what keeps the clause to exactly one however many ids the
    // batch holds.
    if let Some(n) = p
        .get("unknown_job_id_count")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
    {
        push_tail_clause(
            &mut out,
            &format!(
                "{n} unknown job id(s): use monitor without `job_ids` to list the existing jobs."
            ),
        );
    }
    out
}

/// Append one tail clause, separated from the block by a newline only when the
/// block already holds content, so a clause on an otherwise-empty reply never
/// opens it with a blank line.
fn push_tail_clause(out: &mut String, clause: &str) {
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(clause);
}

#[cfg(test)]
#[path = "../../tests/inline/mcp_render.rs"]
mod tests;
