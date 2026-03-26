use std::io::Read;

/// Strip trailing commas from JSON text that has already had comments removed.
/// Handles commas before `]` and `}`, skipping content inside strings.
pub fn strip_trailing_commas(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escape = false;

    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => {
                in_string = true;
                out.push(ch);
            }
            ',' => {
                // Look ahead past whitespace to see if the next meaningful char is ] or }
                let mut whitespace = String::new();
                let mut is_trailing = false;
                while let Some(&next) = chars.peek() {
                    if next.is_ascii_whitespace() {
                        whitespace.push(next);
                        chars.next();
                    } else {
                        is_trailing = next == ']' || next == '}';
                        break;
                    }
                }
                if is_trailing {
                    // Drop the comma, keep the whitespace
                    out.push_str(&whitespace);
                } else {
                    out.push(',');
                    out.push_str(&whitespace);
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Parse JSONC text (with comments and trailing commas) into a serde_json Value.
pub fn parse_jsonc<T: serde::de::DeserializeOwned>(raw: &str) -> serde_json::Result<T> {
    let mut comment_stripped = String::new();
    json_comments::StripComments::new(raw.as_bytes())
        .read_to_string(&mut comment_stripped)
        .expect("StripComments read_to_string should not fail on valid UTF-8");
    let clean = strip_trailing_commas(&comment_stripped);
    serde_json::from_str(&clean)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_trailing_comma_object() {
        let input = r#"{"a": 1, "b": 2,}"#;
        let result = strip_trailing_commas(input);
        assert_eq!(result, r#"{"a": 1, "b": 2}"#);
    }

    #[test]
    fn test_strip_trailing_comma_array() {
        let input = r#"[1, 2, 3,]"#;
        let result = strip_trailing_commas(input);
        assert_eq!(result, r#"[1, 2, 3]"#);
    }

    #[test]
    fn test_strip_trailing_comma_nested() {
        let input = r#"{"features": {"a": {},}, "ports": [80, 443,],}"#;
        let result = strip_trailing_commas(input);
        assert_eq!(result, r#"{"features": {"a": {}}, "ports": [80, 443]}"#);
    }

    #[test]
    fn test_no_trailing_comma() {
        let input = r#"{"a": 1, "b": 2}"#;
        let result = strip_trailing_commas(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_comma_in_string_preserved() {
        let input = r#"{"msg": "hello, world",}"#;
        let result = strip_trailing_commas(input);
        assert_eq!(result, r#"{"msg": "hello, world"}"#);
    }

    #[test]
    fn test_trailing_comma_with_whitespace() {
        let input = "{\n  \"a\": 1,\n}";
        let result = strip_trailing_commas(input);
        assert_eq!(result, "{\n  \"a\": 1\n}");
    }

    #[test]
    fn test_parse_jsonc_with_comments_and_trailing_commas() {
        let input = r#"{
            // This is a comment
            "name": "test",
            "image": "ubuntu:latest",
        }"#;
        let value: serde_json::Value = parse_jsonc(input).unwrap();
        assert_eq!(value["name"], "test");
        assert_eq!(value["image"], "ubuntu:latest");
    }
}
