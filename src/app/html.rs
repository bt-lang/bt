use crate::error::BtError;
use tauri::WebviewUrl;
use url::Url;

/// Virtual path for the unified startup error page under the `bt://app/` protocol.
pub const ERROR_ENTRY: &str = "__bt_error__/error.html";

/// Build the `bt://app/` load URL for the unified error page.
pub fn error_entry_url() -> Result<WebviewUrl, BtError> {
    let url = error_entry_url_value()?;
    Ok(WebviewUrl::External(url))
}

/// Build the unified error page URL value.
pub fn error_entry_url_value() -> Result<Url, BtError> {
    Url::parse(&format!("bt://app/{}", ERROR_ENTRY))
        .map_err(|err| BtError::Config(format!("Invalid error page URL: {}", err)))
}

/// Render the unified error page HTML.
pub fn render_error_html(title: &str, message: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="UTF-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>{}</title>
  <style>
    :root {{
      color-scheme: light;
      font-family: "Microsoft YaHei", "Segoe UI", system-ui, sans-serif;
      color: #20242c;
      background: #f6f7f9;
    }}
    * {{ box-sizing: border-box; }}
    body {{
      margin: 0;
      min-width: 320px;
      min-height: 100vh;
      display: grid;
      place-items: center;
      padding: 24px;
      background: #f6f7f9;
    }}
    main {{
      width: min(100%, 760px);
      display: grid;
      gap: 14px;
      justify-items: center;
      text-align: center;
    }}
    h1 {{
      margin: 0;
      font-size: 22px;
      line-height: 1.35;
      font-weight: 650;
    }}
    pre {{
      margin: 0;
      padding: 14px;
      overflow: auto;
      white-space: pre-wrap;
      word-break: break-word;
      border: 1px solid #f0b8b8;
      border-radius: 6px;
      background: #fff8f8;
      color: #8a1f1f;
      text-align: center;
      font: 14px/1.7 Consolas, "Microsoft YaHei", monospace;
    }}
  </style>
</head>
<body>
  <main>
    <h1>{}</h1>
    <pre>{}</pre>
  </main>
</body>
</html>"#,
        escape_html(title),
        escape_html(title),
        escape_html(message)
    )
}

/// Apply minimal entity escaping to HTML text.
pub fn escape_html(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            _ => output.push(ch),
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The error page must escape user-controlled titles and error details.
    #[test]
    fn render_error_html_escapes_content() {
        let html = render_error_html("A <B>", "x & y");

        assert!(html.contains("A &lt;B&gt;"));
        assert!(html.contains("x &amp; y"));
    }
}
