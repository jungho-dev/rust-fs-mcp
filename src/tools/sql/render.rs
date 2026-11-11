//! render.rs
//! tools::sql::render
//!
//! Renders a MySQL text-protocol result set as one compact JSON object per row, columns in SELECT order.
//! Integers and floats become JSON numbers; DECIMAL and temporal values stay strings so no digit is lost;
//! JSON columns embed their text verbatim; binary values that are not printable UTF-8 become 0x-prefixed hex.
//!

use crate::core::sql::{BINARY_CHARSET, ColumnMeta, ResultSet, column_type};
use serde::de::IgnoredAny;
use serde_json::Number;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;

// 표시할 이진 값 상한
const HEX_PREVIEW_BYTES: usize = 256;

// 1. Row rendering ------------------------------------------------------------
pub(super) fn render_result_rows(result: &ResultSet) -> String {
    // 1. 중복 열 이름 구분
    let mut names: Vec<String> = Vec::with_capacity(result.columns.len());
    let mut taken: HashSet<String> = HashSet::with_capacity(result.columns.len());
    let mut next_serial: HashMap<&str, usize> = HashMap::new();
    for column in &result.columns {
        let mut name = column.name.clone();
        if taken.contains(&name) {
            let serial = next_serial.entry(column.name.as_str()).or_insert(2);
            loop {
                name = format!("{}_{serial}", column.name);
                *serial += 1;
                if !taken.contains(&name) {
                    break;
                }
            }
        }
        taken.insert(name.clone());
        names.push(name);
    }
    let keys: Vec<String> = names
        .iter()
        .map(|name| serde_json::to_string(name).unwrap_or_default())
        .collect();

    // 2. 행별 JSON 객체
    let mut body = String::from("[");
    for (row_index, row) in result.rows.iter().enumerate() {
        body.push_str(if row_index == 0 { "\n{" } else { ",\n{" });
        for (cell_index, ((key, column), cell)) in
            keys.iter().zip(&result.columns).zip(row).enumerate()
        {
            if cell_index > 0 {
                body.push(',');
            }
            body.push_str(key);
            body.push(':');
            body.push_str(&render_cell(column, cell.as_deref()));
        }
        body.push('}');
    }
    body.push_str(if result.rows.is_empty() { "]" } else { "\n]" });
    body
}
// 2. Cell rendering -----------------------------------------------------------
fn render_cell(column: &ColumnMeta, cell: Option<&[u8]>) -> String {
    let Some(bytes) = cell else {
        return "null".to_string();
    };
    let text = String::from_utf8_lossy(bytes);
    let quoted = || serde_json::to_string(text.as_ref()).unwrap_or_default();
    match column.column_type {
        // 정수 (ZEROFILL 0 채움 제거)
        column_type::TINY
        | column_type::SHORT
        | column_type::LONG
        | column_type::LONGLONG
        | column_type::INT24
        | column_type::YEAR => {
            if let Ok(signed) = text.parse::<i64>() {
                signed.to_string()
            } else if let Ok(unsigned) = text.parse::<u64>() {
                unsigned.to_string()
            } else {
                quoted()
            }
        }
        column_type::FLOAT | column_type::DOUBLE => text
            .parse::<f64>()
            .ok()
            .and_then(Number::from_f64)
            .map_or_else(quoted, |number| number.to_string()),
        // 유효 JSON 원문 삽입
        column_type::JSON if serde_json::from_slice::<IgnoredAny>(bytes).is_ok() => {
            text.into_owned()
        }
        column_type::BIT if bytes.len() <= 8 => bytes
            .iter()
            .fold(0u64, |bits, byte| bits << 8 | u64::from(*byte))
            .to_string(),
        // 출력 불가 이진 값
        _ if column.charset == BINARY_CHARSET
            && (std::str::from_utf8(bytes).is_err()
                || text
                    .chars()
                    .any(|ch| ch.is_control() && !matches!(ch, '\t' | '\n' | '\r'))) =>
        {
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
        }
        _ => quoted(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str, column_type: u8, charset: u16) -> ColumnMeta {
        ColumnMeta {
            name: name.to_string(),
            column_type,
            flags: 0,
            charset,
            decimals: 0,
        }
    }
    fn cell(text: &str) -> Option<Vec<u8>> {
        Some(text.as_bytes().to_vec())
    }
    #[test]
    fn renders_typed_cells_in_column_order() {
        let result = ResultSet {
            columns: vec![
                column("zeta", column_type::LONGLONG, BINARY_CHARSET),
                column("amount", column_type::NEWDECIMAL, BINARY_CHARSET),
                column("ratio", column_type::DOUBLE, BINARY_CHARSET),
                column("created", column_type::DATETIME, BINARY_CHARSET),
                column("note", column_type::VAR_STRING, 224),
                column("payload", column_type::JSON, BINARY_CHARSET),
                column("flags", column_type::BIT, BINARY_CHARSET),
                column("missing", column_type::VAR_STRING, 224),
            ],
            rows: vec![vec![
                cell("18446744073709551615"),
                cell("123.450"),
                cell("0.5"),
                cell("2026-11-11 10:20:30"),
                cell("\"quoted\" 한글\nline"),
                cell("{\"b\": 1, \"a\": [1, 2]}"),
                Some(vec![0x01, 0x02]),
                None,
            ]],
            truncated: false,
        };
        assert_eq!(
            render_result_rows(&result),
            "[\n{\"zeta\":18446744073709551615,\"amount\":\"123.450\",\"ratio\":0.5,\"created\":\"2026-11-11 10:20:30\",\"note\":\"\\\"quoted\\\" 한글\\nline\",\"payload\":{\"b\": 1, \"a\": [1, 2]},\"flags\":258,\"missing\":null}\n]"
        );
    }
    #[test]
    fn renders_binary_zerofill_and_duplicate_names() {
        let result = ResultSet {
            columns: vec![
                column("id", column_type::LONG, BINARY_CHARSET),
                column("id", column_type::LONG, BINARY_CHARSET),
                column("hash", column_type::VAR_STRING, BINARY_CHARSET),
                column("token", column_type::VAR_STRING, BINARY_CHARSET),
                column("broken", column_type::JSON, BINARY_CHARSET),
            ],
            rows: vec![
                vec![
                    cell("0042"),
                    cell("-7"),
                    Some(vec![0x00, 0xFF, 0x10]),
                    cell("printable-text"),
                    cell("{not json"),
                ],
                vec![None, None, None, None, None],
            ],
            truncated: false,
        };
        assert_eq!(
            render_result_rows(&result),
            "[\n{\"id\":42,\"id_2\":-7,\"hash\":\"0x00FF10\",\"token\":\"printable-text\",\"broken\":\"{not json\"},\n{\"id\":null,\"id_2\":null,\"hash\":null,\"token\":null,\"broken\":null}\n]"
        );
    }
    #[test]
    fn renders_empty_result_and_long_binary_preview() {
        assert_eq!(render_result_rows(&ResultSet::default()), "[]");
        let rendered = render_cell(
            &column("blob", column_type::BLOB, BINARY_CHARSET),
            Some(&[0xAB; 300]),
        );
        assert!(rendered.starts_with("\"0xABAB"), "{rendered}");
        assert!(rendered.ends_with("... (300 bytes)\""), "{rendered}");
        assert_eq!(rendered.len(), 3 + 512 + 15 + 1);
    }
}
