//! Self-update mechanism — checks for and installs updates from GitHub releases.

use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use semver::Version;
use sha2::{Digest, Sha256};

use crate::UpdateArgs;

// ── Constants ─────────────────────────────────────────────────────────────────

const GITHUB_REPO: &str = "greysquirr3l/accroitre";
const GITHUB_API_BASE: &str = "https://api.github.com";

// ── Public API ────────────────────────────────────────────────────────────────

/// Execute the update command: either check-only, dry-run, or download-and-replace.
///
/// # Errors
///
/// Returns an error if the GitHub API is unreachable, the requested version is
/// not found, download or checksum verification fails, or the binary cannot be
/// replaced on disk.
pub async fn run(args: &UpdateArgs) -> Result<()> {
    let current = current_version()?;
    let releases = fetch_releases().await?;

    if releases.is_empty() {
        println!("No releases found.");
        return Ok(());
    }

    if args.check {
        check_only(&current, &releases);
        return Ok(());
    }

    // Determine target version.
    let target = if let Some(ref v) = args.version {
        let requested = parse_version(v)?;
        releases
            .iter()
            .find(|r| r.version == requested)
            .with_context(|| format!("Version {requested} not found in releases"))?
    } else {
        latest_release(&releases).context("No suitable release found")?
    };

    if target.version <= current {
        println!("Already up to date (v{current}).");
        return Ok(());
    }

    if args.dry_run {
        dry_run(target).await
    } else {
        install(target).await
    }
}

// ── Data types ────────────────────────────────────────────────────────────────

/// A parsed GitHub release.
#[derive(Debug, Clone)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    pub body: String,
    pub assets: Vec<Asset>,
}

/// A release asset (binary or checksum file).
#[derive(Debug, Clone)]
pub struct Asset {
    pub name: String,
    pub download_url: String,
}

// ── Version helpers ───────────────────────────────────────────────────────────

/// Parse the current binary version from `CARGO_PKG_VERSION`.
///
/// # Errors
///
/// Returns an error if the embedded version string is not valid semver.
pub fn current_version() -> Result<Version> {
    let raw = env!("CARGO_PKG_VERSION");
    Version::parse(raw).context("Failed to parse current version")
}

/// Parse a user-supplied version string, tolerating a leading `v`.
///
/// # Errors
///
/// Returns an error if the string is not valid semver.
pub fn parse_version(input: &str) -> Result<Version> {
    let trimmed = input.strip_prefix('v').unwrap_or(input);
    Version::parse(trimmed).with_context(|| format!("Invalid version: {input}"))
}

/// Compare two versions; returns true when `available` is newer than `current`.
#[must_use]
pub fn is_newer(current: &Version, available: &Version) -> bool {
    available > current
}

/// Find the latest (highest semver) non-prerelease release.
fn latest_release(releases: &[Release]) -> Option<&Release> {
    releases
        .iter()
        .filter(|r| r.version.pre.is_empty())
        .max_by(|a, b| a.version.cmp(&b.version))
}

// ── GitHub API ────────────────────────────────────────────────────────────────

/// Fetch available releases from the GitHub API.
///
/// # Errors
///
/// Returns an error if the HTTP request fails or the response cannot be parsed.
pub async fn fetch_releases() -> Result<Vec<Release>> {
    let url = format!("{GITHUB_API_BASE}/repos/{GITHUB_REPO}/releases");
    let client = http_client()?;

    let response = client
        .get(&url)
        .send()
        .await
        .context("Failed to reach GitHub API")?;

    if !response.status().is_success() {
        bail!("GitHub API returned status {}", response.status().as_u16());
    }

    let body: Vec<serde_json::Value> = response
        .json()
        .await
        .context("Failed to parse GitHub releases JSON")?;

    let mut releases = Vec::new();
    for entry in &body {
        if let Some(release) = parse_release(entry) {
            releases.push(release);
        }
    }

    Ok(releases)
}

