//! A call's arguments checked against the JSON Schema its tool declared,
//! before anything else decides the call, as a function call with bad
//! arguments fails at once. The check knows a subset of JSON Schema: `type`,
//! `enum`, `const`, `required`, `properties`, `additionalProperties`,
//! `minProperties`/`maxProperties`, `minLength`/`maxLength` (in characters),
//! `minimum`/`maximum`, `exclusiveMinimum`/`exclusiveMaximum`,
//! `minItems`/`maxItems`, `items`, `prefixItems`, `allOf`, `anyOf`, `oneOf`
//! and a local `$ref`. A keyword it does not know passes: it refuses only
//! what it can judge, so a schema it reads in part never refuses a call
//! that fits the whole. `oneOf` is held only to "at least one", since a
//! branch passed here may not pass in full.
use serde_json::{Map, Value};

/// How deep the check follows a schema into a value, and how many local
/// references it follows in one call: past either, what is left passes.
const MAX_DEPTH: usize = 64;
const MAX_REFS: usize = 256;

/// Why `value` does not fit `schema`: where, as a path into the arguments
/// (`title`, `items[2].name`), and what rule.
#[derive(Debug, PartialEq, Eq)]
pub struct Misfit {
    path: String,
    rule: String,
}

impl std::fmt::Display for Misfit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.path.is_empty() {
            true => f.write_str(&self.rule),
            false => write!(f, "{}: {}", self.path, self.rule),
        }
    }
}

/// Check `value` against `schema`.
pub fn check(schema: &Value, value: &Value) -> Result<(), Misfit> {
    let mut checker = Checker {
        root: schema,
        refs: 0,
    };
    checker.fits(schema, value, 0).map_err(|mut misfit| {
        misfit.path = misfit.path.trim_start_matches('.').to_owned();
        misfit
    })
}

struct Checker<'a> {
    root: &'a Value,
    refs: usize,
}

fn misfit(rule: impl Into<String>) -> Misfit {
    Misfit {
        path: String::new(),
        rule: rule.into(),
    }
}

/// A misfit found inside `segment` of the value: `.name` or `[2]`.
fn inside(segment: String) -> impl FnOnce(Misfit) -> Misfit {
    move |mut misfit| {
        misfit.path.insert_str(0, &segment);
        misfit
    }
}

fn field(name: &str) -> String {
    format!(".{name}")
}

