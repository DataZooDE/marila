//! Mongo-style filter → DuckDB SQL `WHERE` clause.
//!
//! The QueryVectors `filter` field accepts a small subset of MongoDB's
//! query language. We translate it to SQL that operates on the
//! backing-table `meta JSON` column.
//!
//! Supported (CLAUDE.md C-2f):
//! - Implicit `$eq`: `{"field":"value"}` ≡ `{"field":{"$eq":"value"}}`
//! - Comparison: `$eq`, `$ne`, `$gt`, `$gte`, `$lt`, `$lte`
//! - Set membership: `$in`, `$nin`
//! - Logical combinators: `$and`, `$or`, `$not`
//!
//! Anything else returns [`FilterError::Unsupported`] which the handler
//! maps to AWS `ValidationException`.

use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FilterError {
    #[error("filter contains invalid field name `{0}` (must match [a-zA-Z_][a-zA-Z0-9_-]*)")]
    InvalidField(String),
    #[error("filter operator `{0}` is not supported")]
    Unsupported(String),
    #[error("filter `{op}` expects {expected}, got `{got}`")]
    BadValue {
        op: &'static str,
        expected: &'static str,
        got: String,
    },
    #[error("filter must be a JSON object, got `{0}`")]
    NotObject(String),
}

/// Translate a Mongo-style filter object into a SQL fragment suitable
/// for inlining into a `WHERE` clause.
///
/// Returns the SQL string (no surrounding parens needed; we always
/// wrap composite clauses in parens internally). All literals are
/// embedded inline because we strictly validate field names and JSON
/// values before splicing — no untrusted-input SQL surface.
pub fn translate(filter: &Value) -> Result<String, FilterError> {
    let obj = filter
        .as_object()
        .ok_or_else(|| FilterError::NotObject(value_short(filter)))?;
    if obj.is_empty() {
        return Ok("TRUE".to_owned());
    }

    let mut clauses = Vec::with_capacity(obj.len());
    for (key, val) in obj {
        clauses.push(translate_key(key, val)?);
    }
    Ok(join_with_and(clauses))
}

fn translate_key(key: &str, val: &Value) -> Result<String, FilterError> {
    match key {
        "$and" => translate_combinator(val, " AND "),
        "$or" => translate_combinator(val, " OR "),
        "$not" => {
            let inner = translate(val)?;
            Ok(format!("NOT ({inner})"))
        }
        _ if key.starts_with('$') => Err(FilterError::Unsupported(key.to_owned())),
        _ => {
            validate_field(key)?;
            translate_field_filter(key, val)
        }
    }
}

fn translate_combinator(val: &Value, joiner: &str) -> Result<String, FilterError> {
    let arr = val.as_array().ok_or_else(|| FilterError::BadValue {
        op: "$and/$or",
        expected: "an array of sub-filters",
        got: value_short(val),
    })?;
    if arr.is_empty() {
        return Ok("TRUE".to_owned());
    }
    let parts: Result<Vec<String>, _> = arr
        .iter()
        .map(|f| translate(f).map(|s| format!("({s})")))
        .collect();
    Ok(parts?.join(joiner))
}

/// `{"field": <value>}` — either implicit `$eq` for scalar `<value>`
/// or an `{operator: ...}` object.
fn translate_field_filter(field: &str, val: &Value) -> Result<String, FilterError> {
    if let Some(obj) = val.as_object() {
        // Operator object — each key is a `$op`.
        if obj.is_empty() {
            return Err(FilterError::BadValue {
                op: "operator",
                expected: "at least one operator key like $eq, $gt, …",
                got: "{}".into(),
            });
        }
        let mut clauses = Vec::with_capacity(obj.len());
        for (op, v) in obj {
            clauses.push(translate_op(field, op, v)?);
        }
        Ok(join_with_and(clauses))
    } else {
        // Implicit $eq.
        Ok(eq_clause(field, val))
    }
}

fn translate_op(field: &str, op: &str, v: &Value) -> Result<String, FilterError> {
    match op {
        "$eq" => Ok(eq_clause(field, v)),
        "$ne" => Ok(format!("NOT ({})", eq_clause(field, v))),
        "$gt" | "$gte" | "$lt" | "$lte" => {
            let cmp_op = match op {
                "$gt" => ">",
                "$gte" => ">=",
                "$lt" => "<",
                "$lte" => "<=",
                _ => unreachable!(),
            };
            let n = v.as_f64().ok_or(FilterError::BadValue {
                op: match op {
                    "$gt" => "$gt",
                    "$gte" => "$gte",
                    "$lt" => "$lt",
                    "$lte" => "$lte",
                    _ => unreachable!(),
                },
                expected: "a numeric value",
                got: value_short(v),
            })?;
            Ok(format!(
                "CAST(json_extract(meta, '$.{field}') AS DOUBLE) {cmp_op} {n}"
            ))
        }
        "$in" | "$nin" => {
            let arr = v.as_array().ok_or(FilterError::BadValue {
                op: match op {
                    "$in" => "$in",
                    _ => "$nin",
                },
                expected: "an array of values",
                got: value_short(v),
            })?;
            if arr.is_empty() {
                // Empty $in matches nothing; empty $nin matches everything.
                return Ok(if op == "$in" { "FALSE" } else { "TRUE" }.to_owned());
            }
            let parts: Vec<String> = arr.iter().map(|val| eq_clause(field, val)).collect();
            let joined = parts.join(" OR ");
            if op == "$in" {
                Ok(format!("({joined})"))
            } else {
                Ok(format!("NOT ({joined})"))
            }
        }
        other => Err(FilterError::Unsupported(other.to_owned())),
    }
}

