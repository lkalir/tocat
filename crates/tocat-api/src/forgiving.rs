//! forgiving.rs: the deserializer that applies [`normalize`] to a plugin's own
//! config, without the plugin having to know.
//!
//! [`normalize`]: crate::normalize::normalize
//!
//! The host cannot simply normalize the keys it was given. serde matches field
//! names by exact comparison against a set fixed at compile time by
//! `rename_all`, `rename` and `alias`, so rewriting `max-connections` to
//! `maxconnections` would stop matching a `rename_all = "kebab-case"` struct,
//! and `deny_unknown_fields` would turn the miss into a hard error. The host
//! also cannot know a plugin's field names in advance, and for a WASM guest it
//! never will.
//!
//! But serde hands them over at the moment of use.
//! `Deserializer::deserialize_struct` receives the declared field list, and
//! `deserialize_enum` the declared variants. So this deserializer normalizes to
//! *match* and then rewrites the key to the plugin's *own* spelling before the
//! derive ever sees it. The plugin's struct, its `rename_all` and its
//! `deny_unknown_fields` behave exactly as written, aliases keep working, and
//! an option nobody declared still reaches the plugin to be rejected there.
//!
//! Everything else delegates to `serde_json`, so a type with a hand-written
//! `Deserialize` (rate's `Interval`, which asks for `deserialize_any` and
//! parses `"500ms"` itself) is untouched.

use serde::{
    de::{self, DeserializeSeed, Deserializer, IntoDeserializer, MapAccess, SeqAccess, Visitor},
    forward_to_deserialize_any,
};
use serde_json::{Map, Value};

use crate::normalize::canonical;

/// A `serde_json::Value` that matches identifiers the way the rest of tocat
/// does.
pub struct Forgiving(pub Value);

/// Rewrite each key to the declared spelling it means.
///
/// Two spellings of one option are an error rather than a silent win for
/// whichever the map happens to yield last: `Map` here is a `BTreeMap`, so
/// "last" would mean alphabetically last, which is nobody's intent.
fn rekey(
    map: Map<String, Value>,
    fields: &[&str],
) -> Result<Map<String, Value>, serde_json::Error> {
    let mut out = Map::new();

    for (key, value) in map {
        let key = match canonical(&key, fields) {
            Some(field) => field.to_string(),
            None => key,
        };

        if out.contains_key(&key) {
            return Err(de::Error::custom(format!(
                "`{key}` was given more than once, under more than one spelling"
            )));
        }

        out.insert(key, value);
    }

    Ok(out)
}

impl<'de> Deserializer<'de> for Forgiving {
    type Error = serde_json::Error;

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match self.0 {
            Value::Object(map) => visitor.visit_map(ForgivingMap::new(rekey(map, fields)?)),
            other => other.deserialize_struct(name, fields, visitor),
        }
    }

    /// A struct with `#[serde(flatten)]` arrives here instead, without a field
    /// list, so its keys keep exact matching. Its values are still visited
    /// through this deserializer.
    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.0 {
            Value::Object(map) => visitor.visit_map(ForgivingMap::new(map)),
            other => other.deserialize_map(visitor),
        }
    }

    /// Unit variants only, which is all the `key=value` grammar can express.
    /// A variant carrying data is left to `serde_json`.
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        let value = match self.0 {
            Value::String(tag) => match canonical(&tag, variants) {
                Some(variant) => Value::String(variant.to_string()),
                None => Value::String(tag),
            },
            other => other,
        };

        value.deserialize_enum(name, variants, visitor)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.0 {
            Value::Array(items) => visitor.visit_seq(ForgivingSeq(items.into_iter())),
            other => other.deserialize_seq(visitor),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.0 {
            Value::Null => visitor.visit_none(),
            value => visitor.visit_some(Forgiving(value)),
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_newtype_struct(Forgiving(self.0))
    }

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.0.deserialize_any(visitor)
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf unit unit_struct tuple tuple_struct identifier ignored_any
    }
}

struct ForgivingMap {
    entries: serde_json::map::IntoIter,
    value: Option<Value>,
}

impl ForgivingMap {
    fn new(map: Map<String, Value>) -> Self {
        Self {
            entries: map.into_iter(),
            value: None,
        }
    }
}

impl<'de> MapAccess<'de> for ForgivingMap {
    type Error = serde_json::Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        let Some((key, value)) = self.entries.next() else {
            return Ok(None);
        };

        self.value = Some(value);
        seed.deserialize(key.into_deserializer()).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        let value = self.value.take().unwrap_or(Value::Null);
        seed.deserialize(Forgiving(value))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.entries.len())
    }
}

struct ForgivingSeq(std::vec::IntoIter<Value>);

impl<'de> SeqAccess<'de> for ForgivingSeq {
    type Error = serde_json::Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        match self.0.next() {
            Some(value) => seed.deserialize(Forgiving(value)).map(Some),
            None => Ok(None),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.0.len())
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde_json::json;

    use super::*;

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(rename_all = "kebab-case")]
    enum Format {
        Hex,
        #[serde(alias = "raw")]
        RawBinary,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(rename_all = "kebab-case", deny_unknown_fields)]
    struct Config {
        format: Format,
        #[serde(default)]
        max_connections: Option<u32>,
        #[serde(default)]
        label: Option<String>,
    }

    fn parse(value: Value) -> Result<Config, serde_json::Error> {
        Config::deserialize(Forgiving(value))
    }

    #[test]
    fn keys_and_values_tolerate_any_spelling() {
        let config = parse(json!({"Format": "Raw_Binary", "max_connections": 4})).unwrap();

        assert_eq!(config.format, Format::RawBinary);
        assert_eq!(config.max_connections, Some(4));
    }

    #[test]
    fn declared_aliases_still_reach_serde() {
        assert_eq!(
            parse(json!({"format": "raw"})).unwrap().format,
            Format::RawBinary
        );
    }

    #[test]
    fn values_are_not_touched() {
        let config = parse(json!({"format": "hex", "label": "Wire_Tap"})).unwrap();

        assert_eq!(config.label.as_deref(), Some("Wire_Tap"));
    }

    #[test]
    fn unknown_options_are_still_rejected() {
        assert!(parse(json!({"format": "hex", "nonsense": 1})).is_err());
    }

    #[test]
    fn one_option_under_two_spellings_is_an_error() {
        assert!(
            parse(json!({"format": "hex", "max-connections": 1, "maxconnections": 2})).is_err()
        );
    }
}
