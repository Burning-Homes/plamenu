//! Compile-time identity of the running Plamenu binary.
//!
//! The package version comes from the workspace manifest. Build execution,
//! source revision, target, and intended channel are supplied by CI or derived
//! from Git for local builds by `build.rs`.

/// The intended audience and promotion status of a binary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildChannel {
    Release,
    Staging,
    Development,
}

impl BuildChannel {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Staging => "staging",
            Self::Development => "development",
        }
    }
}

#[cfg(plamenu_channel = "release")]
const BUILD_CHANNEL: BuildChannel = BuildChannel::Release;
#[cfg(plamenu_channel = "staging")]
const BUILD_CHANNEL: BuildChannel = BuildChannel::Staging;
#[cfg(plamenu_channel = "development")]
const BUILD_CHANNEL: BuildChannel = BuildChannel::Development;

#[cfg(plamenu_build_number)]
const BUILD_NUMBER: Option<&str> = Some(env!("PLAMENU_BUILD_NUMBER_RESOLVED"));
#[cfg(not(plamenu_build_number))]
const BUILD_NUMBER: Option<&str> = None;

/// Authored software version from `[workspace.package]`.
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Version safe to expose publicly. Production releases deliberately omit the
/// build number; staging and development builds include their intent and ID.
pub const PUBLIC_VERSION: &str = env!("PLAMENU_PUBLIC_VERSION");
/// Exact build identity for administrative and diagnostic surfaces.
pub const FULL_VERSION: &str = env!("PLAMENU_FULL_VERSION");

#[cfg(target_arch = "aarch64")]
const OCI_ARCHITECTURE: &str = "arm64";
#[cfg(target_arch = "x86_64")]
const OCI_ARCHITECTURE: &str = "amd64";
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
const OCI_ARCHITECTURE: &str = std::env::consts::ARCH;

/// Structured identity used by the admin dashboard and diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuildInfo {
    pub version: &'static str,
    pub public_version: &'static str,
    pub full_version: &'static str,
    pub build_number: Option<&'static str>,
    pub revision: &'static str,
    pub short_revision: &'static str,
    pub channel: BuildChannel,
    pub profile: &'static str,
    pub target: &'static str,
    pub architecture: &'static str,
    pub debug_assertions: bool,
    pub dirty: bool,
}

pub const BUILD_INFO: BuildInfo = BuildInfo {
    version: PACKAGE_VERSION,
    public_version: PUBLIC_VERSION,
    full_version: FULL_VERSION,
    build_number: BUILD_NUMBER,
    revision: env!("PLAMENU_GIT_REVISION"),
    short_revision: env!("PLAMENU_GIT_REVISION_SHORT"),
    channel: BUILD_CHANNEL,
    profile: env!("PLAMENU_BUILD_PROFILE"),
    target: env!("PLAMENU_BUILD_TARGET"),
    architecture: OCI_ARCHITECTURE,
    debug_assertions: cfg!(debug_assertions),
    dirty: option_env!("PLAMENU_BUILD_DIRTY").is_some(),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_version_has_one_cargo_source() {
        assert_eq!(PACKAGE_VERSION, env!("CARGO_PKG_VERSION"));
        assert_eq!(BUILD_INFO.version, PACKAGE_VERSION);
    }

    #[test]
    fn non_release_builds_advertise_their_intent() {
        if BUILD_INFO.channel != BuildChannel::Release {
            let marker = match BUILD_INFO.channel {
                BuildChannel::Staging => "staging",
                BuildChannel::Development => "dev",
                BuildChannel::Release => unreachable!(),
            };
            assert!(PUBLIC_VERSION.contains(marker));
            assert!(PUBLIC_VERSION.contains(BUILD_INFO.build_number.unwrap_or("local")));
            assert!(PUBLIC_VERSION.contains(BUILD_INFO.short_revision));
            if BUILD_INFO.debug_assertions {
                assert!(PUBLIC_VERSION.contains("debug"));
            }
        }
    }

    #[test]
    fn release_public_and_full_versions_have_distinct_roles() {
        if BUILD_INFO.channel == BuildChannel::Release {
            assert_eq!(PUBLIC_VERSION, PACKAGE_VERSION);
            assert!(FULL_VERSION.contains("+build."));
        }
    }
}
