//! Webfinger JRD documents (RFC 7033) and host-meta (RFC 6415).

use std::collections::HashMap;
use std::fmt::Write;

use serde::{Deserialize, Deserializer, Serialize};

use crate::ACTIVITY_JSON;
use crate::acct::Acct;
use crate::urls::LocalUserUrls;

/// The `properties` key under which Lemmy (and other FEP-1b12 hosts) advertise
/// an actor's `ActivityStreams` `type` on a `self` link, so a resolver can tell a
/// Person `self` link from a Group `self` link that share one `acct`.
pub const AS_TYPE_PROPERTY: &str = "https://www.w3.org/ns/activitystreams#type";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JrdLink {
    pub rel: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
    /// RFC 6570 URI template (`{uri}`/`{content}`/`{object}` placeholders) —
    /// remote-interaction links carry a template instead of an href.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// RFC 7033 `properties`: a map of URI → string|null. Preserved on ingest so
    /// the AS `type` hint on a `self` link survives (Lemmy publishes it here);
    /// empty on the JRDs Plamenu serves, so it stays off the wire.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub properties: HashMap<String, Option<String>>,
}

impl JrdLink {
    /// The `ActivityStreams` `type` this link advertises via `properties`
    /// (`"Person"`, `"Group"`, …), if any. A hint only — the fetched actor
    /// document is authoritative.
    #[must_use]
    pub fn advertised_type(&self) -> Option<&str> {
        self.properties
            .get(AS_TYPE_PROPERTY)
            .and_then(Option::as_deref)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Jrd {
    pub subject: String,
    #[serde(
        default,
        deserialize_with = "deserialize_aliases",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<JrdLink>,
}

/// RFC 7033 aliases are strings, but Discourse's application actor currently
/// serializes a missing profile URL as `aliases: [null]`. Treat null entries as
/// absent while retaining strict string validation for every real alias.
fn deserialize_aliases<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Vec::<Option<String>>::deserialize(deserializer)?
        .into_iter()
        .flatten()
        .collect())
}

impl Jrd {
    /// The JRD answer for a local actor lookup.
    #[must_use]
    pub fn for_local_actor(acct: &Acct) -> Self {
        Self::for_local_actor_with_id(acct, None)
    }

    /// The JRD answer using the actor's persisted canonical `ActivityPub` ID.
    /// The handle/profile alias remains mutable while `self` stays immutable.
    #[must_use]
    pub fn for_local_actor_with_id(acct: &Acct, actor_id: Option<&str>) -> Self {
        Self::for_local_actor_on_domain(acct, acct.domain(), actor_id)
    }

    /// The JRD answer for an actor whose canonical handle domain differs from
    /// the HTTPS host serving its profile and `ActivityPub` ID. This is the
    /// standard split-domain layout (`user@example.com` hosted at
    /// `social.example.com`): `subject` remains the acct while every URL points
    /// at `actor_domain`.
    #[must_use]
    pub fn for_local_actor_on_domain(
        acct: &Acct,
        actor_domain: &str,
        actor_id: Option<&str>,
    ) -> Self {
        let urls = LocalUserUrls::for_account(actor_domain, acct.username(), actor_id);
        let mut links = vec![
            // The human profile page (`/@name`), so a webfinger lookup
            // surfaces a browsable URL alongside the AP `self` link.
            JrdLink {
                rel: "http://webfinger.net/rel/profile-page".to_owned(),
                media_type: Some("text/html".to_owned()),
                href: Some(urls.web_url.clone()),
                template: None,
                properties: HashMap::new(),
            },
            JrdLink {
                rel: "self".to_owned(),
                media_type: Some(ACTIVITY_JSON.to_owned()),
                href: Some(urls.id.clone()),
                template: None,
                properties: HashMap::new(),
            },
        ];
        links.extend(interaction_links(actor_domain));
        Self {
            subject: format!("acct:{acct}"),
            // Mastodon lists the web URL first, then the AP id.
            aliases: vec![urls.web_url, urls.id],
            links,
        }
    }

    /// Appends the `http://webfinger.net/rel/avatar` link Mastodon lists
    /// last on account JRDs, so remote pickers can show the avatar without
    /// dereferencing the actor.
    #[must_use]
    pub fn with_avatar(mut self, media_type: &str, href: String) -> Self {
        self.links.push(JrdLink {
            rel: "http://webfinger.net/rel/avatar".to_owned(),
            media_type: Some(media_type.to_owned()),
            href: Some(href),
            template: None,
            properties: HashMap::new(),
        });
        self
    }

    /// The JRD answer for the instance actor, the way Mastodon answers
    /// `acct:{domain}@{domain}` for its representative.
    #[must_use]
    pub fn for_instance_actor(domain: &str) -> Self {
        Self::for_instance_actor_on_domain(domain, domain)
    }

