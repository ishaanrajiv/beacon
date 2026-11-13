//! Sending: the wire, Apple's answers, provider-token refresh and the other-environment fallback
//! (`spec/apns.md`, "Connection", "Results and retries", "Dead tokens", "Live Activities").

use crate::Environment;
use crate::payload::{LiveActivity, Notification, activity_payload, is_passive, payload};
use crate::token::{Config, PROVIDER_TOKEN_MS, sign};
use beacon_core::problem::Problem;
use std::sync::Mutex;
use std::time::Duration;

/// Apple stops trying to deliver after this: one hour, in seconds.
const EXPIRY_SECONDS: i64 = 3600;
/// Per-request timeout.
const TIMEOUT: Duration = Duration::from_secs(15);

const DEAD_REASONS: [&str; 3] = ["BadDeviceToken", "Unregistered", "DeviceTokenNotForTopic"];
const STALE_PROVIDER_REASONS: [&str; 2] = ["ExpiredProviderToken", "InvalidProviderToken"];

/// Apple's answer to one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

/// The wire. `Err` means no answer (a timeout, a dropped connection, a TLS error): the push is
/// `uncertain`. The error text must never contain the device token, which is in `path`.
pub trait Transport: Send + Sync {
    fn post(
        &self,
        environment: Environment,
        path: &str,
        headers: &[(&str, String)],
        body: &[u8],
    ) -> Result<Response, String>;
}

/// HTTP/2 over rustls to Apple, one client kept open and reused, 15 s per request.
pub struct HttpTransport {
    client: reqwest::blocking::Client,
}

impl HttpTransport {
    pub fn new() -> Result<HttpTransport, Problem> {
        let client = reqwest::blocking::Client::builder()
            .http2_prior_knowledge()
            .https_only(true)
            .timeout(TIMEOUT)
            .build()
            .map_err(|e| {
                Problem::new(
                    "apns",
                    format!("can't start the HTTP/2 client for Apple: {}", e.without_url()),
                    "check the machine's TLS setup; Beacon still records every push as unconfigured until it starts",
                )
            })?;
        Ok(HttpTransport { client })
    }
}

impl Transport for HttpTransport {
    fn post(
        &self,
        environment: Environment,
        path: &str,
        headers: &[(&str, String)],
        body: &[u8],
    ) -> Result<Response, String> {
        let mut request = self
            .client
            .post(format!("{}{path}", environment.host()))
            .body(body.to_vec());
        for (name, value) in headers {
            request = request.header(*name, value.as_str());
        }
        // reqwest's errors quote the URL, which holds the token: say what kind of failure it was.
        let describe = |e: reqwest::Error| {
            let kind = if e.is_timeout() {
                "timed out"
            } else if e.is_connect() {
                "couldn't connect"
            } else if e.is_body() || e.is_decode() {
                "the answer was cut off"
            } else if e.is_builder() {
                "the request couldn't be built"
            } else {
                "the request failed"
            };
            format!("{kind}: {}", e.without_url())
        };
        let response = request.send().map_err(describe)?;
        let status = response.status().as_u16();
        let body = response.bytes().map_err(describe)?.to_vec();
        Ok(Response { status, body })
    }
}

/// The result of one push to one device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// `200`: Apple took it.
    Accepted,
    /// Apple answered with an error. `reason` is Apple's (letters only, at most 64) or
    /// `HTTP <status>`. `dead`: the token is no good. `retryable`: `429` or a `5xx`.
    Rejected {
        reason: String,
        dead: bool,
        retryable: bool,
    },
    /// No answer: Apple may or may not have it.
    Uncertain { reason: String },
}

impl Answer {
    /// `accepted`, `rejected` or `uncertain`, as recorded in an item's attempts.
    pub fn status(&self) -> &'static str {
        match self {
            Answer::Accepted => "accepted",
            Answer::Rejected { .. } => "rejected",
            Answer::Uncertain { .. } => "uncertain",
        }
    }

    /// The reason recorded with the attempt; None when accepted.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Answer::Accepted => None,
            Answer::Rejected { reason, .. } | Answer::Uncertain { reason } => Some(reason),
        }
    }

    fn is_reason(&self, wanted: &[&str]) -> bool {
        matches!(self, Answer::Rejected { reason, .. } if wanted.contains(&reason.as_str()))
    }

    fn is_dead(&self) -> bool {
        matches!(self, Answer::Rejected { dead: true, .. })
    }
}

