//! The **values schema**: a JSON Schema (draft 2020-12) of the `values` object a
//! profile produces.
//!
//! A contract is a shape of values, and the one party that knows that shape
//! exactly is the engine: it decides what every [`ValueType`] looks like on the
//! wire (see the `Serialize` impl on [`Value`](crate::engine::Value)). So the
//! schema is *derived* from a profile here rather than written by hand next to
//! it, where it would drift the first time someone retyped a watch and forgot
//! the other file.
//!
//! The mapping follows the wire form exactly:
//!
//! - every value is nullable, because an unreadable watch — or field, or
//!   element — is `null` rather than a stale number;
//! - a record is an object whose fields are all present (a broken field is
//!   `null` in place, never missing), while the top-level `values` object
//!   requires nothing, because a diff carries only what changed;
//! - a collection or an `each` derived watch is an array capped at the
//!   collection's `max`.
//!
//! Nothing here closes an object with `additionalProperties: false`. A later
//! minor of the same contract adds watches and record fields, and a reader of
//! the earlier minor has to be able to accept them; a schema that forbade them
//! would make every addition a breaking change.
//!
//! The output is deterministic — `serde_json` keeps object keys sorted — so a
//! generated schema can be committed and diffed.

use serde_json::{json, Map, Value as Json};

use crate::profile::{Field, Profile, ValueType, Watch};

/// The dialect every schema produced here declares.
pub const DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

/// The JSON Schema of the `values` object `profile` produces: one optional,
/// nullable property per watch.
///
/// When the profile declares a contract, its id and version are carried in
/// `$id`, `title` and a machine-readable `x-contract` object, so a schema file
/// that has been copied somewhere still says which contract it describes.
pub fn values_schema(profile: &Profile) -> Json {
    let mut properties = Map::new();
    for w in &profile.watches {
        let (name, schema) = watch_schema(w, &profile.watches);
        properties.insert(name.to_string(), schema);
    }

    let mut root = Map::new();
    root.insert("$schema".into(), json!(DIALECT));
    root.insert("type".into(), json!("object"));
    root.insert("properties".into(), Json::Object(properties));

    match profile.declared_contract() {
        Some(c) => {
            let version = c.version.to_string();
            match c.id {
                Some(id) => {
                    root.insert(
                        "$id".into(),
                        json!(format!("urn:scry:contract:{id}:{version}")),
                    );
                    root.insert("title".into(), json!(format!("{id} {version}")));
                    root.insert("x-contract".into(), json!({ "id": id, "version": version }));
                }
                // The deprecated integer names a version of *some* shape but not
                // which one, so there is no id to put in `$id`; the version is
                // still worth keeping.
                None => {
                    root.insert("title".into(), json!(format!("contract {version}")));
                    root.insert("x-contract".into(), json!({ "version": version }));
                }
            }
        }
        None => {
            let title = profile.label.as_deref().unwrap_or("values");
            root.insert("title".into(), json!(title));
        }
    }
    Json::Object(root)
}

/// One watch's name and the schema of the value it emits. `all` is the whole
/// watch list, needed to size an `each` derived watch by the collection it
/// walks.
fn watch_schema<'a>(w: &'a Watch, all: &[Watch]) -> (&'a str, Json) {
    match w {
        Watch::Tier1 { name, ty, .. } | Watch::Tier2 { name, ty, .. } => (name, scalar(*ty)),
        Watch::Record { name, fields, .. } => (name, record(fields)),
        Watch::Collection {
            name,
            ty,
            fields,
            max,
            ..
        } => {
            // Validation guarantees exactly one of the two is set; should an
            // unvalidated profile reach here with neither, the element is left
            // unconstrained rather than invented.
            let element = match (ty, fields) {
                (Some(ty), _) => scalar(*ty),
                (None, Some(fields)) => record(fields),
                (None, None) => json!({}),
            };
            (name, array(element, Some(*max)))
        }
        Watch::Derived { name, ty, each, .. } => match each {
            // One element per element of the collection it walks, so it
            // inherits that collection's cap.
            Some(each) => {
                let max = all.iter().find_map(|w| match w {
                    Watch::Collection { name, max, .. } if name == each => Some(*max),
                    _ => None,
                });
                (name, array(scalar(*ty), max))
            }
            None => (name, scalar(*ty)),
        },
    }
}

