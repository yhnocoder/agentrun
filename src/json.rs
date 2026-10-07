use serde_json::Value;

pub fn string(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

pub fn optional_string(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

pub fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

pub fn joined_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn joined_text_takes_text_blocks_only() {
        assert_eq!(joined_text(&json!("plain")), "plain");
        assert_eq!(
            joined_text(&json!([
                {"type": "text", "text": "one"},
                {"type": "thinking", "thinking": "skip", "text": "skip"},
                {"type": "text", "text": "two"},
                {"type": "text"}
            ])),
            "one\ntwo"
        );
        assert_eq!(joined_text(&json!(null)), "");
        assert_eq!(joined_text(&json!(3)), "");
    }

    #[test]
    fn first_line_and_string_helpers() {
        assert_eq!(first_line("a\nb"), "a");
        assert_eq!(first_line(""), "");
        assert_eq!(string(&json!("x")), "x");
        assert_eq!(string(&json!(1)), "");
        assert_eq!(optional_string(&json!("x")), Some("x".to_string()));
        assert_eq!(optional_string(&json!(null)), None);
    }
}
