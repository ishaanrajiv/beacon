//! Talking to Beacon: one request, retried like the emitters (`spec/clients.md`): no answer, `408`,
//! `429` or `5xx` is tried again after 1 s, then 3 s, three attempts at most, each within 5 s, all
//! with one Idempotency-Key.

use crate::Fail;
use beacon_core::Problem;
use serde_json::Value;
use std::time::Duration;

/// Waits before the second and third attempts. Tests don't wait.
const RETRY_DELAYS: [Duration; 2] = if cfg!(test) {
    [Duration::ZERO, Duration::ZERO]
} else {
    [Duration::from_secs(1), Duration::from_secs(3)]
};
/// Each attempt's limit.
const TIMEOUT: Duration = Duration::from_secs(5);

pub struct Http {
    url: String,
    token: Option<String>,
    agent: ureq::Agent,
    /// One per invocation: every request and retry of this command carries it.
    key: String,
}

/// An answer: its status and body.
pub struct Answer {
    pub status: u16,
    pub body: String,
}

impl Answer {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    /// The problems in an error answer: Beacon's `problems`, else its `error`, else the text.
    pub fn problems(&self) -> Vec<Problem> {
        let value = self.json();
        let read = |v: &Value| serde_json::from_value::<Problem>(v.clone()).ok();
        if let Some(list) = value.get("problems").and_then(Value::as_array) {
            let problems: Vec<Problem> = list.iter().filter_map(read).collect();
            if !problems.is_empty() {
                return problems;
            }
        }
        if let Some(problem) = value.get("error").and_then(read) {
            return vec![problem];
        }
        let short: String = self.body.chars().take(300).collect();
        vec![Problem::new(
            "beacon",
            format!("Beacon answered {}: {short}", self.status),
            "",
        )]
    }

    /// The answer as a command's result: `Ok` for a 2xx, else refused (1) or unanswered (3).
    pub fn into_result(self) -> Result<Answer, Fail> {
        if self.ok() {
            Ok(self)
        } else if self.status >= 500 {
            Err(Fail::unreachable(self.problems()))
        } else {
            Err(Fail::refused(self.problems()))
        }
    }
}

fn idempotency_key() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        // No randomness from the system: the time and process are unique enough for one key.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        bytes = (nanos ^ (u128::from(std::process::id()) << 64)).to_be_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Http {
    pub fn new(url: String) -> Http {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .http_status_as_error(false)
            .build();
        Http {
            url,
            token: std::env::var("BEACON_TOKEN").ok().filter(|t| !t.is_empty()),
            agent: ureq::Agent::new_with_config(config),
            key: idempotency_key(),
        }
    }

    /// This invocation's Idempotency-Key.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// One `POST` with a key of its own (a spooled request's), not retried. `Err` is no answer.
    pub fn once(&self, path: &str, body: &str, key: &str) -> Result<Answer, String> {
        self.attempt("POST", path, Some(body), key)
    }

    /// One attempt. `Err` is no answer.
    fn attempt(&self, method: &str, path: &str, body: Option<&str>, key: &str) -> Result<Answer, String> {
        if self.url.starts_with("https://") {
            return Err("this command speaks plain HTTP, and the address is https".to_string());
        }
        let url = format!("{}{path}", self.url);
        let auth = self.token.as_ref().map(|token| format!("Bearer {token}"));
        let response = match (method, body) {
            ("GET" | "DELETE", _) | (_, None) => {
                let mut request = if method == "DELETE" {
                    self.agent.delete(&url)
                } else {
                    self.agent.get(&url)
                };
                if let Some(auth) = &auth {
                    request = request.header("Authorization", auth);
                }
                request.call()
            }
            (_, Some(body)) => {
                let mut request = match method {
                    "PUT" => self.agent.put(&url),
                    "PATCH" => self.agent.patch(&url),
                    _ => self.agent.post(&url),
                };
                request = request
                    .header("Content-Type", "application/json")
                    .header("Idempotency-Key", key);
                if let Some(auth) = &auth {
                    request = request.header("Authorization", auth);
                }
                request.send(body)
            }
        };
        let mut response = response.map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(16 << 20)
            .read_to_string()
            .map_err(|e| e.to_string())?;
        Ok(Answer { status, body })
    }

    /// Sends a request, retrying as the spec says. `Err` only when Beacon never answered.
    pub fn request(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Answer, Fail> {
        let text = body.map(Value::to_string);
        let mut delays = RETRY_DELAYS.iter();
        loop {
            let result = self.attempt(method, path, text.as_deref(), &self.key);
            let retry = match &result {
                Err(_) => true,
                Ok(answer) => crate::spool::retryable(answer.status),
            };
            match (retry, delays.next()) {
                (true, Some(delay)) => std::thread::sleep(*delay),
                _ => {
                    return result.map_err(|error| {
                        Fail::unreachable(vec![Problem::new(
                            &self.url,
                            format!("no answer from Beacon: {error}"),
                            "check Beacon is running (`beacon serve`) and --server or BEACON_URL points at it",
                        )])
                    });
                }
            }
        }
    }
}
