//! Bounded strict launcher SemVer and precedence (LAUNCHER_UPDATE.md §4); no clock or I/O.
use std::cmp::Ordering;

/// A validated ASCII SemVer2 version, at most64 bytes.
#[derive(Clone, Debug)]
pub struct LauncherVersion(semver::Version);

/// Invalid syntax, oversized text or overflowing numeric core component.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("expected strict SemVer, 1 to 64 ASCII bytes")]
pub struct InvalidVersion;

impl LauncherVersion {
    /// No whitespace, prefix, missing/fourth component or numeric leading zeroes.
    pub fn parse(text: &str) -> Result<Self, InvalidVersion> {
        if text.is_empty() || text.len() > 64 || !text.is_ascii() {
            return Err(InvalidVersion);
        }
        semver::Version::parse(text)
            .map(Self)
            .map_err(|_| InvalidVersion)
    }

    /// SemVer precedence: prerelease sorts below stable; build metadata is ignored.
    pub fn precedence_cmp(&self, other: &Self) -> Ordering {
        self.0.cmp_precedence(&other.0)
    }
}

/// Higher of deployment and optional game minimum; an override cannot lower global policy.
/// Equal precedence returns the deployment spelling (metadata never changes eligibility).
pub fn effective_min<'a>(
    global: &'a str,
    game: Option<&'a str>,
) -> Result<&'a str, InvalidVersion> {
    let global_version = LauncherVersion::parse(global)?;
    if let Some(game) = game {
        let game_version = LauncherVersion::parse(game)?;
        if game_version.precedence_cmp(&global_version).is_gt() {
            return Ok(game);
        }
    }
    Ok(global)
}

/// A malformed legacy stored version is not compatible with any configured minimum.
pub fn meets_min(version: &str, minimum: &str) -> Result<bool, InvalidVersion> {
    let minimum = LauncherVersion::parse(minimum)?;
    Ok(LauncherVersion::parse(version)
        .is_ok_and(|version| !version.precedence_cmp(&minimum).is_lt()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_grammar_not_a_dotted_number_approximation() {
        for version in [
            "0.1.0",
            "1.10.0",
            "1.0.0-alpha.9+build.007",
            "1.0.0-0",
            "1.0.0+001",
        ] {
            assert!(LauncherVersion::parse(version).is_ok(), "{version}");
        }
        for version in [
            "",
            "1",
            "0.2",
            "v1.0.0",
            " 1.0.0",
            "1.0.0\n",
            "1.0.0.4",
            "01.0.0",
            "1.01.0",
            "1.0.01",
            "1.0.0-01",
            "1.0.0-",
            "1.0.0+",
            "1.0.0-a..b",
            "1.0.0+á",
            "18446744073709551616.0.0",
        ] {
            assert!(LauncherVersion::parse(version).is_err(), "{version}");
        }
        assert!(LauncherVersion::parse(&format!("1.0.0+{}", "x".repeat(59))).is_err());
    }

    #[test]
    fn semver_order_prerelease_and_metadata() {
        let ordered = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
            "1.0.1",
            "1.10.0",
        ];
        for pair in ordered.windows(2) {
            assert!(!meets_min(pair[0], pair[1]).unwrap(), "{pair:?}");
            assert!(meets_min(pair[1], pair[0]).unwrap());
        }
        assert!(meets_min("1.0.0+old", "1.0.0+new").unwrap());
        assert!(meets_min("0.10.0", "0.2.0").unwrap());
        assert!(!meets_min("legacy-invalid", "0.0.0").unwrap());
        assert!(meets_min("1.0.0", "broken").is_err());
    }

    #[test]
    fn game_floor_cannot_lower_global_and_bad_policy_never_falls_back() {
        assert_eq!(effective_min("1.2.0", Some("1.0.0")), Ok("1.2.0"));
        assert_eq!(effective_min("1.2.0", Some("1.3.0")), Ok("1.3.0"));
        assert_eq!(effective_min("1.2.0", None), Ok("1.2.0"));
        assert_eq!(effective_min("1.2.0", Some("invalid")), Err(InvalidVersion));
    }
}