impl<'a> Checker<'a> {
    fn fits(&mut self, schema: &'a Value, value: &Value, depth: usize) -> Result<(), Misfit> {
        let schema = match schema {
            Value::Bool(false) => return Err(misfit("not allowed")),
            Value::Object(schema) => schema,
            _ => return Ok(()),
        };
        if depth > MAX_DEPTH {
            return Ok(());
        }
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            // Only a reference into this schema is followed; another passes.
            if let Some(target) = reference
                .strip_prefix('#')
                .filter(|_| self.refs < MAX_REFS)
                .and_then(|pointer| self.root.pointer(pointer))
            {
                self.refs += 1;
                self.fits(target, value, depth + 1)?;
            }
        }
        if let Some(types) = schema.get("type") {
            typed(types, value)?;
        }
        if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
            && !allowed.iter().any(|one| same(one, value))
        {
            return Err(misfit(format!("must be one of {}", listed(allowed))));
        }
        if let Some(only) = schema.get("const")
            && !same(only, value)
        {
            return Err(misfit(format!("must be {only}")));
        }
        match value {
            Value::String(text) => strings(schema, text)?,
            Value::Number(_) => numbers(schema, value)?,
            Value::Array(items) => self.arrays(schema, items, depth)?,
            Value::Object(fields) => self.objects(schema, fields, depth)?,
            _ => {}
        }
        for every in schema
            .get("allOf")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            self.fits(every, value, depth + 1)?;
        }
        for (keyword, branches) in [
            ("anyOf", schema.get("anyOf")),
            ("oneOf", schema.get("oneOf")),
        ] {
            let Some(branches) = branches.and_then(Value::as_array) else {
                continue;
            };
            if !branches.is_empty()
                && !branches
                    .iter()
                    .any(|branch| self.fits(branch, value, depth + 1).is_ok())
            {
                return Err(misfit(format!("fits none of the schemas {keyword} lists")));
            }
        }
        Ok(())
    }

    fn arrays(
        &mut self,
        schema: &'a Map<String, Value>,
        items: &[Value],
        depth: usize,
    ) -> Result<(), Misfit> {
        let count = items.len() as u64;
        if let Some(least) = schema.get("minItems").and_then(Value::as_u64)
            && count < least
        {
            return Err(misfit(format!(
                "at least {least} {}, got {count}",
                plural(least, "item")
            )));
        }
        if let Some(most) = schema.get("maxItems").and_then(Value::as_u64)
            && count > most
        {
            return Err(misfit(format!(
                "at most {most} {}, got {count}",
                plural(most, "item")
            )));
        }
        // `prefixItems` (or the older array form of `items`) judges items
        // by place; `items` beside `prefixItems` judges the rest.
        let (prefix, rest) = match (schema.get("prefixItems"), schema.get("items")) {
            (Some(Value::Array(prefix)), rest) => (prefix.as_slice(), rest),
            (_, Some(Value::Array(prefix))) => (prefix.as_slice(), None),
            (_, rest) => (&[][..], rest),
        };
        for (at, item) in items.iter().enumerate() {
            let schema = match prefix.get(at) {
                Some(schema) => schema,
                None => match rest {
                    Some(schema) => schema,
                    None => break,
                },
            };
            self.fits(schema, item, depth + 1)
                .map_err(inside(format!("[{at}]")))?;
        }
        Ok(())
    }

    fn objects(
        &mut self,
        schema: &'a Map<String, Value>,
        fields: &Map<String, Value>,
        depth: usize,
    ) -> Result<(), Misfit> {
        for required in schema
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !fields.contains_key(required) {
                return Err(misfit(format!("{required} is required")));
            }
        }
        let count = fields.len() as u64;
        if let Some(least) = schema.get("minProperties").and_then(Value::as_u64)
            && count < least
        {
            return Err(misfit(format!(
                "at least {least} {}, got {count}",
                plural(least, "field")
            )));
        }
        if let Some(most) = schema.get("maxProperties").and_then(Value::as_u64)
            && count > most
        {
            return Err(misfit(format!(
                "at most {most} {}, got {count}",
                plural(most, "field")
            )));
        }
        let properties = schema.get("properties").and_then(Value::as_object);
        // Fields `patternProperties` may name are judged by nothing here:
        // with it, `additionalProperties` is not held either.
        let additional = schema
            .get("additionalProperties")
            .filter(|_| !schema.contains_key("patternProperties"));
        for (name, value) in fields {
            let schema = match properties.and_then(|properties| properties.get(name)) {
                Some(property) => property,
                None => match additional {
                    Some(Value::Bool(false)) => return Err(misfit(format!("no field {name}"))),
                    Some(additional) => additional,
                    None => continue,
                },
            };
            self.fits(schema, value, depth + 1)
                .map_err(inside(field(name)))?;
        }
        Ok(())
    }
}

/// Whether `value` is of one of the types `types` names (a name or a list
/// of them). A name the check does not know passes.
fn typed(types: &Value, value: &Value) -> Result<(), Misfit> {
    let names: Vec<&str> = match types {
        Value::String(name) => vec![name],
        Value::Array(names) => names.iter().filter_map(Value::as_str).collect(),
        _ => return Ok(()),
    };
    let is = |name: &str| match name {
        "string" => Some(value.is_string()),
        "integer" => Some(integer(value)),
        "number" => Some(value.is_number()),
        "boolean" => Some(value.is_boolean()),
        "array" => Some(value.is_array()),
        "object" => Some(value.is_object()),
        "null" => Some(value.is_null()),
        _ => None,
    };
    if names.is_empty() || names.iter().any(|name| is(name) != Some(false)) {
        return Ok(());
    }
    let named: Vec<String> = names.iter().map(|name| article(name)).collect();
    Err(misfit(format!("must be {}", or_list(&named))))
}

/// A whole number, as JSON Schema counts one: `2.0` is.
fn integer(value: &Value) -> bool {
    value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|n| n.fract() == 0.0)
}

fn strings(schema: &Map<String, Value>, text: &str) -> Result<(), Misfit> {
    let least = schema.get("minLength").and_then(Value::as_u64);
    let most = schema.get("maxLength").and_then(Value::as_u64);
    if least.is_none() && most.is_none() {
        return Ok(());
    }
    // Characters, as JSON Schema counts a string's length.
    let length = text.chars().count() as u64;
    if let Some(least) = least
        && length < least
    {
        return Err(misfit(format!(
            "at least {least} {}, got {length}",
            plural(least, "character")
        )));
    }
    if let Some(most) = most
        && length > most
    {
        return Err(misfit(format!(
            "at most {most} {}, got {length}",
            plural(most, "character")
        )));
    }
    Ok(())
}

