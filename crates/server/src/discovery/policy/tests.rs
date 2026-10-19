use super::*;
use serde_json::{Value, json};
use time::Duration;

fn now() -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + Duration::days(1)
}

fn enabled() -> Policy {
    Policy {
        enabled: true,
        ..Policy::default()
    }
}

fn candidate(kind: Kind, document: &Value) -> Candidate {
    Candidate {
        kind,
        visibility: Visibility::Public,
        verification: Verification::Canonical {
            checked_at: now() - Duration::hours(1),
            valid_until: now() + Duration::hours(1),
        },
        signals: DiscoverySignals::from_document(document),
    }
}

fn result(policy: &Policy, usage: Use, candidate: &Candidate) -> Decision {
    policy.evaluate(usage, candidate, Access::Allowed, now())
}

#[test]
fn profile_listing_and_post_indexing_use_independent_signals() {
    let cases = [
        (Signal::Missing, Outcome::Allow),
        (Signal::Allow, Outcome::Allow),
        (Signal::Deny, Outcome::Deny),
        (Signal::Invalid, Outcome::Defer),
    ];
    for (discoverable, profile_outcome) in cases {
        for (indexable, post_outcome) in cases {
            let mut item = candidate(Kind::Profile, &json!({}));
            item.signals = DiscoverySignals {
                discoverable,
                indexable,
            };
            assert_eq!(
                result(&enabled(), Use::ListProfile, &item).outcome,
                profile_outcome
            );
            assert_eq!(
                result(&enabled(), Use::CacheMetadata, &item).outcome,
                profile_outcome
            );
            item.kind = Kind::Post;
            assert_eq!(
                result(&enabled(), Use::IndexPost, &item).outcome,
                post_outcome
            );
            assert_eq!(
                result(&enabled(), Use::CacheMetadata, &item).outcome,
                post_outcome
            );
        }
    }
}

#[test]
fn absent_fields_default_to_allow_without_manufacturing_consent() {
    for (kind, usage) in [
        (Kind::Profile, Use::ListProfile),
        (Kind::Post, Use::IndexPost),
    ] {
        let item = candidate(kind, &json!({}));
        let decision = result(&enabled(), usage, &item);
        assert_eq!(decision.outcome, Outcome::Allow);
        assert_eq!(decision.reason, Reason::InstanceDefault);
        assert_eq!(decision.freshness, Freshness::Fresh);
        assert_eq!(
            decision.evidence_expires_at,
            Some(now() + Duration::hours(1))
        );
        assert_eq!(item.signals, DiscoverySignals::default());
        let explicit = candidate(kind, &json!({"discoverable": true, "indexable": true}));
        assert_eq!(
            result(&enabled(), usage, &explicit).reason,
            Reason::OriginAllows
        );
    }
}

#[test]
fn missing_field_defaults_are_independently_configurable() {
    let profile = candidate(Kind::Profile, &json!({}));
    let post = candidate(Kind::Post, &json!({}));
    for (discoverable, indexable, profile_outcome, post_outcome) in [
        (
            MissingSignalPolicy::Allow,
            MissingSignalPolicy::Allow,
            Outcome::Allow,
            Outcome::Allow,
        ),
        (
            MissingSignalPolicy::Deny,
            MissingSignalPolicy::Allow,
            Outcome::Deny,
            Outcome::Allow,
        ),
        (
            MissingSignalPolicy::Allow,
            MissingSignalPolicy::Deny,
            Outcome::Allow,
            Outcome::Deny,
        ),
        (
            MissingSignalPolicy::Deny,
            MissingSignalPolicy::Deny,
            Outcome::Deny,
            Outcome::Deny,
        ),
    ] {
        let policy = Policy {
            missing_discoverable: discoverable,
            missing_indexable: indexable,
            ..enabled()
        };
        assert_eq!(
            result(&policy, Use::ListProfile, &profile).outcome,
            profile_outcome
        );
        assert_eq!(result(&policy, Use::IndexPost, &post).outcome, post_outcome);
        for (kind, usage) in [
            (Kind::Profile, Use::ListProfile),
            (Kind::Post, Use::IndexPost),
        ] {
            let item = candidate(kind, &json!({"discoverable": true, "indexable": true}));
            assert_eq!(result(&policy, usage, &item).outcome, Outcome::Allow);
        }
    }
}

