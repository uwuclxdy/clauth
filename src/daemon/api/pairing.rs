//! One-time pairing codes: how a device earns a token without holding one.
//!
//! `clauth devices pair <name>` parks a code in `~/.clauth/pairing.json` (its
//! SHA-256, never the code) with the name and tier the device will get, an
//! expiry, and an attempt budget, then waits for the outcome. `POST
//! /api/v1/pair` redeems it. The whole redemption runs inside one state-flock
//! hold, so however many requests race for one code, one mints a device.
//!
//! One code is live at a time: minting a new one replaces it.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use qrcode::render::unicode::Dense1x2;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::devices::{self, DeviceName, Tier};
use super::http::sanitize_for_log;
use crate::daemon::status_json::ListenSlot;
use crate::lock::with_state_lock;
use crate::logline::logline;
use crate::out::{Wrote, errln, write_chunk_result};
use crate::profile::{
    ServeSettings, app_url_fault, atomic_write_600, clauth_dir, origin_entry_fault,
    public_address_fault,
};
use crate::usage::{epoch_secs_to_iso, iso_to_epoch_secs, now_epoch_secs};

const PAIRING_FILE: &str = "pairing.json";
/// Bumped only on a breaking change to the file's shape, like `status.json`.
const SCHEMA: u64 = 1;
/// How long a code stays redeemable.
pub(crate) const CODE_TTL_SECS: i64 = 5 * 60;
/// Wrong tries a code survives; the last one deletes it.
pub(crate) const CODE_ATTEMPTS: u32 = 5;
/// Crockford's base32: the digits and the capitals without I, L, O and U.
pub(crate) const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CODE_LEN: usize = 8;
/// How often the `pair` wait looks for an outcome.
const POLL: Duration = Duration::from_millis(200);

/// A code in canonical form: [`CODE_LEN`] characters of [`ALPHABET`].
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Code(String);

impl Code {
    /// 40 bits from the OS CSPRNG.
    fn generate() -> Result<Self> {
        let mut seed = [0u8; 5];
        getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("CSPRNG failure: {e}"))?;
        Ok(Self::from_bits(
            seed.iter()
                .fold(0u64, |acc, byte| (acc << 8) | u64::from(*byte)),
        ))
    }

    /// The code spelling the low 40 bits of `bits`, five to a character, the
    /// most significant first.
    fn from_bits(bits: u64) -> Self {
        Self(
            (0..CODE_LEN)
                .rev()
                .map(|group| char::from(ALPHABET[((bits >> (group * 5)) & 0x1f) as usize]))
                .collect(),
        )
    }

    /// The canonical form of a code a person typed: case folded, `-` and
    /// whitespace dropped, and Crockford's look-alikes read as the digits they
    /// stand for (`O` as `0`, `I` and `L` as `1`). `None` when what is left is
    /// not a code at all.
    pub(crate) fn normalize(typed: &str) -> Option<Self> {
        let mut code = String::with_capacity(CODE_LEN);
        for c in typed.chars().filter(|c| *c != '-' && !c.is_whitespace()) {
            let c = match c.to_ascii_uppercase() {
                'O' => '0',
                'I' | 'L' => '1',
                c => c,
            };
            if !c.is_ascii() || !ALPHABET.contains(&(c as u8)) {
                return None;
            }
            code.push(c);
        }
        (code.len() == CODE_LEN).then_some(Self(code))
    }

    fn digest(&self) -> [u8; 32] {
        Sha256::digest(self.0.as_bytes()).into()
    }
}

impl std::fmt::Display for Code {
    /// `XXXX-XXXX`, the dash for whoever reads it off a screen;
    /// [`Code::normalize`] drops it again.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", &self.0[..4], &self.0[4..])
    }
}

impl std::fmt::Debug for Code {
    /// A live code is a credential, and a formatter is how one reaches a log
    /// line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Code(<redacted>)")
    }
}

