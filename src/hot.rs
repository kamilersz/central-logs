//! Hot attribute configuration: which JSON keys to promote to typed top-level
//! DuckDB columns for fast filtering (architecture: telemetry optimization).
//!
//! See `docs/ARCHITECTURE.md` §3 + the "telemetry optimization" design: a
//! configurable set of frequently-filtered keys are extracted from the JSON
//! envelope at parse time and stored as their own typed columns. DuckDB's
//! per-row-group min/max zonemaps make `WHERE user_id = 42` essentially free,
//! vs. `WHERE attributes->>'$.user_id' = '42'` which parses JSON per row.

use serde::{Deserialize, Serialize};

/// DuckDB column type for a promoted attribute. Limited to the types that make
/// sense for filter keys (numbers, strings, booleans).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HotType {
    Bigint,
    Double,
    Varchar,
    Boolean,
}

impl HotType {
    /// DuckDB DDL type name.
    pub fn ddl(&self) -> &'static str {
        match self {
            HotType::Bigint => "BIGINT",
            HotType::Double => "DOUBLE",
            HotType::Varchar => "VARCHAR",
            HotType::Boolean => "BOOLEAN",
        }
    }
}

impl std::fmt::Display for HotType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.ddl())
    }
}

/// One promoted attribute: a column name, its DuckDB type, and the JSON path
/// inside the envelope to extract it from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotAttribute {
    /// Column name in the `logs` table (must be a valid SQL identifier).
    pub name: String,
    /// DuckDB type for the column.
    #[serde(default = "default_type")]
    pub duckdb_type: HotType,
    /// JSON path to extract: either bare `user_id` or `$.user_id` or
    /// `$.ctx.user_id` (dotted). Top-level keys are by far the common case.
    pub json_path: String,
}

fn default_type() -> HotType {
    HotType::Varchar
}

impl HotAttribute {
    /// Build from a CLI/TOML shorthand string like `user_id:bigint` or
    /// `route:varchar:$.request.route`. The third segment is optional and
    /// defaults to `$.<name>`.
    pub fn parse_shorthand(s: &str) -> Result<Self, String> {
        let parts: Vec<&str> = s.splitn(3, ':').collect();
        let (name, ty, path): (&str, &str, String) = match parts.as_slice() {
            [name, ty] => (*name, *ty, format!("$.{name}")),
            [name, ty, path] => (*name, *ty, (*path).to_string()),
            [name] => (*name, "varchar", format!("$.{name}")),
            _ => return Err(format!("invalid hot-attribute spec: {s}")),
        };
        let duckdb_type = match ty.to_ascii_lowercase().as_str() {
            "bigint" | "i64" | "int" | "long" => HotType::Bigint,
            "double" | "f64" | "float" => HotType::Double,
            "varchar" | "text" | "string" => HotType::Varchar,
            "boolean" | "bool" => HotType::Boolean,
            other => return Err(format!("unknown hot type: {other}")),
        };
        // Sanity-check the column name to prevent SQL injection via DDL.
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || name.is_empty()
            || name.chars().next().unwrap().is_ascii_digit()
        {
            return Err(format!(
                "invalid column name '{name}' (must be [a-z_][a-z0-9_]*)"
            ));
        }
        Ok(Self {
            name: name.to_string(),
            duckdb_type,
            json_path: path.to_string(),
        })
    }
}

/// A typed value extracted for a hot attribute. Parallel to the configured
/// `Vec<HotAttribute>` list — `hot[i]` corresponds to attribute `i`.
#[derive(Debug, Clone, PartialEq)]
pub enum HotValue {
    Null,
    Bigint(i64),
    Double(f64),
    Varchar(String),
    Boolean(bool),
}

impl HotValue {
    pub fn is_null(&self) -> bool {
        matches!(self, HotValue::Null)
    }
}

/// Resolve `path` against a JSON object, returning the value if found.
/// Supports both `$.field` and bare `field` and dotted `$.a.b.c`.
pub fn resolve_json_path<'v>(
    root: &'v serde_json::Value,
    path: &str,
) -> Option<&'v serde_json::Value> {
    let trimmed = path.strip_prefix("$.").unwrap_or(path);
    let mut cur = root;
    for seg in trimmed.split('.') {
        if seg.is_empty() {
            continue;
        }
        match cur {
            serde_json::Value::Object(map) => {
                cur = map.get(seg)?;
            }
            _ => return None,
        }
    }
    Some(cur)
}

