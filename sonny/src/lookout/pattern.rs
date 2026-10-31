//! Filters: which of a source's events a subscription hears, written as
//! an event pattern the way AWS EventBridge and SNS filter policies write
//! them. A pattern is an object of the event's fields; each names the
//! values it takes in a list, or holds a pattern of its own for a field
//! that is an object. Every field must match (and), any value in a list
//! will do (or), and a field that is a list matches when any item does.
//! A value is a string, number, boolean or null to equal, or one of:
//!
//! - `{"prefix": "src/"}`, `{"suffix": ".rs"}`
//! - `{"wildcard": "src/*/mod.rs"}`, `*` matching any run of characters
//! - `{"equals-ignore-case": "Ana"}`
//! - `{"anything-but": "bot"}`, or a list of values, or `{"prefix": ...}`,
//!   `{"suffix": ...}`, `{"wildcard": ...}`, `{"equals-ignore-case": ...}`
//! - `{"numeric": [">", 0, "<=", 5]}`
//! - `{"exists": true}` or `{"exists": false}`
//!
//! `"$or": [pattern, ...]` beside the fields matches when any of those
//! patterns does. Nothing in a pattern runs: it is data, matched against
//! data, held to a size, and it only ever narrows what is heard.
use serde_json::Value;

/// The most a pattern may say, as compact JSON.
pub const PATTERN_BYTES: usize = 4 * 1024;
/// How deep fields and `$or` may nest.
const DEPTH: usize = 6;
/// The most values one pattern may list, all told.
const VALUES: usize = 256;

const MATCHERS: &str = "a string, number, boolean or null, or {\"prefix\": S}, {\"suffix\": S}, \
    {\"wildcard\": S}, {\"equals-ignore-case\": S}, {\"anything-but\": ...}, \
    {\"numeric\": [OP, N, ...]} or {\"exists\": true|false}";

/// Hold a pattern to what one may say: the reason when it may not.
pub fn check(pattern: &Value) -> Result<(), String> {
    let compact = pattern.to_string();
    if compact.len() > PATTERN_BYTES {
        return Err(format!(
            "a filter is at most {PATTERN_BYTES} bytes, not {}",
            compact.len()
        ));
    }
    let mut values = 0;
    object(pattern, "filter", 0, &mut values)?;
    if values == 0 {
        return Err("a filter names at least one field and the values it takes".into());
    }
    Ok(())
}

fn object(pattern: &Value, at: &str, depth: usize, values: &mut usize) -> Result<(), String> {
    if depth > DEPTH {
        return Err(format!("{at}: a filter nests at most {DEPTH} deep"));
    }
    let Some(fields) = pattern.as_object() else {
        return Err(format!(
            "{at}: a pattern is an object of fields, each a list of values or a pattern"
        ));
    };
    if fields.is_empty() {
        return Err(format!("{at}: a pattern names at least one field"));
    }
    for (field, wanted) in fields {
        let here = format!("{at}.{field}");
        if field == "$or" {
            let Some(any) = wanted.as_array().filter(|any| any.len() >= 2) else {
                return Err(format!("{here}: a list of two or more patterns"));
            };
            for pattern in any {
                object(pattern, &here, depth + 1, values)?;
            }
            continue;
        }
        match wanted {
            Value::Object(_) => object(wanted, &here, depth + 1, values)?,
            Value::Array(list) if !list.is_empty() => {
                for value in list {
                    *values += 1;
                    if *values > VALUES {
                        return Err(format!("a filter lists at most {VALUES} values"));
                    }
                    matcher(value, &here)?;
                }
            }
            _ => {
                return Err(format!(
                    "{here}: a field takes a list of values, as [\"a\", \"b\"], or a pattern of its own"
                ));
            }
        }
    }
    Ok(())
}

