// SPDX-License-Identifier: Apache-2.0
//! The **data steps** (RFC 0027 §5 "Data"): array and text operations without
//! a model — `map`, `filter`, `reduce`, `sort`, `dedupe`, `chunk`, `parse` —
//! as pure functions over JSON values. Element expressions are CEL (`item`,
//! `index`, `acc`, plus the run data) or `{{template}}` strings; `by` keys are
//! dotted paths.

use super::template::{self, Data};
use serde_json::{Map, Value, json};

/// Evaluate an element expression: `CEL: …` (or a bare CEL when it does not
/// look like a template), or a `{{…}}` template.
fn eval_expr(expr: &str, data: &Data) -> Result<Value, String> {
    let t = expr.trim();
    if t.contains("{{") {
        return template::render_str(t, data);
    }
    let cel = t.strip_prefix("CEL:").unwrap_or(t).trim();
    let vars: Vec<(&str, &Value)> = data.iter().map(|(k, v)| (k.as_str(), v)).collect();
    crate::cel::eval_value(cel, &vars).map_err(|e| format!("CEL: {e}"))
}

fn with_item(data: &Data, alias: &str, item: &Value, index: usize) -> Data {
    let mut d = data.clone();
    d.insert(alias.to_string(), item.clone());
    d.insert("item".to_string(), item.clone());
    d.insert("index".to_string(), json!(index));
    d
}

