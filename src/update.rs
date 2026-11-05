use std::env;
use std::fs;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use minisign_verify::{PublicKey, Signature};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::logline::logline;

const API_URL: &str = "https://api.github.com/repos/uwuclxdy/clauth/releases/latest";

/// Test-only [`API_URL`] override: the pin on `spawn`'s test-build gate points
/// it at a listener that accepts nothing. Serialized by `HOME_TEST_LOCK`, held
/// through `HomeSandbox`.
#[cfg(test)]
static API_URL_OVERRIDE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// The release-listing URL a check fetches: the test override when one is set
/// (test builds only), else [`API_URL`].
fn api_url() -> String {
    #[cfg(test)]
    {
        if let Some(url) = API_URL_OVERRIDE.lock().ok().and_then(|g| g.clone()) {
            return url;
        }
    }
    API_URL.to_string()
}

/// Test-only RAII over [`API_URL_OVERRIDE`], cleared on drop; it borrows the
/// `HomeSandbox` for the reason `testutil::EndpointSandbox` does.
#[cfg(test)]
pub(crate) struct ApiUrlSandbox<'a>(std::marker::PhantomData<&'a crate::testutil::HomeSandbox>);

#[cfg(test)]
impl<'a> ApiUrlSandbox<'a> {
    pub(crate) fn new(_home: &'a crate::testutil::HomeSandbox, url: &str) -> Self {
        if let Ok(mut guard) = API_URL_OVERRIDE.lock() {
            *guard = Some(url.to_string());
        }
        Self(std::marker::PhantomData)
    }
}

#[cfg(test)]
impl Drop for ApiUrlSandbox<'_> {
    fn drop(&mut self) {
        if let Ok(mut guard) = API_URL_OVERRIDE.lock() {
            *guard = None;
        }
    }
}

/// Env var: set to `1` to disable all background update work.
const NO_UPDATE_ENV: &str = "CLAUTH_NO_UPDATE";

/// Largest release asset the self-updater will download: 256 MiB. A listing
/// past this is refused in the check, right after the listing is parsed and
/// before any other request; [`fetch_asset`] relies on its caller for that.
const MAX_ASSET_BYTES: u64 = 256 * 1024 * 1024;

/// Whole-call time bound shared by the metadata fetches — the release listing,
/// `sha256sums.txt` and its `.minisig` — via `timeout_global`: 35 s.
const METADATA_CALL_BOUND: Duration = Duration::from_secs(35);

/// Pinned minisign public key (base64 — the key line of `minisign.pub`, WITHOUT
/// the `untrusted comment:` header). When set, every self-update verifies a
/// detached minisign signature over `sha256sums.txt` against this key BEFORE
/// trusting any hash it lists; authenticating the sums file transitively
/// authenticates every asset hash in it.
///
/// A real key is pinned below, so fail-closed authenticity is ACTIVE in
/// shipped builds: a missing/malformed/bad signature means no update. The
/// `trim().is_empty()` branch is retained as a compile-time escape hatch
/// (it keeps the updater on SHA-256-only integrity during a rollout whose
/// releases are not signed), and, being a compile-time constant, a pinned
/// key can never be downgraded at runtime.
///
/// Setup: `minisign -G -W` (passwordless) → paste the secret-key file contents
/// into the `MINISIGN_SECRET_KEY` GitHub Actions secret, the public key here.
const MINISIGN_PUBLIC_KEY: &str = "RWS4HZQQuH8GjgSAz119H+6csSha0uFjoMKt3gx8Ror9Kvh3nObNSmVm";

/// Outcome of the background update check. A failed check writes one
/// `clauth.log` line and sends no event; only actionable results are reported.
pub(crate) enum UpdateEvent {
    /// Newer release downloaded and self-installed; effective next launch.
    Installed(String),
    /// Newer release exists but can't be self-applied (cargo install or no
    /// prebuilt asset); user must update manually.
    Available(String),
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    /// Listed byte size from the release listing. Required (no `#[serde(default)]`):
    /// a listing missing it must fail the parse and log, never read as 0 and
    /// refuse every download with a misleading error.
    size: u64,
}