/// One device's result, with what the caller must change about the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceResult {
    pub answer: Answer,
    /// The environment that answered last.
    pub environment: Environment,
    /// `BadDeviceToken`, then taken in the other environment: change the device's environment to
    /// `environment`.
    pub moved: bool,
    /// The token is dead: delete the device.
    pub dead: bool,
}

/// Reads Apple's answer (`spec/apns.md`, "Results and retries").
fn classify(response: &Response) -> Answer {
    if response.status == 200 {
        return Answer::Accepted;
    }
    let reason = serde_json::from_slice::<serde_json::Value>(&response.body)
        .ok()
        .and_then(|v| v.get("reason").and_then(|r| r.as_str()).map(str::to_string))
        .filter(|r| !r.is_empty() && r.len() <= 64 && r.chars().all(|c| c.is_ascii_alphabetic()))
        .unwrap_or_else(|| format!("HTTP {}", response.status));
    let dead = response.status == 410 || DEAD_REASONS.contains(&reason.as_str());
    let retryable = !dead && (response.status == 429 || response.status >= 500);
    Answer::Rejected {
        reason,
        dead,
        retryable,
    }
}

/// Replaces the token, wherever it appears, with its last six characters.
fn scrub(text: &str, token: &str) -> String {
    if token.is_empty() {
        return text.to_string();
    }
    let tail: String = token
        .chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    text.replace(token, &format!("…{tail}"))
}

/// Pushes to Apple: holds the config, the wire and the cached provider token.
pub struct Client {
    config: Config,
    transport: Box<dyn Transport>,
    /// The provider token and when it was signed (unix ms).
    provider: Mutex<Option<(String, i64)>>,
}

