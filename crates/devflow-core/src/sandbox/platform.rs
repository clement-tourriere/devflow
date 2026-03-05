/// Platform-specific sandbox capabilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformCapability {
    /// Linux Landlock LSM is available with the given ABI version.
    Landlock { abi_version: u32 },
    /// macOS sandbox-exec (Seatbelt) is available.
    Seatbelt,
    /// No OS-level sandbox support detected.
    None,
}

impl std::fmt::Display for PlatformCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlatformCapability::Landlock { abi_version } => {
                write!(f, "landlock (ABI v{})", abi_version)
            }
            PlatformCapability::Seatbelt => write!(f, "seatbelt (sandbox-exec)"),
            PlatformCapability::None => write!(f, "none"),
        }
    }
}

/// Detect available platform sandbox capabilities.
pub fn detect() -> PlatformCapability {
    #[cfg(target_os = "macos")]
    {
        detect_macos()
    }
    #[cfg(target_os = "linux")]
    {
        detect_linux()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        PlatformCapability::None
    }
}

#[cfg(target_os = "macos")]
fn detect_macos() -> PlatformCapability {
    if which::which("sandbox-exec").is_ok() {
        PlatformCapability::Seatbelt
    } else {
        PlatformCapability::None
    }
}

#[cfg(target_os = "linux")]
fn detect_linux() -> PlatformCapability {
    // Try to detect Landlock ABI version via the landlock crate
    #[cfg(feature = "sandbox-landlock")]
    {
        match landlock::ABI::new_current() {
            landlock::CompatResult::Full(abi) | landlock::CompatResult::Partial(abi) => {
                PlatformCapability::Landlock {
                    abi_version: abi as u32,
                }
            }
            _ => PlatformCapability::None,
        }
    }
    #[cfg(not(feature = "sandbox-landlock"))]
    {
        // Without the landlock crate, we can't detect the ABI
        PlatformCapability::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_returns_valid_capability() {
        let cap = detect();
        // Just ensure it doesn't panic and returns a valid variant
        match cap {
            PlatformCapability::Landlock { abi_version } => {
                assert!(abi_version > 0);
            }
            PlatformCapability::Seatbelt => {
                // Expected on macOS
            }
            PlatformCapability::None => {
                // OK on unsupported platforms
            }
        }
    }

    #[test]
    fn test_display() {
        assert_eq!(
            PlatformCapability::Landlock { abi_version: 3 }.to_string(),
            "landlock (ABI v3)"
        );
        assert_eq!(
            PlatformCapability::Seatbelt.to_string(),
            "seatbelt (sandbox-exec)"
        );
        assert_eq!(PlatformCapability::None.to_string(), "none");
    }
}
