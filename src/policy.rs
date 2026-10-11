//! Deterministic target naming and machine ownership labels.
//!
//! The target id doubles as the smolvm machine name. Deriving it from the
//! ownership tuple makes `createTarget` retries converge on the same machine
//! and lets the data route reject a path whose ids do not match.
//!
//! The same tuple is written to the machine as smolvm labels, so ownership
//! survives the loss of the provider's store and reconciliation can rebuild a
//! record from the machine alone.

use std::collections::BTreeMap;

use sha2::{Digest as _, Sha256};

pub const TARGET_PREFIX: &str = "ls-";

pub const LABEL_MANAGED: &str = "lightspeed.managed";
pub const LABEL_UNIVERSE: &str = "lightspeed.universe";
pub const LABEL_BINDING: &str = "lightspeed.binding";
pub const LABEL_ENVIRONMENT: &str = "lightspeed.environment";
pub const LABEL_INCARNATION: &str = "lightspeed.incarnation";
pub const LABEL_REQUEST: &str = "lightspeed.request";
pub const LABEL_TEMPLATE: &str = "lightspeed.template";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ownership {
    pub universe_id: String,
    pub binding_id: String,
    pub environment_id: String,
    pub incarnation_id: String,
    pub request_id: String,
    pub template_id: String,
}

impl Ownership {
    pub fn target_name(&self) -> String {
        target_name(
            &self.universe_id,
            &self.binding_id,
            &self.environment_id,
            &self.incarnation_id,
        )
    }

    pub fn labels(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (LABEL_MANAGED.to_owned(), "true".to_owned()),
            (LABEL_UNIVERSE.to_owned(), self.universe_id.clone()),
            (LABEL_BINDING.to_owned(), self.binding_id.clone()),
            (LABEL_ENVIRONMENT.to_owned(), self.environment_id.clone()),
            (LABEL_INCARNATION.to_owned(), self.incarnation_id.clone()),
            (LABEL_REQUEST.to_owned(), self.request_id.clone()),
            (LABEL_TEMPLATE.to_owned(), self.template_id.clone()),
        ])
    }

    /// Ownership recorded on a machine, when complete and consistent with
    /// the machine's name.
    pub fn from_labels(name: &str, labels: &BTreeMap<String, String>) -> Option<Self> {
        if labels.get(LABEL_MANAGED).map(String::as_str) != Some("true") {
            return None;
        }
        let get = |key: &str| labels.get(key).cloned();
        let ownership = Self {
            universe_id: get(LABEL_UNIVERSE)?,
            binding_id: get(LABEL_BINDING)?,
            environment_id: get(LABEL_ENVIRONMENT)?,
            incarnation_id: get(LABEL_INCARNATION)?,
            request_id: get(LABEL_REQUEST)?,
            template_id: get(LABEL_TEMPLATE)?,
        };
        (ownership.target_name() == name).then_some(ownership)
    }
}
const DIGEST_HEX_LEN: usize = 20;

pub fn target_name(
    universe_id: &str,
    binding_id: &str,
    environment_id: &str,
    incarnation_id: &str,
) -> String {
    let mut hash = Sha256::new();
    hash.update(b"instance");
    for part in [universe_id, binding_id, environment_id, incarnation_id] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    let digest = hash.finalize();
    let mut name = String::from(TARGET_PREFIX);
    for byte in &digest[..DIGEST_HEX_LEN / 2] {
        name.push_str(&format!("{byte:02x}"));
    }
    name
}

/// Whether `value` has the exact shape `target_name` produces. Store paths and
/// smolvm machine names are built from it, so nothing else is accepted.
pub fn is_target_name(value: &str) -> bool {
    value.strip_prefix(TARGET_PREFIX).is_some_and(|rest| {
        rest.len() == DIGEST_HEX_LEN
            && rest
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_stable_scoped_and_well_formed() {
        let name = target_name("u", "b", "e", "i");
        assert_eq!(name, target_name("u", "b", "e", "i"));
        assert_ne!(name, target_name("u2", "b", "e", "i"));
        assert_ne!(name, target_name("u", "b", "e", "i2"));
        assert!(is_target_name(&name));
    }

    #[test]
    fn length_prefixing_separates_component_boundaries() {
        assert_ne!(
            target_name("ab", "c", "e", "i"),
            target_name("a", "bc", "e", "i")
        );
    }

    #[test]
    fn ownership_round_trips_through_labels_and_must_match_the_name() {
        let ownership = Ownership {
            universe_id: "u".to_owned(),
            binding_id: "b".to_owned(),
            environment_id: "e".to_owned(),
            incarnation_id: "i".to_owned(),
            request_id: "r".to_owned(),
            template_id: "t".to_owned(),
        };
        let name = ownership.target_name();
        let labels = ownership.labels();
        assert_eq!(Ownership::from_labels(&name, &labels), Some(ownership));
        assert_eq!(
            Ownership::from_labels("ls-00000000000000000000", &labels),
            None
        );
        let mut unmanaged = labels.clone();
        unmanaged.remove(LABEL_MANAGED);
        assert_eq!(Ownership::from_labels(&name, &unmanaged), None);
    }

    #[test]
    fn foreign_names_are_rejected() {
        assert!(!is_target_name("ls-../../etc/passwd"));
        assert!(!is_target_name("ls-ABCDEF0123456789abcd"));
        assert!(!is_target_name("dev"));
    }
}
