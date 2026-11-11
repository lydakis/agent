//! An `agent()` call's `schema`, as the worker's turn takes it: the daemon
//! hands it to the provider as strict structured output (`output_schema`,
//! as Codex sends a turn's), which takes only objects that list every
//! property as required and allow no others. So an optional property goes
//! as one that may be null, and a null the reply gives one is left out of
//! the object `agent()` returns, which then has the schema's own shape.
use serde_json::{Map, Value, json};

/// What strict structured output does not take, at any depth.
const UNTAKEN: [&str; 13] = [
    "allOf",
    "not",
    "if",
    "then",
    "else",
    "dependentRequired",
    "dependentSchemas",
    "patternProperties",
    "prefixItems",
    "unevaluatedProperties",
    "unevaluatedItems",
    "contains",
    "propertyNames",
];

/// Whether `schema` is one `agent()` takes: an object schema with
/// `properties` at its root, `required` naming only those, and nothing
/// strict structured output does not take.
pub fn check(schema: &Value) -> Result<(), String> {
    const NEEDS: &str = "a schema needs {type: 'object', properties: {...}} at its root, with required a subset of properties";
    let properties = schema["properties"].as_object();
    if schema["type"] != "object" || properties.is_none() {
        return Err(NEEDS.into());
    }
    if let Some(key) = untaken(schema, 0) {
        return Err(format!(
            "a schema's {key} is not one strict structured output takes; say it with anyOf, \
             properties and required instead"
        ));
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

/// The first keyword of [`UNTAKEN`] in `schema` or a schema within it.
fn untaken(schema: &Value, depth: usize) -> Option<&'static str> {
    let fields = schema.as_object().filter(|_| depth < 64)?;
    if let Some(key) = UNTAKEN.iter().find(|key| fields.contains_key(**key)) {
        return Some(key);
    }
    let within = |key: &str| -> Vec<&Value> {
        match fields.get(key) {
            Some(Value::Array(each)) => each.iter().collect(),
            Some(Value::Object(each)) if matches!(key, "properties" | "$defs" | "definitions") => {
                each.values().collect()
            }
            Some(one) => vec![one],
            None => Vec::new(),
        }
    };
    ([
        "properties",
        "$defs",
        "definitions",
        "items",
        "anyOf",
        "oneOf",
        "additionalProperties",
    ]
    .iter())
    .flat_map(|key| within(key))
    .find_map(|inner| untaken(inner, depth + 1))
}

/// `schema` as strict structured output takes it.
pub fn strict(schema: &Value) -> Value {
    let Value::Object(fields) = schema else {
        return schema.clone();
    };
    let mut out = Map::new();
    for (key, value) in fields {
        let value = match key.as_str() {
            "items" => strict(value),
            "anyOf" | "oneOf" => match value {
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

/// `property`, or null: the whole of it, so no enum, const or union
/// within it holds null out.
fn nullable(property: Value) -> Value {
    json!({"anyOf": [property, {"type": "null"}]})
}

/// `value`, a reply to [`strict`]'s `schema`, with the nulls it gives the
/// properties `schema` does not require left out, at every depth.
pub fn loosen(value: Value, schema: &Value) -> Value {
    loosen_in(value, schema, schema, 0)
}

/// [`loosen`] within `root`, whose `$defs` a local `$ref` names. Of a
/// choice, by the first branch whose loosening the schema then holds valid:
/// one coherent alternative, never requirements mixed from several.
fn loosen_in(value: Value, schema: &Value, root: &Value, depth: usize) -> Value {
    let schema = resolve(schema, root);
    let branches: Vec<&Value> = (["anyOf", "oneOf"].iter())
        .flat_map(|key| schema[*key].as_array().into_iter().flatten())
        .collect();
    if depth < 16 && !branches.is_empty() && (value.is_object() || value.is_array()) {
        for branch in branches {
            let alternative = with(schema, resolve(branch, root));
            let loosened = loosen_in(value.clone(), &alternative, root, depth + 1);
            if valid(&loosened, schema, root) {
                return loosened;
            }
        }
    }
    match value {
        Value::Object(fields) => {
            let required: Vec<&str> = (schema["required"].as_array().into_iter().flatten())
                .filter_map(Value::as_str)
                .collect();
            let properties = schema["properties"].as_object();
            Value::Object(
                fields
                    .into_iter()
                    .filter_map(|(name, value)| {
                        let property = properties.and_then(|p| p.get(&name));
                        if value.is_null()
                            && property.is_some()
                            && !required.contains(&name.as_str())
                        {
                            return None;
                        }
                        let inner = property.unwrap_or(&Value::Null);
                        let value = loosen_in(value, inner, root, depth + 1);
                        Some((name, value))
                    })
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| loosen_in(item, &schema["items"], root, depth + 1))
                .collect(),
        ),
        other => other,
    }
}

/// `branch` with what `schema` around it says as well: its properties,
/// what it requires and its items, which hold whichever branch is taken.
fn with(schema: &Value, branch: &Value) -> Value {
    let mut alternative = branch.clone();
    let Some(out) = alternative.as_object_mut() else {
        return alternative;
    };
    if let Some(own) = schema["properties"].as_object() {
        let properties = out.entry("properties").or_insert_with(|| json!({}));
        if let Some(properties) = properties.as_object_mut() {
            for (name, property) in own {
                properties
                    .entry(name.clone())
                    .or_insert_with(|| property.clone());
            }
        }
    }
    if let Some(own) = schema["required"].as_array() {
        let required = out.entry("required").or_insert_with(|| json!([]));
        if let Some(required) = required.as_array_mut() {
            required.extend(
                own.iter()
                    .filter(|name| !required.contains(name))
                    .cloned()
                    .collect::<Vec<_>>(),
            );
        }
    }
    if let Some(items) = schema.get("items") {
        out.entry("items").or_insert_with(|| items.clone());
    }
    alternative
}

/// `schema` as a document of its own: with `root`'s definitions, which
/// its local `$ref`s name.
fn document(schema: &Value, root: &Value) -> Value {
    let mut document = schema.clone();
    if let Some(out) = document.as_object_mut() {
        for key in ["$defs", "definitions"] {
            if let Some(defs) = root.get(key) {
                out.entry(key).or_insert_with(|| defs.clone());
            }
        }
    }
    document
}

/// Whether `value` answers `schema` within `root`. A schema the validator
/// cannot read holds nothing out.
fn valid(value: &Value, schema: &Value, root: &Value) -> bool {
    jsonschema::validator_for(&document(schema, root))
        .map_or(true, |validator| validator.is_valid(value))
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

/// The object a worker's reply to `schema` gives, or why there is none:
/// checked whole against `schema`, as Claude Code checks a structured
/// reply.
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
    let Ok(validator) = jsonschema::validator_for(schema) else {
        return Ok(value);
    };
    let wrong: Vec<String> = (validator.iter_errors(&value).take(3))
        .map(|error| match error.instance_path().as_str() {
            "" => error.to_string(),
            at => format!("{error} at {at}"),
        })
        .collect();
    match wrong.is_empty() {
        true => Ok(value),
        false => Err(wrong.join("; ")),
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
    fn a_null_some_branch_it_may_fit_requires_is_kept() {
        // Told apart only by bounds, which no branch choice checks.
        let schema = json!({"type": "object", "properties": {"x": {"anyOf": [
            {"type": "object", "properties": {"n": {"type": "integer", "minimum": 0},
                "note": {"type": "string"}}, "required": ["n"]},
            {"type": "object", "properties": {"n": {"type": "integer", "maximum": -1},
                "note": {"type": ["string", "null"]}}, "required": ["n", "note"]}]}},
            "required": ["x"]});
        assert_eq!(
            loosen(json!({"x": {"n": -1, "note": null}}), &schema),
            json!({"x": {"n": -1, "note": null}})
        );
        // A whole number written with a point is still an integer.
        let schema = json!({"type": "object", "properties": {"x": {"anyOf": [
            {"type": "object", "properties": {"n": {"type": "integer"},
                "note": {"type": "string"}}, "required": ["n"]},
            {"type": "string"}]}}, "required": ["x"]});
        assert_eq!(
            loosen(json!({"x": {"n": 1.0, "note": null}}), &schema),
            json!({"x": {"n": 1.0}})
        );
    }

    #[test]
    fn a_choice_loosens_by_one_alternative_and_what_is_left_is_checked_whole() {
        // Two overlapping branches: the reply fits both as sent, and goes
        // back as the first, never with requirements mixed from both.
        let schema = json!({"type": "object", "properties": {"v": {"anyOf": [
            {"type": "object", "properties": {"x": {"type": "string"},
                "y": {"type": ["string", "null"]}}, "required": ["y"]},
            {"type": "object", "properties": {"x": {"type": ["string", "null"]},
                "y": {"type": "string"}}, "required": ["x"]}]}},
            "required": ["v"]});
        assert_eq!(
            loosen(json!({"v": {"x": null, "y": null}}), &schema),
            json!({"v": {"y": null}})
        );
        // A reply that holds none of the schema's bounds is no answer.
        let schema = json!({"type": "object", "properties": {
            "n": {"type": "integer", "minimum": 0}}, "required": ["n"]});
        let wrong = parse(r#"{"n": -1}"#, &schema).unwrap_err();
        assert!(wrong.contains("/n"), "{wrong}");
    }

    #[test]
    fn what_strict_output_does_not_take_is_refused_at_any_depth() {
        let schema = json!({"type": "object", "properties": {"v": {"allOf": [
            {"type": "object", "properties": {"note": {"type": "string"}}}]}}});
        assert!(check(&schema).unwrap_err().contains("allOf"));
        let schema = json!({"type": "object", "properties": {"v": {"type": "array",
            "prefixItems": [{"type": "string"}]}}});
        assert!(check(&schema).unwrap_err().contains("prefixItems"));
        // A property may still be called what a keyword is.
        let schema = json!({"type": "object", "properties": {"not": {"type": "string"}}});
        assert!(check(&schema).is_ok());
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
            strict["properties"]["note"],
            json!({"anyOf": [{"type": "string"}, {"type": "null"}]})
        );
        let item = &strict["properties"]["findings"]["items"];
        assert_eq!(item["required"], json!(["file", "line"]));
        assert_eq!(
            item["properties"]["line"],
            json!({"anyOf": [{"type": "integer"}, {"type": "null"}]})
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
        // So may a typed union whose every branch holds null out.
        let union = json!({"type": "string", "anyOf": [{"enum": ["a"]}, {"enum": ["b"]}]});
        let strict_union =
            strict(&json!({"type": "object", "properties": {"pick": union.clone()}}));
        assert_eq!(
            strict_union["properties"]["pick"],
            json!({"anyOf": [strict(&union), {"type": "null"}]})
        );
        // An untyped property may be null as anything else.
        let strict = strict(&json!({"type": "object", "properties": {"a": {"enum": [1, 2]}}}));
        assert_eq!(
            strict["properties"]["a"],
            json!({"anyOf": [{"enum": [1, 2]}, {"type": "null"}]})
        );
    }
}