/// The version this binary is, read off the manifest at build time.
pub(crate) const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The one shared update gate: the persisted `[update].auto_update` setting
/// (default on — an absent table means on) AND `CLAUTH_NO_UPDATE` unset or
/// not `"1"`. Both consumers — [`spawn`] and `herdr::heal_detached` — route
/// through this so the env kill-switch and the saved toggle never disagree.
pub(crate) fn updates_enabled(saved_auto_update: bool) -> bool {
    saved_auto_update && env::var(NO_UPDATE_ENV).as_deref() != Ok("1")
}

/// Spawn a background update check; applies if self-replaceable, toasts result.
/// Returns the update thread (for a bounded exit wait), or `None` when the
/// saved `auto_update` setting or `CLAUTH_NO_UPDATE` disables updates.
pub(crate) fn spawn(tx: Sender<UpdateEvent>, saved_auto_update: bool) -> Option<UpdateThread> {
    if !updates_enabled(saved_auto_update) {
        return None;
    }
    let gate = Arc::new(ReplaceGate::new());
    let flag = Arc::clone(&gate);
    #[allow(clippy::expect_used, reason = "thread spawn failure is unrecoverable")]
    let handle = std::thread::Builder::new()
        .name("clauth-upd".into())
        .spawn(move || {
            // A test build must never reach `api.github.com` or self-replace the
            // test binary: end at once and drop the sender, so the receiver reads
            // disconnected.
            if cfg!(test) {
                return;
            }
            let url = api_url();
            let agent = make_agent();
            run_check_with(&tx, &url, &agent, &flag);
        })
        .expect("failed to spawn update thread");
    Some(UpdateThread { handle, gate })
}

/// Set while this process's own self-update is replacing, or has replaced, the
/// running binary: from just before the replace call until that call fails or
/// unwinds, and for the rest of the run once it succeeds.
static REPLACED_BY_SELF_UPDATE: AtomicBool = AtomicBool::new(false);

/// Whether this process's own self-update has begun replacing the running
/// binary and has not failed. A changed binary (replaced or removed) read while
/// it holds is credited to that self-update, which may be wrong: a change made
/// by anything else reads the same during the replace call, and after a
/// successful one.
pub(crate) fn replaced_by_self_update() -> bool {
    REPLACED_BY_SELF_UPDATE.load(Ordering::SeqCst)
}

/// Poses this process's own self-update replacement until dropped. The mark is
/// process-global, so it serializes on `HOME_TEST_LOCK`; set it under a
/// `HomeSandbox`.
#[cfg(all(test, unix))]
pub(crate) struct OwnReplaceMark(());

#[cfg(all(test, unix))]
impl OwnReplaceMark {
    #[must_use = "the mark clears when the guard drops"]
    pub(crate) fn set() -> Self {
        assert!(
            crate::lockorder::holds::<crate::lockorder::rank::HomeTest>(),
            "the self-update mark serializes on `HOME_TEST_LOCK`; set it under a `HomeSandbox`"
        );
        REPLACED_BY_SELF_UPDATE.store(true, Ordering::SeqCst);
        Self(())
    }
}

#[cfg(all(test, unix))]
impl Drop for OwnReplaceMark {
    fn drop(&mut self) {
        REPLACED_BY_SELF_UPDATE.store(false, Ordering::SeqCst);
    }
}

/// Clear the self-update mark so a test drives the replace step fresh. The
/// mark is process-global, so it serializes on `HOME_TEST_LOCK`; call under a
/// `HomeSandbox`.
#[cfg(test)]
pub(crate) fn reset_self_update_mark_for_test() {
    assert!(
        crate::lockorder::holds::<crate::lockorder::rank::HomeTest>(),
        "the self-update mark serializes on `HOME_TEST_LOCK`; call under a `HomeSandbox`"
    );
    REPLACED_BY_SELF_UPDATE.store(false, Ordering::SeqCst);
}

/// The three states of the replace gate: `idle` (no replace, the exit wait may
/// give up), `replacing` (a replace runs, the wait must hold), `abandoned` (the
/// wait gave up — a replace that has not started never starts).
const GATE_IDLE: u8 = 0;
const GATE_REPLACING: u8 = 1;
const GATE_ABANDONED: u8 = 2;