fn as_array(over: &Value, what: &str) -> Result<Vec<Value>, String> {
    match over {
        Value::Array(a) => Ok(a.clone()),
        Value::Object(o) => Ok(o
            .iter()
            .map(|(k, v)| json!({"key": k, "value": v}))
            .collect()),
        Value::Null => Ok(Vec::new()),
        other => Err(format!(
            "{what}: `over` must be an array (got {})",
            type_name(other)
        )),
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// `map`: `expr` over every element.
pub fn map(over: &Value, expr: &str, alias: &str, data: &Data) -> Result<Value, String> {
    let items = as_array(over, "map")?;
    let mut out = Vec::with_capacity(items.len());
    for (i, it) in items.iter().enumerate() {
        out.push(eval_expr(expr, &with_item(data, alias, it, i))?);
    }
    Ok(Value::Array(out))
}

/// `filter`: keep the elements whose `expr` is true.
pub fn filter(over: &Value, expr: &str, alias: &str, data: &Data) -> Result<Value, String> {
    let items = as_array(over, "filter")?;
    let mut out = Vec::new();
    for (i, it) in items.iter().enumerate() {
        let v = eval_expr(expr, &with_item(data, alias, it, i))?;
        match v {
            Value::Bool(true) => out.push(it.clone()),
            Value::Bool(false) => {}
            other => {
                return Err(format!(
                    "filter: expr must yield a boolean (got {})",
                    type_name(&other)
                ));
            }
        }
    }
    Ok(Value::Array(out))
}

/// `reduce`: fold `expr` over the elements with `acc` (starting at `initial`).
pub fn reduce(
    over: &Value,
    expr: &str,
    initial: Value,
    alias: &str,
    acc_alias: &str,
    data: &Data,
) -> Result<Value, String> {
    let items = as_array(over, "reduce")?;
    let mut acc = initial;
    for (i, it) in items.iter().enumerate() {
        let mut d = with_item(data, alias, it, i);
        d.insert(acc_alias.to_string(), acc.clone());
        d.insert("acc".to_string(), acc.clone());
        acc = eval_expr(expr, &d)?;
    }
    Ok(acc)
}

/// `sort`: by a dotted path (or the element itself), `asc|desc`; stable.
pub fn sort(over: &Value, by: Option<&str>, order: Option<&str>) -> Result<Value, String> {
    let mut items = as_array(over, "sort")?;
    let desc = matches!(order, Some("desc") | Some("descending"));
    let key = |v: &Value| -> Value {
        match by {
            None | Some("") => v.clone(),
            Some(p) => path_of(v, p).unwrap_or(Value::Null),
        }
    };
    items.sort_by(|a, b| {
        let o = cmp_values(&key(a), &key(b));
        if desc { o.reverse() } else { o }
    });
    Ok(Value::Array(items))
}

/// `dedupe`: keep the first occurrence per key (`by` path) / value.
pub fn dedupe(over: &Value, by: Option<&str>) -> Result<Value, String> {
    let items = as_array(over, "dedupe")?;
    let mut seen: Vec<Value> = Vec::new();
    let mut out = Vec::new();
    for it in items {
        let k = match by {
            None | Some("") => it.clone(),
            Some(p) => path_of(&it, p).unwrap_or(Value::Null),
        };
        if !seen.contains(&k) {
            seen.push(k);
            out.push(it);
        }
    }
    Ok(Value::Array(out))
}

/// `chunk`: split text by `chars|lines|tokens` (approximate) or an array into
/// slices of `size`, with `overlap` elements/chars carried over.
pub fn chunk(
    value: &Value,
    by: Option<&str>,
    size: usize,
    overlap: usize,
) -> Result<Value, String> {
    if size == 0 {
        return Err("chunk: size must be > 0".into());
    }
    let overlap = overlap.min(size.saturating_sub(1));
    match value {
        Value::Array(a) => {
            let mut out = Vec::new();
            let mut start = 0;
            while start < a.len() {
                let end = (start + size).min(a.len());
                out.push(Value::Array(a[start..end].to_vec()));
                if end == a.len() {
                    break;
                }
                start = end - overlap;
            }
            Ok(Value::Array(out))
        }
        Value::String(s) => {
            let mode = by.unwrap_or("chars");
            let out: Vec<Value> = match mode {
                "lines" => {
                    let lines: Vec<&str> = s.lines().collect();
                    windows(&lines, size, overlap)
                        .into_iter()
                        .map(|w| Value::String(w.join("\n")))
                        .collect()
                }
                "words" | "tokens" => {
                    // tokens ≈ words × 1.3; chunk by words with size/1.3 words per chunk.
                    let words: Vec<&str> = s.split_whitespace().collect();
                    let per = if mode == "tokens" {
                        ((size as f64) / 1.3).max(1.0) as usize
                    } else {
                        size
                    };
                    let ov = if mode == "tokens" {
                        ((overlap as f64) / 1.3) as usize
                    } else {
                        overlap
                    };
                    windows(&words, per, ov.min(per.saturating_sub(1)))
                        .into_iter()
                        .map(|w| Value::String(w.join(" ")))
                        .collect()
                }
                "chars" => {
                    let chars: Vec<char> = s.chars().collect();
                    windows(&chars, size, overlap)
                        .into_iter()
                        .map(|w| Value::String(w.into_iter().collect()))
                        .collect()
                }
                other => {
                    return Err(format!(
                        "chunk: by must be chars|lines|words|tokens (got {other:?})"
                    ));
                }
            };
            Ok(Value::Array(out))
        }
        other => Err(format!(
            "chunk: value must be a string or an array (got {})",
            type_name(other)
        )),
    }
}

fn windows<T: Clone>(items: &[T], size: usize, overlap: usize) -> Vec<Vec<T>> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < items.len() {
        let end = (start + size).min(items.len());
        out.push(items[start..end].to_vec());
        if end == items.len() {
            break;
        }
        start = end - overlap;
    }
    out
}

/// `parse`: text → JSON (`json` | `yaml` | `csv` | `lines` | `auto`).
pub fn parse(text: &str, format: Option<&str>) -> Result<Value, String> {
    let f = format.unwrap_or("auto");
    match f {
        "json" => serde_json::from_str::<Value>(text).map_err(|e| format!("parse json: {e}")),
        "yaml" => crate::config::yaml::parse(text).map_err(|e| format!("parse yaml: {e}")),
        "lines" => Ok(Value::Array(
            text.lines().map(|l| Value::String(l.to_string())).collect(),
        )),
        "csv" => Ok(parse_csv(text)),
        "auto" => {
            let t = text.trim();
            if let Ok(v) = serde_json::from_str::<Value>(t) {
                return Ok(v);
            }
            if let Ok(v) = crate::config::yaml::parse(t)
                && !matches!(v, Value::String(_))
            {
                return Ok(v);
            }
            Ok(Value::Array(
                text.lines().map(|l| Value::String(l.to_string())).collect(),
            ))
        }
        other => Err(format!(
            "parse: format must be json|yaml|csv|lines|auto (got {other:?})"
        )),
    }
}

/// A minimal CSV reader (RFC 4180 quoting; header row → objects).
fn parse_csv(text: &str) -> Value {
    let rows: Vec<Vec<String>> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(csv_row)
        .collect();
    let Some((header, body)) = rows.split_first() else {
        return json!([]);
    };
    Value::Array(
        body.iter()
            .map(|r| {
                let mut o = Map::new();
                for (i, h) in header.iter().enumerate() {
                    o.insert(
                        h.clone(),
                        r.get(i).map(|c| coerce_scalar(c)).unwrap_or(Value::Null),
                    );
                }
                Value::Object(o)
            })
            .collect(),
    )
}

fn csv_row(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => {
                out.push(std::mem::take(&mut cur));
            }
            other => cur.push(other),
        }
    }
    out.push(cur);
    out
}

fn coerce_scalar(s: &str) -> Value {
    if let Ok(i) = s.parse::<i64>() {
        return json!(i);
    }
    if let Ok(f) = s.parse::<f64>() {
        return json!(f);
    }
    match s {
        "true" => json!(true),
        "false" => json!(false),
        _ => Value::String(s.to_string()),
    }
}

