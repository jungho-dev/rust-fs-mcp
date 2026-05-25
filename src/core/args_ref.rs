use serde_json::{Map, Value};
use std::fs;
use std::path::Path;

// 1. Resolve tool args reference ―――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
pub fn resolve_tool_args(args: Option<Value>) -> Result<Value, String> {
    let Some(Value::Object(map)) = args else {
        return Ok(args.unwrap_or(Value::Object(Map::new())));
    };
    let Some(args_path) = map.get("args_path") else {
        return Ok(Value::Object(map));
    };
    let Some(args_path) = args_path.as_str() else {
        return Err("args_path must be a string".to_string());
    };
    let offset = optional_usize(&map, "args_offset")?.unwrap_or(0);
    let length = optional_usize(&map, "args_length")?;
    let args_text = read_text_slice(args_path, offset, length)?;
    let parsed: Value = serde_json::from_str(&args_text)
        .map_err(|error| format!("args_path must contain valid JSON: {error}"))?;
    let inline: Map<String, Value> = map
        .into_iter()
        .filter(|(key, _)| key != "args_path" && key != "args_offset" && key != "args_length")
        .collect();
    if inline.is_empty() {
        return Ok(parsed);
    }
    let Value::Object(mut parsed_map) = parsed else {
        return Err(
            "args_path JSON must be an object when inline overrides are provided".to_string(),
        );
    };
    for (key, value) in inline {
        parsed_map.insert(key, value);
    }
    Ok(Value::Object(parsed_map))
}

// 2. Read text slice ―――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
pub fn read_text_slice(
    path: impl AsRef<Path>,
    offset: usize,
    length: Option<usize>,
) -> Result<String, String> {
    let text = fs::read_to_string(path.as_ref())
        .map_err(|error| format!("Failed to read {}: {error}", path.as_ref().display()))?;
    let chars = text.chars().skip(offset);
    let result: String = match length {
        Some(length) => chars.take(length).collect(),
        None => chars.collect(),
    };
    Ok(result)
}

// 3. Optional usize ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
fn optional_usize(map: &Map<String, Value>, key: &str) -> Result<Option<usize>, String> {
    match map.get(key) {
        Some(value) => value
            .as_u64()
            .map(|value| Some(value as usize))
            .ok_or_else(|| format!("{key} must be a non-negative integer")),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolves_inline_args() {
        let args = resolve_tool_args(Some(json!({"x": 1}))).unwrap();
        assert_eq!(args["x"], 1);
    }
}