/// `~/.clauth/pairing.json`: the one live code.
#[derive(Serialize, Deserialize)]
struct PairingFile {
    schema: u64,
    /// The device name the code mints.
    name: String,
    tier: Tier,
    /// Whether the device the code mints may mint a session. A code written by
    /// a build before the field existed reads `false`.
    #[serde(default)]
    sessions: bool,
    /// SHA-256 of the canonical code, lowercase hex.
    digest: String,
    /// ISO-8601, from which on the code redeems nothing.
    expires_at: String,
    attempts_left: u32,
}

impl PairingFile {
    fn is_live(&self, now: i64) -> bool {
        iso_to_epoch_secs(&self.expires_at).is_some_and(|expiry| now < expiry)
    }

    /// Constant-time over the digests, so a wrong guess learns nothing from
    /// the timing.
    fn matches(&self, code: &Code) -> bool {
        let mut stored = [0u8; 32];
        hex::decode_to_slice(&self.digest, &mut stored).is_ok()
            && bool::from(stored.ct_eq(&code.digest()))
    }
}

fn pairing_path() -> Result<PathBuf> {
    Ok(clauth_dir()?.join(PAIRING_FILE))
}

/// The live code's record, or `None` when there is none. A file that does not
/// parse is no code: redeeming against it could only guess.
fn read_pairing(path: &Path) -> Result<Option<PairingFile>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn write_pairing(path: &Path, file: &PairingFile) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(file).context("failed to encode the pairing code")?;
    atomic_write_600(path, bytes).with_context(|| format!("failed to write {}", path.display()))
}

fn discard(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("failed to delete {}", path.display())),
    }
}

/// A code this process minted and is waiting on.
pub(crate) struct Pending {
    code: Code,
    name: DeviceName,
    tier: Tier,
    digest: String,
    expires_at: i64,
}

impl Pending {
    pub(crate) fn code(&self) -> &Code {
        &self.code
    }
}

/// Mint a code for `name`, replacing whatever code was live. Refused while a
/// device already holds the name, so a redeemed code always adds a device.
pub(crate) fn begin(name: &DeviceName, tier: Tier, sessions: bool) -> Result<Pending> {
    begin_at(name, tier, sessions, now_epoch_secs())
}

/// [`begin`] at an explicit instant, so a test can mint a code that is already
/// past its expiry.
pub(crate) fn begin_at(name: &DeviceName, tier: Tier, sessions: bool, now: i64) -> Result<Pending> {
    with_state_lock(|_| {
        devices::refuse_taken(&devices::read_store()?, name)?;
        let code = Code::generate()?;
        let expires_at = now + CODE_TTL_SECS;
        let file = PairingFile {
            schema: SCHEMA,
            name: name.as_str().to_string(),
            tier: tier.clone(),
            sessions,
            digest: hex::encode(code.digest()),
            expires_at: epoch_secs_to_iso(expires_at),
            attempts_left: CODE_ATTEMPTS,
        };
        write_pairing(&pairing_path()?, &file)?;
        Ok(Pending {
            code,
            name: name.clone(),
            tier,
            digest: file.digest,
            expires_at,
        })
    })
}

/// What a redemption answered. The refusal carries no reason on purpose: a
/// wrong code, a spent one, an expired one and none at all look the same to
/// whoever is guessing.
pub(crate) enum Redeemed {
    Paired {
        name: String,
        tier: Tier,
        token: String,
    },
    Refused,
}

impl std::fmt::Debug for Redeemed {
    /// Never the token: the one place it may appear is the response that
    /// mints it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Paired { name, tier, .. } => f
                .debug_struct("Paired")
                .field("name", name)
                .field("tier", tier)
                .finish_non_exhaustive(),
            Self::Refused => f.write_str("Refused"),
        }
    }
}