/// A nullable scalar of the given type, with the integer range the wire type
/// can actually carry. The bounds are what the *encoding* allows, not what the
/// game will produce; a view still clamps.
fn scalar(ty: ValueType) -> Json {
    match ty {
        ValueType::I32 => json!({
            "type": ["integer", "null"],
            "minimum": i32::MIN,
            "maximum": i32::MAX,
        }),
        ValueType::U32 => json!({
            "type": ["integer", "null"],
            "minimum": 0,
            "maximum": u32::MAX,
        }),
        ValueType::U64 => json!({
            "type": ["integer", "null"],
            "minimum": 0,
            "maximum": u64::MAX,
        }),
        // A non-finite float already serialises as `null`, so a number that
        // arrives is always finite and needs no further constraint.
        ValueType::F32 => json!({ "type": ["number", "null"] }),
        ValueType::String(_) => json!({ "type": ["string", "null"] }),
    }
}

/// A nullable object with every field present and each field nullable.
fn record(fields: &std::collections::BTreeMap<String, Field>) -> Json {
    let properties: Map<String, Json> = fields
        .iter()
        .map(|(name, f)| (name.clone(), scalar(f.ty)))
        .collect();
    let required: Vec<&String> = fields.keys().collect();
    json!({
        "type": ["object", "null"],
        "properties": properties,
        "required": required,
    })
}

/// A nullable array of `element`, capped at `max` when the cap is known.
fn array(element: Json, max: Option<usize>) -> Json {
    let mut out = Map::new();
    out.insert("type".into(), json!(["array", "null"]));
    out.insert("items".into(), element);
    if let Some(max) = max {
        out.insert("maxItems".into(), json!(max));
    }
    Json::Object(out)
}

