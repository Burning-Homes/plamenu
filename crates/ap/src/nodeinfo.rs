//! `NodeInfo` documents (<https://nodeinfo.diaspora.software/>), versions 2.0 and 2.1.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const NODEINFO_20_REL: &str = "http://nodeinfo.diaspora.software/ns/schema/2.0";
pub const NODEINFO_21_REL: &str = "http://nodeinfo.diaspora.software/ns/schema/2.1";

/// The `/.well-known/nodeinfo` discovery document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfoIndex {
    pub links: Vec<NodeInfoLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfoLink {
    pub rel: String,
    pub href: String,
}

impl NodeInfoIndex {
    #[must_use]
    pub fn for_domain(domain: &str) -> Self {
        Self {
            links: vec![
                NodeInfoLink {
                    rel: NODEINFO_20_REL.to_owned(),
                    href: format!("https://{domain}/nodeinfo/2.0"),
                },
                NodeInfoLink {
                    rel: NODEINFO_21_REL.to_owned(),
                    href: format!("https://{domain}/nodeinfo/2.1"),
                },
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeInfo {
    pub version: String,
    pub software: Software,
    pub protocols: Vec<String>,
    pub services: Services,
    pub open_registrations: bool,
    pub usage: Usage,
    pub metadata: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Software {
    pub name: String,
    pub version: String,
    /// Only present in nodeinfo 2.1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Services {
    pub inbound: Vec<String>,
    pub outbound: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub users: UsageUsers,
    pub local_posts: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageUsers {
    pub total: u64,
    pub active_month: u64,
    pub active_halfyear: u64,
}

/// Live counters that fill a [`NodeInfo`] document.
#[derive(Debug, Clone, Copy, Default)]
pub struct NodeInfoStats {
    pub total_users: u64,
    pub active_month: u64,
    pub active_halfyear: u64,
    pub local_posts: u64,
    /// `registrations_mode == "open"` — `NodeInfo`'s `openRegistrations` means
    /// sign-up without operator involvement, so `approved` counts as closed
    /// (Mastodon reports it the same way).
    pub open_registrations: bool,
}

impl NodeInfo {
    /// Builds the document for the given schema version (`"2.0"` or `"2.1"`).
    /// `repository` is omitted (the 2.1 field is optional) until the project
    /// has a public source URL — a dead placeholder link is worse than none.
    #[must_use]
    pub fn for_plamenu(schema_version: &str, software_version: &str, stats: NodeInfoStats) -> Self {
        Self {
            version: schema_version.to_owned(),
            software: Software {
                name: "plamenu".to_owned(),
                version: software_version.to_owned(),
                repository: None,
            },
            protocols: vec!["activitypub".to_owned()],
            services: Services {
                inbound: vec![],
                outbound: vec![],
            },
            open_registrations: stats.open_registrations,
            usage: Usage {
                users: UsageUsers {
                    total: stats.total_users,
                    active_month: stats.active_month,
                    active_halfyear: stats.active_halfyear,
                },
                local_posts: stats.local_posts,
            },
            metadata: Map::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_links_to_both_schemas() {
        let value = serde_json::to_value(NodeInfoIndex::for_domain("plamenu.local")).unwrap();
        assert_eq!(value["links"][0]["rel"], NODEINFO_20_REL);
        assert_eq!(
            value["links"][0]["href"],
            "https://plamenu.local/nodeinfo/2.0"
        );
        assert_eq!(value["links"][1]["rel"], NODEINFO_21_REL);
        assert_eq!(
            value["links"][1]["href"],
            "https://plamenu.local/nodeinfo/2.1"
        );
    }

    #[test]
    fn document_carries_live_stats_and_no_placeholder_repository() {
        let stats = NodeInfoStats {
            total_users: 3,
            active_month: 2,
            active_halfyear: 3,
            local_posts: 41,
            open_registrations: true,
        };
        let v21 = serde_json::to_value(NodeInfo::for_plamenu("2.1", "0.1.0", stats)).unwrap();
        assert_eq!(v21["version"], "2.1");
        assert_eq!(v21["software"]["name"], "plamenu");
        // `repository` is optional in the 2.1 schema and stays omitted until
        // a public source URL exists.
        assert!(
            !v21["software"]
                .as_object()
                .unwrap()
                .contains_key("repository")
        );
        assert_eq!(v21["usage"]["users"]["total"], 3);
        assert_eq!(v21["usage"]["users"]["activeMonth"], 2);
        assert_eq!(v21["usage"]["users"]["activeHalfyear"], 3);
        assert_eq!(v21["usage"]["localPosts"], 41);
        assert_eq!(v21["openRegistrations"], true);

        let v20 = serde_json::to_value(NodeInfo::for_plamenu("2.0", "0.1.0", stats)).unwrap();
        assert_eq!(v20["version"], "2.0");
        assert!(
            !v20["software"]
                .as_object()
                .unwrap()
                .contains_key("repository")
        );
    }
}
