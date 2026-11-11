//! An `agent()` call's `schema`, as the worker's turn takes it: the daemon
//! hands it to the provider as strict structured output (`output_schema`,
//! as Codex sends a turn's), which takes only objects that list every
//! property as required and allow no others. So an optional property goes
//! as one that may be null, and a null the reply gives one is left out of
//! the object `agent()` returns, which then has the schema's own shape.
use serde_json::{Map, Value, json};

/// Whether `schema` is one `agent()` takes: an object schema with
/// `properties` at its root, `required` naming only those.
pub fn check(schema: &Value) -> Result<(), String> {
    const NEEDS: &str = "a schema needs {type: 'object', properties: {...}} at its root, with required a subset of properties";
    let properties = schema["properties"].as_object();
    if schema["type"] != "object" || properties.is_none() {
        return Err(NEEDS.into());
    }
    let properties = properties.expect("checked");
    match &schema["required"] {
        Value::Null => Ok(()),
        Value::Array(names)
            if names
                .iter()
                .all(|name| name.as_str().is_some_and(|n| properties.contains_key(n))) =>
        {
            Ok(())
        }
        _ => Err(NEEDS.into()),
    }
}

/// `schema` as strict structured output takes it.
pub fn strict(schema: &Value) -> Value {
    let Value::Object(fields) = schema else {
        return schema.clone();
    };
    let mut out = Map::new();
    for (key, value) in fields {
        let value = match key.as_str() {
            "items" | "additionalItems" | "not" => strict(value),
            "anyOf" | "oneOf" | "allOf" | "prefixItems" => match value {
                Value::Array(each) => Value::Array(each.iter().map(strict).collect()),
                other => other.clone(),
            },
            "$defs" | "definitions" => match value {
                Value::Object(defs) => {
                    Value::Object(defs.iter().map(|(k, v)| (k.clone(), strict(v))).collect())
                }
                other => other.clone(),
            },
            _ => value.clone(),
        };
        out.insert(key.clone(), value);
    }
    if let Some(Value::Object(properties)) = fields.get("properties") {
        let required: Vec<&str> = (fields.get("required").and_then(Value::as_array))
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let properties: Map<String, Value> = (properties.iter())
            .map(|(name, property)| {
                let property = strict(property);
                let property = match required.contains(&name.as_str()) {
                    true => property,
                    false => nullable(property),
                };
                (name.clone(), property)
            })
            .collect();
        out.insert(
            "required".into(),
            Value::Array(properties.keys().map(|name| json!(name)).collect()),
        );
        out.insert("properties".into(), Value::Object(properties));
        out.insert("additionalProperties".into(), json!(false));
    }
    Value::Object(out)
}

/// `property`, or null.
fn nullable(mut property: Value) -> Value {
    // An enum or const holds null out whatever the type says.
    if property.get("enum").is_some() || property.get("const").is_some() {
        return json!({"anyOf": [property, {"type": "null"}]});
    }
    match property.get_mut("type") {
        Some(Value::String(kind)) => {
            let kind = std::mem::take(kind);
            property["type"] = json!([kind, "null"]);
            property
        }
        Some(Value::Array(kinds)) => {
            if !kinds.contains(&json!("null")) {
                kinds.push(json!("null"));
            }
            property
        }
        _ => json!({"anyOf": [property, {"type": "null"}]}),
    }
}

/// `value`, a reply to [`strict`]'s `schema`, with the nulls it gives the
/// properties `schema` does not require left out, at every depth.
pub fn loosen(value: Value, schema: &Value) -> Value {
    loosen_in(value, schema, schema)
}

