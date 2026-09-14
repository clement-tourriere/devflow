//! Exercise the installer without network access or writes to the user's bin directory.
#![cfg(any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64")))]
use std::{os::unix::fs::PermissionsExt, path::Path, process::Command};

#[test]
fn optional_proxy_install_verifies_both_assets_and_preserves_existing_proxy_on_bad_checksum() {
    let root = tempfile::tempdir().unwrap();
    let assets = root.path().join("assets");
    let bin = root.path().join("bin");
    let install = root.path().join("install");
    std::fs::create_dir(&assets).unwrap();
    std::fs::create_dir(&bin).unwrap();
    let asset = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "devflow-macos-arm64",
        ("linux", "aarch64") => "devflow-linux-arm64",
        ("linux", "x86_64") => "devflow-linux-amd64",
        platform => panic!("Unsupported installer fixture platform: {platform:?}"),
    };
    std::fs::write(
        assets.join(asset),
        r"#!/bin/sh
printf 'devflow 0.0.0\n'
",
    )
    .unwrap();
    std::fs::write(
        assets.join(format!("{asset}.sha256")),
        "bc85f530ed855ce1bd85371cb5b294bb5161243bf6ce4580ae732650cb11b4ee",
    )
    .unwrap();
    std::fs::write(
        assets.join(format!("{asset}-proxy")),
        r"#!/bin/sh
printf 'devflow-proxy 0.0.0\n'
",
    )
    .unwrap();
    let proxy_checksum = assets.join(format!("{asset}-proxy.sha256"));
    std::fs::write(
        &proxy_checksum,
        "70c8c7a8289cfb1dd33cee6ebab76462dd25bd7f9819e3488b4dcddfb2272a20",
    )
    .unwrap();
    let downloader = bin.join("curl");
    std::fs::write(
        &downloader,
        r#"#!/bin/sh
set -eu
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o) destination="$2"; shift ;;
        https:*) asset="${1##*/}" ;;
    esac
    shift
done
cp "$FIXTURE_ASSETS/$asset" "$destination"
"#,
    )
    .unwrap();
    std::fs::set_permissions(downloader, std::fs::Permissions::from_mode(0o755)).unwrap();
    let run = || {
        Command::new("/bin/sh")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install.sh"))
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("FIXTURE_ASSETS", &assets)
            .env("DEVFLOW_INSTALL_DIR", &install)
            .env("DEVFLOW_INSTALL_PROXY", "1")
            .env("DEVFLOW_VERSION", "v0.0.0")
            .output()
            .unwrap()
    };
    let installed = run();
    assert!(installed.status.success(), "{installed:?}");
    assert_eq!(
        std::fs::read(install.join("devflow")).unwrap(),
        std::fs::read(assets.join(asset)).unwrap()
    );
    let existing = std::fs::read(install.join("devflow-proxy")).unwrap();
    assert_eq!(
        existing,
        std::fs::read(assets.join(format!("{asset}-proxy"))).unwrap()
    );
    std::fs::write(proxy_checksum, "wrong checksum").unwrap();
    let failed = run();
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("checksum mismatch"));
    assert_eq!(
        std::fs::read(install.join("devflow-proxy")).unwrap(),
        existing
    );
}
