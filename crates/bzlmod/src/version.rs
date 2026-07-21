//! Bzlmod module versions.
//!
//! Bazel's module version format is a relaxed SemVer, specified in
//! `Version.java` upstream. The relaxations matter here, because the registry
//! already contains versions that strict SemVer rejects:
//!
//!   * `0.3.0-rc1`                     — prerelease, sorts *below* `0.3.0`
//!   * `0.0.0-20260422-20ac242a`       — googleapis' pseudo-version
//!   * `6.5`                           — rules_jvm_external, two components
//!   * `""`                            — the empty version, used by
//!                                       `*_override`; compares below everything
//!
//! Ordering rules (upstream `Version.compareTo`):
//!
//!   1. The empty version is lower than every non-empty version.
//!   2. Compare `release` identifiers pairwise, left to right. A missing
//!      identifier is treated as lower, so `1.0` < `1.0.1`.
//!   3. All-digit identifiers compare numerically; others compare
//!      lexicographically; a digit-only identifier is **lower** than a
//!      non-digit one.
//!   4. A version *with* a prerelease is lower than the same version without
//!      one (`0.3.0-rc1` < `0.3.0`), and prereleases compare by rule 3.
//!   5. Build metadata (`+build`) is parsed and preserved but **ignored** in
//!      comparison, per SemVer.
//!
//! `rels` has two separate hand-rolled `version_cmp` functions
//! (`tools/rels/src/common.rs` and `tools/rels/src/deps.rs`) that each
//! approximate this. Neither implements rule 3's digit/non-digit precedence,
//! so both order `1.0-alpha` and `1.0-1` wrongly. This is the one
//! implementation the gate uses.

use std::cmp::Ordering;
use std::fmt;

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum VersionError {
    #[error("version {0:?}: empty identifier in {1:?}")]
    EmptyIdentifier(String, String),
    #[error("version {0:?}: identifier {1:?} contains a character outside [A-Za-z0-9-]")]
    BadCharacter(String, String),
    #[error("version {0:?}: release component {1:?} must be all digits")]
    NonNumericRelease(String, String),
}

/// One dot-separated component of a release or prerelease.
///
/// The numeric/alphanumeric distinction is decided at parse time because it
/// drives comparison (rule 3) and we do not want to re-derive it per compare.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Identifier {
    Numeric(u64),
    Alphanumeric(String),
}

impl Ord for Identifier {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Identifier::Numeric(a), Identifier::Numeric(b)) => a.cmp(b),
            (Identifier::Alphanumeric(a), Identifier::Alphanumeric(b)) => a.cmp(b),
            // Rule 3: digit-only sorts below non-digit.
            (Identifier::Numeric(_), Identifier::Alphanumeric(_)) => Ordering::Less,
            (Identifier::Alphanumeric(_), Identifier::Numeric(_)) => Ordering::Greater,
        }
    }
}

impl PartialOrd for Identifier {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Identifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Identifier::Numeric(n) => write!(f, "{n}"),
            Identifier::Alphanumeric(s) => write!(f, "{s}"),
        }
    }
}

/// A parsed bzlmod version.
///
/// `Display` round-trips the original string exactly, so a `Version` can be
/// written back into a `MODULE.bazel` or `metadata.json` without normalizing
/// `6.5` into `6.5.0`. That fidelity is load-bearing: the registry path
/// `modules/<name>/<version>/` is the literal string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Version {
    raw: String,
    release: Vec<Identifier>,
    prerelease: Vec<Identifier>,
    /// Preserved for round-tripping; ignored by `Ord` per SemVer.
    build: Option<String>,
}

impl Version {
    /// The empty version — what `*_override` directives carry, and the bottom
    /// of the ordering.
    pub fn empty() -> Self {
        Version {
            raw: String::new(),
            release: Vec::new(),
            prerelease: Vec::new(),
            build: None,
        }
    }

