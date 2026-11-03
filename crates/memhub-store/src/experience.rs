//! Properties of curated experience memories: lifecycle status, citations
//! back to evidence and code, independent support and validity in time.
//!
//! They live in the memory's front matter next to the basic properties:
//!
//! ```text
//! status: established
//! cites:
//!   evidence: [evidence/2026-11-03/crash-bot/091512-refund-dup]
//!   code: [src/refund.rs:88@4f2a91c]
//! support: 2
//! applies_to: [src/refund.rs]
//! valid_from: 2026-11-03T09:20:00Z
//! last_verified: { at: 2026-11-03T09:20:00Z, commit: 4f2a91c, by: memhub-curator }
//! ```
//!
//! Memories in an agent's work area carry none of these.

use std::{fmt, str::FromStr};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Result, StoreError};

/// Lifecycle of an experience memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Backed by a single independent source, or migrated and not reviewed yet.
    Provisional,
    /// Backed by at least two independent sources, or verified against code.
    Established,
    /// An agent reported that it contradicts what it observed.
    Disputed,
    /// Replaced by a newer memory (`superseded_by`) or retired; kept for history.
    Superseded,
}

impl Status {
    pub const ALL: [Status; 4] = [
        Status::Provisional,
        Status::Established,
        Status::Disputed,
        Status::Superseded,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Provisional => "provisional",
            Status::Established => "established",
            Status::Disputed => "disputed",
            Status::Superseded => "superseded",
        }
    }

    /// Ranking multiplier applied by experience search.
    pub fn search_weight(self) -> f64 {
        match self {
            Status::Established => 1.25,
            Status::Provisional => 1.0,
            Status::Disputed => 0.5,
            Status::Superseded => 0.2,
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Status {
    type Err = StoreError;

    fn from_str(s: &str) -> Result<Self> {
        Status::ALL
            .into_iter()
            .find(|v| v.as_str().eq_ignore_ascii_case(s.trim()))
            .ok_or_else(|| {
                StoreError::Invalid(format!(
                    "unknown status '{s}', expected provisional, established, disputed or superseded"
                ))
            })
    }
}

/// What an experience memory is based on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cites {
    /// Evidence ids, e.g. `evidence/2026-11-03/crash-bot/091512-refund-dup`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    /// Code locations, `path[:line[-line]][@commit]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub code: Vec<String>,
}

impl Cites {
    pub fn is_empty(&self) -> bool {
        self.evidence.is_empty() && self.code.is_empty()
    }

    /// Adds the entries of `other` that are not cited yet.
    pub fn merge(&mut self, other: &Cites) {
        for e in &other.evidence {
            if !self.evidence.contains(e) {
                self.evidence.push(e.clone());
            }
        }
        for c in &other.code {
            if !self.code.contains(c) {
                self.code.push(c.clone());
            }
        }
    }
}

/// The last time a memory was checked against the code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verified {
    pub at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub by: String,
}

/// Experience properties of a memory; all empty for work-area notes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExperienceMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    #[serde(default, skip_serializing_if = "Cites::is_empty")]
    pub cites: Cites,
    /// Independent sources among the cited evidence (memory-derived evidence
    /// does not count).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub support: u32,
    /// Paths or modules the memory is about, e.g. `src/refund.rs`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applies_to: Vec<String>,
    /// When the claim started to hold (when it was learned, if unknown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<DateTime<Utc>>,
    /// When the claim stopped holding (set on supersede / retire).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verified: Option<Verified>,
    /// Where the memory came from when not curated from evidence, e.g. `v1`
    /// for memories ported from Memory Hub 1.0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Last time the curator reviewed the memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curated_at: Option<DateTime<Utc>>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl ExperienceMeta {
    pub fn is_empty(&self) -> bool {
        *self == ExperienceMeta::default()
    }

    pub fn is_superseded(&self) -> bool {
        self.status == Some(Status::Superseded)
    }

    /// Names of the properties that differ, for change records.
    pub(crate) fn changed(&self, other: &ExperienceMeta) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.status != other.status {
            out.push("status");
        }
        if self.cites != other.cites {
            out.push("cites");
        }
        if self.support != other.support {
            out.push("support");
        }
        if self.applies_to != other.applies_to {
            out.push("applies_to");
        }
        if self.valid_from != other.valid_from
            || self.invalid_at != other.invalid_at
            || self.superseded_by != other.superseded_by
        {
            out.push("validity");
        }
        if self.last_verified != other.last_verified {
            out.push("last_verified");
        }
        if self.origin != other.origin || self.curated_at != other.curated_at {
            out.push("curation");
        }
        out
    }
}

/// Validates a code citation: `path[:line[-line]][@commit]`, one line, no spaces.
pub fn validate_code_ref(value: &str) -> Result<()> {
    let v = value.trim();
    let err = |reason: &str| {
        StoreError::Invalid(format!(
            "invalid code reference '{value}': {reason}; expected 'path[:line[-line]][@commit]', \
             e.g. 'src/refund.rs:88@4f2a91c'"
        ))
    };
    if v.is_empty() {
        return Err(err("empty"));
    }
    if v.chars().count() > 300 {
        return Err(err("longer than 300 characters"));
    }
    if v.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(err("must not contain whitespace"));
    }
    let (path, commit) = match v.split_once('@') {
        Some((p, c)) => (p, Some(c)),
        None => (v, None),
    };
    if path.is_empty() {
        return Err(err("missing path"));
    }
    if let Some(c) = commit {
        if c.is_empty() || !c.chars().all(|ch| ch.is_ascii_alphanumeric() || "._-/".contains(ch)) {
            return Err(err("commit must be a hash, tag or branch name"));
        }
    }
    Ok(())
}

/// The commit part of a code citation, if any.
pub fn code_ref_commit(value: &str) -> Option<&str> {
    value.split_once('@').map(|(_, c)| c).filter(|c| !c.is_empty())
}

/// The path part of a code citation (without line and commit).
pub fn code_ref_path(value: &str) -> &str {
    let path = value.split_once('@').map_or(value, |(p, _)| p);
    match path.rsplit_once(':') {
        Some((p, line)) if !line.is_empty() && line.chars().all(|c| c.is_ascii_digit() || c == '-') => p,
        _ => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_refs() {
        for ok in ["src/a.rs", "src/a.rs:12", "src/a.rs:12-30@4f2a91c", "Cargo.toml@v1.2"] {
            validate_code_ref(ok).unwrap();
        }
        for bad in ["", "src/a b.rs", "@abc", "src/a.rs@", "a\nb"] {
            assert!(validate_code_ref(bad).is_err(), "{bad:?}");
        }
        assert_eq!(code_ref_commit("src/a.rs:1@abc"), Some("abc"));
        assert_eq!(code_ref_commit("src/a.rs:1"), None);
        assert_eq!(code_ref_path("src/a.rs:1-9@abc"), "src/a.rs");
        assert_eq!(code_ref_path("src/a.rs"), "src/a.rs");
    }

    #[test]
    fn status_parses_case_insensitively() {
        assert_eq!("Established".parse::<Status>().unwrap(), Status::Established);
        assert!("final".parse::<Status>().is_err());
    }
}
