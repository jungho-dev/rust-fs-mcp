//! params.rs
//! tools::sqlite::params
//!
//! Reads the params / namedParams arguments of sqlite-query and binds them to each prepared statement.
//! JSON null, boolean, 64-bit integer, float, and string map to SQLite NULL, 1/0, INTEGER, REAL, and TEXT.
//!

use rusqlite::Statement;
use rusqlite::types::Value as SqlValue;
use serde_json::Value;

// 호출 단위 바인딩 값
pub(super) enum BindValues {
    Empty,
    Positional(Vec<SqlValue>),
    Named(Vec<(String, SqlValue)>),
}

// 1. Parameter reading --------------------------------------------------------
pub(super) fn read_bind_values(args: &Value) -> Result<BindValues, String> {
    // 1. JSON 값 변환
    let convert = |label: String, value: &Value| -> Result<SqlValue, String> {
        match value {
            Value::Null => Ok(SqlValue::Null),
            Value::Bool(flag) => Ok(SqlValue::Integer(i64::from(*flag))),
            Value::Number(number) if number.is_i64() => {
                Ok(SqlValue::Integer(number.as_i64().unwrap_or_default()))
            }
            Value::Number(number) if number.is_f64() => {
                Ok(SqlValue::Real(number.as_f64().unwrap_or_default()))
            }
            Value::Number(number) => Err(format!(
                "{label}: {number} does not fit a 64-bit signed integer"
            )),
            Value::String(text) => Ok(SqlValue::Text(text.clone())),
            Value::Array(_) | Value::Object(_) => Err(format!(
                "{label}: arrays and objects are not SQL values; pass JSON text as a string or a BLOB as unhex('...')"
            )),
        }
    };

    // 2. 인자 조합 확인
    let positional = args.get("params").filter(|value| !value.is_null());
    let named = args.get("namedParams").filter(|value| !value.is_null());
    if positional.is_some() && named.is_some() {
        return Err("pass params or namedParams, not both".to_string());
    }

    // 3. 위치 인자
    if let Some(positional) = positional {
        let entries = positional
            .as_array()
            .ok_or_else(|| "params must be an array".to_string())?;
        let values: Result<Vec<SqlValue>, String> = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| convert(format!("params[{index}]"), entry))
            .collect();
        return values.map(BindValues::Positional);
    }

    // 4. 이름 인자
    let Some(named) = named else {
        return Ok(BindValues::Empty);
    };
    let fields = named
        .as_object()
        .ok_or_else(|| "namedParams must be an object".to_string())?;
    let values: Result<Vec<(String, SqlValue)>, String> = fields
        .iter()
        .map(|(key, entry)| Ok((key.clone(), convert(format!("namedParams.{key}"), entry)?)))
        .collect();
    values.map(BindValues::Named)
}
// 2. Statement binding --------------------------------------------------------
// 이름 키는 접두어(:, @, $) 있음·없음 모두 허용
pub(super) fn bind_statement(
    statement: &mut Statement<'_>,
    values: &BindValues,
    statement_no: usize,
) -> Result<(), String> {
    for index in 1..=statement.parameter_count() {
        // 1. 값 조회
        let name = statement.parameter_name(index).map(str::to_string);
        let value = match values {
            BindValues::Positional(entries) => entries.get(index - 1).cloned(),
            BindValues::Named(entries) => entries
                .iter()
                .find(|(key, _)| {
                    name.as_deref()
                        .is_some_and(|name| name == key || name.get(1..) == Some(key.as_str()))
                })
                .map(|(_, value)| value.clone()),
            BindValues::Empty => None,
        };

        // 2. 값 누락
        let Some(value) = value else {
            return Err(match values {
                BindValues::Named(_) => format!(
                    "statement {statement_no}: parameter {} has no value in namedParams",
                    name.unwrap_or_else(|| format!("?{index}"))
                ),
                BindValues::Positional(_) | BindValues::Empty => {
                    format!("statement {statement_no}: parameter {index} has no value in params")
                }
            });
        };

        // 3. 바인딩
        statement
            .raw_bind_parameter(index, value)
            .map_err(|error| format!("SQLite error in statement {statement_no}: {error}"))?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn converts_json_values_to_sql_values() {
        let Ok(BindValues::Positional(values)) =
            read_bind_values(&json!({ "params": [null, true, 7, 1.5, "text"] }))
        else {
            panic!("positional values expected");
        };
        assert_eq!(
            values,
            vec![
                SqlValue::Null,
                SqlValue::Integer(1),
                SqlValue::Integer(7),
                SqlValue::Real(1.5),
                SqlValue::Text("text".to_string()),
            ]
        );
        let Ok(BindValues::Named(values)) =
            read_bind_values(&json!({ "namedParams": { "id": 3 } }))
        else {
            panic!("named values expected");
        };
        assert_eq!(values, vec![("id".to_string(), SqlValue::Integer(3))]);
        assert!(matches!(
            read_bind_values(&json!({ "params": null })),
            Ok(BindValues::Empty)
        ));
    }
    #[test]
    fn rejects_invalid_parameter_shapes() {
        let error = |args: Value| read_bind_values(&args).err().unwrap_or_default();
        assert_eq!(
            error(json!({ "params": [1], "namedParams": { "a": 1 } })),
            "pass params or namedParams, not both"
        );
        assert_eq!(
            error(json!({ "params": { "a": 1 } })),
            "params must be an array"
        );
        assert_eq!(
            error(json!({ "namedParams": [1] })),
            "namedParams must be an object"
        );
        assert_eq!(
            error(json!({ "params": [1, [2]] })),
            "params[1]: arrays and objects are not SQL values; pass JSON text as a string or a BLOB as unhex('...')"
        );
        assert_eq!(
            error(json!({ "namedParams": { "doc": { "a": 1 } } })),
            "namedParams.doc: arrays and objects are not SQL values; pass JSON text as a string or a BLOB as unhex('...')"
        );
        assert_eq!(
            error(json!({ "params": [18_446_744_073_709_551_615u64] })),
            "params[0]: 18446744073709551615 does not fit a 64-bit signed integer"
        );
    }
}
