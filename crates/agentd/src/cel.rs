// SPDX-License-Identifier: Apache-2.0
//! CEL (Common Expression Language) evaluation — the ONE gated exception to the
//! zero-dependency moat (`--features cel`, default OFF).
//!
//! CEL is used wherever agentd evaluates a deterministic expression over run
//! data: workflow `{"op":"cel"}` predicates, computed `assign.expr` values,
//! `infer.check` value constraints, and reactive `{"op":"cel"}` wake conditions.
//! Its design properties are exactly the requirements here: non-Turing-complete,
//! no I/O, guaranteed termination — the one form of "code" a model can safely
//! author and agentd can immediately execute.
//!
//! This module is ALWAYS compiled; only its internals are feature-gated. A
//! non-cel build answers every call with a clear "requires the 'cel' build
//! feature" error, so authoring surfaces reject CEL at define/parse time
//! (fail-closed) instead of silently mis-evaluating at run time.

use serde_json::Value;

/// Expression length cap — a routing/shaping expression is a line, not a
/// program; an oversized one is refused at compile-check time.
pub const MAX_CEL_EXPR: usize = 4096;

/// The message every entry point returns on a build without the feature.
pub const FEATURE_MSG: &str = "CEL expressions require the 'cel' build feature";

/// Compile-check an expression (define/parse-time validation): length cap +
/// full CEL parse. `Err` carries the parser's message for the author.
pub fn compile_check(expr: &str) -> Result<(), String> {
    if expr.trim().is_empty() {
        return Err("empty CEL expression".into());
    }
    if expr.len() > MAX_CEL_EXPR {
        return Err(format!(
            "CEL expression is {} bytes (max {MAX_CEL_EXPR})",
            expr.len()
        ));
    }
    #[cfg(feature = "cel")]
    {
        imp::compile(expr).map(|_| ())
    }
    #[cfg(not(feature = "cel"))]
    {
        Err(FEATURE_MSG.into())
    }
}

/// Evaluate an expression to a BOOLEAN with the given variables in scope
/// (each `(name, value)` becomes a top-level identifier). A non-bool result is
/// an error — a predicate must decide, not coerce.
pub fn eval_bool(expr: &str, vars: &[(&str, &Value)]) -> Result<bool, String> {
    #[cfg(feature = "cel")]
    {
        match imp::eval(expr, vars)? {
            cel_interpreter::Value::Bool(b) => Ok(b),
            other => Err(format!(
                "CEL expression returned {:?}, want bool",
                other.type_of()
            )),
        }
    }
    #[cfg(not(feature = "cel"))]
    {
        let _ = (expr, vars);
        Err(FEATURE_MSG.into())
    }
}

/// Evaluate an expression to a JSON VALUE with the given variables in scope —
/// the computed-`assign` path.
pub fn eval_value(expr: &str, vars: &[(&str, &Value)]) -> Result<Value, String> {
    #[cfg(feature = "cel")]
    {
        imp::eval(expr, vars)?
            .json()
            .map_err(|e| format!("CEL result is not JSON-representable: {e}"))
    }
    #[cfg(not(feature = "cel"))]
    {
        let _ = (expr, vars);
        Err(FEATURE_MSG.into())
    }
}

/// Convenience: a blackboard-shaped variable list (`BTreeMap<String, Value>` →
/// the `(name, value)` slice shape the eval fns take).
pub fn vars_of(map: &std::collections::BTreeMap<String, Value>) -> Vec<(&str, &Value)> {
    map.iter().map(|(k, v)| (k.as_str(), v)).collect()
}

#[cfg(feature = "cel")]
mod imp {
    use serde_json::Value;

    pub fn compile(expr: &str) -> Result<cel_interpreter::Program, String> {
        cel_interpreter::Program::compile(expr).map_err(|e| format!("CEL parse: {e}"))
    }