/// Redeem `code` against the live pairing.
///
/// One state-flock hold covers the read, the expiry check, the compare, and
/// then either the attempt count or the mint, the append and the delete, so
/// two redemptions of one code cannot both mint. The device list reaches the
/// disk before `pairing.json` goes, so no crash can spend a code without
/// leaving its device behind.
pub(crate) fn redeem(code: &Code) -> Result<Redeemed> {
    redeem_at(code, now_epoch_secs())
}

/// [`redeem`] at an explicit instant, for the expiry boundary.
pub(crate) fn redeem_at(code: &Code, now: i64) -> Result<Redeemed> {
    // Checked before the flock: with no code waiting there is nothing to
    // redeem, and an unpaired peer must not be able to take a cross-process
    // lock to learn that. A code is only shown once its file exists.
    let path = pairing_path()?;
    if !path.exists() {
        return Ok(Redeemed::Refused);
    }
    with_state_lock(|held| {
        // A file that does not parse is no code. Deleting it keeps every later
        // probe on the lock-free path above.
        let Some(mut live) = read_pairing(&path)? else {
            discard(&path)?;
            return Ok(Redeemed::Refused);
        };
        if !live.is_live(now) {
            discard(&path)?;
            return Ok(Redeemed::Refused);
        }
        if !live.matches(code) {
            live.attempts_left = live.attempts_left.saturating_sub(1);
            if live.attempts_left == 0 {
                discard(&path)?;
                logline!(
                    "clauth api: the pairing code for '{}' was dropped after {CODE_ATTEMPTS} \
                     wrong tries",
                    sanitize_for_log(&live.name)
                );
            } else {
                write_pairing(&path, &live)?;
            }
            return Ok(Redeemed::Refused);
        }
        let token = devices::append_paired(held, &live.name, live.tier.clone(), live.sessions)?;
        discard(&path)?;
        match token {
            Some(token) => Ok(Redeemed::Paired {
                name: live.name,
                tier: live.tier,
                token,
            }),
            None => {
                logline!(
                    "clauth api: the pairing code for '{}' was refused: a device took that name \
                     first",
                    sanitize_for_log(&live.name)
                );
                Ok(Redeemed::Refused)
            }
        }
    })
}

/// How a `pair` wait ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// A device redeemed the code and holds this tier.
    Paired(Tier),
    /// A newer `pair` minted the live code.
    Replaced,
    /// The last wrong try deleted the code.
    Burned,
    /// Nobody redeemed it in time.
    Expired,
}

/// The outcome of `pending`'s wait, or `None` while the code is still live.
///
/// Read without the state flock: every writer replaces the files whole, and a
/// redemption writes the device before it deletes the code, so a code that is
/// gone while a device that joined by pairing holds its name reads as paired.
/// A second code for the same name, minted and redeemed between two polls,
/// would read the same; that takes two `pair` runs on one name inside one
/// poll.
pub(crate) fn observe(pending: &Pending, now: i64) -> Result<Option<Outcome>> {
    match read_pairing(&pairing_path()?)? {
        Some(file) if file.digest == pending.digest => {
            Ok((now >= pending.expires_at).then_some(Outcome::Expired))
        }
        Some(_) => Ok(Some(Outcome::Replaced)),
        None => {
            let store = devices::read_store()?;
            Ok(Some(match store.named(pending.name.as_str()) {
                Some(device) if device.joined == devices::Joined::Pair => {
                    Outcome::Paired(device.tier.clone())
                }
                _ if now >= pending.expires_at => Outcome::Expired,
                _ => Outcome::Burned,
            }))
        }
    }
}

/// Delete `pending`'s code if it is still the live one; a replacement belongs
/// to whoever minted it. Whether it was.
pub(crate) fn withdraw(pending: &Pending) -> Result<bool> {
    with_state_lock(|_| {
        let path = pairing_path()?;
        let live = read_pairing(&path)?.is_some_and(|file| file.digest == pending.digest);
        if live {
            discard(&path)?;
        }
        Ok(live)
    })
}