fn matcher(value: &Value, at: &str) -> Result<(), String> {
    let Value::Object(map) = value else {
        return match value {
            Value::Array(_) => Err(format!("{at}: a value is {MATCHERS}, not a list")),
            _ => Ok(()),
        };
    };
    let bad = || Err(format!("{at}: a value is {MATCHERS}"));
    let Some((kind, arg)) = map.iter().next().filter(|_| map.len() == 1) else {
        return bad();
    };
    match (kind.as_str(), arg) {
        ("prefix" | "suffix" | "wildcard" | "equals-ignore-case", Value::String(_)) => Ok(()),
        ("exists", Value::Bool(_)) => Ok(()),
        ("numeric", Value::Array(terms)) => numeric(terms).map(|_| ()).map_err(|why| {
            format!("{at}: numeric is a list of comparisons, as [\">\", 0, \"<=\", 5]: {why}")
        }),
        ("anything-but", Value::Array(list)) if !list.is_empty() => {
            match list.iter().all(|v| v.is_string() || v.is_number()) {
                true => Ok(()),
                false => Err(format!(
                    "{at}: anything-but takes a value, a list of strings or numbers, or one of \
                     prefix, suffix, wildcard or equals-ignore-case"
                )),
            }
        }
        ("anything-but", Value::Object(inner)) => match inner.iter().next() {
            Some((k, Value::String(_)))
                if inner.len() == 1
                    && ["prefix", "suffix", "wildcard", "equals-ignore-case"]
                        .contains(&k.as_str()) =>
            {
                Ok(())
            }
            _ => Err(format!(
                "{at}: anything-but takes a value, a list, or one of prefix, suffix, wildcard or \
                 equals-ignore-case"
            )),
        },
        ("anything-but", Value::String(_) | Value::Number(_)) => Ok(()),
        _ => bad(),
    }
}

/// The comparisons a `numeric` list makes, as (operator, number).
fn numeric(terms: &[Value]) -> Result<Vec<(&str, f64)>, String> {
    if terms.is_empty() || terms.len() > 4 || !terms.len().is_multiple_of(2) {
        return Err("one or two comparisons".into());
    }
    (terms.chunks(2))
        .map(|pair| {
            let op = (pair[0].as_str())
                .filter(|op| ["=", "<", "<=", ">", ">="].contains(op))
                .ok_or("an operator is =, <, <=, > or >=")?;
            let n = pair[1]
                .as_f64()
                .ok_or("each operator is followed by a number")?;
            Ok((op, n))
        })
        .collect()
}

/// Whether the event's fields match the pattern, which `check` passed.
pub fn matches(pattern: &Value, event: &Value) -> bool {
    let Some(fields) = pattern.as_object() else {
        return false;
    };
    fields.iter().all(|(field, wanted)| {
        if field == "$or" {
            return (wanted.as_array().into_iter().flatten()).any(|p| matches(p, event));
        }
        let got = event.get(field);
        match wanted {
            // A field that is a list of objects matches when one does.
            Value::Object(_) => match got {
                Some(Value::Array(items)) => items.iter().any(|item| matches(wanted, item)),
                Some(got @ Value::Object(_)) => matches(wanted, got),
                _ => false,
            },
            Value::Array(list) => list.iter().any(|value| value_matches(value, got)),
            _ => false,
        }
    })
}

/// Whether one value of a field's list matches what the event holds
/// there, `None` when it holds nothing.
fn value_matches(wanted: &Value, got: Option<&Value>) -> bool {
    if let Value::Object(map) = wanted
        && let Some(Value::Bool(exists)) = map.get("exists")
    {
        let present = got.is_some_and(|got| !got.is_object());
        return present == *exists;
    }
    match got {
        None => false,
        Some(Value::Array(items)) => items.iter().any(|item| one(wanted, item)),
        Some(got) => one(wanted, got),
    }
}

/// Whether one value matches one of the event's.
fn one(wanted: &Value, got: &Value) -> bool {
    let Value::Object(map) = wanted else {
        return equal(wanted, got);
    };
    let Some((kind, arg)) = map.iter().next() else {
        return false;
    };
    let text = got.as_str();
    match (kind.as_str(), arg) {
        ("prefix", Value::String(p)) => text.is_some_and(|t| t.starts_with(p.as_str())),
        ("suffix", Value::String(s)) => text.is_some_and(|t| t.ends_with(s.as_str())),
        ("wildcard", Value::String(w)) => text.is_some_and(|t| super::subs::matches(w, t)),
        ("equals-ignore-case", Value::String(s)) => {
            text.is_some_and(|t| t.to_lowercase() == s.to_lowercase())
        }
        ("numeric", Value::Array(terms)) => {
            let (Some(n), Ok(terms)) = (got.as_f64(), numeric(terms)) else {
                return false;
            };
            terms.iter().all(|(op, bound)| match *op {
                "=" => n == *bound,
                "<" => n < *bound,
                "<=" => n <= *bound,
                ">" => n > *bound,
                _ => n >= *bound,
            })
        }
        ("anything-but", Value::Array(list)) => !list.iter().any(|v| equal(v, got)),
        ("anything-but", inner @ Value::Object(_)) => text.is_some() && !one(inner, got),
        ("anything-but", value) => !equal(value, got),
        _ => false,
    }
}