/// [`loosen`] within `root`, whose `$defs` a local `$ref` names.
fn loosen_in(value: Value, schema: &Value, root: &Value) -> Value {
    let schema = chosen(&value, schema, root);
    match value {
        Value::Object(fields) => {
            let required: Vec<&str> = (schema["required"].as_array().into_iter().flatten())
                .filter_map(Value::as_str)
                .collect();
            let properties = schema["properties"].as_object();
            Value::Object(
                fields
                    .into_iter()
                    .filter(|(name, value)| {
                        !(value.is_null()
                            && properties.is_some_and(|p| p.contains_key(name))
                            && !required.contains(&name.as_str()))
                    })
                    .map(|(name, value)| {
                        let inner = (properties.and_then(|p| p.get(&name))).unwrap_or(&Value::Null);
                        let value = loosen_in(value, inner, root);
                        (name, value)
                    })
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| loosen_in(item, &schema["items"], root))
                .collect(),
        ),
        other => other,
    }
}

/// The schema `value` answers: `schema` itself or, of a choice, the first
/// branch `value` fits as [`strict`] sent it, followed down.
fn chosen<'a>(value: &Value, schema: &'a Value, root: &'a Value) -> &'a Value {
    let mut schema = resolve(schema, root);
    for _ in 0..16 {
        match (["anyOf", "oneOf"].iter())
            .flat_map(|key| schema[*key].as_array().into_iter().flatten())
            .find(|branch| fits(value, branch, root, 0))
        {
            Some(branch) => schema = resolve(branch, root),
            None => break,
        }
    }
    schema
}

/// Whether `value` fits `schema` as [`strict`] sent it: optional
/// properties may be null. What it does not check, or a schema deeper
/// than `depth` allows, it lets pass.
fn fits(value: &Value, schema: &Value, root: &Value, depth: usize) -> bool {
    if depth > 64 {
        return true;
    }
    let depth = depth + 1;
    let schema = resolve(schema, root);
    let branches: Vec<&Value> = (["anyOf", "oneOf"].iter())
        .flat_map(|key| schema[*key].as_array().into_iter().flatten())
        .collect();
    if !branches.is_empty()
        && !branches
            .iter()
            .any(|branch| fits(value, branch, root, depth))
    {
        return false;
    }
    if schema.get("const").is_some_and(|c| c != value)
        || (schema["enum"].as_array()).is_some_and(|values| !values.contains(value))
    {
        return false;
    }
    let typed = |kind: &str| match kind {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => true,
    };
    let kinds_fit = match &schema["type"] {
        Value::String(kind) => typed(kind),
        Value::Array(kinds) => kinds.iter().filter_map(Value::as_str).any(typed),
        _ => true,
    };
    if !kinds_fit {
        return false;
    }
    match value {
        Value::Object(fields) => {
            let required: Vec<&str> = (schema["required"].as_array().into_iter().flatten())
                .filter_map(Value::as_str)
                .collect();
            if !required.iter().all(|name| fields.contains_key(*name)) {
                return false;
            }
            let Some(properties) = schema["properties"].as_object() else {
                return true;
            };
            fields
                .iter()
                .all(|(name, field)| match properties.get(name) {
                    Some(property) => {
                        (field.is_null() && !required.contains(&name.as_str()))
                            || fits(field, property, root, depth)
                    }
                    None => schema["additionalProperties"] != false,
                })
        }
        Value::Array(items) => {
            let Some(item) = schema.get("items") else {
                return true;
            };
            items.iter().all(|value| fits(value, item, root, depth))
        }
        _ => true,
    }
}

/// `schema`, or what its local `$ref` names in `root`, followed a few
/// times at most.
fn resolve<'a>(mut schema: &'a Value, root: &'a Value) -> &'a Value {
    for _ in 0..16 {
        match schema["$ref"].as_str().and_then(|r| r.strip_prefix('#')) {
            Some(pointer) => match root.pointer(pointer) {
                Some(named) => schema = named,
                None => break,
            },
            None => break,
        }
    }
    schema
}