/// Refuse `name` while a live code is waiting to mint it. Without this, an
/// `add` under the same name would make the code's redemption fail, and its
/// waiter would read the added device as its own success.
pub(super) fn refuse_pending(name: &DeviceName) -> Result<()> {
    if let Some(file) = read_pairing(&pairing_path()?)?
        && file.is_live(now_epoch_secs())
        && file.name.eq_ignore_ascii_case(name.as_str())
    {
        bail!(
            "a pairing code for '{}' is waiting to be entered; let it finish or pick another name",
            file.name
        );
    }
    Ok(())
}

/// Why a [`wait_for`] returned.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Waited {
    Done(Outcome),
    /// A signal arrived before the wait saw an outcome; the code may still
    /// have reached one since its last look.
    Interrupted(i32),
}

/// Wait on `pending` until it reaches an [`Outcome`] or `caught` reports a
/// signal, looking once per `poll`.
pub(crate) fn wait_for(
    pending: &Pending,
    caught: impl Fn() -> Option<i32>,
    poll: Duration,
) -> Result<Waited> {
    loop {
        if let Some(signal) = caught() {
            return Ok(Waited::Interrupted(signal));
        }
        if let Some(outcome) = observe(pending, now_epoch_secs())? {
            return Ok(Waited::Done(outcome));
        }
        std::thread::sleep(poll);
    }
}

/// How `pair` shows its code: `--json`, `--no-qr`, whether stderr is a
/// terminal, and that terminal's width. The link carries the code, so its
/// stderr line prints only when stderr is a terminal; `--json` carries it in
/// the stdout object a script reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Show {
    pub(crate) json: bool,
    pub(crate) qr: bool,
    pub(crate) terminal: bool,
    /// The stderr terminal's width in columns, when it reported one: `None`
    /// when stderr is no terminal or its size could not be read. A QR code
    /// wider than the terminal wraps, and a wrapped code scans as nothing.
    pub(crate) columns: Option<u16>,
}

impl Show {
    /// The switches `pair`'s flags and surroundings ask for: `--no-qr` is the
    /// inverse of the QR switch, `terminal` says whether stderr can carry the
    /// link, and `columns` is that terminal's width.
    pub(crate) fn from_flags(
        json: bool,
        no_qr: bool,
        terminal: bool,
        columns: Option<u16>,
    ) -> Self {
        Self {
            json,
            qr: !no_qr,
            terminal,
            columns,
        }
    }
}

/// What `pair` reads about this host before it shows its code.
pub(crate) struct Surroundings {
    /// Whether the singleton probe says positively that no daemon runs.
    pub(crate) daemon_absent: bool,
    /// `[serve]`, or why `profiles.toml` did not read.
    pub(crate) serve: Result<ServeSettings, String>,
    /// What a running daemon's `--listen` API published in `status.json`.
    pub(crate) listen: Option<ListenSlot>,
}

impl Surroundings {
    fn read() -> Self {
        let daemon_absent = matches!(crate::daemon::singleton_held(), Ok(false));
        let serve = crate::profile::read_app_state()
            .map(|state| state.map(|state| state.serve).unwrap_or_default())
            .map_err(|e| format!("{e:#}"));
        // A feed left by a daemon that is gone names a listener nobody runs.
        let listen = if daemon_absent {
            None
        } else {
            crate::daemon::published_listen()
        };
        Self {
            daemon_absent,
            serve,
            listen,
        }
    }
}

/// `pair`'s switches, named at the call site so no two bools can swap places.
#[derive(Debug, Clone, Copy)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "pair's four command-line switches, one field each"
)]
pub(crate) struct PairFlags {
    pub(crate) control: bool,
    pub(crate) sessions: bool,
    pub(crate) json: bool,
    pub(crate) no_qr: bool,
}

