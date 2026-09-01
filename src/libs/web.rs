//! BT Web response-control state.
//!
//! This module records response intent for a request without depending directly on Salvo or another Web framework.
//! Calls to `web.header()`, `web.status_code()`, `web.redirect()`, and `web.send_file()` update this state while the script runs.
//! After execution, the Web layer converts it into an HTTP response, keeping framework types out of the script hot path.

use crate::value::Value;
use indexmap::IndexMap;

/// Response-control state for a single Web request.
#[derive(Debug, Clone, PartialEq)]
pub struct BtWebResponse {
    /// Response headers to write.
    pub headers: IndexMap<String, String>,
    /// HTTP status code to be set.
    pub status_code: Option<u16>,
    /// Redirect target.
    pub redirect: Option<String>,
    /// The local file path to be sent directly.
    pub file: Option<String>,
}

impl BtWebResponse {
    /// Creates empty response-control state.
    pub fn new() -> Self {
        Self {
            headers: IndexMap::new(),
            status_code: None,
            redirect: None,
            file: None,
        }
    }

    /// Dispatches a response-control method.
    pub fn call_method(&mut self, method: &str, args: Vec<Value>) -> Result<Value, String> {
        match method {
            "header" => self.header(args),
            "status_code" => self.status_code(args),
            "redirect" => self.redirect(args),
            "send_file" => self.send_file(args),
            _ => Err(format!("web response control has no method `{}`", method)),
        }
    }

    /// Sets one or more response headers.
    fn header(&mut self, args: Vec<Value>) -> Result<Value, String> {
        match args.as_slice() {
            [Value::Object(values)] => {
                for (key, value) in values.borrow().iter() {
                    self.headers.insert(key.clone(), value.to_string());
                }
                Ok(Value::Bool(true))
            }
            [key, value, ..] => {
                self.headers.insert(key.to_string(), value.to_string());
                Ok(Value::Bool(true))
            }
            _ => Err("header() requires an object or key/value argument".to_string()),
        }
    }

    /// Sets the HTTP status code.
    fn status_code(&mut self, args: Vec<Value>) -> Result<Value, String> {
        let code = args
            .first()
            .map(Value::to_i64_lossy)
            .ok_or_else(|| "status_code() requires status code parameter".to_string())?;
        if !(100..=999).contains(&code) {
            return Err(format!(
                "HTTP status code `{}` is outside the valid range",
                code
            ));
        }
        self.status_code = Some(code as u16);
        Ok(Value::Bool(true))
    }

    /// Sets the redirect target.
    fn redirect(&mut self, args: Vec<Value>) -> Result<Value, String> {
        let url = args
            .first()
            .map(Value::to_string)
            .filter(|url| !url.is_empty())
            .ok_or_else(|| "redirect() requires a non-empty URL parameter".to_string())?;
        self.redirect = Some(url);
        self.file = None;
        self.status_code.get_or_insert(302);
        Ok(Value::Bool(true))
    }

    /// Sets the local file to send directly.
    ///
    /// The VM has already resolved the path against the current source directory
    /// and project root. After script execution, the Web layer passes the path to
    /// the HTTP framework for streaming, avoiding a large in-memory response string.
    fn send_file(&mut self, args: Vec<Value>) -> Result<Value, String> {
        let path = args
            .first()
            .map(Value::to_string)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| "send_file() requires a file path argument".to_string())?;
        self.file = Some(path);
        self.redirect = None;
        Ok(Value::Bool(true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `send_file()` should record the file path and override any earlier redirect.
    #[test]
    fn send_file_overrides_redirect() {
        let mut response = BtWebResponse::new();
        response
            .call_method("redirect", vec![Value::Str("/login".to_string())])
            .expect("redirection should be set up successfully");

        let result = response
            .call_method("send_file", vec![Value::Str("download.bin".to_string())])
            .expect("file direct export should be set up successfully");

        assert_eq!(result, Value::Bool(true));
        assert_eq!(response.file.as_deref(), Some("download.bin"));
        assert_eq!(response.redirect, None);
    }

    /// `send_file()` rejects empty paths rather than treating a project or process directory as a file.
    #[test]
    fn send_file_rejects_empty_path() {
        let mut response = BtWebResponse::new();
        assert!(response
            .call_method("send_file", vec![Value::Str(String::new())])
            .is_err());
    }
}