#[test]
fn recommendations_respect_profile_refusal_and_post_indexing_refusal() {
    for (document, reason) in [
        (
            json!({"discoverable": false, "indexable": true}),
            Reason::ExplicitRefusal(SignalKind::Discoverable),
        ),
        (
            json!({"discoverable": true, "indexable": false}),
            Reason::ExplicitRefusal(SignalKind::Indexable),
        ),
        // Refusal takes precedence over a malformed or absent other signal.
        (
            json!({"discoverable": null, "indexable": false}),
            Reason::ExplicitRefusal(SignalKind::Indexable),
        ),
        (
            json!({"indexable": false}),
            Reason::ExplicitRefusal(SignalKind::Indexable),
        ),
    ] {
        let decision = result(
            &enabled(),
            Use::Recommend,
            &candidate(Kind::Post, &document),
        );
        assert_eq!(decision.outcome, Outcome::Deny);
        assert_eq!(decision.reason, reason);
    }
    let profile = candidate(
        Kind::Profile,
        &json!({"discoverable": true, "indexable": false}),
    );
    assert_eq!(
        result(&enabled(), Use::Recommend, &profile).outcome,
        Outcome::Allow
    );
    let post = candidate(Kind::Post, &json!({}));
    assert_eq!(
        result(&enabled(), Use::Recommend, &post).reason,
        Reason::InstanceDefault
    );
    let policy = Policy {
        missing_indexable: MissingSignalPolicy::Deny,
        ..enabled()
    };
    assert_eq!(
        result(&policy, Use::Recommend, &post).reason,
        Reason::MissingSignalDenied(SignalKind::Indexable)
    );
}

#[test]
fn malformed_signals_never_use_the_permissive_fallback() {
    for (kind, usage, field, signal) in [
        (
            Kind::Profile,
            Use::ListProfile,
            "discoverable",
            SignalKind::Discoverable,
        ),
        (
            Kind::Post,
            Use::IndexPost,
            "indexable",
            SignalKind::Indexable,
        ),
    ] {
        for invalid in [json!(null), json!("true"), json!(1), json!([])] {
            let item = candidate(kind, &json!({field: invalid}));
            let decision = result(&enabled(), usage, &item);
            assert_eq!(decision.outcome, Outcome::Defer);
            assert_eq!(decision.reason, Reason::InvalidSignal(signal));
        }
    }
}

#[test]
fn a_hint_or_failed_verification_cannot_grant_any_use() {
    for (verification, reason) in [
        (Verification::Unverified, Reason::Unverified),
        (Verification::ThirdParty, Reason::ThirdPartyHint),
    ] {
        for document in [json!({}), json!({"discoverable": true, "indexable": true})] {
            for (kind, usage) in [
                (Kind::Source, Use::CacheMetadata),
                (Kind::Profile, Use::CacheMetadata),
                (Kind::Profile, Use::ListProfile),
                (Kind::Profile, Use::Recommend),
                (Kind::Post, Use::CacheMetadata),
                (Kind::Post, Use::IndexPost),
                (Kind::Post, Use::Recommend),
            ] {
                let mut item = candidate(kind, &document);
                item.verification = verification;
                let decision = result(&enabled(), usage, &item);
                assert_eq!(decision.outcome, Outcome::Defer);
                assert_eq!(decision.reason, reason);
                assert_eq!(decision.freshness, Freshness::Unverified);
                assert_eq!(decision.evidence_expires_at, None);
            }
        }
    }
}

#[test]
fn expired_or_invalid_evidence_cannot_be_served() {
    for (checked_at, valid_until, freshness, reason) in [
        (
            now() - Duration::hours(2),
            now() - Duration::hours(1),
            Freshness::Expired,
            Reason::ExpiredEvidence,
        ),
        (
            now() - Duration::hours(1),
            now(),
            Freshness::Expired,
            Reason::ExpiredEvidence,
        ),
        (
            now() + Duration::seconds(1),
            now() + Duration::hours(1),
            Freshness::Invalid,
            Reason::InvalidEvidenceTimes,
        ),
        (
            now(),
            now(),
            Freshness::Invalid,
            Reason::InvalidEvidenceTimes,
        ),
        (
            now(),
            now() - Duration::hours(1),
            Freshness::Invalid,
            Reason::InvalidEvidenceTimes,
        ),
    ] {
        let mut item = candidate(Kind::Profile, &json!({"discoverable": true}));
        item.verification = Verification::Canonical {
            checked_at,
            valid_until,
        };
        for usage in [Use::ListProfile, Use::Recommend, Use::CacheMetadata] {
            let decision = result(&enabled(), usage, &item);
            assert_eq!(decision.outcome, Outcome::Defer);
            assert_eq!(decision.reason, reason);
            assert_eq!(decision.freshness, freshness);
            assert_eq!(decision.evidence_expires_at, Some(valid_until));
        }
    }
}