/// `clauth devices pair <name> [--control] [--sessions] [--no-qr]`: the code
/// alone on stdout (`{"code","link"}` under `--json`), the link, its QR code
/// and the wait on stderr.
pub(crate) fn run_pair(name: &str, flags: PairFlags) -> Result<()> {
    let PairFlags {
        control,
        sessions,
        json,
        no_qr,
    } = flags;
    let name = DeviceName::parse(name)?;
    let tier = Tier::chosen(control);
    let around = Surroundings::read();
    // Installed before the code exists, so no signal lands between minting it
    // and being able to withdraw it.
    let interrupt = Interrupt::install()?;
    let pending = begin(&name, tier, sessions)?;
    let terminal = std::io::stderr().is_terminal();
    // Only a stderr that is a terminal can carry the QR at all, so it is the
    // gate on asking for a size; crossterm reads the controlling terminal.
    let columns = terminal
        .then(ratatui::crossterm::terminal::size)
        .and_then(Result::ok)
        .map(|(columns, _)| columns);
    let show = Show::from_flags(json, no_qr, terminal, columns);
    announce(
        &mut std::io::stdout().lock(),
        &pending,
        sessions,
        show,
        &around,
    )?;
    let waited = wait_for(&pending, || interrupt.caught(), POLL);
    finish(&pending, waited)
}

/// Print the code `pending` minted and what goes with it: the code (or its
/// JSON) on `out`, everything else on stderr. A code that never reached `out`
/// is withdrawn before the error returns.
fn announce<W: Write>(
    out: &mut W,
    pending: &Pending,
    sessions: bool,
    show: Show,
    around: &Surroundings,
) -> Result<()> {
    let name = &pending.name;
    let tier = &pending.tier;
    let code = pending.code().to_string();
    // With no daemon running, nothing can redeem the code through an address,
    // so neither `public_address` nor a feed left behind builds a link.
    let link = if around.daemon_absent {
        None
    } else {
        Some(around.serve.as_ref().map(|serve| {
            pair_link(
                serve.app_url.as_deref(),
                &serve.app_origins.entries,
                serve.public_address.as_deref(),
                around
                    .listen
                    .as_ref()
                    .map(|listen| (listen.host.as_str(), listen.port)),
                &code,
            )
        }))
    };
    let shown = match &link {
        Some(Ok(PairLink::Link(link))) => Some(link.as_str()),
        _ => None,
    };
    let line = if show.json {
        match serde_json::to_string(&PairJson {
            code: &code,
            link: shown,
        }) {
            Ok(line) => line,
            Err(e) => {
                let encode_err = std::io::Error::other(format!(
                    "failed to encode the pairing code as JSON: {e}"
                ));
                return withdraw_lost(pending, Some(encode_err));
            }
        }
    } else {
        code
    };
    let lost = match write_chunk_result(out, format_args!("{line}"), true) {
        Ok(Wrote::Yes) => None,
        Ok(Wrote::ReaderGone) => Some(None),
        Err(e) => Some(Some(e)),
    };
    if let Some(write_err) = lost {
        withdraw_lost(pending, write_err)?;
    }
    if show.terminal
        && let Some(Ok(PairLink::Link(link))) = &link
    {
        if show.qr {
            match render_qr(link) {
                Ok(qr) => {
                    let widest = qr
                        .lines()
                        .map(crate::format::grapheme_cells)
                        .max()
                        .unwrap_or(0);
                    match show.columns {
                        Some(columns) if usize::from(columns) < widest => errln!(
                            "clauth: the terminal is too narrow for the QR code: it \
                             needs {widest} columns and this one has {columns}; pass \
                             --no-qr to drop it"
                        ),
                        _ => errln!("{qr}"),
                    }
                }
                Err(e) => errln!("clauth: no QR code for the pairing link: {e}"),
            }
        }
        errln!("clauth: pairing link: {link}");
    }
    if show.terminal || show.json {
        match &link {
            Some(Ok(unlinked)) => {
                if let Some(why) = no_link_line(unlinked, around) {
                    errln!("{why}");
                }
            }
            Some(Err(e)) => errln!("clauth: no pairing link: profiles.toml does not read: {e}"),
            None => {}
        }
    }
    errln!(
        "clauth: enter the code on the device within {} minutes to pair '{name}' ({tier}); \
         Ctrl-C withdraws it",
        CODE_TTL_SECS / 60
    );
    if *tier == Tier::Control {
        errln!(
            "clauth: until it is used the code is a control credential: whoever enters it first \
             can switch this host's accounts"
        );
    }
    if sessions {
        errln!(
            "clauth: until it is used the code also grants sessions: whoever enters it first \
             can mint them through the API"
        );
    }
    if around.daemon_absent {
        errln!(
            "clauth: no clauth daemon is running here, so nothing can redeem the code; start one \
             with `clauth daemon --listen`"
        );
    }
    Ok(())
}

