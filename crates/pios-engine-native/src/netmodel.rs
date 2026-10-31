//! User-defined networks and list filters: the parts of the Docker API that
//! `docker compose` leans on (a project network, label filters).

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A user-defined bridge network (`docker network create`, compose's
/// `<project>_default`). The default `bridge` network is not stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Network {
    pub id: String,
    pub name: String,
    pub subnet: Ipv4Addr,
    pub prefix: u8,
    pub gateway: Ipv4Addr,
    /// Linux bridge device, `br-<12 hex>` like Docker.
    pub bridge: String,
    pub labels: BTreeMap<String, String>,
    pub created: i64,
    pub internal: bool,
}

/// Body of `POST /networks/create`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct NetworkCreate {
    pub name: String,
    pub driver: String,
    pub labels: Option<BTreeMap<String, String>>,
    pub internal: bool,
    pub check_duplicate: bool,
}

/// Next free /16 for a new network: 172.18.0.0 .. 172.31.0.0.
pub fn allocate_subnet(used: &[Ipv4Addr]) -> Option<Ipv4Addr> {
    (18..=31u8).map(|b| Ipv4Addr::new(172, b, 0, 0)).find(|s| !used.contains(s))
}

/// Docker's `filters` query parameter, in either encoding:
/// `{"label":{"a=b":true}}` (current) or `{"label":["a=b"]}` (old).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Filters(pub BTreeMap<String, Vec<String>>);

impl Filters {
    pub fn parse(q: Option<&str>) -> Result<Self, String> {
        let Some(q) = q.filter(|q| !q.is_empty()) else {
            return Ok(Self::default());
        };
        let v: Value = serde_json::from_str(q).map_err(|e| format!("invalid filters: {e}"))?;
        let mut out = BTreeMap::new();
        for (k, vals) in v.as_object().ok_or("invalid filters: not an object")? {
            let list: Vec<String> = match vals {
                Value::Object(m) => m.iter().filter(|(_, on)| on.as_bool() != Some(false)).map(|(s, _)| s.clone()).collect(),
                Value::Array(a) => a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect(),
                Value::String(s) => vec![s.clone()],
                _ => Vec::new(),
            };
            out.insert(k.clone(), list);
        }
        Ok(Self(out))
    }

    pub fn get(&self, key: &str) -> &[String] {
        self.0.get(key).map_or(&[], Vec::as_slice)
    }

    /// Every `label` filter (`key` or `key=value`) matches `labels`.
    pub fn labels_match(&self, labels: &BTreeMap<String, String>) -> bool {
        self.get("label").iter().all(|f| match f.split_once('=') {
            Some((k, v)) => labels.get(k).is_some_and(|x| x == v),
            None => labels.contains_key(f.as_str()),
        })
    }

    /// Any of the values for `key` satisfies `pred` (no values: true).
    pub fn any(&self, key: &str, pred: impl Fn(&str) -> bool) -> bool {
        let v = self.get(key);
        v.is_empty() || v.iter().any(|x| pred(x))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_filter_encodings() {
        let f = Filters::parse(Some(r#"{"label":{"com.docker.compose.project=gitea":true,"x":true},"name":["web"]}"#)).unwrap();
        let mut labels = BTreeMap::new();
        labels.insert("com.docker.compose.project".to_string(), "gitea".to_string());
        assert!(!f.labels_match(&labels), "label x missing");
        labels.insert("x".into(), "1".into());
        assert!(f.labels_match(&labels));
        assert!(f.any("name", |n| n == "web") && !f.any("name", |n| n == "db"));
        assert!(f.any("status", |_| false), "absent key matches");
        let old = Filters::parse(Some(r#"{"label":["a=b"]}"#)).unwrap();
        assert_eq!(old.get("label"), ["a=b"]);
        assert!(Filters::parse(Some("not json")).is_err());
        assert_eq!(Filters::parse(None).unwrap(), Filters::default());
    }

    #[test]
    fn subnets() {
        assert_eq!(allocate_subnet(&[]), Some(Ipv4Addr::new(172, 18, 0, 0)));
        assert_eq!(allocate_subnet(&[Ipv4Addr::new(172, 18, 0, 0)]), Some(Ipv4Addr::new(172, 19, 0, 0)));
    }
}
