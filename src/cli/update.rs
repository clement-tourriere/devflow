//! Self-update: download a devflow release binary from GitHub and replace
//! the running executable in place.

use std::path::Path;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// GitHub repository that release binaries are published to.
const GITHUB_REPO: &str = "clement-tourriere/devflow";

/// Version compiled into this binary.
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Release asset name for the current platform, if releases include one.
fn platform_asset() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("devflow-linux-amd64"),
        ("linux", "aarch64") => Some("devflow-linux-arm64"),
        ("macos", "aarch64") => Some("devflow-macos-arm64"),
        _ => None,
    }
}

/// Parse a semver-ish version ("0.5.0", "v0.5.0", "1.2.3-rc.1") into a
/// comparable (major, minor, patch) triple. Pre-release and build-metadata
/// suffixes are ignored for ordering purposes.
fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let core = version
        .trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = match parts.next() {
        Some(p) => p.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// True when `remote` is strictly newer than `local`. Falls back to "any
/// difference counts as an update" when either side does not parse.
fn remote_is_newer(remote: &str, local: &str) -> bool {
    match (parse_version(remote), parse_version(local)) {
        (Some(r), Some(l)) => r > l,
        _ => remote.trim_start_matches('v') != local.trim_start_matches('v'),
    }
}

fn same_version(a: &str, b: &str) -> bool {
    match (parse_version(a), parse_version(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a.trim_start_matches('v') == b.trim_start_matches('v'),
    }
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(format!("devflow/{CURRENT_VERSION}"))
        .timeout(std::time::Duration::from_secs(300))
        .build()?)
}

/// Resolve the latest release tag by following the GitHub `releases/latest`
/// redirect. Avoids the REST API and its unauthenticated rate limits.
async fn resolve_latest_tag() -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent(format!("devflow/{CURRENT_VERSION}"))
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let url = format!("https://github.com/{GITHUB_REPO}/releases/latest");
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("Failed to reach {url}"))?;
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .context("GitHub did not redirect to a release tag (no releases published?)")?;
    if !location.contains("/releases/tag/") {
        bail!("Unexpected redirect while resolving the latest release: {location}");
    }
    let tag = location.rsplit('/').next().unwrap_or_default();
    if tag.is_empty() {
        bail!("Could not extract a release tag from: {location}");
    }
    Ok(tag.to_string())
}

async fn download(client: &reqwest::Client, url: &str, what: &str) -> Result<Vec<u8>> {
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("Failed to download {what} from {url}"))?;
    if !resp.status().is_success() {
        bail!(
            "Failed to download {what}: HTTP {} for {url}",
            resp.status()
        );
    }
    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("Failed to read {what} from {url}"))?;
    Ok(bytes.to_vec())
}

/// Atomically swap the running executable with `new_binary`: stage next to
/// the target (same filesystem), back up the old binary, rename the new one
/// into place, then verify it runs — restoring the backup on failure.
fn install_binary(exe: &Path, new_binary: &[u8]) -> Result<()> {
    let dir = exe
        .parent()
        .context("Current executable has no parent directory")?;
    let pid = std::process::id();
    let staged = dir.join(format!(".devflow-update-{pid}.new"));
    let backup = dir.join(format!(".devflow-update-{pid}.bak"));

    let swap = || -> std::io::Result<()> {
        std::fs::write(&staged, new_binary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(exe)
                .map(|m| m.permissions().mode())
                .unwrap_or(0o755);
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(mode))?;
        }
        std::fs::copy(exe, &backup)?;
        std::fs::rename(&staged, exe)
    };

    if let Err(e) = swap() {
        let _ = std::fs::remove_file(&staged);
        let _ = std::fs::remove_file(&backup);
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            bail!(
                "Permission denied replacing {} — re-run with elevated permissions (e.g. 'sudo devflow update') or reinstall with the install script",
                exe.display()
            );
        }
        return Err(anyhow::Error::new(e).context(format!("Failed to replace {}", exe.display())));
    }

    // Sanity check: the new binary must at least answer --version.
    let healthy = std::process::Command::new(exe)
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    if !healthy {
        let restored = std::fs::rename(&backup, exe).is_ok();
        if restored {
            bail!("Updated binary failed to run — previous version was restored");
        }
        bail!(
            "Updated binary failed to run and the previous version could not be restored. Reinstall from https://github.com/{GITHUB_REPO}/releases"
        );
    }
    let _ = std::fs::remove_file(&backup);
    Ok(())
}