/// A dotted path inside a value.
pub fn path_of(v: &Value, path: &str) -> Option<Value> {
    let mut cur = v;
    for seg in path.split('.') {
        cur = match cur {
            Value::Object(m) => m.get(seg)?,
            Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur.clone())
}

/// A total order over JSON values: null < bool < number < string < array < object.
pub fn cmp_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let rank = |v: &Value| match v {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Number(_) => 2,
        Value::String(_) => 3,
        Value::Array(_) => 4,
        Value::Object(_) => 5,
    };
    match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Number(x), Value::Number(y)) => {
            x.as_f64().partial_cmp(&y.as_f64()).unwrap_or(Equal)
        }
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Array(x), Value::Array(y)) => {
            for (p, q) in x.iter().zip(y.iter()) {
                let o = cmp_values(p, q);
                if o != Equal {
                    return o;
                }
            }
            x.len().cmp(&y.len())
        }
        _ => rank(a).cmp(&rank(b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "cel")]
    fn data() -> Data {
        let mut d = Data::new();
        d.insert("vars".into(), json!({"min": 2}));
        d
    }

    #[cfg(feature = "cel")]
    #[test]
    fn map_filter_reduce_with_cel_and_templates() {
        let d = data();
        assert_eq!(
            map(&json!([1, 2, 3]), "item * 2", "item", &d).unwrap(),
            json!([2, 4, 6])
        );
        assert_eq!(
            map(
                &json!([{"n": 1}, {"n": 5}]),
                "{{item.n}}-{{index}}",
                "item",
                &d
            )
            .unwrap(),
            json!(["1-0", "5-1"])
        );
        assert_eq!(
            filter(&json!([1, 2, 3, 4]), "CEL: x > vars.min", "x", &d).unwrap(),
            json!([3, 4])
        );
        assert!(
            filter(&json!([1]), "item", "item", &d).is_err(),
            "non-boolean"
        );
        assert_eq!(
            reduce(&json!([1, 2, 3]), "acc + item", json!(0), "item", "acc", &d).unwrap(),
            json!(6)
        );
        assert_eq!(
            reduce(
                &json!(["a", "b"]),
                "total + \"|\" + s",
                json!(""),
                "s",
                "total",
                &d
            )
            .unwrap(),
            json!("|a|b")
        );
        // Objects iterate as {key, value}.
        assert_eq!(
            map(
                &json!({"a": 1, "b": 2}),
                "item.key + \"=\" + string(item.value)",
                "item",
                &d
            )
            .unwrap(),
            json!(["a=1", "b=2"])
        );
    }

    #[test]
    fn sort_dedupe_chunk_parse() {
        assert_eq!(
            sort(&json!([3, 1, 2]), None, None).unwrap(),
            json!([1, 2, 3])
        );
        assert_eq!(
            sort(
                &json!([{"n": 3, "s": "c"}, {"n": 1, "s": "a"}]),
                Some("n"),
                Some("desc")
            )
            .unwrap(),
            json!([{"n": 3, "s": "c"}, {"n": 1, "s": "a"}])
        );
        assert_eq!(
            sort(&json!(["b", null, 2, "a", true]), None, None).unwrap(),
            json!([null, true, 2, "a", "b"])
        );
        assert_eq!(
            dedupe(&json!([1, 2, 1, 3, 2]), None).unwrap(),
            json!([1, 2, 3])
        );
        assert_eq!(
            dedupe(
                &json!([{"id": 1, "x": "a"}, {"id": 1, "x": "b"}, {"id": 2}]),
                Some("id")
            )
            .unwrap(),
            json!([{"id": 1, "x": "a"}, {"id": 2}])
        );
        assert_eq!(
            chunk(&json!([1, 2, 3, 4, 5]), None, 2, 0).unwrap(),
            json!([[1, 2], [3, 4], [5]])
        );
        assert_eq!(
            chunk(&json!([1, 2, 3, 4, 5]), None, 3, 1).unwrap(),
            json!([[1, 2, 3], [3, 4, 5]])
        );
        assert_eq!(
            chunk(&json!("abcdefg"), Some("chars"), 3, 0).unwrap(),
            json!(["abc", "def", "g"])
        );
        assert_eq!(
            chunk(&json!("l1\nl2\nl3"), Some("lines"), 2, 0).unwrap(),
            json!(["l1\nl2", "l3"])
        );
        assert_eq!(
            chunk(&json!("a b c d"), Some("words"), 2, 0).unwrap(),
            json!(["a b", "c d"])
        );
        assert!(chunk(&json!("x"), None, 0, 0).is_err());
        assert_eq!(parse("{\"a\": 1}", None).unwrap(), json!({"a": 1}));
        assert_eq!(
            parse("a: 1\nb: [x, y]", Some("yaml")).unwrap(),
            json!({"a": 1, "b": ["x", "y"]})
        );
        assert_eq!(parse("x\ny", Some("lines")).unwrap(), json!(["x", "y"]));
        assert_eq!(
            parse("name,age\n\"Doe, J\",42\nAnn,7", Some("csv")).unwrap(),
            json!([{"name": "Doe, J", "age": 42}, {"name": "Ann", "age": 7}])
        );
        assert_eq!(parse("plain text", None).unwrap(), json!(["plain text"]));
        assert!(parse("x", Some("xml")).is_err());
        assert_eq!(
            path_of(&json!({"a": {"b": [10, 20]}}), "a.b.1"),
            Some(json!(20))
        );
    }
}