/// Parse a single release JSON object.
fn parse_release(value: &serde_json::Value) -> Option<Release> {
    let tag = value.get("tag_name")?.as_str()?;
    let version_str = tag.strip_prefix('v').unwrap_or(tag);
    let version = Version::parse(version_str).ok()?;
    let body = value
        .get("body")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_owned();

    let assets = value
        .get("assets")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    let name = a.get("name")?.as_str()?.to_owned();
                    let download_url = a.get("browser_download_url")?.as_str()?.to_owned();
                    Some(Asset { name, download_url })
                })
                .collect()
        })
        .unwrap_or_default();

    Some(Release {
        version,
        tag: tag.to_owned(),
        body,
        assets,
    })
}

// ── Check-only ────────────────────────────────────────────────────────────────

fn check_only(current: &Version, releases: &[Release]) {
    if let Some(latest) = latest_release(releases) {
        if is_newer(current, &latest.version) {
            println!("Update available: v{} → v{}", current, latest.version);
            if !latest.body.is_empty() {
                println!("\nRelease notes:\n{}", latest.body);
            }
        } else {
            println!("Already up to date (v{current}).");
        }
    } else {
        println!("No stable releases found.");
    }
}

// ── Install ───────────────────────────────────────────────────────────────────

async fn install(release: &Release) -> Result<()> {
    let binary_bytes = download_and_verify(release).await?;

    // Replace the running binary.
    let current_exe = env::current_exe().context("Cannot determine current executable path")?;
    replace_binary(&current_exe, &binary_bytes)?;

    println!("Updated to accro v{}.", release.version);
    Ok(())
}

/// Download and verify the target release without installing it.
///
/// Writes the verified binary to a temp file and prints its path and SHA-256
/// digest. Used by `--dry-run` (CI smoke test) and by tests that exercise the
/// download + checksum pipeline without touching the running binary.
async fn dry_run(release: &Release) -> Result<()> {
    let binary_bytes = download_and_verify(release).await?;

    let digest = compute_sha256(&binary_bytes);
    let staging_dir = env::temp_dir();
    let staging_path = staging_dir.join(format!(
        "accro-{}-{}.dryrun",
        release.version,
        platform_asset_name()
    ));
    fs::write(&staging_path, &binary_bytes)
        .with_context(|| format!("Failed to write {}", staging_path.display()))?;

    println!(
        "Dry-run complete: v{} verified, {} bytes written to {}",
        release.version,
        binary_bytes.len(),
        staging_path.display()
    );
    println!("SHA-256: {digest}");

    // Best-effort cleanup. The operator may inspect the file, so we don't
    // remove it on exit.
    Ok(())
}

/// Download the platform-matching binary asset, plus its SHA-256 checksum if
/// present, and verify integrity. Returns the verified raw bytes.
async fn download_and_verify(release: &Release) -> Result<Vec<u8>> {
    let asset_name = platform_asset_name();
    let checksum_name = format!("{asset_name}.sha256");

    let binary_asset = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .with_context(|| {
            format!(
                "No binary asset '{}' found in release v{}",
                asset_name, release.version
            )
        })?;

    let checksum_asset = release.assets.iter().find(|a| a.name == checksum_name);

    println!("Downloading accro v{}…", release.version);

    let client = http_client()?;
    let binary_bytes = download_asset(&client, binary_asset).await?;

    if let Some(cs_asset) = checksum_asset {
        let cs_bytes = download_asset(&client, cs_asset).await?;
        let expected_hex =
            String::from_utf8(cs_bytes).context("Checksum file is not valid UTF-8")?;
        verify_sha256(&binary_bytes, expected_hex.trim())?;
        println!("SHA-256 verified.");
    } else {
        eprintln!("Warning: no checksum file found — skipping integrity check.");
    }

    Ok(binary_bytes)
}

/// Download raw bytes for an asset.
async fn download_asset(client: &reqwest::Client, asset: &Asset) -> Result<Vec<u8>> {
    let response = client
        .get(&asset.download_url)
        .send()
        .await
        .with_context(|| format!("Failed to download {}", asset.name))?;

    if !response.status().is_success() {
        bail!(
            "Download of {} returned status {}",
            asset.name,
            response.status().as_u16()
        );
    }

    response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .with_context(|| format!("Failed to read bytes for {}", asset.name))
}

// ── Checksumming ──────────────────────────────────────────────────────────────

