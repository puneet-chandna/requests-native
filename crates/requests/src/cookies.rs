//! Python-free cookie snapshots and stage-local mechanics.
//!
//! Python cookie jars and policies remain authoritative in the binding crate.
//! This module only receives owned snapshots, computes mapping observations,
//! and describes the ordered private pipeline stages.

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CookieSnapshot {
    pub name: String,
    pub value: Option<String>,
    pub domain: Option<String>,
    pub path: Option<String>,
    pub secure: bool,
    pub expires: Option<i64>,
    pub discard: bool,
    pub rest: Vec<(String, CookieScalar)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CookieScalar {
    None,
    Bool(bool),
    Integer(i64),
    Text(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JarSnapshot {
    cookies: Vec<CookieSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LookupQuery {
    pub name: String,
    pub domain: Option<String>,
    pub path: Option<String>,
    pub default: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LookupValue {
    Value(String),
    Missing,
    Conflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CookiePipelineStage {
    Prepare,
    SnapshotAfterPrepare,
    Hook,
    SnapshotAfterHook,
    Extract,
    SnapshotAfterExtract,
    Digest,
    SnapshotAfterDigest,
    Header,
    SnapshotAfterHeader,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotDelta {
    pub before: JarSnapshot,
    pub after: JarSnapshot,
}

impl JarSnapshot {
    #[must_use]
    pub fn new(cookies: Vec<CookieSnapshot>) -> Self {
        Self { cookies }
    }

    #[must_use]
    pub fn cookies(&self) -> &[CookieSnapshot] {
        &self.cookies
    }

    #[must_use]
    pub fn keys(&self) -> Vec<&str> {
        self.cookies
            .iter()
            .map(|cookie| cookie.name.as_str())
            .collect()
    }

    #[must_use]
    pub fn values(&self) -> Vec<Option<&str>> {
        self.cookies
            .iter()
            .map(|cookie| cookie.value.as_deref())
            .collect()
    }

    #[must_use]
    pub fn items(&self) -> Vec<(&str, Option<&str>)> {
        self.cookies
            .iter()
            .map(|cookie| (cookie.name.as_str(), cookie.value.as_deref()))
            .collect()
    }

    #[must_use]
    pub fn find_no_duplicates(&self, query: &LookupQuery) -> LookupValue {
        let mut selected: Option<&str> = None;
        for cookie in self.matching(query) {
            let Some(value) = cookie.value.as_deref() else {
                continue;
            };
            if selected.is_some() {
                return LookupValue::Conflict;
            }
            selected = Some(value);
        }
        selected
            .map(|value| LookupValue::Value(value.to_owned()))
            .unwrap_or(LookupValue::Missing)
    }

    #[must_use]
    pub fn get_dict(
        &self,
        domain: Option<&str>,
        path: Option<&str>,
    ) -> Vec<(String, Option<String>)> {
        let mut values: Vec<(String, Option<String>)> = Vec::new();
        for cookie in &self.cookies {
            if domain.is_some_and(|selected| cookie.domain.as_deref() != Some(selected))
                || path.is_some_and(|selected| cookie.path.as_deref() != Some(selected))
            {
                continue;
            }
            if let Some((_, value)) = values.iter_mut().find(|(name, _)| name == &cookie.name) {
                *value = cookie.value.clone();
            } else {
                values.push((cookie.name.clone(), cookie.value.clone()));
            }
        }
        values
    }

    #[must_use]
    pub fn domains(&self) -> Vec<Option<&str>> {
        let mut domains = Vec::new();
        for cookie in &self.cookies {
            let domain = cookie.domain.as_deref();
            if !domains.contains(&domain) {
                domains.push(domain);
            }
        }
        domains
    }

    #[must_use]
    pub fn paths(&self) -> Vec<Option<&str>> {
        let mut paths = Vec::new();
        for cookie in &self.cookies {
            let path = cookie.path.as_deref();
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        paths
    }

    /// Preserve Requests' historical oddity: this returns true when a
    /// non-`None` domain repeats, rather than when two distinct domains exist.
    #[must_use]
    pub fn multiple_domains(&self) -> bool {
        let mut domains: Vec<&str> = Vec::new();
        for cookie in &self.cookies {
            let Some(domain) = cookie.domain.as_deref() else {
                continue;
            };
            if domains.contains(&domain) {
                return true;
            }
            domains.push(domain);
        }
        false
    }

    fn matching<'a>(&'a self, query: &'a LookupQuery) -> impl Iterator<Item = &'a CookieSnapshot> {
        self.cookies.iter().filter(|cookie| {
            cookie.name == query.name
                && query
                    .domain
                    .as_deref()
                    .is_none_or(|domain| cookie.domain.as_deref() == Some(domain))
                && query
                    .path
                    .as_deref()
                    .is_none_or(|path| cookie.path.as_deref() == Some(path))
        })
    }
}

impl SnapshotDelta {
    #[must_use]
    pub fn new(before: JarSnapshot, after: JarSnapshot) -> Self {
        Self { before, after }
    }

    #[must_use]
    pub fn changed(&self) -> bool {
        self.before != self.after
    }
}

#[must_use]
pub const fn private_pipeline_stages() -> [CookiePipelineStage; 10] {
    [
        CookiePipelineStage::Prepare,
        CookiePipelineStage::SnapshotAfterPrepare,
        CookiePipelineStage::Hook,
        CookiePipelineStage::SnapshotAfterHook,
        CookiePipelineStage::Extract,
        CookiePipelineStage::SnapshotAfterExtract,
        CookiePipelineStage::Digest,
        CookiePipelineStage::SnapshotAfterDigest,
        CookiePipelineStage::Header,
        CookiePipelineStage::SnapshotAfterHeader,
    ]
}

#[cfg(test)]
mod tests {
    use super::{
        CookiePipelineStage, CookieScalar, CookieSnapshot, JarSnapshot, LookupQuery, LookupValue,
        SnapshotDelta, private_pipeline_stages,
    };

    fn cookie(
        name: &str,
        value: Option<&str>,
        domain: Option<&str>,
        path: Option<&str>,
    ) -> CookieSnapshot {
        CookieSnapshot {
            name: name.to_owned(),
            value: value.map(str::to_owned),
            domain: domain.map(str::to_owned),
            path: path.map(str::to_owned),
            secure: false,
            expires: None,
            discard: true,
            rest: vec![("HttpOnly".to_owned(), CookieScalar::None)],
        }
    }

    #[test]
    fn mapping_snapshot_preserves_order_none_and_requests_domain_oddity() {
        let snapshot = JarSnapshot::new(vec![
            cookie("sid", Some("root"), Some("a.test"), Some("/")),
            cookie("empty", None, Some("a.test"), Some("/")),
            cookie("sid", Some("nested"), Some("a.test"), Some("/nested")),
            cookie("sid", Some("other"), Some("b.test"), Some("/")),
        ]);
        assert_eq!(snapshot.keys(), vec!["sid", "empty", "sid", "sid"]);
        assert_eq!(
            snapshot.get_dict(None, None),
            vec![
                ("sid".to_owned(), Some("other".to_owned())),
                ("empty".to_owned(), None),
            ]
        );
        assert!(snapshot.multiple_domains());
    }

    #[test]
    fn none_does_not_count_as_a_found_no_duplicate_value() {
        let snapshot = JarSnapshot::new(vec![cookie("empty", None, Some("a.test"), Some("/"))]);
        assert_eq!(
            snapshot.find_no_duplicates(&LookupQuery {
                name: "empty".to_owned(),
                domain: Some("a.test".to_owned()),
                path: Some("/".to_owned()),
                default: "missing".to_owned(),
            }),
            LookupValue::Missing
        );
    }

    #[test]
    fn pipeline_and_delta_are_ordered_owned_data() {
        assert_eq!(
            private_pipeline_stages(),
            [
                CookiePipelineStage::Prepare,
                CookiePipelineStage::SnapshotAfterPrepare,
                CookiePipelineStage::Hook,
                CookiePipelineStage::SnapshotAfterHook,
                CookiePipelineStage::Extract,
                CookiePipelineStage::SnapshotAfterExtract,
                CookiePipelineStage::Digest,
                CookiePipelineStage::SnapshotAfterDigest,
                CookiePipelineStage::Header,
                CookiePipelineStage::SnapshotAfterHeader,
            ]
        );
        let before = JarSnapshot::new(Vec::new());
        let after = JarSnapshot::new(vec![cookie("name", Some("value"), None, Some("/"))]);
        assert!(SnapshotDelta::new(before, after).changed());
    }
}