/// Equal as JSON, numbers by value: `5` is `5.0`, and whole numbers
/// exactly, past what a float holds.
fn equal(wanted: &Value, got: &Value) -> bool {
    let (Value::Number(a), Value::Number(b)) = (wanted, got) else {
        return wanted == got;
    };
    if let (Some(a), Some(b)) = (a.as_i64(), b.as_i64()) {
        return a == b;
    }
    if let (Some(a), Some(b)) = (a.as_u64(), b.as_u64()) {
        return a == b;
    }
    // A whole number and a fraction, or one past i64 against a negative.
    match (a.is_f64() || b.is_f64(), a.as_f64(), b.as_f64()) {
        (true, Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// The fields, at the pattern's top, that keep it from matching any event
/// that has only the fields `had` says it has: none when such an event may
/// match. A field the pattern lets be missing (`{"exists": false}`) never
/// keeps it from matching, and `$or` is kept from matching only when every
/// one of its patterns is.
pub fn missing(pattern: &Value, had: &dyn Fn(&str) -> bool) -> Vec<String> {
    let mut out = Vec::new();
    for (field, wanted) in pattern.as_object().into_iter().flatten() {
        if field == "$or" {
            let each: Vec<Vec<String>> = (wanted.as_array().into_iter().flatten())
                .map(|p| missing(p, had))
                .collect();
            if each.iter().all(|m| !m.is_empty()) {
                out.extend(each.into_iter().flatten());
            }
            continue;
        }
        let may_lack = (wanted.as_array().into_iter().flatten())
            .any(|value| value.get("exists") == Some(&Value::Bool(false)));
        if !may_lack && !had(field) {
            out.push(field.clone());
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cr() -> Value {
        json!({
            "text": "CR-12: tidy the lookout",
            "author": "ana",
            "repo": "agent",
            "paths": ["sonny/src/lookout/mod.rs", "docs/LOOKOUT.md"],
            "lines": 120,
            "draft": false,
            "reviewers": [{"name": "bo", "state": "approved"}, {"name": "cy", "state": "pending"}],
            "meta": {"team": "core"}
        })
    }

    fn hears(pattern: Value) -> bool {
        check(&pattern).unwrap_or_else(|why| panic!("{pattern}: {why}"));
        matches(&pattern, &cr())
    }

    #[test]
    fn fields_and_values_or_and_lists_match_any_item() {
        assert!(hears(json!({"repo": ["agent"]})));
        assert!(hears(json!({"repo": ["other", "agent"]})));
        assert!(!hears(json!({"repo": ["other"]})));
        assert!(!hears(json!({"repo": ["agent"], "author": ["bo"]})));
        assert!(hears(json!({"paths": ["docs/LOOKOUT.md"]})));
        assert!(!hears(json!({"nope": ["x"]})));
        assert!(hears(json!({"draft": [false]})));
        assert!(hears(json!({"lines": [120.0]})));
        assert!(hears(json!({"meta": {"team": ["core"]}})));
        assert!(!hears(json!({"meta": {"team": ["web"]}})));
        assert!(hears(json!({"reviewers": {"state": ["approved"]}})));
        assert!(!hears(json!({"reviewers": {"state": ["rejected"]}})));
    }

    #[test]
    fn string_matchers() {
        assert!(hears(json!({"paths": [{"prefix": "sonny/src/lookout/"}]})));
        assert!(!hears(json!({"paths": [{"prefix": "client/"}]})));
        assert!(hears(json!({"paths": [{"suffix": ".md"}]})));
        assert!(hears(json!({"paths": [{"wildcard": "sonny/*/mod.rs"}]})));
        assert!(hears(json!({"text": [{"wildcard": "*lookout*"}]})));
        assert!(!hears(json!({"text": [{"wildcard": "*daemon*"}]})));
        assert!(hears(json!({"author": [{"equals-ignore-case": "ANA"}]})));
        // Matchers on strings never match numbers.
        assert!(!hears(json!({"lines": [{"prefix": "1"}]})));
    }

    #[test]
    fn anything_but_numeric_and_exists() {
        assert!(hears(json!({"author": [{"anything-but": "bot"}]})));
        assert!(!hears(
            json!({"author": [{"anything-but": ["bot", "ana"]}]})
        ));
        assert!(!hears(
            json!({"author": [{"anything-but": {"prefix": "a"}}]})
        ));
        assert!(hears(
            json!({"author": [{"anything-but": {"prefix": "z"}}]})
        ));
        // A field the event lacks matches no anything-but.
        assert!(!hears(json!({"nope": [{"anything-but": "x"}]})));
        assert!(hears(
            json!({"lines": [{"numeric": [">", 100, "<=", 500]}]})
        ));
        assert!(!hears(json!({"lines": [{"numeric": ["<", 100]}]})));
        assert!(hears(json!({"lines": [{"numeric": ["=", 120]}]})));
        assert!(!hears(json!({"author": [{"numeric": [">", 0]}]})));
        assert!(hears(json!({"author": [{"exists": true}]})));
        assert!(hears(json!({"nope": [{"exists": false}]})));
        assert!(!hears(json!({"author": [{"exists": false}]})));
        assert!(!hears(json!({"nope": [{"exists": true}]})));
    }

    #[test]
    fn or_takes_any_of_its_patterns() {
        let either = json!({"$or": [{"author": ["bo"]}, {"paths": [{"prefix": "docs/"}]}]});
        assert!(hears(either.clone()));
        assert!(!hears(
            json!({"$or": [{"author": ["bo"]}, {"repo": ["x"]}]})
        ));
        assert!(!hears(
            json!({"repo": ["agent"], "$or": [{"author": ["bo"]}, {"lines": [1]}]})
        ));
    }

    #[test]
    fn whole_numbers_past_a_float_stay_apart() {
        let big = json!({"id": 9007199254740993u64});
        let wanted = json!({"id": [9007199254740992u64]});
        assert!(!matches(&wanted, &big));
        assert!(matches(&json!({"id": [9007199254740993u64]}), &big));
        assert!(matches(
            &json!({"id": [{"anything-but": 9007199254740992u64}]}),
            &big
        ));
        assert!(matches(&json!({"n": [-3]}), &json!({"n": -3})));
        assert!(!matches(&json!({"n": [-1]}), &json!({"n": u64::MAX})));
        assert!(matches(&json!({"n": [5]}), &json!({"n": 5.0})));
    }

    #[test]
    fn what_keeps_a_pattern_from_matching_the_fields_a_source_has() {
        let had = |field: &str| ["text", "paths"].contains(&field);
        assert!(missing(&json!({"paths": ["a"]}), &had).is_empty());
        assert_eq!(
            missing(&json!({"path": ["a"], "text": ["b"]}), &had),
            ["path"]
        );
        // A field it lets be missing never does.
        assert!(missing(&json!({"draft": [{"exists": false}]}), &had).is_empty());
        // $or, only when every pattern in it is kept from matching.
        let either = json!({"$or": [{"author": ["a"]}, {"paths": ["b"]}]});
        assert!(missing(&either, &had).is_empty());
        let neither = json!({"$or": [{"author": ["a"]}, {"repo": ["b"]}]});
        assert_eq!(missing(&neither, &had), ["author", "repo"]);
    }

    #[test]
    fn a_pattern_is_held_to_what_one_may_say() {
        let refused = |pattern: Value| check(&pattern).unwrap_err();
        assert!(refused(json!([])).contains("an object of fields"));
        assert!(refused(json!({})).contains("at least one field"));
        assert!(refused(json!({"repo": "agent"})).contains("list of values"));
        assert!(refused(json!({"repo": []})).contains("list of values"));
        assert!(refused(json!({"repo": [["a"]]})).contains("not a list"));
        assert!(refused(json!({"repo": [{"regex": ".*"}]})).contains("prefix"));
        assert!(refused(json!({"repo": [{"prefix": "a", "suffix": "b"}]})).contains("prefix"));
        assert!(refused(json!({"n": [{"numeric": [">"]}]})).contains("comparisons"));
        assert!(refused(json!({"n": [{"numeric": ["~", 1]}]})).contains("operator"));
        assert!(refused(json!({"n": [{"anything-but": [{"a": 1}]}]})).contains("anything-but"));
        assert!(refused(json!({"$or": [{"a": ["b"]}]})).contains("two or more"));
        let deep = (0..=DEPTH).fold(json!({"a": ["b"]}), |inner, _| json!({"x": inner}));
        assert!(refused(deep).contains("deep"));
        let many: Vec<u32> = (0..=VALUES as u32).collect();
        assert!(refused(json!({"n": many})).contains("at most"));
        let big = "x".repeat(PATTERN_BYTES);
        assert!(refused(json!({"n": [big]})).contains("bytes"));
    }
}