#[test]
fn restricted_and_unknown_audiences_are_never_broadly_discovered() {
    for visibility in [
        Visibility::Unlisted,
        Visibility::FollowersOnly,
        Visibility::Direct,
        Visibility::LocalOnly,
        Visibility::MembersOnly,
        Visibility::Unknown,
    ] {
        for (kind, usage) in [
            (Kind::Source, Use::CacheMetadata),
            (Kind::Profile, Use::ListProfile),
            (Kind::Post, Use::IndexPost),
            (Kind::Post, Use::Recommend),
            (Kind::Post, Use::CacheMetadata),
        ] {
            let mut item = candidate(kind, &json!({"discoverable": true, "indexable": true}));
            item.visibility = visibility;
            let decision = result(&enabled(), usage, &item);
            if visibility == Visibility::Unknown {
                assert_eq!(decision.outcome, Outcome::Defer);
                assert_eq!(decision.reason, Reason::UnknownVisibility);
            } else {
                assert_eq!(decision.outcome, Outcome::Deny);
                assert_eq!(decision.reason, Reason::NonPublic);
            }
        }
    }
}

#[test]
fn current_restrictions_override_fresh_evidence_and_fetching() {
    let item = candidate(Kind::Profile, &json!({"discoverable": true}));
    for access in [
        Access::InstanceBlocked,
        Access::ViewerHidden,
        Access::RemoteDenied,
        Access::Withdrawn,
    ] {
        for usage in [Use::ListProfile, Use::Recommend, Use::CacheMetadata] {
            let decision = enabled().evaluate(usage, &item, access, now());
            assert_eq!(decision.outcome, Outcome::Deny);
            assert_eq!(decision.reason, Reason::Restricted(access));
        }
        assert_eq!(
            enabled()
                .admit_fetch(FetchMethod::Directory, access)
                .outcome,
            Outcome::Deny
        );
    }
}

#[test]
fn master_switch_disables_every_use_and_fetch_method() {
    let item = candidate(Kind::Profile, &json!({"discoverable": true}));
    for usage in [
        Use::CacheMetadata,
        Use::ListProfile,
        Use::IndexPost,
        Use::Recommend,
        Use::Share,
    ] {
        assert_eq!(
            result(&Policy::default(), usage, &item).reason,
            Reason::DiscoveryDisabled
        );
    }
    for method in [
        FetchMethod::SourceInspection,
        FetchMethod::PeerExploration,
        FetchMethod::Directory,
        FetchMethod::Outbox,
    ] {
        let decision = Policy::default().admit_fetch(method, Access::Allowed);
        assert_eq!(decision.outcome, Outcome::Deny);
        assert_eq!(decision.reason, Reason::DiscoveryDisabled);
    }
}

#[test]
fn acquisition_methods_can_be_disabled_independently() {
    let methods = [
        (FetchMethod::SourceInspection, Mechanism::SourceInspection),
        (FetchMethod::PeerExploration, Mechanism::PeerExploration),
        (FetchMethod::Directory, Mechanism::Directory),
        (FetchMethod::Outbox, Mechanism::Outbox),
    ];
    for (disabled_method, mechanism) in methods {
        let mut policy = enabled();
        policy.disabled_mechanisms.insert(mechanism);
        for (method, _) in methods {
            let decision = policy.admit_fetch(method, Access::Allowed);
            if method == disabled_method {
                assert_eq!(decision.outcome, Outcome::Deny);
                assert_eq!(decision.reason, Reason::MechanismDisabled(mechanism));
            } else {
                assert_eq!(decision.outcome, Outcome::Allow);
            }
        }
        let item = candidate(Kind::Profile, &json!({}));
        assert_eq!(
            result(&policy, Use::ListProfile, &item).outcome,
            Outcome::Allow
        );
    }
}