/// `json_extract(meta, '$.<field>') = <json-literal>::JSON`
///
/// DuckDB's JSON equality is type-aware: `"1"`::JSON ≠ `1`::JSON.
/// That matches the implicit Mongo semantic ("compare types as well
/// as values") without us having to dispatch on the value's type.
fn eq_clause(field: &str, val: &Value) -> String {
    let lit = json_literal(val);
    format!("json_extract(meta, '$.{field}') = {lit}")
}

/// Render a serde_json::Value as a DuckDB JSON literal:
/// `'<serialised-json>'::JSON` with single-quotes escaped by doubling.
fn json_literal(val: &Value) -> String {
    let raw = serde_json::to_string(val).unwrap_or_else(|_| "null".to_owned());
    format!("'{}'::JSON", raw.replace('\'', "''"))
}

fn validate_field(field: &str) -> Result<(), FilterError> {
    if field.is_empty() {
        return Err(FilterError::InvalidField(field.to_owned()));
    }
    let mut chars = field.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(FilterError::InvalidField(field.to_owned()));
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(FilterError::InvalidField(field.to_owned()));
    }
    Ok(())
}

fn join_with_and(clauses: Vec<String>) -> String {
    if clauses.len() == 1 {
        clauses.into_iter().next().unwrap()
    } else {
        clauses
            .into_iter()
            .map(|c| format!("({c})"))
            .collect::<Vec<_>>()
            .join(" AND ")
    }
}

fn value_short(v: &Value) -> String {
    let s = v.to_string();
    if s.len() > 60 {
        format!("{}…", &s[..60])
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn t(f: Value) -> String {
        translate(&f).unwrap()
    }

    #[test]
    fn empty_filter_is_true() {
        assert_eq!(t(json!({})), "TRUE");
    }

    #[test]
    fn implicit_eq_string() {
        assert_eq!(
            t(json!({"label": "a"})),
            r#"json_extract(meta, '$.label') = '"a"'::JSON"#
        );
    }

    #[test]
    fn implicit_eq_number() {
        assert_eq!(
            t(json!({"tier": 1})),
            r#"json_extract(meta, '$.tier') = '1'::JSON"#
        );
    }

    #[test]
    fn explicit_eq_and_ne() {
        assert!(
            t(json!({"label": {"$eq": "a"}}))
                .contains(r#"json_extract(meta, '$.label') = '"a"'::JSON"#)
        );
        let ne = t(json!({"label": {"$ne": "a"}}));
        assert!(ne.starts_with("NOT ("), "{ne}");
    }

    #[test]
    fn comparison_operators_cast_to_double() {
        assert_eq!(
            t(json!({"tier": {"$gt": 1}})),
            "CAST(json_extract(meta, '$.tier') AS DOUBLE) > 1"
        );
        assert!(t(json!({"tier": {"$lte": 1}})).contains("<= 1"));
    }

    #[test]
    fn in_translates_to_or_chain() {
        let s = t(json!({"tier": {"$in": [1, 2]}}));
        assert!(s.starts_with("(json_extract"));
        assert!(s.contains(" OR "));
    }

    #[test]
    fn empty_in_is_false_empty_nin_is_true() {
        assert_eq!(t(json!({"tier": {"$in": []}})), "FALSE");
        assert_eq!(t(json!({"tier": {"$nin": []}})), "TRUE");
    }

    #[test]
    fn and_combinator() {
        let s = t(json!({"$and": [{"label":"a"}, {"tier":{"$gte":1}}]}));
        assert!(s.contains(" AND "), "{s}");
    }

    #[test]
    fn or_combinator() {
        let s = t(json!({"$or": [{"label":"a"}, {"label":"b"}]}));
        assert!(s.contains(" OR "), "{s}");
    }

    #[test]
    fn not_combinator_wraps_negation() {
        let s = t(json!({"$not": {"label": "x"}}));
        assert!(s.starts_with("NOT ("), "{s}");
    }

    #[test]
    fn rejects_invalid_field_names() {
        for bad in ["", "1abc", "weird;DROP", "a b", "$leading"] {
            let err = translate(&json!({bad: 1})).unwrap_err();
            assert!(
                matches!(
                    err,
                    FilterError::InvalidField(_) | FilterError::Unsupported(_)
                ),
                "{bad} should be rejected, got: {err:?}"
            );
        }
    }

    #[test]
    fn rejects_unsupported_operator() {
        let err = translate(&json!({"label": {"$regex": "foo"}})).unwrap_err();
        assert!(matches!(err, FilterError::Unsupported(s) if s == "$regex"));
    }

    #[test]
    fn single_quote_in_string_is_escaped() {
        let s = t(json!({"label": "o'brien"}));
        // The literal must round-trip safely; SQL escaping doubles single quotes.
        assert!(s.contains("''"), "{s}");
    }
}
