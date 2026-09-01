//! BT HTML standard library.
//!
//! `html(text)` creates a lightweight HTML text object for escaping, unescaping, and stripping tags.
//! These domain-specific operations live here instead of bloating the core String prototype with HTML/XML behavior.

use crate::value::Value;

/// HTML text standard library object.
#[derive(Debug, Clone, PartialEq)]
pub struct BtHtml {
    /// The current HTML text.
    text: String,
}

impl BtHtml {
    /// Creates an HTML object.
    pub fn new(args: Vec<Value>) -> Result<Value, String> {
        let text = args.first().map(Value::to_string).unwrap_or_default();
        Ok(Value::Html(Self { text }))
    }

    /// Dispatches an HTML method.
    pub fn call_method(&self, method: &str, _args: Vec<Value>) -> Result<Value, String> {
        match method {
            "escape" => Ok(Value::Str(escape_html(&self.text))),
            "unescape" => Ok(Value::Str(unescape_html(&self.text))),
            "strip" => Ok(Value::Str(strip_html_tags(&self.text))),
            "to_string" => Ok(Value::Str(self.text.clone())),
            _ => Err(format!("html has no method `{}`", method)),
        }
    }
}

/// Escapes HTML special characters.
pub fn escape_html(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// Restore common HTML entities.
pub fn unescape_html(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// Strips HTML tags from a string.
///
/// The HTML tag boundary itself only uses ASCII bytes, and scanning directly by bytes can avoid character-by-character decoding; the output string is only allocated when a complete
/// `<...>` fragment is encountered, and ordinary plain text still goes through `to_string()` once.
pub fn strip_html_tags(text: &str) -> String {
    let bytes = text.as_bytes();
    let Some(first_start) = bytes.iter().position(|byte| *byte == b'<') else {
        return text.to_string();
    };
    let mut output = String::with_capacity(text.len());
    output.push_str(&text[..first_start]);

    let mut read = first_start;
    let mut copy_from = first_start;
    while read < bytes.len() {
        if bytes[read] != b'<' {
            read += 1;
            continue;
        }
        let Some(end_offset) = bytes[read + 1..].iter().position(|byte| *byte == b'>') else {
            break;
        };
        output.push_str(&text[copy_from..read]);
        read += end_offset + 2;
        copy_from = read;
    }
    output.push_str(&text[copy_from..]);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The HTML library should cover three common types of text processing: escaping, anti-escaping and tag stripping.
    #[test]
    fn html_methods_cover_escape_unescape_and_strip() {
        let Value::Html(html) = BtHtml::new(vec![Value::Str("<p>BT & \"Web\"</p>".to_string())])
            .expect("html() should create an HTML object")
        else {
            panic!("html() should return an Html value");
        };

        assert_eq!(
            html.call_method("escape", Vec::new()),
            Ok(Value::Str(
                "&lt;p&gt;BT &amp; &quot;Web&quot;&lt;/p&gt;".to_string()
            ))
        );
        assert_eq!(
            html.call_method("strip", Vec::new()),
            Ok(Value::Str("BT & \"Web\"".to_string()))
        );

        let Value::Html(html) = BtHtml::new(vec![Value::Str("&lt;b&gt;BT&lt;/b&gt;".to_string())])
            .expect("html() should create an HTML object")
        else {
            panic!("html() should return an Html value");
        };
        assert_eq!(
            html.call_method("unescape", Vec::new()),
            Ok(Value::Str("<b>BT</b>".to_string()))
        );
    }
}