#[test]
fn uses_can_be_disabled_independently() {
    let uses = [
        (Use::CacheMetadata, Kind::Profile, Mechanism::MetadataCache),
        (Use::ListProfile, Kind::Profile, Mechanism::ProfileListing),
        (Use::IndexPost, Kind::Post, Mechanism::PublicPostIndexing),
        (Use::Recommend, Kind::Profile, Mechanism::Recommendations),
    ];
    for (disabled_use, _, mechanism) in uses {
        let mut policy = enabled();
        policy.disabled_mechanisms.insert(mechanism);
        for (usage, kind, _) in uses {
            let decision = result(&policy, usage, &candidate(kind, &json!({})));
            if usage == disabled_use {
                assert_eq!(decision.outcome, Outcome::Deny);
                assert_eq!(decision.reason, Reason::MechanismDisabled(mechanism));
            } else {
                assert_eq!(decision.outcome, Outcome::Allow);
            }
        }
        assert_eq!(
            policy
                .admit_fetch(FetchMethod::SourceInspection, Access::Allowed)
                .outcome,
            Outcome::Allow
        );
    }
}

#[test]
fn revised_policy_and_origin_signals_apply_to_previously_eligible_records() {
    let mut policy = enabled();
    let mut item = candidate(Kind::Profile, &json!({}));
    assert_eq!(
        result(&policy, Use::ListProfile, &item).outcome,
        Outcome::Allow
    );
    policy.missing_discoverable = MissingSignalPolicy::Deny;
    assert_eq!(
        result(&policy, Use::ListProfile, &item).reason,
        Reason::MissingSignalDenied(SignalKind::Discoverable)
    );
    policy.missing_discoverable = MissingSignalPolicy::Allow;
    item.signals.discoverable = Signal::Deny;
    assert_eq!(
        result(&policy, Use::ListProfile, &item).reason,
        Reason::ExplicitRefusal(SignalKind::Discoverable)
    );
    item.signals.discoverable = Signal::Allow;
    policy.disabled_mechanisms.insert(Mechanism::ProfileListing);
    assert_eq!(
        result(&policy, Use::ListProfile, &item).outcome,
        Outcome::Deny
    );
}

#[test]
fn caching_a_source_does_not_grant_profile_or_post_uses() {
    let source = candidate(Kind::Source, &json!({}));
    assert_eq!(
        result(&enabled(), Use::CacheMetadata, &source).reason,
        Reason::VerifiedSource
    );
    for usage in [Use::ListProfile, Use::IndexPost, Use::Recommend] {
        assert_eq!(
            result(&enabled(), usage, &source).reason,
            Reason::UnsupportedUse
        );
    }
    let profile = candidate(Kind::Profile, &json!({"discoverable": true}));
    assert_eq!(
        result(&enabled(), Use::IndexPost, &profile).reason,
        Reason::UnsupportedUse
    );
}

#[test]
fn neither_fetch_admission_nor_consent_grants_peer_sharing() {
    let policy = enabled();
    let fetch = policy.admit_fetch(FetchMethod::SourceInspection, Access::Allowed);
    assert_eq!(fetch.outcome, Outcome::Allow);
    assert_eq!(fetch.freshness, Freshness::Unverified);
    assert_eq!(fetch.evidence_expires_at, None);
    for kind in [Kind::Source, Kind::Profile, Kind::Post] {
        let item = candidate(kind, &json!({"discoverable": true, "indexable": true}));
        let decision = result(&policy, Use::Share, &item);
        assert_eq!(decision.outcome, Outcome::Deny);
        assert_eq!(decision.reason, Reason::SharingNotImplemented);
    }
}

#[test]
fn settings_round_trip_and_reject_unknown_controls() {
    let policy: Policy = serde_json::from_value(json!({
        "enabled": true,
        "missing_discoverable": "deny",
        "disabled_mechanisms": ["outbox", "recommendations"],
    }))
    .unwrap();
    assert_eq!(policy.missing_indexable, MissingSignalPolicy::Allow);
    assert_eq!(policy.missing_discoverable, MissingSignalPolicy::Deny);
    assert!(policy.disabled_mechanisms.contains(&Mechanism::Outbox));
    assert_eq!(
        serde_json::from_value::<Policy>(serde_json::to_value(&policy).unwrap()).unwrap(),
        policy
    );
    assert_eq!(
        serde_json::from_value::<Policy>(json!({})).unwrap(),
        Policy::default()
    );
    assert!(serde_json::from_value::<Policy>(json!({"missing_indexable": "sometimes"})).is_err());
    assert!(serde_json::from_value::<Policy>(json!({"disabled_mechanisms": ["typo"]})).is_err());
    assert!(serde_json::from_value::<Policy>(json!({"missing_indexible": "deny"})).is_err());
}
