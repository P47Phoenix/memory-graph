//! A small JSON Schema checker for the subset of the vocabulary the tool
//! schemas use (`type` as a name or a list, `properties`, `required`,
//! `additionalProperties` as a bool or a schema, `items`, `enum`, `minimum`,
//! `maximum`). It exists so tests (here and in `graph-cli`) can check every
//! `structuredContent` against its tool's `outputSchema`, and every input
//! against its `inputSchema`, without a JSON Schema crate (the common ones
//! pull an HTTP client for remote `$ref`s). A keyword outside the subset is
//! an error, so a schema can never silently go unchecked.
use serde_json::Value;

/// Keywords [`validate`] understands; anything else in a schema is refused.
const KNOWN: &[&str] = &[
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "minimum",
    "maximum",
    "description",
    "default",
    "title",
];

/// Check `v` against `schema`. The error names the JSON path that failed.
pub fn validate(schema: &Value, v: &Value) -> Result<(), String> {
    check(schema, v, "$")
}

fn type_ok(t: &str, v: &Value) -> Result<bool, String> {
    Ok(match t {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        "number" => v.is_number(),
        // An integral float (5.0) is an integer, as JSON Schema says.
        "integer" => v.is_i64() || v.is_u64() || v.as_f64().is_some_and(|f| f.fract() == 0.0),
        other => return Err(format!("schema uses unsupported type `{other}`")),
    })
}

fn check(schema: &Value, v: &Value, path: &str) -> Result<(), String> {
    let s = schema
        .as_object()
        .ok_or_else(|| format!("{path}: schema is not an object"))?;
    if let Some(k) = s.keys().find(|k| !KNOWN.contains(&k.as_str())) {
        return Err(format!("{path}: schema keyword `{k}` is not supported"));
    }
    if let Some(t) = s.get("type") {
        let ok = match t {
            Value::String(t) => type_ok(t, v)?,
            Value::Array(ts) => {
                let mut any = false;
                for t in ts {
                    let t = t.as_str().ok_or_else(|| format!("{path}: bad type list"))?;
                    any |= type_ok(t, v)?;
                }
                any
            }
            _ => return Err(format!("{path}: bad `type`")),
        };
        if !ok {
            return Err(format!("{path}: {v} is not of type {t}"));
        }
    }
    if let Some(e) = s.get("enum").and_then(Value::as_array) {
        if !e.contains(v) {
            return Err(format!(
                "{path}: {v} is not one of {}",
                Value::Array(e.clone())
            ));
        }
    }
    if let (Some(min), Some(n)) = (s.get("minimum").and_then(Value::as_f64), v.as_f64()) {
        if n < min {
            return Err(format!("{path}: {n} is below the minimum {min}"));
        }
    }
    if let (Some(max), Some(n)) = (s.get("maximum").and_then(Value::as_f64), v.as_f64()) {
        if n > max {
            return Err(format!("{path}: {n} is above the maximum {max}"));
        }
    }
    if let Some(o) = v.as_object() {
        let props = s.get("properties").and_then(Value::as_object);
        if let Some(req) = s.get("required").and_then(Value::as_array) {
            for r in req.iter().filter_map(Value::as_str) {
                if !o.contains_key(r) {
                    return Err(format!("{path}: required property `{r}` is missing"));
                }
            }
        }
        for (k, val) in o {
            let sub = format!("{path}.{k}");
            match props.and_then(|p| p.get(k)) {
                Some(ps) => check(ps, val, &sub)?,
                None => match s.get("additionalProperties") {
                    Some(Value::Bool(false)) => {
                        return Err(format!("{path}: property `{k}` is not allowed"))
                    }
                    Some(ap @ Value::Object(_)) => check(ap, val, &sub)?,
                    _ => {}
                },
            }
        }
    }
    if let (Some(items), Some(a)) = (s.get("items"), v.as_array()) {
        for (i, x) in a.iter().enumerate() {
            check(items, x, &format!("{path}[{i}]"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_subset_is_enforced() {
        let s = json!({
            "type": "object",
            "properties": {
                "a": {"type": "integer", "minimum": 1, "maximum": 3},
                "b": {"type": ["string", "null"], "enum": ["x", null]},
                "c": {"type": "array", "items": {"type": "boolean"}}
            },
            "required": ["a"],
            "additionalProperties": false
        });
        assert!(validate(&s, &json!({"a": 2, "b": null, "c": [true]})).is_ok());
        assert!(validate(&s, &json!({"a": 2, "b": "x"})).is_ok());
        assert!(validate(&s, &json!({})).is_err());
        assert!(validate(&s, &json!({"a": 0})).is_err());
        assert!(validate(&s, &json!({"a": 4})).is_err());
        assert!(validate(&s, &json!({"a": 1.5})).is_err());
        assert!(validate(&s, &json!({"a": 1, "b": "y"})).is_err());
        assert!(validate(&s, &json!({"a": 1, "c": [1]})).is_err());
        assert!(validate(&s, &json!({"a": 1, "d": 1})).is_err());
        assert!(validate(&s, &json!([])).is_err());
        let m = json!({"type": "object", "additionalProperties": {"type": "integer"}});
        assert!(validate(&m, &json!({"x": 1})).is_ok());
        assert!(validate(&m, &json!({"x": "1"})).is_err());
        assert!(validate(&json!({"pattern": "x"}), &json!("x")).is_err());
    }
}
