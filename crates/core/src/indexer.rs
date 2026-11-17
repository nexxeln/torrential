//! Torznab indexers the engine searches beside its built-in sources: Jackett, Prowlarr, or
//! bitmagnet endpoints that reach trackers torrential does not, private ones among them.

use std::fmt;

use serde::{Deserialize, Serialize};

/// An indexer's API key. Nothing prints it: not `Debug`, logs, errors, or `--json` output.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ApiKey(String);

impl ApiKey {
    /// `None` for an empty key.
    pub fn new(key: &str) -> Option<Self> {
        let key = key.trim();
        (!key.is_empty()).then(|| Self(key.to_string()))
    }

    /// The key itself, for the request that carries it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

/// A `sources.torznab` entry, as `torrential indexers` lists it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Indexer {
    /// The `--source` name.
    pub name: String,
    /// The Torznab endpoint, which never holds the key.
    pub url: String,
    pub key: KeyFrom,
    /// What it answered to `t=caps` just now.
    pub check: Check,
}

/// Where an indexer's API key comes from.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyFrom {
    /// It takes none.
    None,
    /// `api_key` in config.yaml.
    Config,
    /// `api_key_env`: the engine's environment variable of this name.
    Env(String),
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum Check {
    /// It answered as a Torznab indexer, with these top-level categories.
    Answers { categories: Vec<IndexerCategory> },
    /// It did not, and why, in plain words.
    Fails { reason: String },
}

/// A Newznab category an indexer lists, such as 2000 Movies or 5000 TV.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct IndexerCategory {
    pub id: u32,
    pub name: String,
}

/// The indexers in config.yaml after a request, and the names an add wrote.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct IndexerList {
    pub indexers: Vec<Indexer>,
    /// The entries an add wrote, new or with a new key. Empty for any other request.
    #[serde(default)]
    pub added: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_prints_as_redacted_and_serializes_as_itself() {
        let key = ApiKey::new(" k3y \n").unwrap();
        assert_eq!(format!("{key:?}"), "ApiKey(<redacted>)");
        assert_eq!(serde_json::to_string(&key).unwrap(), r#""k3y""#);
        assert_eq!(ApiKey::new("  "), None);
    }

    #[test]
    fn a_list_reads_as_plain_json() {
        let list = IndexerList {
            indexers: vec![Indexer {
                name: "filelist".into(),
                url: "http://127.0.0.1:9696/3/api".into(),
                key: KeyFrom::Env("PROWLARR_KEY".into()),
                check: Check::Answers { categories: vec![IndexerCategory { id: 5000, name: "TV".into() }] },
            }],
            added: vec!["filelist".into()],
        };
        let json = serde_json::to_value(&list).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "indexers": [{
                    "name": "filelist",
                    "url": "http://127.0.0.1:9696/3/api",
                    "key": {"env": "PROWLARR_KEY"},
                    "check": {"state": "answers", "categories": [{"id": 5000, "name": "TV"}]},
                }],
                "added": ["filelist"],
            })
        );
        assert_eq!(serde_json::from_value::<IndexerList>(json).unwrap(), list);
    }
}