/// The gate between the exit wait and the replace step. Once the wait gives up
/// ([`ReplaceGate::abandon`]), a replace that has not started never starts (an
/// exit would cut it short); while a replace runs ([`ReplaceGate::begin`] took
/// the idle state), the wait cannot give up; after the replace ends, it can.
struct ReplaceGate(AtomicU8);

impl ReplaceGate {
    fn new() -> Self {
        Self(AtomicU8::new(GATE_IDLE))
    }

    /// Move idle -> replacing, returning a guard that restores idle on drop.
    /// `None` once abandoned (or while a replace already runs — which cannot
    /// happen: the guard is the only holder while replacing).
    fn begin(&self) -> Option<ReplaceGuard<'_>> {
        match self.0.compare_exchange(
            GATE_IDLE,
            GATE_REPLACING,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => Some(ReplaceGuard(&self.0)),
            Err(_) => None,
        }
    }

    /// Move idle -> abandoned; `true` when the gate is now abandoned (an
    /// already-abandoned gate also answers `true`), `false` while a replace
    /// runs.
    fn abandon(&self) -> bool {
        match self.0.compare_exchange(
            GATE_IDLE,
            GATE_ABANDONED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) | Err(GATE_ABANDONED) => true,
            Err(_) => false,
        }
    }

    #[cfg(test)]
    fn is_replacing(&self) -> bool {
        self.0.load(Ordering::SeqCst) == GATE_REPLACING
    }
}

/// Sets the gate to `replacing` for its lifetime and restores idle on drop
/// (panic included), so the exit wait never detaches mid-replace. It exists
/// only after a successful [`ReplaceGate::begin`] — the only holder while
/// replacing — so its drop is a plain store.
struct ReplaceGuard<'a>(&'a AtomicU8);

impl Drop for ReplaceGuard<'_> {
    fn drop(&mut self) {
        self.0.store(GATE_IDLE, Ordering::SeqCst);
    }
}

/// The exit-wait handle for the background update thread: the [`JoinHandle`]
/// plus the replace gate, so [`wait_on_exit`] can detach a slow download but
/// never cut short a running self-replace.
pub(crate) struct UpdateThread {
    handle: JoinHandle<()>,
    gate: Arc<ReplaceGate>,
}

/// Absolute ceiling on how long [`wait_on_exit`] holds past `detach_after` for
/// a still-running replace: the replace is a local copy + rename, so 60 s is
/// ample, and it bounds the exit wait even if the gate never returns to idle.
const REPLACE_WAIT_CAP: Duration = Duration::from_secs(60);

