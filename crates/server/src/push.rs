//! The push sender: the queue the store keeps, sent through Apple one push at a time
//! (`spec/apns.md`, "Results and retries"), and `POST /v1/live-activity`, sent at once.

use crate::App;
use crate::api::{bad, object, store_error, unknown};
use crate::http::{Reply, Request};
use beacon_apns::{Answer, Environment, LiveActivity, Next, Notification};
use beacon_core::Problem;
use beacon_core::model::{Class, PushReason};
use beacon_store::{Attempt, PushJob, PushNext};
use serde_json::{Value, json};

/// The relevance score of a push (`spec/apns.md`, "A notification").
fn relevance(job: &PushJob) -> f64 {
    if job.purpose == "recovery" {
        return 0.3;
    }
    match job.class {
        Class::Alert => 1.0,
        Class::Action => 0.8,
        Class::Update | Class::Record => 0.5,
    }
}

/// Sends every push due now and records each round of attempts.
pub(crate) fn drain(app: &App, now: i64) -> Result<(), beacon_store::Error> {
    for job in app.store.pushes_due(now)? {
        // Every push due was listed at once, and each send can take seconds: one whose item
        // closed, or that a newer push replaced, while an earlier one was sending is skipped.
        if !app.store.push_queued(job.id)? {
            continue;
        }
        let Some(client) = &app.apns else {
            app.store
                .push_attempted(job.id, &[], PushNext::Failed(PushReason::Unconfigured), now)?;
            continue;
        };
        if job.devices.is_empty() {
            app.store
                .push_attempted(job.id, &[], PushNext::Failed(PushReason::NoDevices), now)?;
            continue;
        }
        let notification = Notification {
            title: &job.title,
            body: &job.body,
            level: job.level,
            collapse_id: &job.collapse_id,
            category: job.category.as_deref(),
            thread: &job.thread,
            url: job.url.as_deref(),
            data: &job.data,
            relevance: relevance(&job),
        };
        let mut attempts = Vec::new();
        let mut answers = Vec::new();
        let mut dead = Vec::new();
        let mut moved = Vec::new();
        for (token, environment) in &job.devices {
            let environment = Environment::parse(environment).unwrap_or(Environment::Production);
            let result = client.send(&notification, token, environment, app.now());
            attempts.push(Attempt {
                token: token.clone(),
                environment: result.environment.as_str().to_string(),
                status: result.answer.status().to_string(),
                reason: result.answer.reason().map(str::to_string),
            });
            if result.dead {
                dead.push(token.clone());
            } else if result.moved {
                moved.push((token.clone(), result.environment));
            }
            answers.push(result.answer);
        }
        let s = app.store.settings();
        let next = match beacon_apns::next(
            &answers,
            job.attempts,
            &app.config.retry,
            job.queued_at,
            s.give_up,
            now,
        ) {
            Next::Sent => PushNext::Sent,
            Next::Failed => PushNext::Failed(PushReason::Rejected),
            Next::Expired => PushNext::Failed(PushReason::Expired),
            Next::RetryAt(at) => PushNext::RetryAt(at),
        };
        app.store.push_attempted(job.id, &attempts, next, now)?;
        for (token, environment) in moved {
            app.store
                .set_device_environment(&token, environment.as_str(), now)?;
        }
        for token in dead {
            app.store.forget_device(&token, now)?;
        }
    }
    Ok(())
}

const ACTIVITY_FIELDS: [&str; 9] = [
    "token",
    "event",
    "contentState",
    "timestamp",
    "dismissalDate",
    "staleDate",
    "alert",
    "priority",
    "environment",
];