/// Render a schema the way it is meant to be committed: two-space indented,
/// with a trailing newline, so regenerating an unchanged schema is a no-op diff.
pub fn to_pretty(schema: &Json) -> String {
    // Serialising a `serde_json::Value` cannot fail: every key is a string and
    // there are no non-finite floats in a schema.
    let mut out = serde_json::to_string_pretty(schema).expect("a JSON value always serialises");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(json: &str) -> Profile {
        Profile::from_json(json).expect("parse")
    }

    /// Every tier in one profile: the scalar types, a record, both collection
    /// shapes and both derived shapes.
    const EVERY_TIER: &str = r#"
    {
      "label": "Example (Steam)",
      "contract": { "id": "example", "version": "2.1" },
      "match": { "process": "g.exe", "module": "g.exe", "probe": "90 90" },
      "watches": [
        { "tier": "tier1", "name": "hp", "module": "g.exe", "offsets": [16], "type": "i32" },
        { "tier": "tier1", "name": "gold", "module": "g.exe", "offsets": [20], "type": "u32" },
        { "tier": "tier1", "name": "ticks", "module": "g.exe", "offsets": [24], "type": "u64" },
        { "tier": "tier2", "name": "speed", "anchor": "90", "offsets": [0], "type": "f32" },
        { "tier": "tier1", "name": "zone", "module": "g.exe", "offsets": [32],
          "type": { "string": "il2cpp" } },
        { "tier": "record", "name": "player",
          "base": { "tier": "tier1", "module": "g.exe", "offsets": [40] },
          "fields": { "sp": { "offsets": [4], "type": "i32" }, "hp": { "type": "i32" } } },
        { "tier": "collection", "name": "ids",
          "base": { "tier": "tier1", "module": "g.exe", "offsets": [48] },
          "count": [24], "stride": 4, "type": "u32", "max": 16 },
        { "tier": "collection", "name": "party",
          "base": { "tier": "tier1", "module": "g.exe", "offsets": [56] },
          "count": [24], "stride": 8, "element": [0], "max": 8,
          "fields": { "name": { "type": { "string": "il2cpp" } }, "hp": { "offsets": [16], "type": "i32" } } },
        { "tier": "derived", "name": "hp_percent", "type": "f32",
          "value": { "div": [{ "watch": "hp" }, { "const": 100 }] } },
        { "tier": "derived", "name": "party_hp", "type": "i32", "each": "party",
          "value": { "item": "hp" } }
      ]
    }
    "#;

    #[test]
    fn every_watch_becomes_one_nullable_property() {
        let schema = values_schema(&profile(EVERY_TIER));
        let props = schema["properties"].as_object().expect("properties");
        assert_eq!(props.len(), 10);
        // Nothing at the top level is required: a diff carries only what moved.
        assert!(schema.get("required").is_none());
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["$schema"], DIALECT);

        for (name, prop) in props {
            let types = prop["type"].as_array().expect("type array");
            assert!(
                types.contains(&json!("null")),
                "{name} must be nullable: an unreadable watch is null"
            );
        }
    }

    #[test]
    fn scalar_types_follow_the_wire_form() {
        let schema = values_schema(&profile(EVERY_TIER));
        let p = &schema["properties"];
        assert_eq!(p["hp"]["type"], json!(["integer", "null"]));
        assert_eq!(p["hp"]["minimum"], json!(i32::MIN));
        assert_eq!(p["hp"]["maximum"], json!(i32::MAX));
        assert_eq!(p["gold"]["minimum"], json!(0));
        assert_eq!(p["gold"]["maximum"], json!(u32::MAX));
        assert_eq!(p["ticks"]["maximum"], json!(u64::MAX));
        assert_eq!(p["speed"], json!({ "type": ["number", "null"] }));
        assert_eq!(p["zone"], json!({ "type": ["string", "null"] }));
        // The derived tier types like any other scalar.
        assert_eq!(p["hp_percent"], json!({ "type": ["number", "null"] }));
    }

    #[test]
    fn records_require_every_field_and_let_each_be_null() {
        let schema = values_schema(&profile(EVERY_TIER));
        let player = &schema["properties"]["player"];
        assert_eq!(player["type"], json!(["object", "null"]));
        assert_eq!(player["required"], json!(["hp", "sp"]));
        assert_eq!(
            player["properties"]["sp"]["type"],
            json!(["integer", "null"])
        );
        // Left open so a later minor can add a field without breaking a reader.
        assert!(player.get("additionalProperties").is_none());
    }

    #[test]
    fn collections_are_capped_arrays_of_nullable_elements() {
        let schema = values_schema(&profile(EVERY_TIER));
        let p = &schema["properties"];

        assert_eq!(p["ids"]["type"], json!(["array", "null"]));
        assert_eq!(p["ids"]["maxItems"], json!(16));
        assert_eq!(p["ids"]["items"]["type"], json!(["integer", "null"]));

        let party = &p["party"];
        assert_eq!(party["maxItems"], json!(8));
        assert_eq!(party["items"]["type"], json!(["object", "null"]));
        assert_eq!(party["items"]["required"], json!(["hp", "name"]));
        assert_eq!(
            party["items"]["properties"]["name"]["type"],
            json!(["string", "null"])
        );

        // An `each` derived watch has one element per element of the
        // collection it walks, so it inherits that collection's cap.
        assert_eq!(p["party_hp"]["type"], json!(["array", "null"]));
        assert_eq!(p["party_hp"]["maxItems"], json!(8));
        assert_eq!(p["party_hp"]["items"]["type"], json!(["integer", "null"]));
    }

    #[test]
    fn the_contract_names_the_schema() {
        let schema = values_schema(&profile(EVERY_TIER));
        assert_eq!(schema["$id"], "urn:scry:contract:example:2.1");
        assert_eq!(schema["title"], "example 2.1");
        assert_eq!(
            schema["x-contract"],
            json!({ "id": "example", "version": "2.1" })
        );
    }

    #[test]
    fn a_legacy_contract_version_keeps_its_version_but_has_no_id() {
        let schema = values_schema(&profile(
            r#"{ "contractVersion": 3,
                 "match": { "process": "g.exe", "module": "g.exe", "probe": "90" },
                 "watches": [] }"#,
        ));
        assert!(schema.get("$id").is_none());
        assert_eq!(schema["x-contract"], json!({ "version": "3.0" }));
        assert_eq!(schema["title"], "contract 3.0");
    }

    #[test]
    fn no_contract_falls_back_to_the_label() {
        let schema = values_schema(&profile(
            r#"{ "label": "Scratch",
                 "match": { "process": "g.exe", "module": "g.exe", "probe": "90" },
                 "watches": [] }"#,
        ));
        assert!(schema.get("$id").is_none());
        assert!(schema.get("x-contract").is_none());
        assert_eq!(schema["title"], "Scratch");
    }

    #[test]
    fn output_is_deterministic_and_ends_in_a_newline() {
        // Declaration order must not leak into the output: the same watches in
        // another order are the same contract, and must diff as nothing.
        let mut reordered = profile(EVERY_TIER);
        reordered.watches.swap(0, 3);
        let a = to_pretty(&values_schema(&profile(EVERY_TIER)));
        let b = to_pretty(&values_schema(&reordered));
        assert_eq!(a, b);
        assert!(a.ends_with("}\n"));
        // Keys come out sorted, `$`-prefixed ones first.
        assert!(a.starts_with("{\n  \"$id\""), "{a}");
    }
}
