//! A shared protocol tree renders identically as JSON, XML and safe JSONP.
use axum::{
    http::header,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::fmt::Write;
#[derive(Default)]
pub struct Params(pub Vec<(String, String)>);
impl Params {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }
    pub fn all(&self, key: &str) -> Vec<&str> {
        self.0
            .iter()
            .filter(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
            .collect()
    }
    pub fn require(&self, key: &str) -> Result<&str, Error> {
        self.get(key)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error {
                code: 10,
                message: format!("Required parameter is missing: {key}"),
            })
    }
    pub fn number(&self, key: &str, default: usize, max: usize) -> usize {
        self.get(key)
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(default)
            .min(max)
    }
    pub fn boolean(&self, key: &str, default: bool) -> bool {
        self.get(key).map_or(default, |value| {
            matches!(value.to_ascii_lowercase().as_str(), "true" | "1" | "yes")
        })
    }
}
#[derive(Debug)]
pub struct Error {
    pub code: u16,
    pub message: String,
}
impl From<crate::error::ApiError> for Error {
    fn from(error: crate::error::ApiError) -> Self {
        use crate::error::ApiError;
        let (code, message) = match error {
            ApiError::NotFound => (70, "Not found"),
            ApiError::Unauthorized(_) => (40, "Wrong username or password"),
            ApiError::Forbidden(_) => (50, "Not authorized"),
            ApiError::Unprocessable(_) | ApiError::BadRequest(_) => (10, "Invalid parameters"),
            ApiError::Conflict(_) => (0, "Music changed; reload and retry"),
            ApiError::TooManyRequests => (0, "Too many requests"),
            _ => (0, "Server temporarily unavailable"),
        };
        Self {
            code,
            message: message.into(),
        }
    }
}
pub fn version(params: &Params) -> Result<(), Error> {
    if let Some(raw) = params.get("v")
        && let Ok(parts) = raw
            .split('.')
            .map(str::parse::<u32>)
            .collect::<Result<Vec<_>, _>>()
        && parts.as_slice() > [1, 16, 1].as_slice()
    {
        return Err(Error {
            code: 30,
            message: "Client requires a newer protocol than 1.16.1".into(),
        });
    }
    Ok(())
}
fn callback(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value.split('.').all(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .is_some_and(|ch| ch.is_ascii_alphabetic() || matches!(ch, '_' | '$'))
                && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '$'))
        })
}
fn escape(value: &str) -> String {
    value
        .chars()
        .filter(|ch| *ch >= ' ' || matches!(ch, '\n' | '\r' | '\t'))
        .fold(String::new(), |mut out, ch| {
            match ch {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&apos;"),
                _ => out.push(ch),
            }
            out
        })
}

fn scalar(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}
fn element(out: &mut String, name: &str, value: &Value) {
    match value {
        Value::Array(items) => {
            for item in items {
                element(out, name, item);
            }
        }
        Value::Null => {}
        Value::Object(fields) => {
            let _ = write!(out, "<{name}");
            for (key, item) in fields {
                if !item.is_null() && !item.is_array() && !item.is_object() {
                    let _ = write!(out, " {key}=\"{}\"", escape(&scalar(item)));
                }
            }
            out.push('>');
            for (key, item) in fields {
                if item.is_array() || item.is_object() {
                    element(out, key, item);
                }
            }
            let _ = write!(out, "</{name}>");
        }
        _ => {
            let _ = write!(out, "<{name}>{}</{name}>", escape(&scalar(value)));
        }
    }
}
pub fn render(params: &Params, result: Result<Value, Error>) -> Response {
    let mut envelope = json!({"status":"ok","version":"1.16.1","type":"plamenu","serverVersion":env!("CARGO_PKG_VERSION"),"openSubsonic":true});
    match result {
        Ok(Value::Object(fields)) => envelope.as_object_mut().expect("object").extend(fields),
        Ok(_) => {}
        Err(error) => {
            envelope["status"] = json!("failed");
            envelope["error"] = json!({"code":error.code,"message":error.message});
        }
    }
    let format = params.get("f").unwrap_or("xml").to_ascii_lowercase();
    let (body, content_type) = if matches!(format.as_str(), "json" | "jsonp") {
        let json = json!({"subsonic-response":envelope}).to_string();
        if format == "jsonp"
            && let Some(callback) = params.get("callback").filter(|value| callback(value))
        {
            (
                format!("{callback}({json})"),
                "text/javascript; charset=utf-8",
            )
        } else {
            (json, "application/json; charset=utf-8")
        }
    } else {
        envelope["xmlns"] = json!("http://subsonic.org/restapi");
        let mut xml = "<?xml version=\"1.0\" encoding=\"utf-8\"?>".to_owned();
        element(&mut xml, "subsonic-response", &envelope);
        (xml, "text/xml; charset=utf-8")
    };
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "private, no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::VARY, "Authorization, Cookie"),
        ],
        body,
    )
        .into_response()
}
