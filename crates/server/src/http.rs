//! Beacon's reply, and how it goes onto the shared transport (`httpd.rs`), which reads the
//! request off the socket and writes the response.

use crate::httpd::Response;
use beacon_core::Problem;
use serde_json::{Value, json};

pub use crate::httpd::Request;

/// An answer: a status, a JSON body (none for `304`), and extra headers.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: Option<Value>,
    pub headers: Vec<(&'static str, String)>,
}

impl Reply {
    pub fn json(status: u16, body: Value) -> Reply {
        Reply {
            status,
            body: Some(body),
            headers: Vec::new(),
        }
    }

    pub fn ok(body: Value) -> Reply {
        Reply::json(200, body)
    }

    /// Beacon's error shape (`spec/api.md`, "Errors").
    pub fn problems(status: u16, problems: &[Problem]) -> Reply {
        let first = problems
            .first()
            .cloned()
            .unwrap_or_else(|| Problem::new("request", "the request failed", "report it as a bug"));
        Reply::json(status, json!({"error": first, "problems": problems}))
    }

    pub fn problem(status: u16, at: &str, message: impl Into<String>, fix: impl Into<String>) -> Reply {
        Reply::problems(status, &[Problem::new(at, message, fix)])
    }

    pub fn with(mut self, name: &'static str, value: String) -> Reply {
        self.headers.push((name, value));
        self
    }
}

impl From<Reply> for Response {
    fn from(reply: Reply) -> Response {
        let body = match (&reply.body, reply.status) {
            (_, 304) | (None, _) => String::new(),
            (Some(body), _) => body.to_string(),
        };
        Response {
            status: reply.status,
            headers: reply.headers,
            body,
        }
    }
}
