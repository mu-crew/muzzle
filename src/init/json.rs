//! An order-keeping JSON tree for the harness settings files `init`
//! rewrites. serde_json's `Value` sorts object keys unless its
//! `preserve_order` feature is on, and that feature pulls a crate the build
//! doesn't have yet. The files are shared (every-harness D13), so every key
//! muzzle doesn't own must come back where it was.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Json>),
    Object(Object),
}

/// Object members in document order. A repeated key is kept as written;
/// lookups take the last one, as serde_json and the harnesses do.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Object(Vec<(String, Json)>);

impl Json {
    pub fn str(s: &str) -> Json {
        Json::String(s.to_string())
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&Object> {
        match self {
            Json::Object(o) => Some(o),
            _ => None,
        }
    }

    /// Pretty text as the harnesses and murmur write it: two-space indent
    /// and a final newline.
    pub fn to_pretty(&self) -> String {
        let mut text = serde_json::to_string_pretty(self).expect("JSON tree serialises");
        text.push('\n');
        text
    }
}

impl Object {
    pub fn new() -> Object {
        Object(Vec::new())
    }

    pub fn from_pairs(pairs: Vec<(&str, Json)>) -> Object {
        Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    fn position(&self, key: &str) -> Option<usize> {
        self.0.iter().rposition(|(k, _)| k == key)
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        self.position(key).map(|i| &self.0[i].1)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        self.position(key).map(|i| &mut self.0[i].1)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.position(key).is_some()
    }

    /// Replaces the value in place, or appends the member.
    pub fn insert(&mut self, key: &str, value: Json) {
        match self.position(key) {
            Some(i) => self.0[i].1 = value,
            None => self.0.push((key.to_string(), value)),
        }
    }

    /// Removes every member named `key`, keeping the others' order.
    pub fn remove(&mut self, key: &str) {
        self.0.retain(|(k, _)| k != key);
    }

    pub fn keys(&self) -> Vec<String> {
        self.0.iter().map(|(k, _)| k.clone()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Serialize for Json {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Json::Null => s.serialize_unit(),
            Json::Bool(b) => s.serialize_bool(*b),
            Json::Number(n) => n.serialize(s),
            Json::String(v) => s.serialize_str(v),
            Json::Array(a) => {
                let mut seq = s.serialize_seq(Some(a.len()))?;
                for v in a {
                    seq.serialize_element(v)?;
                }
                seq.end()
            }
            Json::Object(o) => {
                let mut map = s.serialize_map(Some(o.0.len()))?;
                for (k, v) in &o.0 {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Json, E> {
        Ok(Json::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Json, E> {
        Ok(Json::Number(v.into()))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Json, E> {
        Ok(Json::Number(v.into()))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Json, E> {
        serde_json::Number::from_f64(v)
            .map(Json::Number)
            .ok_or_else(|| E::custom("a number JSON can't hold"))
    }

    fn visit_str<E>(self, v: &str) -> Result<Json, E> {
        Ok(Json::str(v))
    }

    fn visit_string<E>(self, v: String) -> Result<Json, E> {
        Ok(Json::String(v))
    }

    fn visit_unit<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_none<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Json, D::Error> {
        Json::deserialize(d)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
        let mut out = Vec::new();
        while let Some(v) = seq.next_element()? {
            out.push(v);
        }
        Ok(Json::Array(out))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        let mut out = Vec::new();
        while let Some((k, v)) = map.next_entry::<String, Json>()? {
            out.push((k, v));
        }
        Ok(Json::Object(Object(out)))
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Json, D::Error> {
        d.deserialize_any(JsonVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_come_back_in_document_order() {
        let text = r#"{"z":1,"a":{"y":[true,null,"s"],"b":-2.5},"m":18446744073709551615}"#;
        let v: Json = serde_json::from_str(text).unwrap();
        assert_eq!(serde_json::to_string(&v).unwrap(), text);
    }

    #[test]
    fn insert_replaces_in_place_and_remove_keeps_the_rest() {
        let mut o = Object::from_pairs(vec![("a", Json::Null), ("b", Json::Null)]);
        o.insert("a", Json::Bool(true));
        o.insert("c", Json::Null);
        assert_eq!(o.keys(), ["a", "b", "c"]);
        o.remove("b");
        assert_eq!(o.keys(), ["a", "c"]);
        assert_eq!(o.get("a"), Some(&Json::Bool(true)));
    }

    #[test]
    fn a_repeated_key_reads_as_the_last_and_is_written_back() {
        let text = r#"{"k":1,"k":2}"#;
        let v: Json = serde_json::from_str(text).unwrap();
        let o = v.as_object().unwrap();
        assert_eq!(o.get("k"), Some(&Json::Number(2.into())));
        assert_eq!(serde_json::to_string(&v).unwrap(), text);
    }
}