impl UpdateThread {
    /// Wait for the update thread to finish; if it has not within
    /// `detach_after`, give up the gate (so no replace can start after this
    /// returns) — unless a replace is already running, in which case keep
    /// waiting for it to end (polling at 50 ms), up to [`REPLACE_WAIT_CAP`]
    /// past `detach_after`.
    pub(crate) fn wait_on_exit(self, detach_after: Duration) {
        let deadline = Instant::now() + detach_after;
        while !self.handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        if self.handle.is_finished() {
            return;
        }
        let cap_deadline = Instant::now() + REPLACE_WAIT_CAP;
        while Instant::now() < cap_deadline {
            if self.handle.is_finished() {
                return;
            }
            if self.gate.abandon() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Run one update check against `api_url`, using `agent` for every metadata
/// call (listing, `sha256sums.txt`, `.minisig`). A failure writes exactly one
/// `clauth.log` line; the up-to-date and cargo-installed paths are not failures
/// and write nothing.
fn run_check_with(
    tx: &Sender<UpdateEvent>,
    api_url: &str,
    agent: &ureq::Agent,
    gate: &ReplaceGate,
) {
    if let Err(e) = try_update(tx, api_url, agent, gate) {
        logline!("update check failed: {e:#}");
    }
}

fn try_update(
    tx: &Sender<UpdateEvent>,
    api_url: &str,
    agent: &ureq::Agent,
    gate: &ReplaceGate,
) -> anyhow::Result<()> {
    let release = fetch_latest(agent, api_url)?;

    if !is_newer(&release.tag_name, CURRENT_VERSION) {
        return Ok(());
    }
    let version = release.tag_name.trim_start_matches('v').to_string();

    // Cargo install or unsupported platform: notify only.
    let Some(asset) = asset_name() else {
        let _ = tx.send(UpdateEvent::Available(version));
        return Ok(());
    };
    if is_cargo_installed() {
        let _ = tx.send(UpdateEvent::Available(version));
        return Ok(());
    }

    let Some(asset_entry) = release.assets.iter().find(|a| a.name == asset) else {
        anyhow::bail!("release {version} lists no {asset} asset; refusing update");
    };
    let url = asset_entry.browser_download_url.clone();
    let size = asset_entry.size;

    // Ceiling, right after the listing parse and before any other request: a
    // listing past this must not fetch the sums file or the signature.
    if size > MAX_ASSET_BYTES {
        anyhow::bail!(
            "asset of {size} bytes exceeds the {MAX_ASSET_BYTES}-byte ceiling; refusing download"
        );
    }

    let sums_url = derive_sums_url(&url, asset);

    if download_and_replace(&url, &sums_url, asset, size, agent, gate)? {
        let _ = tx.send(UpdateEvent::Installed(version));
    }
    Ok(())
}

/// Build the `sha256sums.txt` URL from `asset_url` by replacing the asset filename.
fn derive_sums_url(asset_url: &str, asset: &str) -> String {
    if let Some(idx) = asset_url.rfind(asset) {
        format!("{}sha256sums.txt", &asset_url[..idx])
    } else {
        // Fallback: append alongside (shouldn't happen with canonical GH URLs).
        format!("{asset_url}/sha256sums.txt")
    }
}

/// Parse a single `sha256sum`-format line: `<64-hex-chars>  <filename>`.
/// Returns `(hex, filename)` or `None` on malformed input.
pub(crate) fn parse_sums_line(line: &str) -> Option<(&str, &str)> {
    let (hex, rest) = line.split_once("  ")?;
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some((hex, rest.trim_end()))
}

/// Return the expected SHA-256 hex for `asset` from a `sha256sums.txt` body,
/// or `None` when the asset isn't listed.
pub(crate) fn find_expected_sha(sums_text: &str, asset: &str) -> Option<String> {
    sums_text.lines().find_map(|line| {
        let (hex, name) = parse_sums_line(line)?;
        (name == asset).then(|| hex.to_owned())
    })
}

/// Verify that `bytes` hash to `expected_hex` (lowercase SHA-256).
/// Returns `true` on a match, `false` on mismatch or malformed hex string.
pub(crate) fn verify_sha256(bytes: &[u8], expected_hex: &str) -> bool {
    if expected_hex.len() != 64 {
        return false;
    }
    let digest = Sha256::digest(bytes);
    // Hex-string equality check (not constant-time; fine for an integrity
    // compare, do NOT reuse for secret/MAC comparison).
    let actual = digest.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    });
    actual == expected_hex.to_ascii_lowercase()
}

/// Verify a minisign `signature_text` (full `.minisig` body, comments included)
/// over `signed_bytes` using the base64 `public_key`. An empty key short-circuits
/// to `Ok(())` — signature enforcement is inactive until a key is pinned. Every
/// other outcome (unparseable key, malformed signature, bad signature) is an
/// `Err`, so callers fail closed.
fn verify_minisign(
    public_key: &str,
    signed_bytes: &[u8],
    signature_text: &str,
) -> anyhow::Result<()> {
    let key = public_key.trim();
    if key.is_empty() {
        return Ok(());
    }
    let public_key = PublicKey::from_base64(key)
        .map_err(|e| anyhow::anyhow!("pinned minisign public key is invalid: {e}"))?;
    let signature = Signature::decode(signature_text)
        .map_err(|e| anyhow::anyhow!("malformed minisign signature: {e}"))?;
    public_key
        .verify(signed_bytes, &signature, false)
        .map_err(|e| anyhow::anyhow!("minisign signature did not verify; refusing update: {e}"))
}

/// Fetch `sha256sums.txt.minisig` and verify it against `MINISIGN_PUBLIC_KEY`.
/// No-op (and no network call) while the key is unset; fail-closed on any fetch
/// or verification error once it's pinned.
fn verify_sums_signature(
    agent: &ureq::Agent,
    sums_url: &str,
    sums_text: &str,
) -> anyhow::Result<()> {
    if MINISIGN_PUBLIC_KEY.trim().is_empty() {
        return Ok(());
    }
    let sig_url = format!("{sums_url}.minisig");
    let sig_text = agent
        .get(&sig_url)
        .header("User-Agent", "clauth-updater")
        .call()
        .map_err(|e| anyhow::anyhow!("signature fetch failed (skipping update): {e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| anyhow::anyhow!("signature read failed (skipping update): {e}"))?;
    verify_minisign(MINISIGN_PUBLIC_KEY, sums_text.as_bytes(), &sig_text)
}

/// One agent for the metadata calls (release listing, `sha256sums.txt`,
/// `.minisig`): connect 5 s, response headers 30 s, plus a whole-call bound of
/// `bound` via `timeout_global`, which also covers the body read.
fn metadata_agent(bound: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_global(Some(bound))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(crate::tls_roots::root_certs())
                .build(),
        )
        .build()
        .into()
}

fn make_agent() -> ureq::Agent {
    metadata_agent(METADATA_CALL_BOUND)
}

fn fetch_latest(agent: &ureq::Agent, api_url: &str) -> anyhow::Result<Release> {
    let text = agent
        .get(api_url)
        .header("User-Agent", "clauth-updater")
        .call()
        .map_err(anyhow::Error::from)?
        .body_mut()
        .read_to_string()
        .map_err(anyhow::Error::from)?;

    Ok(serde_json::from_str(&text)?)
}

/// Whole-download time budget: the metadata bound plus the listed size at
/// 16 KiB/s, whole seconds rounded up.
fn download_budget(size: u64) -> Duration {
    METADATA_CALL_BOUND + Duration::from_secs(size.div_ceil(16 * 1024))
}

/// Download one release asset: one GET, whole-call-bounded by `budget`, read
/// with a ceiling of the listed `size` (a server sending more than listed is
/// refused). It does not itself refuse an over-ceiling listing — the check
/// does that right after the listing parse, before any other request — so this
/// relies on its caller.
fn fetch_asset(url: &str, size: u64, budget: Duration) -> anyhow::Result<Vec<u8>> {
    metadata_agent(budget)
        .get(url)
        .header("User-Agent", "clauth-updater")
        .call()
        .map_err(|e| anyhow::anyhow!("asset download failed: {e}"))?
        .body_mut()
        .with_config()
        // ureq 3.4.2's `limit(n)` errors the moment a read starts with `n` bytes
        // already consumed, and `read_to_end` probes once more for EOF — so an
        // exactly-`n`-byte body trips `limit(n)`. Bound at `size + 1`: `size`
        // bytes read cleanly, anything larger is refused. Saturating so the
        // bound can never overflow for an extreme listed size.
        .limit(size.saturating_add(1))
        .read_to_vec()
        .map_err(|e| anyhow::anyhow!("asset read failed: {e}"))
}

/// The replace step — staging, self-replace, cleanup — holds the gate so an
/// exit wait never detaches mid-replace; the download before it is not gated. A
/// refused begin (the exit wait already gave up) is silent: `Ok(false)`, so no
/// log line and no `Installed` event — the process is exiting. The replace call
/// itself is injected, so a test drives the step to failure without replacing
/// the running binary.
fn replace_verified(
    bytes: &[u8],
    gate: &ReplaceGate,
    replace: impl FnOnce(&std::path::Path) -> std::io::Result<()>,
) -> anyhow::Result<bool> {
    let Some(_replace) = gate.begin() else {
        return Ok(false);
    };
    let (_staging_dir, staged) = stage_verified(bytes)?;
    replace_marked(&staged, replace)?;
    // `_staging_dir` drops here, removing the staged file.
    Ok(true)
}

fn download_and_replace(
    url: &str,
    sums_url: &str,
    asset: &str,
    size: u64,
    agent: &ureq::Agent,
    gate: &ReplaceGate,
) -> anyhow::Result<bool> {
    // 1. Fetch the sums file; treat any error as fail-closed (no integrity = no update).
    let sums_text = agent
        .get(sums_url)
        .header("User-Agent", "clauth-updater")
        .call()
        .map_err(|e| anyhow::anyhow!("sha256sums.txt fetch failed (skipping update): {e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| anyhow::anyhow!("sha256sums.txt read failed (skipping update): {e}"))?;

    // 1b. Authenticate sha256sums.txt with the pinned minisign key BEFORE
    //     trusting any hash it lists. No-op (no network call) until a key is
    //     pinned; fail-closed (missing/invalid signature ⇒ no update) once one is.
    verify_sums_signature(agent, sums_url, &sums_text)?;

    let expected_hex = find_expected_sha(&sums_text, asset)
        .ok_or_else(|| anyhow::anyhow!("asset {asset} not listed in sha256sums.txt; aborting"))?;

    let bytes = fetch_asset(url, size, download_budget(size))?;

    if !verify_sha256(&bytes, &expected_hex) {
        anyhow::bail!(
            "SHA-256 mismatch for {asset}: download corrupted or tampered; aborting update"
        );
    }

    replace_verified(&bytes, gate, |staged| self_replace::self_replace(staged))
}

/// Runs `replace(staged)` under the self-update mark. The mark is set before
/// the call, since the rename lands inside it: one set after it returns leaves
/// a window where the new file reads unmarked. A call that fails or unwinds
/// has not put the update's binary in place, so the mark clears; a call that
/// succeeds keeps it for the rest of the run.
fn replace_marked(
    staged: &std::path::Path,
    replace: impl FnOnce(&std::path::Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    REPLACED_BY_SELF_UPDATE.store(true, Ordering::SeqCst);
    let mut clear = ClearSelfUpdateMark { armed: true };
    replace(staged)?;
    clear.armed = false;
    Ok(())
}

/// Clears the self-update mark on drop while armed, so an `Err` return and an
/// unwind out of the replace call both clear it.
struct ClearSelfUpdateMark {
    armed: bool,
}

impl Drop for ClearSelfUpdateMark {
    fn drop(&mut self) {
        if self.armed {
            REPLACED_BY_SELF_UPDATE.store(false, Ordering::SeqCst);
        }
    }
}

/// Stage the verified bytes as an executable file inside a fresh private
/// directory per update, so two clauth processes updating at once never write,
/// truncate or read each other's file between the sha256 check and the
/// self-replace's copy. The file is written, `sync_all`'d and CLOSED before
/// return; on unix the dir is 0700 and the file 0755. The returned path lives
/// inside the dir, and dropping the `TempDir` removes both.
fn stage_verified(bytes: &[u8]) -> std::io::Result<(tempfile::TempDir, std::path::PathBuf)> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("clauth_update");
    #[cfg(unix)]
    {
        // tempfile's default dir mode is 0o755; the staged dir must be 0o700 so
        // no other local user can open it or plant a file beside the binary.
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    let dir = builder.tempdir()?;
    let path = dir.path().join("clauth_update.bin");
    {
        let mut f = fs::File::create(&path)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    }
    Ok((dir, path))
}

fn is_cargo_installed() -> bool {
    let Ok(exe) = env::current_exe() else {
        return false;
    };
    // Through `profile::home_dir` rather than `dirs` directly: that resolver is
    // the one a test can redirect, so this stays inside the sandbox with every
    // other home-derived path.
    let Ok(home) = crate::profile::home_dir() else {
        return false;
    };
    exe.starts_with(home.join(".cargo").join("bin"))
}

fn asset_name() -> Option<&'static str> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("clauth-linux-x86_64")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("clauth-macos-x86_64")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("clauth-macos-aarch64")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("clauth-windows-x86_64.exe")
    } else {
        None
    }
}

fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let v = v.trim_start_matches('v');
    let mut it = v.splitn(3, '.');
    Some((
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    ))
}

pub(crate) fn is_newer(tag: &str, current: &str) -> bool {
    match (parse_version(tag), parse_version(current)) {
        (Some(latest), Some(cur)) => latest > cur,
        _ => false,
    }
}

#[cfg(test)]
#[path = "../tests/inline/update.rs"]
mod tests;
