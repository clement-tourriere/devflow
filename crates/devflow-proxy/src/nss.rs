//! Manual browser trust instructions. No privileged configuration writes.
/// Firefox policy paths for informational display.
pub fn firefox_policy_info() -> String {
    #[cfg(target_os = "macos")]
    {
        "Firefox trusts the macOS Keychain automatically (security.enterprise_roots.enabled)."
            .to_string()
    }

    #[cfg(target_os = "linux")]
    {
        format!(
            "Firefox policy file: {}\n\
            If Firefox was installed via Snap or Flatpak, the policy may not be detected.\n\
            You can import the CA manually in Firefox:\n\
            Settings > Privacy & Security > Certificates > View Certificates > Authorities > Import",
            "/etc/firefox/policies/policies.json"
        )
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        "Import the CA certificate manually in Firefox:\n\
        Settings > Privacy & Security > Certificates > View Certificates > Authorities > Import"
            .to_string()
    }
}