    /// Instance-actor JRD for a split-domain deployment. The representative's
    /// canonical acct uses `account_domain`; its immutable actor URL remains on
    /// `actor_domain` with the rest of the service.
    #[must_use]
    pub fn for_instance_actor_on_domain(account_domain: &str, actor_domain: &str) -> Self {
        let urls = crate::urls::InstanceActorUrls::new(actor_domain);
        let mut links = vec![JrdLink {
            rel: "self".to_owned(),
            media_type: Some(ACTIVITY_JSON.to_owned()),
            href: Some(urls.id.clone()),
            template: None,
            properties: HashMap::new(),
        }];
        links.extend(interaction_links(actor_domain));
        Self {
            subject: format!("acct:{account_domain}@{account_domain}"),
            aliases: vec![urls.id],
            links,
        }
    }
}

/// The remote-interaction template links Mastodon serves on every account
/// JRD: the `OStatus` subscribe rel (remote-follow buttons on Mastodon,
/// Pleroma and friends resolve it) and the FEP-3b86 Create/Object intents.
/// Plamenu's targets: `/interact?uri=` resolves for a signed-in viewer and
/// offers the handle form to anyone else, `/search?q=` dereferences a
/// URI for a signed-in viewer, `/compose?text=` prefills the composer.
fn interaction_links(domain: &str) -> [JrdLink; 3] {
    [
        JrdLink {
            rel: "http://ostatus.org/schema/1.0/subscribe".to_owned(),
            media_type: None,
            href: None,
            template: Some(format!("https://{domain}/interact?uri={{uri}}")),
            properties: HashMap::new(),
        },
        JrdLink {
            rel: "https://w3id.org/fep/3b86/Create".to_owned(),
            media_type: None,
            href: None,
            template: Some(format!("https://{domain}/compose?text={{content}}")),
            properties: HashMap::new(),
        },
        JrdLink {
            rel: "https://w3id.org/fep/3b86/Object".to_owned(),
            media_type: None,
            href: None,
            template: Some(format!("https://{domain}/search?q={{object}}")),
            properties: HashMap::new(),
        },
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostMetaLink {
    pub rel: String,
    pub template: String,
}

/// `/.well-known/host-meta` document pointing remotes at our webfinger
/// endpoint. Serializes to the JSON form; [`HostMeta::to_xml`] renders the
/// XRD form Mastodon serves by default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostMeta {
    pub links: Vec<HostMetaLink>,
}

impl HostMeta {
    #[must_use]
    pub fn for_domain(domain: &str) -> Self {
        Self {
            links: vec![HostMetaLink {
                rel: "lrdd".to_owned(),
                template: format!("https://{domain}/.well-known/webfinger?resource={{uri}}"),
            }],
        }
    }

    /// The XRD XML form, byte-identical to Mastodon's rendering. Values are
    /// our own rel literals and config-derived URLs, so no XML escaping.
    #[must_use]
    pub fn to_xml(&self) -> String {
        let mut xml = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <XRD xmlns=\"http://docs.oasis-open.org/ns/xri/xrd-1.0\">\n",
        );
        for link in &self.links {
            let _ = writeln!(
                xml,
                "  <Link rel=\"{}\" template=\"{}\"/>",
                link.rel, link.template
            );
        }
        xml.push_str("</XRD>");
        xml
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_actor_jrd_shape() {
        let acct: Acct = "alice@plamenu.local".parse().unwrap();
        let value = serde_json::to_value(Jrd::for_local_actor(&acct)).unwrap();
        assert_eq!(value["subject"], "acct:alice@plamenu.local");
        assert_eq!(value["aliases"][0], "https://plamenu.local/@alice");
        assert_eq!(value["aliases"][1], "https://plamenu.local/users/alice");
        assert_eq!(
            value["links"][0]["rel"],
            "http://webfinger.net/rel/profile-page"
        );
        assert_eq!(value["links"][0]["type"], "text/html");
        assert_eq!(value["links"][0]["href"], "https://plamenu.local/@alice");
        assert_eq!(value["links"][1]["rel"], "self");
        assert_eq!(value["links"][1]["type"], ACTIVITY_JSON);
        assert_eq!(
            value["links"][1]["href"],
            "https://plamenu.local/users/alice"
        );
        assert_eq!(
            value["links"][2],
            serde_json::json!({
                "rel": "http://ostatus.org/schema/1.0/subscribe",
                "template": "https://plamenu.local/interact?uri={uri}",
            })
        );
        assert_eq!(
            value["links"][3],
            serde_json::json!({
                "rel": "https://w3id.org/fep/3b86/Create",
                "template": "https://plamenu.local/compose?text={content}",
            })
        );
        assert_eq!(
            value["links"][4],
            serde_json::json!({
                "rel": "https://w3id.org/fep/3b86/Object",
                "template": "https://plamenu.local/search?q={object}",
            })
        );
    }

    #[test]
    fn split_domain_jrd_keeps_acct_and_hosts_every_url_on_actor_domain() {
        let acct: Acct = "alice@example.com".parse().unwrap();
        let value = serde_json::to_value(Jrd::for_local_actor_on_domain(
            &acct,
            "social.example.com",
            Some("https://social.example.com/ap/accounts/42"),
        ))
        .unwrap();
        assert_eq!(value["subject"], "acct:alice@example.com");
        assert_eq!(value["aliases"][0], "https://social.example.com/@alice");
        assert_eq!(
            value["aliases"][1],
            "https://social.example.com/ap/accounts/42"
        );
        assert_eq!(
            value["links"][1]["href"],
            "https://social.example.com/ap/accounts/42"
        );
        for link in value["links"].as_array().unwrap() {
            for key in ["href", "template"] {
                if let Some(url) = link[key].as_str() {
                    assert!(url.starts_with("https://social.example.com/"), "{url}");
                }
            }
        }
    }

    #[test]
    fn discourse_null_webfinger_alias_is_ignored() {
        let jrd: Jrd = serde_json::from_value(serde_json::json!({
            "subject": "acct:discourse.internal@discourse.test",
            "aliases": [null],
            "links": [{
                "rel": "self",
                "type": ACTIVITY_JSON,
                "href": "https://discourse.test/ap/actor/application",
            }],
        }))
        .unwrap();

        assert!(jrd.aliases.is_empty());
        assert_eq!(
            jrd.links[0].href.as_deref(),
            Some("https://discourse.test/ap/actor/application")
        );
    }

    #[test]
    fn local_actor_jrd_avatar_link_is_last() {
        let acct: Acct = "alice@plamenu.local".parse().unwrap();
        let jrd = Jrd::for_local_actor(&acct).with_avatar(
            "image/png",
            "https://plamenu.local/media/avatar.png".to_owned(),
        );
        let value = serde_json::to_value(jrd).unwrap();
        assert_eq!(
            value["links"][5],
            serde_json::json!({
                "rel": "http://webfinger.net/rel/avatar",
                "type": "image/png",
                "href": "https://plamenu.local/media/avatar.png",
            })
        );
    }

    #[test]
    fn instance_actor_jrd_shape() {
        let value = serde_json::to_value(Jrd::for_instance_actor("plamenu.local")).unwrap();
        assert_eq!(value["subject"], "acct:plamenu.local@plamenu.local");
        assert_eq!(value["aliases"][0], "https://plamenu.local/actor");
        assert_eq!(value["links"][0]["rel"], "self");
        assert_eq!(value["links"][0]["href"], "https://plamenu.local/actor");
        assert_eq!(
            value["links"][1]["rel"],
            "http://ostatus.org/schema/1.0/subscribe"
        );
        assert_eq!(
            value["links"][1]["template"],
            "https://plamenu.local/interact?uri={uri}"
        );
        assert_eq!(value["links"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn host_meta_json_shape() {
        let value = serde_json::to_value(HostMeta::for_domain("plamenu.local")).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "links": [{
                    "rel": "lrdd",
                    "template": "https://plamenu.local/.well-known/webfinger?resource={uri}",
                }],
            })
        );
    }

    #[test]
    fn host_meta_xml_matches_mastodon_byte_for_byte() {
        assert_eq!(
            HostMeta::for_domain("plamenu.local").to_xml(),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <XRD xmlns=\"http://docs.oasis-open.org/ns/xri/xrd-1.0\">\n  \
             <Link rel=\"lrdd\" template=\"https://plamenu.local/.well-known/webfinger?resource={uri}\"/>\n\
             </XRD>"
        );
    }

    #[test]
    fn parses_activitystreams_type_hint_on_self_links() {
        // A Lemmy JRD: one `acct` answered by both a Person and a Group `self`
        // link, each tagged with its AS type under `properties`.
        let doc = serde_json::json!({
            "subject": "acct:collision@lemmy.example",
            "links": [
                {
                    "rel": "self",
                    "type": ACTIVITY_JSON,
                    "href": "https://lemmy.example/u/collision",
                    "properties": { "https://www.w3.org/ns/activitystreams#type": "Person" },
                },
                {
                    "rel": "self",
                    "type": ACTIVITY_JSON,
                    "href": "https://lemmy.example/c/collision",
                    "properties": { "https://www.w3.org/ns/activitystreams#type": "Group" },
                },
            ],
        });
        let jrd: Jrd = serde_json::from_value(doc).unwrap();
        assert_eq!(jrd.links[0].advertised_type(), Some("Person"));
        assert_eq!(jrd.links[1].advertised_type(), Some("Group"));
        // A link with no properties has no hint.
        assert_eq!(
            Jrd::for_local_actor(&"a@b.test".parse().unwrap()).links[1].advertised_type(),
            None
        );
    }

    #[test]
    fn served_jrds_omit_empty_properties() {
        // Plamenu never advertises `properties`, so the key stays off the wire.
        let value = serde_json::to_value(Jrd::for_local_actor(
            &"alice@plamenu.local".parse().unwrap(),
        ))
        .unwrap();
        assert!(value["links"][1].get("properties").is_none());
    }
}