fn numbers(schema: &Map<String, Value>, value: &Value) -> Result<(), Misfit> {
    let Some(number) = value.as_f64() else {
        return Ok(());
    };
    let bound = |key: &str| schema.get(key).and_then(Value::as_f64);
    // Draft 4 says an exclusive bound with `true` beside `minimum`.
    let exclusive = |key: &str| schema.get(key) == Some(&Value::Bool(true));
    let shown = |key: &str| schema[key].to_string();
    if let Some(least) = bound("minimum") {
        if exclusive("exclusiveMinimum") && number <= least {
            return Err(misfit(format!(
                "more than {}, got {value}",
                shown("minimum")
            )));
        }
        if number < least {
            return Err(misfit(format!(
                "at least {}, got {value}",
                shown("minimum")
            )));
        }
    }
    if let Some(most) = bound("maximum") {
        if exclusive("exclusiveMaximum") && number >= most {
            return Err(misfit(format!(
                "less than {}, got {value}",
                shown("maximum")
            )));
        }
        if number > most {
            return Err(misfit(format!("at most {}, got {value}", shown("maximum"))));
        }
    }
    if let Some(least) = bound("exclusiveMinimum")
        && number <= least
    {
        return Err(misfit(format!(
            "more than {}, got {value}",
            shown("exclusiveMinimum")
        )));
    }
    if let Some(most) = bound("exclusiveMaximum")
        && number >= most
    {
        return Err(misfit(format!(
            "less than {}, got {value}",
            shown("exclusiveMaximum")
        )));
    }
    Ok(())
}

/// Equal as JSON Schema compares: numbers by value, so `1` is `1.0`.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_i64(), y.as_i64()) {
            (Some(x), Some(y)) => x == y,
            _ => match (x.as_u64(), y.as_u64()) {
                (Some(x), Some(y)) => x == y,
                _ => x.as_f64() == y.as_f64(),
            },
        },
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        _ => a == b,
    }
}

/// The values an `enum` allows, the first few of a long list.
fn listed(allowed: &[Value]) -> String {
    const SHOWN: usize = 8;
    let mut named: Vec<String> = allowed.iter().take(SHOWN).map(Value::to_string).collect();
    if allowed.len() > SHOWN {
        named.push(format!("{} more", allowed.len() - SHOWN));
    }
    or_list(&named)
}

fn article(name: &str) -> String {
    match name {
        "null" => "null".into(),
        "array" | "integer" | "object" => format!("an {name}"),
        _ => format!("a {name}"),
    }
}

fn plural(count: u64, word: &str) -> String {
    match count {
        1 => word.to_owned(),
        _ => format!("{word}s"),
    }
}

