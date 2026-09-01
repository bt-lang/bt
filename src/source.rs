//! BT source-file mode detection.
//!
//! A `#` directive is recognized only at the first character of the first line and selects the mode for the entire file.
//! Currently only `# TPL comment` is supported. Future `# XXX` directives should be dispatched here so file-level checks stay out of the lexer and parser.

/// Source-file execution mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceMode {
    /// Normal BT script.
    Script,
    /// Template file; all content after the directive line is treated as a template string.
    Template,
}

/// Result of source-file mode analysis.
#[derive(Debug, Clone)]
pub struct SourceDocument {
    /// Execution mode.
    pub mode: SourceMode,
    /// Source content that participates in execution.
    pub body: String,
    /// One-based line where `body` starts in the original file.
    pub body_line: usize,
}

/// Analyzes the directive on the first line of a source file.
pub fn analyze_source(file: &str, source: &str) -> Result<SourceDocument, String> {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    if !source.starts_with('#') {
        return Ok(SourceDocument {
            mode: SourceMode::Script,
            body: source.to_string(),
            body_line: 1,
        });
    }

    let (first_line, body) = split_first_line(source);
    let mut parts = first_line[1..].trim_start().splitn(2, char::is_whitespace);
    let mode = parts.next().unwrap_or("").trim();
    let _comment = parts.next().unwrap_or("").trim();
    match mode {
        "TPL" => Ok(SourceDocument {
            mode: SourceMode::Template,
            body: body.to_string(),
            body_line: 2,
        }),
        "" => Err(format!(
            "{}:1:1: The source file directive is missing a type, for example, `# TPL comment`",
            file
        )),
        other => Err(format!(
            "{}:1:1: Unsupported source-file directive `# {}`; only `# TPL` is currently supported",
            file, other
        )),
    }
}

/// Split the first line and the remaining text.
fn split_first_line(source: &str) -> (&str, &str) {
    if let Some(index) = source.find('\n') {
        let first = &source[..index].trim_end_matches('\r');
        let body = &source[index + 1..];
        (first, body)
    } else {
        (source.trim_end_matches('\r'), "")
    }
}
