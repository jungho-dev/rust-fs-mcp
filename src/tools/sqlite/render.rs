//! render.rs
//! tools::sqlite::render
//!
//! Renders SQLite rows as one compact JSON object per row, columns in SELECT order.
//! INTEGER and REAL become JSON numbers, TEXT a JSON string, and BLOB or non-UTF-8 TEXT 0x-prefixed hex.
//!

use rusqlite::Row;
use rusqlite::types::ValueRef;
use serde_json::Number;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;

// 표시할 이진 값 상한
const HEX_PREVIEW_BYTES: usize = 256;

// 1. Column keys --------------------------------------------------------------
pub(super) fn render_column_keys(columns: &[String]) -> Vec<String> {
    // 1. 중복 열 이름 구분
    let mut names: Vec<String> = Vec::with_capacity(columns.len());
    let mut taken: HashSet<String> = HashSet::with_capacity(columns.len());
    let mut next_serial: HashMap<&str, usize> = HashMap::new();
    for column in columns {
        let mut name = column.clone();
        if taken.contains(&name) {
            let serial = next_serial.entry(column.as_str()).or_insert(2);
            loop {
                name = format!("{column}_{serial}");
                *serial += 1;
                if !taken.contains(&name) {
                    break;
                }
            }
        }
        taken.insert(name.clone());
        names.push(name);
    }

    // 2. JSON 키 인코딩
    names
        .iter()
        .map(|name| serde_json::to_string(name).unwrap_or_default())
        .collect()
}
// 2. Row rendering ------------------------------------------------------------
pub(super) fn render_row(keys: &[String], row: &Row<'_>) -> rusqlite::Result<String> {
    // 1. 이진 값 hex
    let render_hex = |bytes: &[u8]| -> String {
        let shown = &bytes[..bytes.len().min(HEX_PREVIEW_BYTES)];
        let mut hex = String::with_capacity(shown.len() * 2 + 32);
        hex.push_str("\"0x");
        for byte in shown {
            let _ = write!(hex, "{byte:02X}");
        }
        if shown.len() < bytes.len() {
            let _ = write!(hex, "... ({} bytes)", bytes.len());
        }
        hex.push('"');
        hex
    };

    // 2. 열별 JSON 값
    let mut line = String::from("{");
    for (index, key) in keys.iter().enumerate() {
        if index > 0 {
            line.push(',');
        }
        line.push_str(key);
        line.push(':');
        let cell = match row.get_ref(index)? {
            ValueRef::Null => "null".to_string(),
            ValueRef::Integer(integer) => integer.to_string(),
            ValueRef::Real(real) if real.is_infinite() && real.is_sign_positive() => {
                "\"Infinity\"".to_string()
            }
            ValueRef::Real(real) if real.is_infinite() => "\"-Infinity\"".to_string(),
            ValueRef::Real(real) => Number::from_f64(real)
                .map_or_else(|| "null".to_string(), |number| number.to_string()),
            ValueRef::Text(bytes) => std::str::from_utf8(bytes).map_or_else(
                |_| render_hex(bytes),
                |text| serde_json::to_string(text).unwrap_or_default(),
            ),
            ValueRef::Blob(bytes) => render_hex(bytes),
        };
        line.push_str(&cell);
    }
    line.push('}');
    Ok(line)
}
#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn render_query(sql: &str) -> String {
        let conn = Connection::open_in_memory().unwrap();
        let mut statement = conn.prepare(sql).unwrap();
        let columns: Vec<String> = statement.column_names().map(str::to_string).collect();
        let keys = render_column_keys(&columns);
        statement
            .query_row([], |row| render_row(&keys, row))
            .unwrap()
    }
    #[test]
    fn renders_typed_cells_in_column_order() {
        assert_eq!(
            render_query(
                "SELECT 42 AS zeta, 0.5 AS ratio, '\"q\" 한글' AS note, NULL AS missing, 1e999 AS high, -1e999 AS low, x'00FF10' AS hash, CAST(x'FF' AS TEXT) AS broken"
            ),
            "{\"zeta\":42,\"ratio\":0.5,\"note\":\"\\\"q\\\" 한글\",\"missing\":null,\"high\":\"Infinity\",\"low\":\"-Infinity\",\"hash\":\"0x00FF10\",\"broken\":\"0xFF\"}"
        );
    }
    #[test]
    fn renders_duplicate_names_and_long_blob_preview() {
        let keys = render_column_keys(&["id".to_string(), "id".to_string(), "id_2".to_string()]);
        assert_eq!(keys, vec!["\"id\"", "\"id_2\"", "\"id_2_2\""]);
        let rendered = render_query("SELECT zeroblob(300) AS blob");
        assert!(rendered.starts_with("{\"blob\":\"0x0000"), "{rendered}");
        assert!(rendered.ends_with("... (300 bytes)\"}"), "{rendered}");
        assert_eq!(rendered.len(), 8 + 3 + 512 + 15 + 1 + 1);
    }
}
