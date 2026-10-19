//! Pure discovery decisions, evaluated with current operator policy and evidence.
//!
//! Callers must validate canonical identity/attribution and supply current
//! instance, viewer and remote-access restrictions. Parsing an actor or receiving
//! a directory hint alone never establishes `Verification::Canonical`.
//! This module performs no I/O and does not change personal search or federation.

use std::collections::BTreeSet;

use plamenu_ap::discovery::{DiscoverySignals, Signal};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingSignalPolicy {
    #[default]
    Allow,
    Deny,
}

/// Independently switchable acquisition methods and uses. Peer sharing remains
/// unsupported even when not disabled, until a scoped exchange contract exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mechanism {
    SourceInspection,
    PeerExploration,
    Directory,
    CommunityDirectory,
    VideoCatalogue,
    WiderSearch,
    Outbox,
    MetadataCache,
    ProfileListing,
    PublicPostIndexing,
    VideoIndexing,
    Recommendations,
    PeerSharing,
}

/// A pure policy value; [`super::store`] maps it to persisted settings.
/// Missing-signal defaults are permissive; the rollout master switch starts off.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub enabled: bool,
    pub missing_discoverable: MissingSignalPolicy,
    pub missing_indexable: MissingSignalPolicy,
    pub disabled_mechanisms: BTreeSet<Mechanism>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchMethod {
    SourceInspection,
    PeerExploration,
    Directory,
    CommunityDirectory,
    VideoCatalogue,
    WiderSearch,
    Outbox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Use {
    /// Retain displayable catalogue payload. Minimal policy evidence and
    /// withdrawal guards are separate bookkeeping, not public cached metadata.
    CacheMetadata,
    ListProfile,
    IndexPost,
    IndexVideo,
    Recommend,
    Share,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Source,
    Profile,
    Post,
    Video,
}

/// Only `Public` is eligible. The adapter must distinguish public `to` from
/// unlisted public `cc`, and apply any narrower catalogue/group audience.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Unlisted,
    FollowersOnly,
    Direct,
    LocalOnly,
    MembersOnly,
    Unknown,
}

