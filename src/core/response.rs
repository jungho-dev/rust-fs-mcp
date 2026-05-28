use serde_json::{Map, Value, json};
use std::time::Duration;

const END_TOKEN: &str = "<|endoftext|>";
const SAFE_END_TOKEN: &str = "<|endoftext |>";

#[derive(Clone, Debug)]
pub struct RawResult {
    pub content: Vec<Value>,
    pub structured: Option<Value>,
    pub is_error: bool,
    pub meta: Map<String, Value>,
}

impl RawResult {
    // 1. Text result ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![text_content(text.into())],
            structured: None,
            is_error: false,
            meta: Map::new(),
        }
    }

    // 2. Structured result ―――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
    pub fn structured(text: impl Into<String>, structured: Value) -> Self {
        Self {
            content: vec![text_content(text.into())],
            structured: Some(sanitize_json(structured)),
            is_error: false,
            meta: Map::new(),
        }
    }

    // 3. Error result ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![text_content(format!("Error: {}", message.into()))],
            structured: None,
            is_error: true,
            meta: Map::new(),
        }
    }
}

// 4. Text content ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
pub fn text_content(text: String) -> Value {
    json!({
        "type": "text",
        "text": sanitize_owned(text)
    })
}

// 5. Normalize tool result ―――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
pub fn normalize_tool_result(tool_name: &str, result: RawResult, duration: Duration) -> Value {
    let is_error = result.is_error;
    let content = normalize_content(result.content);
    let text = combined_text(&content);
    let status = if is_error { "error" } else { "success" };
    let duration_ms = duration.as_millis() as u64;
    let structured = result.structured.unwrap_or(Value::Null);
    let has_structured = !structured.is_null();
    let error = if is_error {
        json!({ "message": text })
    } else {
        Value::Null
    };
    let error_message = if is_error {
        Value::String(text.clone())
    } else {
        Value::Null
    };
    let standard = json!({
        "data": {
            "content": content,
            "structuredContent": structured,
            "text": text
        },
        "durationMs": duration_ms,
        "error": error,
        "schemaVersion": 1,
        "status": status,
        "toolName": tool_name
    });
    let display = create_display_text(tool_name, status, &standard, duration_ms);
    let mut fs_meta = Map::new();
    fs_meta.insert("contentTypes".to_string(), json!(["text"]));
    fs_meta.insert("durationMs".to_string(), json!(duration_ms));
    fs_meta.insert("errorMessage".to_string(), error_message);
    fs_meta.insert("hasStructuredContent".to_string(), Value::Bool(has_structured));
    fs_meta.insert("schemaVersion".to_string(), json!(1));
    fs_meta.insert("status".to_string(), json!(status));
    fs_meta.insert("toolName".to_string(), json!(tool_name));
    fs_meta.extend(
        result
            .meta
            .into_iter()
            .map(|(key, value)| (key, sanitize_json(value))),
    );
    let meta = json!({ "fsMcpResult": fs_meta });
    let mut out = Map::new();
    out.insert("content".to_string(), json!([text_content(display)]));
    out.insert("structuredContent".to_string(), standard);
    out.insert("_meta".to_string(), meta);
    if is_error {
        out.insert("isError".to_string(), Value::Bool(true));
    }
    Value::Object(out)
}

// 6. Normalize content ―――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
fn normalize_content(content: Vec<Value>) -> Vec<Value> {
    if content.is_empty() {
        return vec![text_content(String::new())];
    }
    content
        .into_iter()
        .map(|item| {
            if item.get("type").and_then(Value::as_str) == Some("text") {
                let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                text_content(text.to_string())
            } else {
                sanitize_json(item)
            }
        })
        .collect()
}

// 7. Combined text ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
// 중간 `Vec<&str>` 할당 없이 사전 capacity 잡힌 String 으로 직접 누적한다.
fn combined_text(content: &[Value]) -> String {
    let mut total_len = 0usize;
    let mut first = true;
    for item in content {
        if let Some(text) = item.get("text").and_then(Value::as_str) {
            if !first {
                total_len += 1;
            }
            total_len += text.len();
            first = false;
        }
    }
    let mut out = String::with_capacity(total_len);
    let mut first = true;
    for item in content {
        if let Some(text) = item.get("text").and_then(Value::as_str) {
            if !first {
                out.push('\n');
            }
            out.push_str(text);
            first = false;
        }
    }
    out
}

// 8. Display text ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
fn create_display_text(
    tool_name: &str,
    status: &str,
    standard: &Value,
    duration_ms: u64,
) -> String {
    let items = standard["data"]["structuredContent"]["totalCount"]
        .as_u64()
        .or_else(|| {
            standard["data"]["structuredContent"]["results"]
                .as_array()
                .map(|items| items.len() as u64)
        })
        .unwrap_or(1);
    let duration = duration_ms as f64 / 1000.0;

    format!(
        "\u{1b}[38;5;214m―――――――――――――――――――――――――――――――――――\u{1b}[0m\n\
         \u{1b}[38;5;231m• tool = \u{1b}[38;2;0;180;216m{tool_name}\u{1b}[0m\n\
         \u{1b}[38;5;231m• items = \u{1b}[38;2;0;180;216m{items}\u{1b}[0m\n\
         \u{1b}[38;5;231m• status = \u{1b}[38;2;0;180;216m{status}\u{1b}[0m\n\
         \u{1b}[38;5;231m• duration = \u{1b}[38;2;0;180;216m{duration:.3} sec\u{1b}[0m\n\
         \u{1b}[38;5;214m―――――――――――――――――――――――――――――――――――\u{1b}[0m"
    )
}

// 9. Sanitize text ―――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
pub fn sanitize_text(value: &str) -> String {
    if value.contains(END_TOKEN) {
        value.replace(END_TOKEN, SAFE_END_TOKEN)
    } else {
        value.to_string()
    }
}

// 토큰 미포함 시 소유 String 을 그대로 이동해 불필요한 재할당을 피한다.
fn sanitize_owned(text: String) -> String {
    if text.contains(END_TOKEN) {
        text.replace(END_TOKEN, SAFE_END_TOKEN)
    } else {
        text
    }
}

// 10. Sanitize JSON ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
pub fn sanitize_json(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(sanitize_owned(text)),
        Value::Array(items) => Value::Array(items.into_iter().map(sanitize_json).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, sanitize_json(value)))
                .collect(),
        ),
        item => item,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_error_result() {
        let result = normalize_tool_result("x", RawResult::error("boom"), Duration::from_millis(1));
        assert_eq!(result["isError"], true);
        assert_eq!(result["structuredContent"]["status"], "error");
    }
}