fn is_token(text: &str) -> bool {
    (64..=400).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `POST /v1/live-activity`: one update straight to Apple, the same contract as the service
/// Beacon replaces. Unknown fields are ignored; callers act on the status.
pub(crate) fn live_activity(app: &App, req: &Request) -> Result<Reply, Reply> {
    let map = object(req, "live-activity")?;
    // Other programs' contract: fields Beacon doesn't read are ignored, not refused.
    let _ignored = unknown(&map, "live-activity", &ACTIVITY_FIELDS);
    let mut problems = Vec::new();
    let mut need = |ok: bool, field: &str, rule: &str, fix: &str| {
        if !ok {
            problems.push(Problem::new(format!("live-activity {field}"), rule, fix));
        }
    };
    let text = |field: &str| map.get(field).and_then(Value::as_str);
    let seconds = |field: &str| map.get(field).and_then(Value::as_i64).filter(|n| *n > 0);
    let token = text("token").unwrap_or_default();
    need(
        is_token(token),
        "token",
        "the activity's push token is 64 to 400 lowercase hex characters",
        "send the token the activity got from pushTokenUpdates, hex-encoded in lowercase",
    );
    let event = text("event").unwrap_or_default();
    need(
        matches!(event, "update" | "end"),
        "event",
        "event is `update` or `end`",
        "send \"update\" or \"end\"",
    );
    need(
        matches!(map.get("contentState"), Some(Value::Object(_))),
        "contentState",
        "contentState is a required object",
        "send the activity's content state as the app decodes it",
    );
    let timestamp = seconds("timestamp");
    need(
        timestamp.is_some(),
        "timestamp",
        "timestamp is unix seconds, more than 0",
        "send the time the state is from, in seconds",
    );
    for field in ["dismissalDate", "staleDate"] {
        need(
            map.get(field)
                .is_none_or(|v| v.is_null() || seconds(field).is_some()),
            field,
            "a time in unix seconds, more than 0",
            "send unix seconds, or leave it out",
        );
    }
    let alert = match map.get("alert") {
        None | Some(Value::Null) => None,
        Some(Value::Object(a)) => {
            let title = a.get("title").and_then(Value::as_str);
            let body = a.get("body").and_then(Value::as_str);
            need(
                title.is_some() && body.is_some(),
                "alert",
                "alert is {title, body}, both strings",
                "send {\"title\": \"…\", \"body\": \"…\"}",
            );
            Some((
                title.unwrap_or_default().to_string(),
                body.unwrap_or_default().to_string(),
            ))
        }
        Some(_) => {
            need(
                false,
                "alert",
                "alert is {title, body}, both strings",
                "send {\"title\": \"…\", \"body\": \"…\"}",
            );
            None
        }
    };
    let priority = match map.get("priority") {
        None | Some(Value::Null) => Some(10),
        Some(v) => v.as_u64().filter(|p| *p == 5 || *p == 10).map(|p| p as u8),
    };
    need(
        priority.is_some(),
        "priority",
        "priority is 5 or 10",
        "send 10 (at once) or 5 (when convenient)",
    );
    let environment = match map.get("environment") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let parsed = v.as_str().and_then(Environment::parse);
            need(
                parsed.is_some(),
                "environment",
                "environment is `sandbox` or `production`",
                "send one, or leave it out",
            );
            parsed
        }
    };
    if !problems.is_empty() {
        return Err(bad(problems));
    }
    let activity = LiveActivity {
        event: event.to_string(),
        content_state: map.get("contentState").cloned().unwrap_or(json!({})),
        timestamp: timestamp.unwrap_or_default(),
        dismissal_date: seconds("dismissalDate"),
        stale_date: seconds("staleDate"),
        alert,
        priority: priority.unwrap_or(10),
    };
    if let Err(problem) = beacon_apns::activity_payload(&activity) {
        return Err(Reply::problems(400, &[problem]));
    }
    let Some(client) = &app.apns else {
        return Err(Reply::problem(
            503,
            "apns",
            "no APNs signing key is configured",
            "set [apns] key_path, team_id and topic in beacon.toml",
        ));
    };
    let environments: Vec<Environment> = match environment {
        Some(e) => vec![e],
        None => {
            let latest = app
                .store
                .devices()
                .map_err(store_error)?
                .first()
                .and_then(|d| Environment::parse(&d.environment))
                .unwrap_or(Environment::Production);
            vec![latest, latest.other()]
        }
    };
    let result = client
        .live_activity(&activity, token, &environments, app.now())
        .map_err(|p| Reply::problems(400, &[p]))?;
    match result.answer {
        Answer::Accepted => Ok(Reply::ok(json!({"ok": true}))),
        _ if result.dead => Err(Reply::problem(
            410,
            "live-activity token",
            "Apple says the activity's push token is no longer valid",
            "the activity has ended; stop sending to this token",
        )),
        Answer::Rejected { reason, .. } => Err(Reply::problem(
            502,
            "apns",
            format!("Apple didn't take the update: {reason}"),
            "the next update supersedes this one; check the payload and the app's topic if it repeats",
        )),
        Answer::Uncertain { reason } => Err(Reply::problem(
            502,
            "apns",
            format!("Apple didn't answer: {reason}"),
            "the next update supersedes this one",
        )),
    }
}