pub(super) async fn handle_update_command(
    check: bool,
    requested_version: Option<String>,
    force: bool,
    json_output: bool,
) -> Result<()> {
    // Resolve the target release tag.
    let tag = match &requested_version {
        Some(v) => format!("v{}", v.trim_start_matches('v')),
        None => resolve_latest_tag().await?,
    };
    let target_version = tag.trim_start_matches('v').to_string();
    let release_url = format!("https://github.com/{GITHUB_REPO}/releases/tag/{tag}");

    let newer = remote_is_newer(&target_version, CURRENT_VERSION);
    let already_on_target = same_version(&target_version, CURRENT_VERSION);

    if check {
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "status": "ok",
                    "action": "update-check",
                    "current_version": CURRENT_VERSION,
                    "target_version": target_version,
                    "update_available": newer,
                }))?
            );
        } else if newer {
            println!(
                "Update available: {CURRENT_VERSION} → {target_version} ({release_url})\nRun 'devflow update' to install."
            );
        } else {
            println!("devflow {CURRENT_VERSION} is up to date (latest release: {target_version})");
        }
        return Ok(());
    }

    // With an explicit --version, install whenever it differs from the
    // current build; otherwise only move forward.
    let should_install = if requested_version.is_some() {
        !already_on_target || force
    } else {
        newer || force
    };
    if !should_install {
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "status": "ok",
                    "action": "update",
                    "current_version": CURRENT_VERSION,
                    "target_version": target_version,
                    "updated": false,
                }))?
            );
        } else {
            println!("devflow {CURRENT_VERSION} is up to date (latest release: {target_version})");
            println!("Use --force to reinstall.");
        }
        return Ok(());
    }

    let asset = platform_asset().with_context(|| {
        format!(
            "No prebuilt binary for {}/{} — install from source: cargo install --git https://github.com/{GITHUB_REPO}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let base_url = format!("https://github.com/{GITHUB_REPO}/releases/download/{tag}");
    let client = http_client()?;

    if !json_output {
        println!("Downloading devflow {target_version} ({asset})...");
    }
    let binary = download(&client, &format!("{base_url}/{asset}"), asset)
        .await
        .with_context(|| format!("Release {tag} may not provide a binary for this platform"))?;

    let checksum_raw = download(
        &client,
        &format!("{base_url}/{asset}.sha256"),
        "the SHA-256 checksum",
    )
    .await?;
    let checksum_text = String::from_utf8_lossy(&checksum_raw);
    let expected = checksum_text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase();
    let actual = sha256_hex(&binary);
    if expected.len() != 64 || expected != actual {
        bail!(
            "Checksum mismatch for {asset} (expected {expected}, got {actual}) — aborting update"
        );
    }

    let exe = std::env::current_exe().context("Failed to locate the current executable")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    install_binary(&exe, &binary)?;

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": "ok",
                "action": "update",
                "current_version": CURRENT_VERSION,
                "target_version": target_version,
                "updated": true,
                "asset": asset,
                "path": exe.display().to_string(),
            }))?
        );
    } else {
        println!(
            "Updated devflow {CURRENT_VERSION} → {target_version} ({})",
            exe.display()
        );
        println!("Release notes: {release_url}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_prefixed_versions() {
        assert_eq!(parse_version("0.5.0"), Some((0, 5, 0)));
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version(" v10.20.30 "), Some((10, 20, 30)));
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
    }

    #[test]
    fn ignores_prerelease_and_build_suffixes() {
        assert_eq!(parse_version("1.2.3-rc.1"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2.3+build.5"), Some((1, 2, 3)));
    }

    #[test]
    fn rejects_unparseable_versions() {
        assert_eq!(parse_version("abc"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn compares_versions_numerically() {
        assert!(remote_is_newer("0.6.0", "0.5.0"));
        assert!(remote_is_newer("0.5.10", "0.5.9"));
        assert!(remote_is_newer("v1.0.0", "0.9.9"));
        assert!(!remote_is_newer("0.5.0", "0.5.0"));
        assert!(!remote_is_newer("0.4.9", "0.5.0"));
    }

    #[test]
    fn same_version_handles_v_prefix() {
        assert!(same_version("v0.5.0", "0.5.0"));
        assert!(!same_version("0.5.1", "0.5.0"));
    }

    #[test]
    fn release_platforms_map_to_published_assets() {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        assert_eq!(platform_asset(), Some("devflow-linux-amd64"));
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        assert_eq!(platform_asset(), Some("devflow-linux-arm64"));
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert_eq!(platform_asset(), Some("devflow-macos-arm64"));
    }

    #[test]
    fn current_version_is_parseable() {
        assert!(parse_version(CURRENT_VERSION).is_some());
    }
}