    pub fn parse(s: &str) -> Result<Self, VersionError> {
        if s.is_empty() {
            return Ok(Version::empty());
        }

        let (rest, build) = match s.split_once('+') {
            Some((rest, build)) => (rest, Some(build.to_string())),
            None => (s, None),
        };

        // Split on the FIRST '-': everything after it is the prerelease, which
        // may itself contain '-' (googleapis' `0.0.0-20260422-20ac242a` is one
        // prerelease of two identifiers, not three release components).
        let (release_str, prerelease_str) = match rest.split_once('-') {
            Some((r, p)) => (r, Some(p)),
            None => (rest, None),
        };

        let release = parse_identifiers(s, release_str)?;
        // Release components must be numeric — this is what distinguishes a
        // release from a prerelease identifier upstream.
        for (raw, id) in release_str.split('.').zip(release.iter()) {
            if matches!(id, Identifier::Alphanumeric(_)) {
                return Err(VersionError::NonNumericRelease(
                    s.to_string(),
                    raw.to_string(),
                ));
            }
        }

        let prerelease = match prerelease_str {
            Some(p) => parse_identifiers(s, p)?,
            None => Vec::new(),
        };

        Ok(Version {
            raw: s.to_string(),
            release,
            prerelease,
            build,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    pub fn is_prerelease(&self) -> bool {
        !self.prerelease.is_empty()
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The major component, or `None` for the empty version. Used by the V1
    /// gate to decide whether a module has reached 1.0.0 and therefore owes a
    /// `compatibility_level`.
    pub fn major(&self) -> Option<u64> {
        match self.release.first() {
            Some(Identifier::Numeric(n)) => Some(*n),
            _ => None,
        }
    }

    /// True once the module is at or past 1.0.0 and not a prerelease of it.
    pub fn is_stable(&self) -> bool {
        self.major().is_some_and(|m| m >= 1) && !self.is_prerelease()
    }
}

fn parse_identifiers(whole: &str, part: &str) -> Result<Vec<Identifier>, VersionError> {
    part.split('.')
        .map(|id| {
            if id.is_empty() {
                return Err(VersionError::EmptyIdentifier(
                    whole.to_string(),
                    part.to_string(),
                ));
            }
            if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                return Err(VersionError::BadCharacter(whole.to_string(), id.to_string()));
            }
            // A leading-zero numeric like `007` is kept alphanumeric so it does
            // not silently compare equal to `7`; upstream treats only canonical
            // digit strings as numeric.
            if id.chars().all(|c| c.is_ascii_digit()) && (id == "0" || !id.starts_with('0')) {
                match id.parse::<u64>() {
                    Ok(n) => Ok(Identifier::Numeric(n)),
                    // Overflows a u64 — keep it comparable rather than failing.
                    Err(_) => Ok(Identifier::Alphanumeric(id.to_string())),
                }
            } else {
                Ok(Identifier::Alphanumeric(id.to_string()))
            }
        })
        .collect()
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        // Rule 1.
        match (self.is_empty(), other.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            (false, false) => {}
        }

        // Rule 2 + 3: pairwise over release, missing == lower.
        let len = self.release.len().max(other.release.len());
        for i in 0..len {
            let ord = match (self.release.get(i), other.release.get(i)) {
                (Some(a), Some(b)) => a.cmp(b),
                (Some(_), None) => Ordering::Greater,
                (None, Some(_)) => Ordering::Less,
                (None, None) => Ordering::Equal,
            };
            if ord != Ordering::Equal {
                return ord;
            }
        }

        // Rule 4: having a prerelease sorts below not having one.
        match (self.is_prerelease(), other.is_prerelease()) {
            (false, false) => return Ordering::Equal,
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            (true, true) => {}
        }

        let len = self.prerelease.len().max(other.prerelease.len());
        for i in 0..len {
            let ord = match (self.prerelease.get(i), other.prerelease.get(i)) {
                (Some(a), Some(b)) => a.cmp(b),
                (Some(_), None) => Ordering::Greater,
                (None, Some(_)) => Ordering::Less,
                (None, None) => Ordering::Equal,
            };
            if ord != Ordering::Equal {
                return ord;
            }
        }

        // Rule 5: build metadata is not compared.
        Ordering::Equal
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|e| panic!("parse {s:?}: {e}"))
    }

    #[test]
    fn round_trips_the_literal_string() {
        // The registry path is the literal version string, so `6.5` must not
        // normalize to `6.5.0`.
        for s in ["1.0.0", "6.5", "0.3.0-rc1", "0.0.0-20260422-20ac242a", ""] {
            assert_eq!(v(s).to_string(), s);
        }
    }

    #[test]
    fn empty_is_the_bottom() {
        assert!(v("") < v("0.0.0"));
        assert_eq!(v(""), Version::empty());
        assert!(v("").is_empty());
    }

    #[test]
    fn release_components_compare_numerically() {
        // The classic string-compare bug: "10" < "3" lexicographically.
        assert!(v("0.3.0") < v("0.10.0"));
        assert!(v("1.0.0") < v("2.0.0"));
        assert!(v("0.0.10") < v("0.0.11"));
    }

    #[test]
    fn missing_release_component_is_lower() {
        assert!(v("1.0") < v("1.0.1"));
        assert!(v("6.5") < v("6.6"));
        assert_eq!(v("1.0").cmp(&v("1.0")), Ordering::Equal);
    }

    #[test]
    fn prerelease_sorts_below_its_release() {
        assert!(v("0.3.0-rc1") < v("0.3.0"));
        assert!(v("0.2.2") < v("0.3.0-rc1"));
        // The full registry ordering for rules_lean's early line.
        let mut got = vec![v("0.3.0"), v("0.2.2"), v("0.3.0-rc1"), v("0.3.1")];
        got.sort();
        let want: Vec<String> = ["0.2.2", "0.3.0-rc1", "0.3.0", "0.3.1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(got.iter().map(|x| x.to_string()).collect::<Vec<_>>(), want);
    }

    #[test]
    fn digit_only_prerelease_identifier_sorts_below_alphanumeric() {
        // Rule 3. This is what both of rels' version_cmp functions get wrong.
        assert!(v("1.0.0-1") < v("1.0.0-alpha"));
        assert!(v("1.0.0-2") < v("1.0.0-11"));
    }

    #[test]
    fn googleapis_pseudo_version_parses_as_one_prerelease() {
        let g = v("0.0.0-20260422-20ac242a");
        assert!(g.is_prerelease());
        assert_eq!(g.major(), Some(0));
        assert!(g < v("0.0.0"));
    }

    #[test]
    fn build_metadata_is_preserved_but_not_compared() {
        let a = v("1.0.0+abc");
        let b = v("1.0.0+xyz");
        assert_eq!(a.cmp(&b), Ordering::Equal);
        assert_eq!(a.to_string(), "1.0.0+abc");
    }

    #[test]
    fn stability_drives_the_v1_gate() {
        assert!(!v("0.5.5").is_stable());
        assert!(!v("1.0.0-rc1").is_stable());
        assert!(v("1.0.0").is_stable());
        assert!(v("2.22.5").is_stable());
        // No module in the registry is stable today — that is the V1 premise.
        assert!(!v("").is_stable());
    }

    #[test]
    fn rejects_non_numeric_release_components() {
        assert!(matches!(
            Version::parse("1.x.0"),
            Err(VersionError::NonNumericRelease(..))
        ));
    }

    #[test]
    fn rejects_malformed_identifiers() {
        assert!(matches!(
            Version::parse("1..0"),
            Err(VersionError::EmptyIdentifier(..))
        ));
        assert!(matches!(
            Version::parse("1.0.0-a_b"),
            Err(VersionError::BadCharacter(..))
        ));
    }

    #[test]
    fn leading_zero_identifier_stays_distinct() {
        // `007` must not compare equal to `7`.
        assert_ne!(v("1.0.0-007"), v("1.0.0-7"));
    }
}