/// Whether `property` takes null: typed so, or untyped.
fn takes_null(property: &Value) -> bool {
    let any = |key: &str| (property[key].as_array().into_iter().flatten()).any(takes_null);
    match &property["type"] {
        Value::String(kind) => kind == "null",
        Value::Array(kinds) => kinds.contains(&json!("null")),
        _ if property.get("anyOf").is_some() || property.get("oneOf").is_some() => {
            any("anyOf") || any("oneOf")
        }
        _ => {
            property.get("enum").is_none_or(|values| {
                values
                    .as_array()
                    .is_some_and(|values| values.contains(&Value::Null))
            }) && property.get("const").is_none_or(Value::is_null)
        }
    }
}

/// The object a worker's reply to `schema` gives, or why there is none.
pub fn parse(text: &str, schema: &Value) -> Result<Value, String> {
    let text = text.trim();
    // A model that fences its JSON in spite of the format.
    let text = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .and_then(|rest| rest.strip_suffix("```"))
        .unwrap_or(text)
        .trim();
    let value: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    if !value.is_object() {
        return Err("not a JSON object".into());
    }
    let value = loosen(value, schema);
    let missing: Vec<&str> = (schema["required"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .filter(|name| match value.get(name) {
            None => true,
            Some(Value::Null) => !takes_null(&schema["properties"][name]),
            Some(_) => false,
        })
        .collect();
    match missing.is_empty() {
        true => Ok(value),
        false => Err(format!("missing {}", missing.join(", "))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_referenced_or_chosen_object_loses_its_optional_nulls_too() {
        let schema = json!({"type": "object",
            "$defs": {"r": {"type": "object", "properties": {"note": {"type": "string"},
                "n": {"type": "integer"}}, "required": ["n"]}},
            "properties": {
                "result": {"$ref": "#/$defs/r"},
                "either": {"anyOf": [{"type": "string"}, {"$ref": "#/$defs/r"}]}},
            "required": ["result", "either"]});
        let reply = json!({"result": {"note": null, "n": 1}, "either": {"note": null, "n": 2}});
        assert_eq!(
            loosen(reply, &schema),
            json!({"result": {"n": 1}, "either": {"n": 2}})
        );
    }

    #[test]
    fn a_choice_is_made_by_its_consts_as_well_as_its_names() {
        let schema = json!({"type": "object", "properties": {"x": {"anyOf": [
            {"type": "object", "properties": {"kind": {"const": "a"}, "value": {"type": "string"}},
                "required": ["kind"]},
            {"type": "object", "properties": {"kind": {"const": "b"},
                "value": {"type": ["string", "null"]}}, "required": ["kind", "value"]}]}},
            "required": ["x"]});
        assert_eq!(
            loosen(json!({"x": {"kind": "b", "value": null}}), &schema),
            json!({"x": {"kind": "b", "value": null}})
        );
        assert_eq!(
            loosen(json!({"x": {"kind": "a", "value": null}}), &schema),
            json!({"x": {"kind": "a"}})
        );
        // An array in a choice is loosened by the branch that takes it.
        let schema = json!({"type": "object", "properties": {"results": {"anyOf": [
            {"type": "array", "items": {"type": "object",
                "properties": {"note": {"type": "string"}}}},
            {"type": "string"}]}}, "required": ["results"]});
        assert_eq!(
            loosen(json!({"results": [{"note": null}]}), &schema),
            json!({"results": [{}]})
        );
        // Of two array branches, the one whose items the reply fits.
        let item = |kind: &str, required: Value| {
            json!({"type": "object", "properties": {
            "kind": {"const": kind}, "note": {"type": ["string", "null"]}}, "required": required})
        };
        let schema = json!({"type": "object", "properties": {"results": {"anyOf": [
            {"type": "array", "items": item("a", json!(["kind"]))},
            {"type": "array", "items": item("b", json!(["kind", "note"]))}]}},
            "required": ["results"]});
        assert_eq!(
            loosen(json!({"results": [{"kind": "b", "note": null}]}), &schema),
            json!({"results": [{"kind": "b", "note": null}]})
        );
        assert_eq!(
            loosen(json!({"results": [{"kind": "a", "note": null}]}), &schema),
            json!({"results": [{"kind": "a"}]})
        );
        // An optional enum's null placeholder does not rule its branch out.
        let schema = json!({"type": "object", "properties": {"x": {"oneOf": [
            {"type": "object", "properties": {"kind": {"const": "a"},
                "level": {"enum": ["low", "high"]}}, "required": ["kind"]},
            {"type": "object", "properties": {"kind": {"const": "b"}}, "required": ["kind"]}]}},
            "required": ["x"]});
        assert_eq!(
            loosen(json!({"x": {"kind": "a", "level": null}}), &schema),
            json!({"x": {"kind": "a"}})
        );
    }

    #[test]
    fn optional_properties_go_as_nullable_and_come_back_left_out() {
        let schema = json!({"type": "object", "properties": {
            "findings": {"type": "array", "items": {"type": "object",
                "properties": {"file": {"type": "string"}, "line": {"type": "integer"}},
                "required": ["file"]}},
            "note": {"type": "string"}},
            "required": ["findings"]});
        check(&schema).unwrap();
        let strict = strict(&schema);
        assert_eq!(strict["additionalProperties"], false);
        assert_eq!(strict["required"], json!(["findings", "note"]));
        assert_eq!(
            strict["properties"]["note"]["type"],
            json!(["string", "null"])
        );
        let item = &strict["properties"]["findings"]["items"];
        assert_eq!(item["required"], json!(["file", "line"]));
        assert_eq!(
            item["properties"]["line"]["type"],
            json!(["integer", "null"])
        );
        assert_eq!(item["additionalProperties"], false);
        let reply = r#"{"findings": [{"file": "a.rs", "line": null}], "note": null}"#;
        assert_eq!(
            parse(reply, &schema).unwrap(),
            json!({"findings": [{"file": "a.rs"}]})
        );
        assert_eq!(
            parse("```json\n{\"findings\": []}\n```", &schema).unwrap(),
            json!({"findings": []})
        );
        assert!(parse("{}", &schema).unwrap_err().contains("findings"));
        // A required property that may be null is there when null.
        let nullable = json!({"type": "object", "properties": {
            "answer": {"type": ["string", "null"]}, "why": {"anyOf": [{"type": "string"}, {"type": "null"}]},
            "plain": {"type": "string"}}, "required": ["answer", "why", "plain"]});
        assert_eq!(
            parse(r#"{"answer": null, "why": null, "plain": "x"}"#, &nullable).unwrap(),
            json!({"answer": null, "why": null, "plain": "x"})
        );
        assert!(
            parse(r#"{"answer": null, "why": null, "plain": null}"#, &nullable)
                .unwrap_err()
                .contains("plain")
        );
        assert!(parse("nope", &schema).is_err());
        assert!(parse("[1]", &schema).is_err());
    }

    #[test]
    fn a_schema_is_an_object_one_whose_required_it_has() {
        assert!(check(&json!({"type": "object", "properties": {}})).is_ok());
        assert!(check(&json!({"type": "array", "items": {}})).is_err());
        assert!(check(&json!({"type": "object"})).is_err());
        assert!(
            check(&json!({"type": "object", "properties": {"a": {}}, "required": ["b"]})).is_err()
        );
        // An optional enum may be null, as a choice beside its values.
        let strict_enum = strict(&json!({"type": "object", "properties": {
            "level": {"type": "string", "enum": ["a", "b"]}}}));
        assert_eq!(
            strict_enum["properties"]["level"],
            json!({"anyOf": [{"type": "string", "enum": ["a", "b"]}, {"type": "null"}]})
        );
        // An untyped property may be null as anything else.
        let strict = strict(&json!({"type": "object", "properties": {"a": {"enum": [1, 2]}}}));
        assert_eq!(
            strict["properties"]["a"],
            json!({"anyOf": [{"enum": [1, 2]}, {"type": "null"}]})
        );
    }
}
