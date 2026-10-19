//! Discovery signals from an actor document, without applying local policy.
//!
//! Read the original document before `RemoteActor` defaults absent flags to
//! false. Parsing does not establish the document's authority or freshness.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// An origin's declaration. Missing and malformed fields are distinct: only a
/// missing field may use an operator's fallback policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    #[default]
    Missing,
    Allow,
    Deny,
    Invalid,
}

/// Independent profile-discovery and public-post indexing declarations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoverySignals {
    pub discoverable: Signal,
    pub indexable: Signal,
}

impl DiscoverySignals {
    /// Reads compact field names, the conventional `toot:` spellings, and full
    /// Mastodon vocabulary IRIs. Conflicting spellings are invalid. This does
    /// not resolve arbitrary JSON-LD contexts or fetch external documents.
    #[must_use]
    pub fn from_document(document: &Value) -> Self {
        Self {
            discoverable: read_signal(
                document,
                &[
                    "discoverable",
                    "toot:discoverable",
                    "http://joinmastodon.org/ns#discoverable",
                ],
            ),
            indexable: read_signal(
                document,
                &[
                    "indexable",
                    "toot:indexable",
                    "http://joinmastodon.org/ns#indexable",
                ],
            ),
        }
    }
}

fn read_signal(document: &Value, names: &[&str]) -> Signal {
    let Some(fields) = document.as_object() else {
        return Signal::Invalid;
    };
    let mut found = Signal::Missing;
    for value in names.iter().filter_map(|name| fields.get(*name)) {
        let signal = match value {
            Value::Bool(true) => Signal::Allow,
            Value::Bool(false) => Signal::Deny,
            _ => return Signal::Invalid,
        };
        if found != Signal::Missing && found != signal {
            return Signal::Invalid;
        }
        found = signal;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::RemoteActor;
    use serde_json::json;

    #[test]
    fn missing_false_and_true_remain_independent() {
        for (discoverable, expected_discoverable) in [
            (None, Signal::Missing),
            (Some(false), Signal::Deny),
            (Some(true), Signal::Allow),
        ] {
            for (indexable, expected_indexable) in [
                (None, Signal::Missing),
                (Some(false), Signal::Deny),
                (Some(true), Signal::Allow),
            ] {
                let mut document = json!({});
                if let Some(value) = discoverable {
                    document["discoverable"] = json!(value);
                }
                if let Some(value) = indexable {
                    document["indexable"] = json!(value);
                }
                assert_eq!(
                    DiscoverySignals::from_document(&document),
                    DiscoverySignals {
                        discoverable: expected_discoverable,
                        indexable: expected_indexable,
                    },
                    "{document}"
                );
            }
        }
    }

    #[test]
    fn malformed_fields_do_not_become_missing() {
        for invalid in [json!(null), json!("false"), json!(0), json!([]), json!({})] {
            let signals = DiscoverySignals::from_document(&json!({
                "discoverable": invalid,
                "indexable": true,
            }));
            assert_eq!(signals.discoverable, Signal::Invalid);
            assert_eq!(signals.indexable, Signal::Allow);
            let signals = DiscoverySignals::from_document(&json!({"indexable": invalid}));
            assert_eq!(signals.discoverable, Signal::Missing);
            assert_eq!(signals.indexable, Signal::Invalid);
        }
        for invalid in [json!(null), json!([]), json!(true), json!("actor")] {
            assert_eq!(
                DiscoverySignals::from_document(&invalid),
                DiscoverySignals {
                    discoverable: Signal::Invalid,
                    indexable: Signal::Invalid,
                }
            );
        }
    }

    #[test]
    fn qualified_refusals_and_conflicts_are_preserved() {
        for prefix in ["", "toot:", "http://joinmastodon.org/ns#"] {
            let signals = DiscoverySignals::from_document(&json!({
                format!("{prefix}discoverable"): false,
                format!("{prefix}indexable"): false,
            }));
            assert_eq!(signals.discoverable, Signal::Deny);
            assert_eq!(signals.indexable, Signal::Deny);
        }
        for field in ["discoverable", "indexable"] {
            let document = json!({field: true, format!("toot:{field}"): false});
            let signals = DiscoverySignals::from_document(&document);
            assert_eq!(
                if field == "discoverable" {
                    signals.discoverable
                } else {
                    signals.indexable
                },
                Signal::Invalid
            );
        }
        assert_eq!(
            DiscoverySignals::from_document(&json!({
                "discoverable": false, "toot:discoverable": false,
            }))
            .discoverable,
            Signal::Deny
        );
    }

    #[test]
    fn nested_claims_are_not_actor_declarations() {
        assert_eq!(
            DiscoverySignals::from_document(&json!({
                "directory": {"discoverable": true, "indexable": true},
            })),
            DiscoverySignals::default()
        );
    }

    #[test]
    fn legacy_actor_defaults_are_unchanged() {
        let mut document = json!({
            "id": "https://small.example/actor",
            "type": "Person",
            "inbox": "https://small.example/inbox",
        });
        let actor: RemoteActor = serde_json::from_value(document.clone()).unwrap();
        assert!(!actor.discoverable);
        assert!(!actor.indexable);
        assert_eq!(
            DiscoverySignals::from_document(&document),
            DiscoverySignals::default()
        );

        document["discoverable"] = json!(false);
        document["indexable"] = json!(true);
        let actor: RemoteActor = serde_json::from_value(document.clone()).unwrap();
        assert!(!actor.discoverable);
        assert!(actor.indexable);
        assert_eq!(
            DiscoverySignals::from_document(&document).discoverable,
            Signal::Deny
        );
    }
}
