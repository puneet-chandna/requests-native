use http::{HeaderName, HeaderValue};
use indexmap::IndexMap;

#[derive(Clone, Debug)]
pub struct CaseInsensitiveMap<V> {
    entries: IndexMap<String, (String, V)>,
}

impl<V> Default for CaseInsensitiveMap<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V> CaseInsensitiveMap<V> {
    pub fn new() -> Self {
        Self {
            entries: IndexMap::new(),
        }
    }

    pub fn insert(
        &mut self,
        normalized_key: String,
        cased_key: String,
        value: V,
    ) -> Option<(String, V)> {
        self.entries.insert(normalized_key, (cased_key, value))
    }

    pub fn get(&self, normalized_key: &str) -> Option<&V> {
        self.entries.get(normalized_key).map(|(_, value)| value)
    }

    pub fn remove(&mut self, normalized_key: &str) -> Option<(String, V)> {
        self.entries.shift_remove(normalized_key)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &V)> {
        self.entries
            .values()
            .map(|(cased_key, value)| (cased_key.as_str(), value))
    }

    pub fn lower_items(&self) -> impl Iterator<Item = (&str, &V)> {
        self.entries
            .iter()
            .map(|(normalized_key, (_, value))| (normalized_key.as_str(), value))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct HeaderValues {
    values: CaseInsensitiveMap<HeaderValue>,
}

impl Default for HeaderValues {
    fn default() -> Self {
        Self::new()
    }
}

impl HeaderValues {
    pub fn new() -> Self {
        Self {
            values: CaseInsensitiveMap::new(),
        }
    }

    pub fn insert(&mut self, name: HeaderName, value: HeaderValue) -> Option<HeaderValue> {
        let name = name.as_str().to_owned();
        self.values
            .insert(name.clone(), name, value)
            .map(|(_, previous)| previous)
    }

    pub fn get(&self, name: &HeaderName) -> Option<&HeaderValue> {
        self.values.get(name.as_str())
    }
}

#[cfg(test)]
mod tests {
    use http::{HeaderName, HeaderValue};

    use super::{CaseInsensitiveMap, HeaderValues};

    #[test]
    fn overwrite_updates_casing_and_value_without_reordering() {
        let mut values = CaseInsensitiveMap::new();
        values.insert("alpha".into(), "Alpha".into(), 1);
        values.insert("beta".into(), "BETA".into(), 2);
        values.insert("alpha".into(), "aLPHa".into(), 3);

        assert_eq!(
            values.iter().collect::<Vec<_>>(),
            vec![("aLPHa", &3), ("BETA", &2)]
        );
        assert_eq!(
            values.lower_items().collect::<Vec<_>>(),
            vec![("alpha", &3), ("beta", &2)]
        );
        assert_eq!(values.get("ALPHA"), None);
        assert_eq!(values.get("alpha"), Some(&3));
        assert_eq!(values.len(), 2);
    }

    #[test]
    fn remove_preserves_remaining_order_and_clone_is_independent() {
        let mut values = CaseInsensitiveMap::new();
        values.insert("one".into(), "One".into(), String::from("first"));
        values.insert("two".into(), "Two".into(), String::from("second"));
        values.insert("three".into(), "Three".into(), String::from("third"));

        let removed = values.remove("two");
        let mut copied = values.clone();
        copied.insert("one".into(), "ONE".into(), String::from("replacement"));

        assert_eq!(removed, Some(("Two".into(), "second".into())));
        assert_eq!(
            values.iter().map(|(key, _)| key).collect::<Vec<_>>(),
            vec!["One", "Three"]
        );
        assert_eq!(values.get("one").map(String::as_str), Some("first"));
        assert_eq!(copied.get("one").map(String::as_str), Some("replacement"));
    }

    #[test]
    fn header_values_accept_only_validated_http_types() {
        let mut headers = HeaderValues::new();
        headers.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );

        assert_eq!(
            headers
                .get(&HeaderName::from_static("content-type"))
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
    }
}