/// Current restrictions, checked again when retaining or serving a candidate.
/// Remote denial is not equivalent to a missing discovery flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Allowed,
    InstanceBlocked,
    ViewerHidden,
    RemoteDenied,
    Withdrawn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verification {
    Unverified,
    ThirdParty,
    /// Identity, attribution, audience and actor signals have all been checked
    /// at their authority. These times bound the entire evidence set: use its
    /// earliest expiry, not the time a third party repeated an old assertion.
    Canonical {
        checked_at: OffsetDateTime,
        valid_until: OffsetDateTime,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct Candidate {
    pub kind: Kind,
    pub visibility: Visibility,
    pub verification: Verification,
    pub signals: DiscoverySignals,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Allow,
    Deny,
    Defer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalKind {
    Discoverable,
    Indexable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    FetchPermitted,
    VerifiedSource,
    OriginAllows,
    InstanceDefault,
    DiscoveryDisabled,
    MechanismDisabled(Mechanism),
    Restricted(Access),
    Unverified,
    ThirdPartyHint,
    InvalidEvidenceTimes,
    ExpiredEvidence,
    UnknownVisibility,
    NonPublic,
    UnsupportedUse,
    SharingNotImplemented,
    ExplicitRefusal(SignalKind),
    InvalidSignal(SignalKind),
    MissingSignalDenied(SignalKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Unverified,
    Fresh,
    Expired,
    Invalid,
}

/// An explanation plus evidence freshness. This is not a durable authorization:
/// policy/access changes require reevaluation even before the evidence expires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub outcome: Outcome,
    pub reason: Reason,
    pub freshness: Freshness,
    pub evidence_expires_at: Option<OffsetDateTime>,
}

impl Policy {
    /// Permits a bounded verification request only. Transport still enforces
    /// SSRF, redirects and budgets. Outbox/catalogue traversal must additionally
    /// check eligibility for its intended use; this never grants caching,
    /// indexing, recommendation or sharing rights.
    #[must_use]
    pub fn admit_fetch(&self, method: FetchMethod, access: Access) -> Decision {
        let mechanism = match method {
            FetchMethod::SourceInspection => Mechanism::SourceInspection,
            FetchMethod::PeerExploration => Mechanism::PeerExploration,
            FetchMethod::Directory => Mechanism::Directory,
            FetchMethod::CommunityDirectory => Mechanism::CommunityDirectory,
            FetchMethod::VideoCatalogue => Mechanism::VideoCatalogue,
            FetchMethod::WiderSearch => Mechanism::WiderSearch,
            FetchMethod::Outbox => Mechanism::Outbox,
        };
        let reason = self.restriction(mechanism, access);
        Decision {
            outcome: if reason.is_some() {
                Outcome::Deny
            } else {
                Outcome::Allow
            },
            reason: reason.unwrap_or(Reason::FetchPermitted),
            freshness: Freshness::Unverified,
            evidence_expires_at: None,
        }
    }

    #[must_use]
    pub fn evaluate(
        &self,
        usage: Use,
        candidate: &Candidate,
        access: Access,
        now: OffsetDateTime,
    ) -> Decision {
        let (freshness, expiry, verification_error) = verify(candidate.verification, now);
        let decision = |outcome, reason| Decision {
            outcome,
            reason,
            freshness,
            evidence_expires_at: expiry,
        };
        let mechanism = match usage {
            Use::CacheMetadata => Mechanism::MetadataCache,
            Use::ListProfile => Mechanism::ProfileListing,
            Use::IndexPost => Mechanism::PublicPostIndexing,
            Use::IndexVideo => Mechanism::VideoIndexing,
            Use::Recommend => Mechanism::Recommendations,
            Use::Share => Mechanism::PeerSharing,
        };
        if let Some(reason) = self.restriction(mechanism, access) {
            return decision(Outcome::Deny, reason);
        }
        if usage == Use::Share {
            return decision(Outcome::Deny, Reason::SharingNotImplemented);
        }
        let Some(required) = required_signals(usage, candidate.kind) else {
            return decision(Outcome::Deny, Reason::UnsupportedUse);
        };
        if let Some(reason) = verification_error {
            return decision(Outcome::Defer, reason);
        }
        match candidate.visibility {
            Visibility::Public => (),
            Visibility::Unknown => return decision(Outcome::Defer, Reason::UnknownVisibility),
            _ => return decision(Outcome::Deny, Reason::NonPublic),
        }
        // Explicit refusal wins even if another required declaration is missing
        // or malformed. Neither an operator fallback nor a peer can override it.
        for &kind in required {
            if signal(candidate.signals, kind) == Signal::Deny {
                return decision(Outcome::Deny, Reason::ExplicitRefusal(kind));
            }
        }
        for &kind in required {
            if signal(candidate.signals, kind) == Signal::Invalid {
                return decision(Outcome::Defer, Reason::InvalidSignal(kind));
            }
        }
        let mut reason = if required.is_empty() {
            Reason::VerifiedSource
        } else {
            Reason::OriginAllows
        };
        for &kind in required {
            if signal(candidate.signals, kind) == Signal::Missing {
                let fallback = match kind {
                    SignalKind::Discoverable => self.missing_discoverable,
                    SignalKind::Indexable => self.missing_indexable,
                };
                if fallback == MissingSignalPolicy::Deny {
                    return decision(Outcome::Deny, Reason::MissingSignalDenied(kind));
                }
                reason = Reason::InstanceDefault;
            }
        }
        decision(Outcome::Allow, reason)
    }

    fn restriction(&self, mechanism: Mechanism, access: Access) -> Option<Reason> {
        if !self.enabled {
            Some(Reason::DiscoveryDisabled)
        } else if self.disabled_mechanisms.contains(&mechanism) {
            Some(Reason::MechanismDisabled(mechanism))
        } else if access != Access::Allowed {
            Some(Reason::Restricted(access))
        } else {
            None
        }
    }
}

fn required_signals(usage: Use, kind: Kind) -> Option<&'static [SignalKind]> {
    use SignalKind::{Discoverable, Indexable};
    match (usage, kind) {
        (Use::CacheMetadata, Kind::Source) => Some(&[]),
        (Use::CacheMetadata | Use::ListProfile | Use::Recommend, Kind::Profile) => {
            Some(&[Discoverable])
        }
        (Use::CacheMetadata | Use::IndexPost, Kind::Post)
        | (Use::CacheMetadata | Use::IndexVideo, Kind::Video) => Some(&[Indexable]),
        // Promoting a post also promotes its author; respect both declarations.
        (Use::Recommend, Kind::Post | Kind::Video) => Some(&[Discoverable, Indexable]),
        _ => None,
    }
}

fn signal(signals: DiscoverySignals, kind: SignalKind) -> Signal {
    match kind {
        SignalKind::Discoverable => signals.discoverable,
        SignalKind::Indexable => signals.indexable,
    }
}

fn verify(
    verification: Verification,
    now: OffsetDateTime,
) -> (Freshness, Option<OffsetDateTime>, Option<Reason>) {
    match verification {
        Verification::Unverified => (Freshness::Unverified, None, Some(Reason::Unverified)),
        Verification::ThirdParty => (Freshness::Unverified, None, Some(Reason::ThirdPartyHint)),
        Verification::Canonical {
            checked_at,
            valid_until,
        } => {
            let (freshness, reason) = if checked_at > now || valid_until <= checked_at {
                (Freshness::Invalid, Some(Reason::InvalidEvidenceTimes))
            } else if valid_until <= now {
                (Freshness::Expired, Some(Reason::ExpiredEvidence))
            } else {
                (Freshness::Fresh, None)
            };
            (freshness, Some(valid_until), reason)
        }
    }
}

#[cfg(test)]
mod tests;