    /// Convert JSON → CEL with CANONICAL number typing: JSON has one number
    /// type, CEL has three (Int/UInt/Float) that do not mix in arithmetic or
    /// comparison. The default Serialize-based conversion maps a non-negative
    /// integer to UInt — making `count + 1` a type error against the Int
    /// literal. Normalizing every i64-fitting integer to Int (else Float) makes
    /// expressions over JSON data behave the way their authors expect.
    fn to_cel(v: &Value) -> cel_interpreter::Value {
        use cel_interpreter::Value as C;
        use std::sync::Arc;
        match v {
            Value::Null => C::Null,
            Value::Bool(b) => C::Bool(*b),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    C::Int(i)
                } else if let Some(f) = n.as_f64() {
                    C::Float(f)
                } else {
                    C::Null
                }
            }
            Value::String(s) => C::String(Arc::new(s.clone())),
            Value::Array(a) => C::List(Arc::new(a.iter().map(to_cel).collect())),
            Value::Object(o) => {
                let map: std::collections::HashMap<String, C> =
                    o.iter().map(|(k, v)| (k.clone(), to_cel(v))).collect();
                C::Map(map.into())
            }
        }
    }

    pub fn eval(expr: &str, vars: &[(&str, &Value)]) -> Result<cel_interpreter::Value, String> {
        let program = compile(expr)?;
        let mut ctx = cel_interpreter::Context::default();
        for (name, value) in vars {
            ctx.add_variable_from_value(name.to_string(), to_cel(value));
        }
        program.execute(&ctx).map_err(|e| format!("CEL eval: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_and_oversized_expressions_are_refused() {
        assert!(compile_check("").is_err());
        assert!(compile_check(&"1 + ".repeat(2000)).is_err());
    }

    #[cfg(feature = "cel")]
    mod with_cel {
        use super::*;

        #[test]
        fn compile_check_accepts_valid_and_names_parse_errors() {
            assert!(compile_check("a.b >= 3 && c in ['x','y']").is_ok());
            let e = compile_check("a >=< 3").unwrap_err();
            assert!(e.contains("CEL parse"), "{e}");
        }

        #[test]
        fn eval_bool_computes_arithmetic_and_macros_over_variables() {
            let a = json!({"count": 7, "items": [{"s": "ok"}, {"s": "bad"}]});
            let b = json!({"limit": 5});
            let vars = vec![("a", &a), ("b", &b)];
            assert!(eval_bool("a.count + 1 > b.limit * 1", &vars).unwrap());
            assert!(eval_bool("a.items.exists(i, i.s == 'bad')", &vars).unwrap());
            assert!(eval_bool("a.items.filter(i, i.s == 'ok').size() == 1", &vars).unwrap());
            // Non-bool result is an error, not a coercion.
            assert!(eval_bool("a.count", &vars).is_err());
            // An undeclared reference is an eval error (callers fail closed).
            assert!(eval_bool("ghost > 1", &vars).is_err());
        }

        #[test]
        fn eval_value_shapes_json() {
            let scan = json!({"items": [{"id": 1, "ok": true}, {"id": 2, "ok": false}, {"id": 3, "ok": true}]});
            let vars = vec![("scan", &scan)];
            let v = eval_value("scan.items.filter(i, i.ok).map(i, i.id)", &vars).unwrap();
            assert_eq!(v, json!([1, 3]));
            let v = eval_value(
                "{'total': scan.items.size(), 'first': scan.items[0].id}",
                &vars,
            )
            .unwrap();
            assert_eq!(v, json!({"total": 3, "first": 1}));
        }
    }

    #[cfg(not(feature = "cel"))]
    #[test]
    fn without_the_feature_every_entry_point_names_it() {
        let v = json!(1);
        let vars = vec![("a", &v)];
        assert!(compile_check("a > 0").unwrap_err().contains("'cel'"));
        assert!(eval_bool("a > 0", &vars).unwrap_err().contains("'cel'"));
        assert!(eval_value("a", &vars).unwrap_err().contains("'cel'"));
    }
}