/// Withdraw the code a lost line minted, naming the loss. `cause` is
/// `Some(e)` when the line failed before reaching `out` — a full disk behind a
/// redirect, an unencodable JSON line — rather than the reader closing the
/// pipe, so the operator sees why.
/// `Ok(false)` from [`withdraw`] means a newer `pair` already replaced the code
/// before anyone read it, so the message says replaced, not withdrawn.
fn withdraw_lost(pending: &Pending, cause: Option<std::io::Error>) -> Result<()> {
    let name = &pending.name;
    let cause = cause.map(|e| format!(" ({e})")).unwrap_or_default();
    match withdraw(pending) {
        Ok(true) => {
            bail!("the pairing code for '{name}' never reached its reader{cause}; it was withdrawn")
        }
        Ok(false) => bail!(
            "the pairing code for '{name}' never reached its reader{cause}; a newer `clauth \
             devices pair` had already replaced it"
        ),
        Err(e) => bail!(
            "the pairing code for '{name}' never reached its reader{cause} and could not be \
             withdrawn: {e:#}; it stays redeemable until it expires in {} minutes",
            CODE_TTL_SECS / 60
        ),
    }
}

/// Turn a wait into what `pair` prints and exits with. Only a signal or an
/// error can leave the code live, so those two withdraw it first.
fn finish(pending: &Pending, waited: Result<Waited>) -> Result<()> {
    let name = &pending.name;
    match waited {
        Ok(Waited::Done(Outcome::Paired(tier))) => {
            errln!("paired '{name}' ({tier})");
            Ok(())
        }
        Ok(Waited::Done(Outcome::Replaced)) => {
            bail!("a newer `clauth devices pair` replaced this code before anyone entered it")
        }
        Ok(Waited::Done(Outcome::Burned)) => bail!(
            "the code was dropped after {CODE_ATTEMPTS} wrong tries; run `clauth devices pair \
             {name}` for a new one"
        ),
        Ok(Waited::Done(Outcome::Expired)) => {
            withdraw_or_say(pending);
            bail!("the code expired unused; run `clauth devices pair {name}` for a new one")
        }
        Ok(Waited::Interrupted(signal)) => match withdraw_or_say(pending) {
            Some(true) => {
                errln!("clauth: pairing code withdrawn");
                Err(crate::Interrupted(signal).into())
            }
            // Already gone when the signal was read: the code reached an
            // outcome between two polls, and that outcome is what happened.
            Some(false) => match observe(pending, now_epoch_secs()) {
                Ok(Some(outcome)) => finish(pending, Ok(Waited::Done(outcome))),
                Ok(None) => Err(crate::Interrupted(signal).into()),
                Err(e) => {
                    errln!(
                        "clauth: the pairing code was no longer waiting, and what became of it \
                         could not be read: {e:#}"
                    );
                    Err(crate::Interrupted(signal).into())
                }
            },
            None => Err(crate::Interrupted(signal).into()),
        },
        Err(e) => {
            withdraw_or_say(pending);
            Err(e)
        }
    }
}