/// `a`, `a or b`, `a, b or c`.
fn or_list(names: &[String]) -> String {
    match names.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn said(schema: Value, value: Value) -> String {
        check(&schema, &value).unwrap_err().to_string()
    }

    #[test]
    fn a_string_longer_than_its_most_is_named_with_its_rule() {
        let schema =
            json!({"type": "object", "properties": {"title": {"type": "string", "maxLength": 30}}});
        assert!(check(&schema, &json!({"title": "x".repeat(30)})).is_ok());
        assert_eq!(
            said(schema.clone(), json!({"title": "x".repeat(31)})),
            "title: at most 30 characters, got 31"
        );
        // Characters, not bytes.
        assert!(check(&schema, &json!({"title": "é".repeat(30)})).is_ok());
        assert_eq!(
            said(json!({"minLength": 2}), json!("a")),
            "at least 2 characters, got 1"
        );
    }

    #[test]
    fn types_required_and_closed_objects() {
        let schema = json!({"type": "object", "required": ["name"],
            "properties": {"name": {"type": "string"}, "count": {"type": "integer"},
                "tags": {"type": ["array", "null"]}},
            "additionalProperties": false});
        assert!(check(&schema, &json!({"name": "a", "count": 2.0, "tags": null})).is_ok());
        assert_eq!(said(schema.clone(), json!({})), "name is required");
        assert_eq!(
            said(schema.clone(), json!({"name": 1})),
            "name: must be a string"
        );
        assert_eq!(
            said(schema.clone(), json!({"name": "a", "count": 1.5})),
            "count: must be an integer"
        );
        assert_eq!(
            said(schema.clone(), json!({"name": "a", "tags": "x"})),
            "tags: must be an array or null"
        );
        assert_eq!(
            said(schema.clone(), json!({"name": "a", "other": 1})),
            "no field other"
        );
        assert_eq!(said(schema, json!([])), "must be an object");
    }

    #[test]
    fn nested_misfits_name_their_path() {
        let schema = json!({"type": "object", "properties": {"items": {"type": "array",
            "maxItems": 3, "items": {"type": "object", "required": ["id"],
                "properties": {"id": {"type": "integer", "minimum": 1}}}}}});
        assert_eq!(
            said(schema.clone(), json!({"items": [{"id": 1}, {"id": 0}]})),
            "items[1].id: at least 1, got 0"
        );
        assert_eq!(
            said(schema.clone(), json!({"items": [{"id": 1}, {}]})),
            "items[1]: id is required"
        );
        assert_eq!(
            said(
                schema,
                json!({"items": [{"id": 1}, {"id": 1}, {"id": 1}, {"id": 1}]})
            ),
            "items: at most 3 items, got 4"
        );
    }

    #[test]
    fn enums_consts_and_ranges() {
        assert_eq!(
            said(json!({"enum": ["low", "high"]}), json!("mid")),
            "must be one of \"low\" or \"high\""
        );
        assert!(check(&json!({"enum": [1, 2]}), &json!(2.0)).is_ok());
        assert_eq!(said(json!({"const": true}), json!(false)), "must be true");
        assert_eq!(
            said(json!({"maximum": 10}), json!(11)),
            "at most 10, got 11"
        );
        assert_eq!(
            said(json!({"exclusiveMinimum": 0}), json!(0)),
            "more than 0, got 0"
        );
        assert_eq!(
            said(json!({"minimum": 0, "exclusiveMinimum": true}), json!(0)),
            "more than 0, got 0"
        );
    }

    #[test]
    fn what_it_cannot_judge_passes() {
        // Keywords it does not know, types it does not know, a reference
        // outside the schema, and `not`, whose branch may pass here only.
        let schema = json!({"type": "object", "properties": {
            "a": {"type": "string", "pattern": "^[0-9]+$", "format": "email"},
            "b": {"type": "decimal"},
            "c": {"$ref": "https://example.com/schema"},
            "d": {"not": {"type": "string"}},
            "e": {"if": {"type": "string"}, "then": {"maxLength": 1}}}});
        assert!(
            check(
                &schema,
                &json!({"a": "letters", "b": "x", "c": 1, "d": "s", "e": "long"})
            )
            .is_ok()
        );
        // `patternProperties` lifts `additionalProperties`.
        let open = json!({"properties": {}, "patternProperties": {"^x": {}},
            "additionalProperties": false});
        assert!(check(&open, &json!({"y": 1})).is_ok());
        assert!(check(&json!(true), &json!(1)).is_ok());
        assert!(check(&json!({}), &json!(1)).is_ok());
    }

    #[test]
    fn combinators_and_local_references() {
        let schema = json!({"$defs": {"id": {"type": "string", "maxLength": 4}},
            "type": "object", "properties": {
                "id": {"$ref": "#/$defs/id"},
                "either": {"anyOf": [{"type": "string"}, {"type": "integer"}]},
                "both": {"allOf": [{"minimum": 1}, {"maximum": 3}]},
                "one": {"oneOf": [{"type": "string"}, {"maxLength": 2}]},
                "never": false}});
        assert!(
            check(
                &schema,
                &json!({"id": "abcd", "either": 3, "both": 2, "one": "abc"})
            )
            .is_ok()
        );
        assert_eq!(
            said(schema.clone(), json!({"id": "abcde"})),
            "id: at most 4 characters, got 5"
        );
        assert_eq!(
            said(schema.clone(), json!({"either": true})),
            "either: fits none of the schemas anyOf lists"
        );
        assert_eq!(
            said(schema.clone(), json!({"both": 4})),
            "both: at most 3, got 4"
        );
        assert_eq!(said(schema, json!({"never": 1})), "never: not allowed");
        // A reference to itself ends.
        let looped = json!({"properties": {"next": {"$ref": "#"}}});
        let mut deep = json!({});
        for _ in 0..300 {
            deep = json!({"next": deep});
        }
        assert!(check(&looped, &deep).is_ok());
    }

    #[test]
    fn items_by_place() {
        let schema = json!({"prefixItems": [{"type": "string"}], "items": {"type": "integer"}});
        assert!(check(&schema, &json!(["a", 1, 2])).is_ok());
        assert_eq!(
            said(schema, json!(["a", 1, "b"])),
            "[2]: must be an integer"
        );
        let older = json!({"items": [{"type": "string"}]});
        assert!(check(&older, &json!(["a", 1])).is_ok());
    }
}