/// Coerce a JSON value to the requested type. Returns None on type mismatch
/// (the row gets a SQL NULL for that column).
pub fn coerce(value: &serde_json::Value, ty: HotType) -> HotValue {
    use serde_json::Value;
    match (value, ty) {
        (Value::Null, _) => HotValue::Null,
        (Value::Bool(b), HotType::Boolean) => HotValue::Boolean(*b),
        (Value::Bool(b), HotType::Bigint) => HotValue::Bigint(*b as i64),
        (Value::Bool(b), HotType::Double) => HotValue::Double(if *b { 1.0 } else { 0.0 }),
        (Value::Bool(b), HotType::Varchar) => HotValue::Varchar(b.to_string()),

        (Value::Number(n), HotType::Bigint) => match n.as_i64() {
            Some(i) => HotValue::Bigint(i),
            None => match n.as_f64() {
                Some(f) => HotValue::Bigint(f as i64),
                None => HotValue::Null,
            },
        },
        (Value::Number(n), HotType::Double) => match n.as_f64() {
            Some(f) => HotValue::Double(f),
            None => HotValue::Null,
        },
        (Value::Number(n), HotType::Varchar) => HotValue::Varchar(n.to_string()),
        (Value::Number(n), HotType::Boolean) => match n.as_i64() {
            Some(0) => HotValue::Boolean(false),
            Some(1) => HotValue::Boolean(true),
            _ => HotValue::Null,
        },

        (Value::String(s), HotType::Varchar) => HotValue::Varchar(s.clone()),
        (Value::String(s), HotType::Bigint) => match s.parse::<i64>() {
            Ok(i) => HotValue::Bigint(i),
            Err(_) => HotValue::Null,
        },
        (Value::String(s), HotType::Double) => match s.parse::<f64>() {
            Ok(f) => HotValue::Double(f),
            Err(_) => HotValue::Null,
        },
        (Value::String(s), HotType::Boolean) => match s.to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" | "on" => HotValue::Boolean(true),
            "false" | "no" | "0" | "off" => HotValue::Boolean(false),
            _ => HotValue::Null,
        },

        // Arrays/objects can't be coerced losslessly to a scalar hot column.
        (Value::Array(_) | Value::Object(_), _) => HotValue::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_shorthand_short() {
        let h = HotAttribute::parse_shorthand("user_id:bigint").unwrap();
        assert_eq!(h.name, "user_id");
        assert_eq!(h.duckdb_type, HotType::Bigint);
        assert_eq!(h.json_path, "$.user_id");
    }

    #[test]
    fn parse_shorthand_full() {
        let h = HotAttribute::parse_shorthand("route:varchar:$.request.route").unwrap();
        assert_eq!(h.name, "route");
        assert_eq!(h.json_path, "$.request.route");
    }

    #[test]
    fn parse_shorthand_default_type() {
        let h = HotAttribute::parse_shorthand("env").unwrap();
        assert_eq!(h.duckdb_type, HotType::Varchar);
    }

    #[test]
    fn parse_shorthand_rejects_bad_name() {
        assert!(HotAttribute::parse_shorthand("1abc:int").is_err());
        assert!(HotAttribute::parse_shorthand("col with space:int").is_err());
        assert!(HotAttribute::parse_shorthand("; DROP TABLE:bigint").is_err());
    }

    #[test]
    fn resolve_dotted() {
        let v = json!({"ctx": {"user_id": 42}});
        let r = resolve_json_path(&v, "$.ctx.user_id").unwrap();
        assert_eq!(r, &json!(42));
    }

    #[test]
    fn resolve_bare() {
        let v = json!({"user_id": 7});
        let r = resolve_json_path(&v, "user_id").unwrap();
        assert_eq!(r, &json!(7));
    }

    #[test]
    fn coerce_int_from_float_string() {
        let v = json!("42");
        let got = coerce(&v, HotType::Bigint);
        assert_eq!(got, HotValue::Bigint(42));
    }

    #[test]
    fn coerce_returns_null_on_type_mismatch() {
        let v = json!([1, 2, 3]);
        let got = coerce(&v, HotType::Bigint);
        assert_eq!(got, HotValue::Null);
    }

    #[test]
    fn coerce_string_to_bool() {
        assert_eq!(
            coerce(&json!("true"), HotType::Boolean),
            HotValue::Boolean(true)
        );
        assert_eq!(
            coerce(&json!("no"), HotType::Boolean),
            HotValue::Boolean(false)
        );
        assert_eq!(coerce(&json!("maybe"), HotType::Boolean), HotValue::Null);
    }
}
