use std::env;
use std::fs;
use std::io::Write;
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::Duration;

use minisign_verify::{PublicKey, Signature};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

const API_URL: &str = "https://api.github.com/repos/uwuclxdy/clauth/releases/latest";
/// Env var: set to `1` to disable all background update work.
const NO_UPDATE_ENV: &str = "CLAUTH_NO_UPDATE";

/// This is the macOS fork (`xingfanxia/clauth`), which has **no release
/// pipeline**. The upstream self-updater ([`API_URL`] → `download_and_replace`)
/// would fetch `uwuclxdy/clauth`'s latest release — a newer version number
/// (0.7.2 > this 0.7.1) built WITHOUT the daemon/keychain features — and
/// self-replace this binary with it, silently deleting the whole fork. So the
/// updater is disabled at the source: [`spawn`] returns `None` and
/// [`should_self_replace`] is always `false`. Retargeting [`API_URL`] to the
/// fork is deliberately NOT done — the fork ships no signed release assets.
const FORK_BUILD: bool = true;

/// Pinned minisign public key (base64 — the key line of `minisign.pub`, WITHOUT
/// the `untrusted comment:` header). When set, every self-update verifies a
/// detached minisign signature over `sha256sums.txt` against this key BEFORE
/// trusting any hash it lists; authenticating the sums file transitively
/// authenticates every asset hash in it.
///
/// EMPTY (the default) keeps signature enforcement OFF — the updater stays on
/// SHA-256-only integrity, exactly as before, so auto-update keeps working
/// during rollout. Pinning a real key here ACTIVATES fail-closed authenticity
/// (missing/invalid signature ⇒ no update) and, being a compile-time constant,
/// can never be downgraded at runtime.
///
/// Setup: `minisign -G -W` (passwordless) → paste the secret-key file contents
/// into the `MINISIGN_SECRET_KEY` GitHub Actions secret, the public key here.
const MINISIGN_PUBLIC_KEY: &str = "RWS4HZQQuH8GjgSAz119H+6csSha0uFjoMKt3gx8Ror9Kvh3nObNSmVm";

/// Outcome of the background update check. Errors are silent; only actionable
/// results are reported.
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
}

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Returns `true` when the update system is active (env var unset or not `"1"`).
pub(crate) fn updates_enabled() -> bool {
    env::var(NO_UPDATE_ENV).as_deref() != Ok("1")
}

/// Spawn a background update check; applies if self-replaceable, toasts result.
/// Returns a `JoinHandle` for clean shutdown, or `None` when updates are disabled.
pub(crate) fn spawn(tx: Sender<UpdateEvent>) -> Option<JoinHandle<()>> {
    // Fork builds have no release pipeline — never run the upstream self-updater.
    if FORK_BUILD {
        return None;
    }
    if !updates_enabled() {
        return None;
    }
    #[allow(clippy::expect_used, reason = "thread spawn failure is unrecoverable")]
    Some(
        std::thread::Builder::new()
            .name("clauth-upd".into())
            .spawn(move || {
                let _ = try_update(&tx);
            })
            .expect("failed to spawn update thread"),
    )
}

fn try_update(tx: &Sender<UpdateEvent>) -> anyhow::Result<()> {
    let release = fetch_latest()?;

    if !is_newer(&release.tag_name, CURRENT_VERSION) {
        return Ok(());
    }
    let version = release.tag_name.trim_start_matches('v').to_string();

    // Cargo install or unsupported platform: notify only.
    let Some(asset) = asset_name() else {
        let _ = tx.send(UpdateEvent::Available(version));
        return Ok(());
    };
    // A fork build (no release pipeline) or a cargo install must never
    // self-replace — notify only. Backstop; `spawn` already returns `None` for
    // fork builds, so `try_update` isn't reached in production.
    if !should_self_replace(is_cargo_installed()) {
        let _ = tx.send(UpdateEvent::Available(version));
        return Ok(());
    }

    let Some(url) = release
        .assets
        .iter()
        .find(|a| a.name == asset)
        .map(|a| a.browser_download_url.clone())
    else {
        return Ok(());
    };

    let sums_url = derive_sums_url(&url, asset);

    download_and_replace(&url, &sums_url, asset)?;
    let _ = tx.send(UpdateEvent::Installed(version));
    Ok(())
}

/// Whether this build may download-and-replace its own binary from a GitHub
/// release. A **fork build** never may (no release pipeline — see [`FORK_BUILD`]);
/// a **cargo install** never may (upgrade with `cargo install`). Pure so the
/// contract is unit-testable without the network.
fn should_self_replace(is_cargo_installed: bool) -> bool {
    !FORK_BUILD && !is_cargo_installed
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

fn make_agent() -> ureq::Agent {
    // CAUTION if FORK_BUILD is ever flipped back on: in ureq 3
    // `timeout_recv_response` keeps counting through the BODY read (pinned by
    // the proxy's timeout-semantics tests) — 30 s over the multi-MB release
    // binary aborts the download below ~340 KB/s. Dead code today (spawn()
    // short-circuits on FORK_BUILD), but re-enabling self-update must size
    // this for the full asset at worst-case throughput, or stream with the
    // proxy's terminal-detection approach (timeout-sweep 2026-07-18).
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .build()
        .into()
}

fn fetch_latest() -> anyhow::Result<Release> {
    let text = make_agent()
        .get(API_URL)
        .header("User-Agent", "clauth-updater")
        .call()
        .map_err(anyhow::Error::from)?
        .body_mut()
        .read_to_string()
        .map_err(anyhow::Error::from)?;

    Ok(serde_json::from_str(&text)?)
}

fn download_and_replace(url: &str, sums_url: &str, asset: &str) -> anyhow::Result<()> {
    let agent = make_agent();

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
    verify_sums_signature(&agent, sums_url, &sums_text)?;

    let expected_hex = find_expected_sha(&sums_text, asset)
        .ok_or_else(|| anyhow::anyhow!("asset {asset} not listed in sha256sums.txt; aborting"))?;

    let bytes = agent
        .get(url)
        .header("User-Agent", "clauth-updater")
        .call()
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .body_mut()
        .read_to_vec()
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if !verify_sha256(&bytes, &expected_hex) {
        anyhow::bail!(
            "SHA-256 mismatch for {asset}: download corrupted or tampered; aborting update"
        );
    }

    let tmp_path = env::temp_dir().join("clauth_update.tmp");
    let _ = fs::remove_file(&tmp_path);

    {
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o755))?;
    }

    self_replace::self_replace(&tmp_path)?;
    let _ = fs::remove_file(&tmp_path);

    Ok(())
}

fn is_cargo_installed() -> bool {
    let Ok(exe) = env::current_exe() else {
        return false;
    };
    let Some(home) = dirs::home_dir() else {
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

fn is_newer(tag: &str, current: &str) -> bool {
    match (parse_version(tag), parse_version(current)) {
        (Some(latest), Some(cur)) => latest > cur,
        _ => false,
    }
}

#[cfg(test)]
#[path = "../tests/inline/update.rs"]
mod tests;