/// [`withdraw`], saying so on stderr when it fails, since the code then stays
/// redeemable until it expires. Whether it removed the code, or `None` when it
/// could not tell.
fn withdraw_or_say(pending: &Pending) -> Option<bool> {
    match withdraw(pending) {
        Ok(removed) => Some(removed),
        Err(e) => {
            errln!(
                "clauth: could not withdraw the pairing code, which stays redeemable until it \
                 expires: {e:#}"
            );
            None
        }
    }
}

#[cfg(unix)]
const WITHDRAW_SIGNALS: [std::ffi::c_int; 3] = [
    signal_hook::consts::signal::SIGINT,
    signal_hook::consts::signal::SIGTERM,
    signal_hook::consts::signal::SIGHUP,
];
#[cfg(not(unix))]
const WITHDRAW_SIGNALS: [std::ffi::c_int; 2] = [
    signal_hook::consts::signal::SIGINT,
    signal_hook::consts::signal::SIGTERM,
];

/// The signals that end a `pair` wait, recorded instead of obeyed so the code
/// is withdrawn before the process exits. A second one exits at once, skipping
/// the withdrawal.
struct Interrupt {
    caught: Arc<AtomicUsize>,
}

impl Interrupt {
    fn install() -> Result<Self> {
        let caught = Arc::new(AtomicUsize::new(0));
        let armed = Arc::new(AtomicBool::new(false));
        for signal in WITHDRAW_SIGNALS {
            // Registered first so it runs first: the first signal finds it
            // unarmed and the next registration arms it.
            signal_hook::flag::register_conditional_shutdown(
                signal,
                128 + signal,
                Arc::clone(&armed),
            )
            .and_then(|_| signal_hook::flag::register(signal, Arc::clone(&armed)))
            .and_then(|_| {
                signal_hook::flag::register_usize(signal, Arc::clone(&caught), signal as usize)
            })
            .context("failed to install the signal handlers that withdraw the pairing code")?;
        }
        Ok(Self { caught })
    }

    fn caught(&self) -> Option<i32> {
        match self.caught.load(Ordering::SeqCst) {
            0 => None,
            signal => i32::try_from(signal).ok(),
        }
    }
}

/// The link `clauth devices pair` prints beside its code, or why it prints none.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum PairLink {
    /// `<app page>#daemon=<host:port>&code=<code>`.
    Link(String),
    /// No `--listen` daemon published an address and `[serve] public_address`
    /// is unset.
    NoAddress,
    /// `[serve] app_url` is unset and `[serve] app_origins` holds no entry.
    NoAppUrl,
    /// `[serve] app_url` is unset and `[serve] app_origins` holds more than one
    /// entry, so no entry names the app's page.
    SeveralOrigins,
    /// `[serve] app_url` names no page a phone can open
    /// ([`crate::profile::app_url_fault`] says why).
    BadAppUrl(&'static str),
    /// `[serve] app_url` is unset and the one `[serve] app_origins` entry is
    /// plain http (loopback), which a phone cannot open.
    HttpOrigin(String),
    /// `[serve] public_address` is no `host:port`
    /// ([`crate::profile::public_address_fault`] says why).
    BadPublicAddress(&'static str),
}

impl std::fmt::Debug for PairLink {
    /// The link carries the live code, and a formatter is how one reaches a
    /// log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Link(_) => f.write_str("Link(<redacted>)"),
            Self::NoAddress => f.write_str("NoAddress"),
            Self::NoAppUrl => f.write_str("NoAppUrl"),
            Self::SeveralOrigins => f.write_str("SeveralOrigins"),
            Self::BadAppUrl(fault) => f.debug_tuple("BadAppUrl").field(fault).finish(),
            Self::HttpOrigin(origin) => f.debug_tuple("HttpOrigin").field(origin).finish(),
            Self::BadPublicAddress(fault) => {
                f.debug_tuple("BadPublicAddress").field(fault).finish()
            }
        }
    }
}