/// Verify that the SHA-256 of `data` matches the expected hex string.
///
/// # Errors
///
/// Returns an error if the computed hash does not match the expected value.
pub fn verify_sha256(data: &[u8], expected_hex: &str) -> Result<()> {
    let actual = compute_sha256(data);

    // The checksum file may have `<hash>  <filename>` format.
    let expected = expected_hex
        .split_whitespace()
        .next()
        .unwrap_or(expected_hex);

    if !constant_time_eq(actual.as_bytes(), expected.as_bytes()) {
        bail!("SHA-256 mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

/// Compute the SHA-256 hex digest of `data`.
#[must_use]
pub fn compute_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    hex_encode(&result)
}

/// Constant-time equality comparison (to prevent timing side-channels on hashes).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Encode bytes as lowercase hex.
fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

// ── Binary replacement ────────────────────────────────────────────────────────

/// Replace the current binary with new content, using a rename-based atomic
/// swap where possible.  Falls back to direct write on platforms that require
/// it (some Windows configurations).
fn replace_binary(exe_path: &Path, new_bytes: &[u8]) -> Result<()> {
    let parent = exe_path
        .parent()
        .context("Cannot determine parent directory of executable")?;

    let backup_path = parent.join(".accro.backup");
    let staging_path = parent.join(".accro.update");

    // Write new binary to staging file.
    {
        let mut f =
            fs::File::create(&staging_path).context("Failed to create staging file for update")?;
        f.write_all(new_bytes)
            .context("Failed to write update to staging file")?;
        f.sync_all().context("Failed to sync staging file")?;
    }

    // Set executable permissions on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o755);
        fs::set_permissions(&staging_path, perms)
            .context("Failed to set permissions on staged binary")?;
    }

    // Rename current → backup, staging → current.
    // If the rename fails, restore from backup.
    if let Err(e) = fs::rename(exe_path, &backup_path) {
        // Clean up staging file on failure.
        let _ = fs::remove_file(&staging_path);
        return Err(e).context("Failed to back up current binary");
    }

    if let Err(e) = fs::rename(&staging_path, exe_path) {
        // Try to restore backup.
        let _ = fs::rename(&backup_path, exe_path);
        return Err(e).context("Failed to install new binary");
    }

    // Clean up backup (best-effort).
    let _ = fs::remove_file(&backup_path);

    Ok(())
}

// ── Platform detection ────────────────────────────────────────────────────────

/// Determine the expected asset name for the current platform.
#[must_use]
pub fn platform_asset_name() -> String {
    let (os, ext) = if cfg!(target_os = "linux") {
        ("linux", "")
    } else if cfg!(target_os = "macos") {
        ("darwin", "")
    } else if cfg!(target_os = "windows") {
        ("windows", ".exe")
    } else {
        ("unknown", "")
    };

    let arch = if cfg!(target_arch = "x86_64") {
        "amd64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "unknown"
    };

    format!("accro-{os}-{arch}{ext}")
}

// ── HTTP client ───────────────────────────────────────────────────────────────

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("accro/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("Failed to build HTTP client")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_version_is_valid_semver() {
        let v = current_version();
        assert!(v.is_ok());
    }

    #[test]
    fn parse_version_with_v_prefix() -> Result<(), Box<dyn std::error::Error>> {
        let v = parse_version("v1.2.3")?;
        assert_eq!(v, Version::new(1, 2, 3));
        Ok(())
    }

    #[test]
    fn parse_version_without_prefix() -> Result<(), Box<dyn std::error::Error>> {
        let v = parse_version("2.0.0")?;
        assert_eq!(v, Version::new(2, 0, 0));
        Ok(())
    }

    #[test]
    fn is_newer_detects_upgrade() {
        let current = Version::new(0, 1, 0);
        let available = Version::new(0, 2, 0);
        assert!(is_newer(&current, &available));
    }

    #[test]
    fn is_newer_rejects_same_version() {
        let v = Version::new(1, 0, 0);
        assert!(!is_newer(&v, &v));
    }

    #[test]
    fn is_newer_rejects_downgrade() {
        let current = Version::new(2, 0, 0);
        let older = Version::new(1, 0, 0);
        assert!(!is_newer(&current, &older));
    }

    #[test]
    fn sha256_verification_succeeds() {
        let data = b"hello world";
        let hash = compute_sha256(data);
        assert!(verify_sha256(data, &hash).is_ok());
    }

    #[test]
    fn sha256_verification_fails_on_mismatch() {
        let data = b"hello world";
        let wrong = "0000000000000000000000000000000000000000000000000000000000000000";
        assert!(verify_sha256(data, wrong).is_err());
    }

    #[test]
    fn sha256_checksum_file_format() {
        // The checksum file may look like: `<hash>  filename`
        let data = b"test data";
        let hash = compute_sha256(data);
        let checksum_line = format!("{hash}  accro-linux-amd64");
        assert!(verify_sha256(data, &checksum_line).is_ok());
    }

    #[test]
    fn parse_release_valid_json() -> Result<(), Box<dyn std::error::Error>> {
        let json = serde_json::json!({
            "tag_name": "v1.0.0",
            "body": "First release",
            "assets": [
                {
                    "name": "accro-linux-amd64",
                    "browser_download_url": "https://example.com/accro-linux-amd64"
                }
            ]
        });
        let release = parse_release(&json).ok_or("expected release")?;
        assert_eq!(release.version, Version::new(1, 0, 0));
        assert_eq!(release.tag, "v1.0.0");
        assert_eq!(release.body, "First release");
        assert_eq!(release.assets.len(), 1);
        let first_asset = release.assets.first().ok_or("expected one asset")?;
        assert_eq!(first_asset.name, "accro-linux-amd64");
        Ok(())
    }

    #[test]
    fn parse_release_skips_invalid_tag() {
        let json = serde_json::json!({
            "tag_name": "not-semver",
            "body": "",
            "assets": []
        });
        assert!(parse_release(&json).is_none());
    }

    #[test]
    fn latest_release_skips_prereleases() -> Result<(), Box<dyn std::error::Error>> {
        let releases = vec![
            Release {
                version: Version::parse("1.0.0-beta")?,
                tag: "v1.0.0-beta".into(),
                body: String::new(),
                assets: vec![],
            },
            Release {
                version: Version::new(0, 9, 0),
                tag: "v0.9.0".into(),
                body: String::new(),
                assets: vec![],
            },
        ];
        let latest = latest_release(&releases).ok_or("should find one")?;
        assert_eq!(latest.version, Version::new(0, 9, 0));
        Ok(())
    }

    #[test]
    fn platform_asset_name_is_non_empty() {
        let name = platform_asset_name();
        assert!(!name.is_empty());
        assert!(name.starts_with("accro-"));
    }

    #[test]
    fn replace_binary_writes_and_cleans_up() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let exe = dir.path().join("accro");

        // Create a "current" binary.
        fs::write(&exe, b"old binary")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&exe, fs::Permissions::from_mode(0o755))?;
        }

        let new_content = b"new binary v2";
        replace_binary(&exe, new_content)?;

        let actual = fs::read(&exe)?;
        assert_eq!(actual, new_content);

        // Verify backup was cleaned up.
        assert!(!dir.path().join(".accro.backup").exists());
        Ok(())
    }

    #[test]
    fn hex_encode_matches_expected() {
        assert_eq!(hex_encode(&[0x00, 0xff, 0xab]), "00ffab");
    }

    #[test]
    fn constant_time_eq_basic() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"ab", b"abc"));
    }

    #[test]
    fn hex_encode_handles_empty_input() {
        assert_eq!(hex_encode(&[]), "");
    }

    #[test]
    fn hex_encode_handles_long_input() -> Result<(), Box<dyn std::error::Error>> {
        let input: Vec<u8> = (0..=255).collect();
        let encoded = hex_encode(&input);
        assert_eq!(encoded.len(), input.len() * 2);
        // Round-trip
        let mut decoded = Vec::with_capacity(input.len());
        for i in (0..encoded.len()).step_by(2) {
            let pair = encoded.get(i..i + 2).ok_or("hex pair out of range")?;
            let byte = u8::from_str_radix(pair, 16).map_err(|e| format!("hex: {e}"))?;
            decoded.push(byte);
        }
        assert_eq!(decoded, input);
        Ok(())
    }
}
