//! Standard builds delegate to the companion without embedding proxy networking.
#![cfg(all(unix, not(feature = "proxy")))]
use std::{os::unix::fs::PermissionsExt, process::Command};

#[test]
fn standard_cli_forwards_proxy_arguments_and_preserves_structured_errors() {
    let root = tempfile::tempdir().unwrap();
    let cli = root.path().join("devflow");
    let companion = root.path().join("devflow-proxy");
    std::fs::copy(env!("CARGO_BIN_EXE_devflow"), &cli).unwrap();
    std::fs::write(
        &companion,
        r#"#!/bin/sh
printf '{"args":['
separator=''
for argument in "$@"; do
    printf '%s"%s"' "$separator" "$argument"
    separator=','
done
printf ']}\n'
"#,
    )
    .unwrap();
    std::fs::set_permissions(&companion, std::fs::Permissions::from_mode(0o755)).unwrap();
    let run = |args: &[&str]| {
        Command::new(&cli)
            .current_dir(root.path())
            .env("DEVFLOW_CONFIG_DIR", root.path().join("config"))
            .args(args)
            .output()
            .unwrap()
    };
    let forwarded = run(&[
        "--json",
        "proxy",
        "start",
        "--daemon",
        "--api-port",
        "45678",
        "--domain-suffix",
        "fixture.localhost",
        "--no-auto-network",
        "--no-mdns",
    ]);
    assert!(forwarded.status.success(), "{forwarded:?}");
    let value: serde_json::Value = serde_json::from_slice(&forwarded.stdout).unwrap();
    assert_eq!(
        value["args"],
        serde_json::json!([
            "--json",
            "--non-interactive",
            "proxy",
            "start",
            "--daemon",
            "--no-auto-network",
            "--no-mdns",
            "--api-port",
            "45678",
            "--domain-suffix",
            "fixture.localhost"
        ])
    );
    let human = run(&["proxy", "trust", "info"]);
    assert!(human.status.success(), "{human:?}");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&human.stdout).unwrap()["args"],
        serde_json::json!(["--non-interactive", "proxy", "trust", "info"])
    );
    std::fs::write(
        &companion,
        "#!/bin/sh\nprintf '{\"error\":\"fixture failure\"}\\n'\nexit 7\n",
    )
    .unwrap();
    let failed = run(&["--json", "proxy", "status"]);
    assert!(!failed.status.success());
    let error: serde_json::Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("fixture failure"));
}