/// Where a pairing link's two halves come from: the app's page (`app_url`,
/// else the single `app_origins` entry) and the daemon's address
/// (`public_address`, else the `host`/`port` a `--listen` daemon published).
/// The `app_origins` entries that count are the ones the API counts.
pub(crate) fn pair_link(
    app_url: Option<&str>,
    app_origins: &[String],
    public_address: Option<&str>,
    listen: Option<(&str, u16)>,
    code: &str,
) -> PairLink {
    let page = match app_url {
        Some(url) => match app_url_fault(url) {
            Some(fault) => return PairLink::BadAppUrl(fault),
            None => url,
        },
        None => {
            let mut counted = app_origins
                .iter()
                .filter(|entry| origin_entry_fault(entry).is_none());
            match (counted.next(), counted.next()) {
                (None, _) => return PairLink::NoAppUrl,
                (Some(_), Some(_)) => return PairLink::SeveralOrigins,
                (Some(origin), None) if !origin.starts_with("https://") => {
                    return PairLink::HttpOrigin(origin.clone());
                }
                (Some(origin), None) => origin.as_str(),
            }
        }
    };
    let address = match (public_address, listen) {
        (Some(address), _) => match public_address_fault(address) {
            Some(fault) => return PairLink::BadPublicAddress(fault),
            None => address.to_owned(),
        },
        (None, Some((host, port))) => format!("{host}:{port}"),
        (None, None) => return PairLink::NoAddress,
    };
    // A bare origin's page is its root; a page with a path keeps it as given.
    let root = if page
        .split_once("://")
        .is_some_and(|(_, rest)| !rest.contains('/'))
    {
        "/"
    } else {
        ""
    };
    PairLink::Link(format!("{page}{root}#daemon={address}&code={code}"))
}

/// `clauth devices --json pair`'s stdout line.
#[derive(Serialize)]
struct PairJson<'a> {
    code: &'a str,
    link: Option<&'a str>,
}

/// The line saying why `pair` shows no link, naming the key that fixes it.
fn no_link_line(link: &PairLink, around: &Surroundings) -> Option<String> {
    let serve = around.serve.as_ref().ok();
    let why = match link {
        PairLink::Link(_) => return None,
        PairLink::NoAddress => "the running daemon publishes no `--listen` address and [serve] \
                                public_address is unset"
            .to_owned(),
        PairLink::NoAppUrl => "[serve] app_url is unset and [serve] app_origins lists no \
                               origin; set [serve] app_url to the app's page"
            .to_owned(),
        PairLink::SeveralOrigins => "[serve] app_origins lists several origins; set [serve] \
                                     app_url to the one that is the app's page"
            .to_owned(),
        PairLink::BadAppUrl(fault) => format!(
            "[serve] app_url {:?} does not count: {fault}",
            serve
                .and_then(|serve| serve.app_url.as_deref())
                .unwrap_or_default()
        ),
        PairLink::HttpOrigin(origin) => format!(
            "the one [serve] app_origins entry {origin:?} is plain http, which a phone cannot \
             open; set [serve] app_url to the app's https page"
        ),
        PairLink::BadPublicAddress(fault) => format!(
            "[serve] public_address {:?} does not count: {fault}",
            serve
                .and_then(|serve| serve.public_address.as_deref())
                .unwrap_or_default()
        ),
    };
    Some(format!("clauth: no pairing link: {why}"))
}

/// The terminal QR code of `link`: two modules to a character cell, its light
/// modules drawn as blocks, so a dark terminal shows the dark-on-light symbol
/// a camera expects.
pub(crate) fn render_qr(link: &str) -> Result<String, qrcode::types::QrError> {
    Ok(qrcode::QrCode::new(link.as_bytes())?
        .render::<Dense1x2>()
        .dark_color(Dense1x2::Light)
        .light_color(Dense1x2::Dark)
        .build())
}

#[cfg(test)]
#[path = "../../../tests/inline/daemon_api_pairing.rs"]
mod tests;