impl Client {
    pub fn new(config: Config, transport: Box<dyn Transport>) -> Client {
        Client {
            config,
            transport,
            provider: Mutex::new(None),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The cached provider token while it is younger than 40 minutes, else a new one.
    fn provider_token(&self, now: i64) -> Result<String, String> {
        let mut cached = self
            .provider
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((jwt, at)) = cached.as_ref()
            && now >= *at
            && now - *at < PROVIDER_TOKEN_MS
        {
            return Ok(jwt.clone());
        }
        let jwt = sign(&self.config, now)?;
        *cached = Some((jwt.clone(), now));
        Ok(jwt)
    }

    fn drop_provider_token(&self) {
        *self
            .provider
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    /// One request, with one immediate retry on a stale provider token (not counted).
    fn attempt(
        &self,
        environment: Environment,
        token: &str,
        headers: &[(&str, String)],
        body: &[u8],
        now: i64,
    ) -> Answer {
        let path = format!("/3/device/{token}");
        let mut answer = Answer::Uncertain {
            reason: "not sent".into(),
        };
        for retried in [false, true] {
            let jwt = match self.provider_token(now) {
                Ok(jwt) => jwt,
                Err(why) => {
                    return Answer::Rejected {
                        reason: format!("the signing key can't sign: {why}"),
                        dead: false,
                        retryable: false,
                    };
                }
            };
            let mut all = headers.to_vec();
            all.push(("authorization", format!("bearer {jwt}")));
            let response = match self.transport.post(environment, &path, &all, body) {
                Ok(response) => response,
                Err(why) => {
                    return Answer::Uncertain {
                        reason: scrub(&why, token),
                    };
                }
            };
            answer = classify(&response);
            if !retried && response.status == 403 && answer.is_reason(&STALE_PROVIDER_REASONS) {
                self.drop_provider_token();
                continue;
            }
            break;
        }
        answer
    }

    fn expiration(now: i64) -> String {
        (now.div_euclid(1000) + EXPIRY_SECONDS).to_string()
    }

    /// One push to one device. `now` is unix ms: the provider token's `iat` and
    /// `apns-expiration` (an hour on) come from it. On `BadDeviceToken` the push is tried once in
    /// the other environment.
    pub fn send(&self, n: &Notification, token: &str, environment: Environment, now: i64) -> DeviceResult {
        let body = payload(n);
        let mut headers: Vec<(&str, String)> = vec![
            ("apns-push-type", "alert".into()),
            ("apns-topic", self.config.topic.clone()),
            (
                "apns-priority",
                if is_passive(n.level) { "5" } else { "10" }.into(),
            ),
            ("apns-expiration", Client::expiration(now)),
        ];
        if !n.collapse_id.is_empty() {
            headers.push(("apns-collapse-id", n.collapse_id.into()));
        }
        let first = self.attempt(environment, token, &headers, &body, now);
        if first.is_reason(&["BadDeviceToken"]) {
            let other = environment.other();
            let second = self.attempt(other, token, &headers, &body, now);
            return DeviceResult {
                moved: second == Answer::Accepted,
                dead: second.is_dead(),
                answer: second,
                environment: other,
            };
        }
        DeviceResult {
            dead: first.is_dead(),
            answer: first,
            environment,
            moved: false,
        }
    }

    /// One Live Activity update or end, sent at once. `environments` are tried in order, moving to
    /// the next only on `BadDeviceToken`. `dead` in the result means the activity's token is dead
    /// (the caller answers `410` and deletes nothing).
    pub fn live_activity(
        &self,
        a: &LiveActivity,
        token: &str,
        environments: &[Environment],
        now: i64,
    ) -> Result<DeviceResult, Problem> {
        if a.priority != 5 && a.priority != 10 {
            return Err(Problem::new(
                "priority",
                format!("{} is not an APNs priority", a.priority),
                "send 10 (at once) or 5 (when the phone is awake anyway)",
            ));
        }
        let body = activity_payload(a)?;
        let headers: Vec<(&str, String)> = vec![
            ("apns-push-type", "liveactivity".into()),
            (
                "apns-topic",
                format!("{}.push-type.liveactivity", self.config.topic),
            ),
            ("apns-priority", a.priority.to_string()),
            ("apns-expiration", Client::expiration(now)),
        ];
        let mut last: Option<DeviceResult> = None;
        for (i, environment) in environments.iter().enumerate() {
            let answer = self.attempt(*environment, token, &headers, &body, now);
            let bad_token = answer.is_reason(&["BadDeviceToken"]);
            let result = DeviceResult {
                moved: i > 0 && answer == Answer::Accepted,
                dead: answer.is_dead(),
                answer,
                environment: *environment,
            };
            last = Some(result);
            if !bad_token {
                break;
            }
        }
        last.ok_or_else(|| {
            Problem::new(
                "environment",
                "no environment to send the Live Activity update to",
                "give \"production\" or \"sandbox\", or register a device first",
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::key_pem;
    use beacon_core::model::PushLevel;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::Arc;

    const TOKEN: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
    const T0: i64 = 1_794_571_200_000;
    const MINUTE: i64 = 60_000;

    #[derive(Debug, Clone)]
    struct Sent {
        environment: Environment,
        path: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Sent {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.as_str())
        }
    }

    #[derive(Default)]
    struct Script {
        answers: Mutex<VecDeque<Result<Response, String>>>,
        sent: Mutex<Vec<Sent>>,
    }

    struct Fake(Arc<Script>);

    impl Transport for Fake {
        fn post(
            &self,
            environment: Environment,
            path: &str,
            headers: &[(&str, String)],
            body: &[u8],
        ) -> Result<Response, String> {
            self.0.sent.lock().unwrap().push(Sent {
                environment,
                path: path.to_string(),
                headers: headers.iter().map(|(n, v)| (n.to_string(), v.clone())).collect(),
                body: body.to_vec(),
            });
            self.0.answers.lock().unwrap().pop_front().unwrap_or(Ok(ok()))
        }
    }

    fn ok() -> Response {
        Response {
            status: 200,
            body: Vec::new(),
        }
    }

    fn apple(status: u16, reason: &str) -> Result<Response, String> {
        Ok(Response {
            status,
            body: json!({"reason": reason}).to_string().into_bytes(),
        })
    }

    fn client(answers: Vec<Result<Response, String>>) -> (Client, Arc<Script>) {
        let script = Arc::new(Script::default());
        script.answers.lock().unwrap().extend(answers);
        let config = Config {
            key_pem: key_pem(),
            key_id: "ABC123DEFG".into(),
            team_id: "TEAM123456".into(),
            topic: "com.example.lab".into(),
        };
        (Client::new(config, Box::new(Fake(script.clone()))), script)
    }

    fn sent(script: &Script) -> Vec<Sent> {
        script.sent.lock().unwrap().clone()
    }

    fn data() -> serde_json::Value {
        json!({"id": 4182, "kind": "backup.failed", "key": "nightly", "collapse_id": "b1-29fd692930019189"})
    }

    fn note(data: &serde_json::Value) -> Notification<'_> {
        Notification {
            title: "Backup failed",
            body: "The disk is full.",
            level: PushLevel::Urgent,
            collapse_id: "b1-29fd692930019189",
            category: Some("backup.failed"),
            thread: "backup",
            url: Some("/system"),
            data,
            relevance: 1.0,
        }
    }

    fn jwt_iat(s: &Sent) -> i64 {
        let jwt = s
            .header("authorization")
            .unwrap()
            .strip_prefix("bearer ")
            .unwrap();
        let claims = jwt.split('.').nth(1).unwrap();
        crate::testkit::decode_part(claims)["iat"].as_i64().unwrap()
    }

    #[test]
    fn a_push_carries_the_spec_headers() {
        let (c, script) = client(vec![]);
        let data = data();
        let result = c.send(&note(&data), TOKEN, Environment::Production, T0 + 999);
        assert_eq!(
            result,
            DeviceResult {
                answer: Answer::Accepted,
                environment: Environment::Production,
                moved: false,
                dead: false
            }
        );
        let sent = sent(&script);
        assert_eq!(sent.len(), 1);
        let s = &sent[0];
        assert_eq!(s.environment, Environment::Production);
        assert_eq!(s.path, format!("/3/device/{TOKEN}"));
        assert_eq!(s.header("apns-push-type"), Some("alert"));
        assert_eq!(s.header("apns-topic"), Some("com.example.lab"));
        assert_eq!(s.header("apns-priority"), Some("10"));
        assert_eq!(s.header("apns-expiration"), Some("1794574800"));
        assert_eq!(s.header("apns-collapse-id"), Some("b1-29fd692930019189"));
        assert_eq!(jwt_iat(s), 1_794_571_200);
        assert_eq!(s.body, payload(&note(&data)));
    }

    #[test]
    fn a_passive_push_has_priority_5() {
        let (c, script) = client(vec![]);
        let data = data();
        c.send(
            &Notification {
                level: PushLevel::Silent,
                ..note(&data)
            },
            TOKEN,
            Environment::Sandbox,
            T0,
        );
        let s = &sent(&script)[0];
        assert_eq!(s.header("apns-priority"), Some("5"));
        assert_eq!(s.environment, Environment::Sandbox);
    }

    #[test]
    fn the_provider_token_is_reused_for_40_minutes() {
        let (c, script) = client(vec![]);
        let data = data();
        for at in [T0, T0 + 39 * MINUTE, T0 + 40 * MINUTE, T0 + 41 * MINUTE] {
            c.send(&note(&data), TOKEN, Environment::Production, at);
        }
        let sent = sent(&script);
        let auth: Vec<&str> = sent.iter().map(|s| s.header("authorization").unwrap()).collect();
        assert_eq!(auth[0], auth[1], "reused within 40 minutes");
        assert_ne!(auth[1], auth[2], "signed again at 40 minutes");
        assert_eq!(auth[2], auth[3]);
        assert_eq!(jwt_iat(&sent[1]), T0 / 1000);
        assert_eq!(jwt_iat(&sent[2]), (T0 + 40 * MINUTE) / 1000);
    }

    #[test]
    fn a_stale_provider_token_is_replaced_and_retried_at_once() {
        for reason in ["ExpiredProviderToken", "InvalidProviderToken"] {
            let (c, script) = client(vec![Ok(ok()), apple(403, reason), Ok(ok())]);
            let data = data();
            c.send(&note(&data), TOKEN, Environment::Production, T0);
            let r = c.send(&note(&data), TOKEN, Environment::Production, T0 + MINUTE);
            assert_eq!(r.answer, Answer::Accepted, "{reason}");
            let sent = sent(&script);
            assert_eq!(sent.len(), 3);
            assert_eq!(
                sent[0].header("authorization"),
                sent[1].header("authorization"),
                "cached"
            );
            assert_ne!(
                sent[1].header("authorization"),
                sent[2].header("authorization"),
                "new token"
            );
            assert_eq!(jwt_iat(&sent[2]), (T0 + MINUTE) / 1000);
        }
    }

    #[test]
    fn a_second_stale_answer_is_rejected_not_looped() {
        let (c, script) = client(vec![
            apple(403, "InvalidProviderToken"),
            apple(403, "InvalidProviderToken"),
        ]);
        let data = data();
        let r = c.send(&note(&data), TOKEN, Environment::Production, T0);
        assert_eq!(
            r.answer,
            Answer::Rejected {
                reason: "InvalidProviderToken".into(),
                dead: false,
                retryable: false
            }
        );
        assert_eq!(sent(&script).len(), 2);
    }

    #[test]
    fn bad_device_token_moves_to_the_other_environment() {
        let (c, script) = client(vec![apple(400, "BadDeviceToken"), Ok(ok())]);
        let data = data();
        let r = c.send(&note(&data), TOKEN, Environment::Production, T0);
        assert_eq!(
            r,
            DeviceResult {
                answer: Answer::Accepted,
                environment: Environment::Sandbox,
                moved: true,
                dead: false
            }
        );
        let envs: Vec<Environment> = sent(&script).iter().map(|s| s.environment).collect();
        assert_eq!(envs, [Environment::Production, Environment::Sandbox]);
    }

    #[test]
    fn bad_device_token_in_both_is_dead() {
        let (c, _) = client(vec![apple(400, "BadDeviceToken"), apple(400, "BadDeviceToken")]);
        let data = data();
        let r = c.send(&note(&data), TOKEN, Environment::Sandbox, T0);
        assert!(r.dead && !r.moved);
        assert_eq!(r.environment, Environment::Production);

        // No answer from the other environment: not dead, retried later from the device's own.
        let (c, _) = client(vec![apple(400, "BadDeviceToken"), Err("timed out".into())]);
        let r = c.send(&note(&data), TOKEN, Environment::Sandbox, T0);
        assert!(!r.dead && !r.moved);
        assert_eq!(
            r.answer,
            Answer::Uncertain {
                reason: "timed out".into()
            }
        );
    }

    #[test]
    fn dead_answers_are_not_tried_elsewhere() {
        for (status, reason) in [(410, "Unregistered"), (410, ""), (400, "DeviceTokenNotForTopic")] {
            let (c, script) = client(vec![apple(status, reason)]);
            let data = data();
            let r = c.send(&note(&data), TOKEN, Environment::Production, T0);
            assert!(r.dead, "{status} {reason}");
            assert!(matches!(
                r.answer,
                Answer::Rejected {
                    dead: true,
                    retryable: false,
                    ..
                }
            ));
            assert_eq!(sent(&script).len(), 1, "{status} {reason}");
        }
    }

    #[test]
    fn reasons_are_apples_when_plain_else_the_status() {
        let cases = [
            (
                Response {
                    status: 400,
                    body: br#"{"reason":"BadTopic"}"#.to_vec(),
                },
                "BadTopic",
                false,
            ),
            (
                Response {
                    status: 429,
                    body: br#"{"reason":"TooManyRequests"}"#.to_vec(),
                },
                "TooManyRequests",
                true,
            ),
            (
                Response {
                    status: 500,
                    body: b"<html>oops</html>".to_vec(),
                },
                "HTTP 500",
                true,
            ),
            (
                Response {
                    status: 503,
                    body: br#"{"reason":"Service Unavailable"}"#.to_vec(),
                },
                "HTTP 503",
                true,
            ),
            (
                Response {
                    status: 400,
                    body: br#"{"reason":"Bad_Topic1"}"#.to_vec(),
                },
                "HTTP 400",
                false,
            ),
            (
                Response {
                    status: 400,
                    body: format!(r#"{{"reason":"{}"}}"#, "A".repeat(65)).into_bytes(),
                },
                "HTTP 400",
                false,
            ),
            (
                Response {
                    status: 413,
                    body: Vec::new(),
                },
                "HTTP 413",
                false,
            ),
            (
                Response {
                    status: 400,
                    body: br#"{"reason":42}"#.to_vec(),
                },
                "HTTP 400",
                false,
            ),
        ];
        for (response, reason, retryable) in cases {
            assert_eq!(
                classify(&response),
                Answer::Rejected {
                    reason: reason.into(),
                    dead: false,
                    retryable
                },
                "{response:?}"
            );
        }
        let long_ok = "A".repeat(64);
        let r = Response {
            status: 400,
            body: format!(r#"{{"reason":"{long_ok}"}}"#).into_bytes(),
        };
        assert_eq!(classify(&r).reason(), Some(long_ok.as_str()));
        assert_eq!(classify(&ok()), Answer::Accepted);
        assert_eq!(Answer::Accepted.status(), "accepted");
        assert_eq!(classify(&r).status(), "rejected");
        assert_eq!(Answer::Uncertain { reason: "x".into() }.status(), "uncertain");
    }

    #[test]
    fn the_token_never_appears_in_a_reason() {
        let quoting = format!("error sending request for url (https://api.push.apple.com/3/device/{TOKEN})");
        let (c, _) = client(vec![Err(quoting)]);
        let data = data();
        let r = c.send(&note(&data), TOKEN, Environment::Production, T0);
        let reason = r.answer.reason().unwrap();
        assert!(!reason.contains(TOKEN), "{reason}");
        assert!(reason.contains("…7e8f90"), "{reason}");
        assert_eq!(r.answer.status(), "uncertain");
        assert!(!r.dead);

        let (c, _) = client(vec![Ok(Response {
            status: 400,
            body: format!(r#"{{"reason":"{TOKEN}"}}"#).into_bytes(),
        })]);
        let r = c.send(&note(&data), TOKEN, Environment::Production, T0);
        assert_eq!(r.answer.reason(), Some("HTTP 400"));
    }

    fn activity() -> LiveActivity {
        LiveActivity {
            event: "update".into(),
            content_state: json!({"step": "Reading the logs"}),
            timestamp: 1_794_571_200,
            dismissal_date: None,
            stale_date: None,
            alert: None,
            priority: 10,
        }
    }

    #[test]
    fn a_live_activity_has_its_own_headers() {
        let (c, script) = client(vec![]);
        let a = LiveActivity {
            priority: 5,
            ..activity()
        };
        let r = c.live_activity(&a, TOKEN, &[Environment::Sandbox], T0).unwrap();
        assert_eq!(r.answer, Answer::Accepted);
        assert!(!r.moved && !r.dead);
        let s = &sent(&script)[0];
        assert_eq!(s.environment, Environment::Sandbox);
        assert_eq!(s.path, format!("/3/device/{TOKEN}"));
        assert_eq!(s.header("apns-push-type"), Some("liveactivity"));
        assert_eq!(
            s.header("apns-topic"),
            Some("com.example.lab.push-type.liveactivity")
        );
        assert_eq!(s.header("apns-priority"), Some("5"));
        assert_eq!(s.header("apns-expiration"), Some("1794574800"));
        assert_eq!(s.header("apns-collapse-id"), None);
        assert!(s.header("authorization").unwrap().starts_with("bearer "));
        assert_eq!(s.body, activity_payload(&a).unwrap());
    }

    #[test]
    fn a_live_activity_tries_the_next_environment_only_on_bad_device_token() {
        let both = [Environment::Production, Environment::Sandbox];
        let (c, script) = client(vec![apple(400, "BadDeviceToken"), Ok(ok())]);
        let r = c.live_activity(&activity(), TOKEN, &both, T0).unwrap();
        assert_eq!(r.answer, Answer::Accepted);
        assert_eq!(r.environment, Environment::Sandbox);
        assert!(r.moved);
        assert_eq!(sent(&script).len(), 2);

        let (c, script) = client(vec![apple(410, "Unregistered")]);
        let r = c.live_activity(&activity(), TOKEN, &both, T0).unwrap();
        assert!(r.dead);
        assert_eq!(sent(&script).len(), 1, "410 is not a reason to try elsewhere");

        let (c, script) = client(vec![apple(400, "BadDeviceToken"), apple(400, "BadDeviceToken")]);
        let r = c.live_activity(&activity(), TOKEN, &both, T0).unwrap();
        assert!(r.dead && !r.moved);
        assert_eq!(sent(&script).len(), 2);

        let (c, script) = client(vec![apple(400, "BadDeviceToken")]);
        let r = c
            .live_activity(&activity(), TOKEN, &[Environment::Production], T0)
            .unwrap();
        assert!(r.dead);
        assert_eq!(sent(&script).len(), 1, "only the environments given");

        let (c, _) = client(vec![Err("timed out".into())]);
        let r = c.live_activity(&activity(), TOKEN, &both, T0).unwrap();
        assert_eq!(r.answer.status(), "uncertain");
    }

    #[test]
    fn a_live_activity_refreshes_a_stale_provider_token() {
        let (c, script) = client(vec![apple(403, "ExpiredProviderToken"), Ok(ok())]);
        let r = c
            .live_activity(&activity(), TOKEN, &[Environment::Production], T0)
            .unwrap();
        assert_eq!(r.answer, Answer::Accepted);
        assert_eq!(sent(&script).len(), 2);
    }

    #[test]
    fn a_live_activity_refuses_what_it_cant_send() {
        let (c, script) = client(vec![]);
        let big = LiveActivity {
            content_state: json!({"log": "x".repeat(5000)}),
            ..activity()
        };
        let p = c
            .live_activity(&big, TOKEN, &[Environment::Production], T0)
            .unwrap_err();
        assert_eq!(p.at, "live activity");

        let p = c
            .live_activity(
                &LiveActivity {
                    priority: 7,
                    ..activity()
                },
                TOKEN,
                &[Environment::Production],
                T0,
            )
            .unwrap_err();
        assert_eq!(p.at, "priority");
        assert!(p.fix.contains("10"));

        let p = c.live_activity(&activity(), TOKEN, &[], T0).unwrap_err();
        assert_eq!(p.at, "environment");
        assert!(sent(&script).is_empty());
    }

    #[test]
    fn a_broken_key_is_rejected_without_sending() {
        let script = Arc::new(Script::default());
        let config = Config {
            key_pem: "not a key".into(),
            key_id: "ABC123DEFG".into(),
            team_id: "TEAM123456".into(),
            topic: "com.example.lab".into(),
        };
        let c = Client::new(config, Box::new(Fake(script.clone())));
        let data = data();
        let r = c.send(&note(&data), TOKEN, Environment::Production, T0);
        assert!(
            matches!(
                r.answer,
                Answer::Rejected {
                    dead: false,
                    retryable: false,
                    ..
                }
            ),
            "{r:?}"
        );
        assert!(r.answer.reason().unwrap().contains("signing key"));
        assert!(sent(&script).is_empty());
    }

    #[test]
    fn scrub_keeps_the_last_six() {
        assert_eq!(scrub("x/abcdef123456/y", "abcdef123456"), "x/…123456/y");
        assert_eq!(scrub("nothing here", "abcdef123456"), "nothing here");
        assert_eq!(scrub("text", ""), "text");
    }

    #[test]
    fn the_http_transport_builds() {
        assert!(HttpTransport::new().is_ok());
    }
}
