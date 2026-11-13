//! The JSON bodies Apple takes (`spec/apns.md`, "A notification" and "Live Activities").

use beacon_core::model::PushLevel;
use beacon_core::problem::Problem;
use serde_json::{Map, Number, Value, json};

/// Apple's limit on a push body.
pub const MAX_PAYLOAD_BYTES: usize = 4096;

/// What ends a body that was cut to fit.
const ELLIPSIS: &str = "…";

/// The title a body that still doesn't fit is cut to, in characters.
const TITLE_CHARS: usize = 40;

/// One notification as the server hands it over: rendered text plus what the app needs.
#[derive(Debug, Clone, Copy)]
pub struct Notification<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub level: PushLevel,
    /// `apns-collapse-id`: at most 64 bytes.
    pub collapse_id: &'a str,
    /// `aps.category`. None for a recovery, which shows no buttons.
    pub category: Option<&'a str>,
    /// `aps.thread-id`: the item's source.
    pub thread: &'a str,
    pub url: Option<&'a str>,
    /// The item's data map, `{"id": 4182, "kind", "key", "collapse_id", "context"?}`. Sent as
    /// strings, with `id` renamed `item` and null entries left out.
    pub data: &'a Value,
    /// `aps.relevance-score`, 0 to 1.
    pub relevance: f64,
}

/// A passive push: no sound, priority 5. `none` is never sent; if it is, it goes quietly.
pub(crate) fn is_passive(level: PushLevel) -> bool {
    matches!(level, PushLevel::Silent | PushLevel::None)
}

/// The body of a notification push, within 4096 bytes when it can be: the body text is cut first
/// (by bytes, at a character boundary, ending in `…`), then `url` dropped, then the title cut to
/// 40 characters. `category`, `thread-id` and `data` are never cut.
pub fn payload(n: &Notification) -> Vec<u8> {
    let data = data_strings(n.data);
    let build = |title: &str, body: &str, url: Option<&str>| -> Vec<u8> {
        let mut aps = Map::new();
        aps.insert("alert".into(), json!({"title": title, "body": body}));
        let level = n.level.interruption_level().unwrap_or("passive");
        aps.insert("interruption-level".into(), level.into());
        if !is_passive(n.level) {
            aps.insert("sound".into(), "default".into());
        }
        if !n.thread.is_empty() {
            aps.insert("thread-id".into(), n.thread.into());
        }
        if let Some(category) = n.category {
            aps.insert("category".into(), category.into());
        }
        if let Some(score) = Number::from_f64(n.relevance.clamp(0.0, 1.0)) {
            aps.insert("relevance-score".into(), Value::Number(score));
        }
        let mut root = Map::new();
        root.insert("aps".into(), Value::Object(aps));
        if let Some(url) = url {
            root.insert("url".into(), url.into());
        }
        if !data.is_empty() {
            root.insert("data".into(), Value::Object(data.clone()));
        }
        Value::Object(root).to_string().into_bytes()
    };

    let mut title = n.title.to_string();
    let mut body = n.body.to_string();
    let mut url = n.url;
    let mut out = build(&title, &body, url);

    // The cut counts each character at its JSON-escaped size, so one pass is usually enough; the
    // loop is there for when it isn't. Every pass shortens the body, so it ends.
    let mut cut = false;
    while out.len() > MAX_PAYLOAD_BYTES && !body.is_empty() {
        let excess = out.len() - MAX_PAYLOAD_BYTES;
        let (stem, owed) = match body.strip_suffix(ELLIPSIS) {
            // Our own ellipsis is put back after the cut: the stem owes only the excess.
            Some(stem) if cut => (stem, excess),
            _ => (body.as_str(), excess + ELLIPSIS.len()),
        };
        let keep = cut_point(stem, owed);
        body = if keep == 0 {
            String::new()
        } else {
            format!("{}{ELLIPSIS}", &stem[..keep])
        };
        cut = true;
        out = build(&title, &body, url);
    }
    if out.len() > MAX_PAYLOAD_BYTES && url.is_some() {
        url = None;
        out = build(&title, &body, url);
    }
    if out.len() > MAX_PAYLOAD_BYTES && title.chars().count() > TITLE_CHARS {
        title = title.chars().take(TITLE_CHARS).collect();
        out = build(&title, &body, url);
    }
    out
}

/// Where to cut `text` so the JSON-escaped bytes after the cut are at least `owed`: a character
/// boundary, as late as that allows.
fn cut_point(text: &str, owed: usize) -> usize {
    let mut freed = 0;
    let mut keep = text.len();
    for (at, c) in text.char_indices().rev() {
        if freed >= owed {
            break;
        }
        freed += escaped_len(c);
        keep = at;
    }
    if freed >= owed { keep } else { 0 }
}

/// The bytes a character takes inside a JSON string, as serde_json writes it.
fn escaped_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// The data map as Apple's payload carries it: string values, `id` named `item`, nulls left out.
fn data_strings(data: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    if let Value::Object(entries) = data {
        for (key, value) in entries {
            let key = if key == "id" { "item" } else { key.as_str() };
            let text = match value {
                Value::Null => continue,
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            out.insert(key.to_string(), Value::String(text));
        }
    }
    out
}

/// One Live Activity update or end, as `POST /v1/live-activity` takes it. Times are unix seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveActivity {
    /// `update` or `end`.
    pub event: String,
    /// Passed to the app as it is.
    pub content_state: Value,
    pub timestamp: i64,
    pub dismissal_date: Option<i64>,
    pub stale_date: Option<i64>,
    /// `(title, body)`.
    pub alert: Option<(String, String)>,
    /// `apns-priority`: 5 or 10.
    pub priority: u8,
}

/// The body of a Live Activity push. Never trimmed: over 4096 bytes is an error.
pub fn activity_payload(a: &LiveActivity) -> Result<Vec<u8>, Problem> {
    let mut aps = Map::new();
    aps.insert("timestamp".into(), a.timestamp.into());
    aps.insert("event".into(), a.event.as_str().into());
    aps.insert("content-state".into(), a.content_state.clone());
    if let Some(at) = a.dismissal_date {
        aps.insert("dismissal-date".into(), at.into());
    }
    if let Some(at) = a.stale_date {
        aps.insert("stale-date".into(), at.into());
    }
    if let Some((title, body)) = &a.alert {
        aps.insert(
            "alert".into(),
            json!({"title": title, "body": body, "sound": "default"}),
        );
    }
    let out = json!({"aps": aps}).to_string().into_bytes();
    if out.len() > MAX_PAYLOAD_BYTES {
        return Err(Problem::new(
            "live activity",
            format!(
                "the payload is {} bytes, over Apple's {MAX_PAYLOAD_BYTES}",
                out.len()
            ),
            "send a smaller contentState or alert: Live Activity updates are never trimmed",
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> Value {
        json!({"id": 4182, "kind": "backup.failed", "key": "nightly", "collapse_id": "b1-29fd692930019189"})
    }

    fn note<'a>(data: &'a Value) -> Notification<'a> {
        Notification {
            title: "Backup failed",
            body: "The disk is full. The last good backup was 02:00.",
            level: PushLevel::Urgent,
            collapse_id: "b1-29fd692930019189",
            category: Some("backup.failed"),
            thread: "backup",
            url: Some("/system"),
            data,
            relevance: 1.0,
        }
    }

    fn parse(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).unwrap()
    }

    #[test]
    fn the_spec_example() {
        let data = data();
        assert_eq!(
            parse(&payload(&note(&data))),
            json!({
                "aps": {
                    "alert": {"title": "Backup failed", "body": "The disk is full. The last good backup was 02:00."},
                    "interruption-level": "time-sensitive",
                    "sound": "default",
                    "thread-id": "backup",
                    "category": "backup.failed",
                    "relevance-score": 1.0
                },
                "url": "/system",
                "data": {"item": "4182", "kind": "backup.failed", "key": "nightly", "collapse_id": "b1-29fd692930019189"}
            })
        );
    }

    #[test]
    fn levels_set_interruption_and_sound() {
        let data = data();
        for (level, interruption, sound) in [
            (PushLevel::Urgent, "time-sensitive", true),
            (PushLevel::Normal, "active", true),
            (PushLevel::Silent, "passive", false),
            (PushLevel::None, "passive", false),
        ] {
            let v = parse(&payload(&Notification { level, ..note(&data) }));
            assert_eq!(v["aps"]["interruption-level"], interruption, "{level:?}");
            assert_eq!(v["aps"].get("sound").is_some(), sound, "{level:?}");
            assert!(v["aps"].get("badge").is_none() && v["aps"].get("mutable-content").is_none());
        }
    }

    #[test]
    fn recovery_has_no_category_and_no_url_is_left_out() {
        let data = data();
        let v = parse(&payload(&Notification {
            category: None,
            url: None,
            relevance: 0.3,
            level: PushLevel::Silent,
            ..note(&data)
        }));
        assert!(v["aps"].get("category").is_none());
        assert!(v.get("url").is_none());
        assert_eq!(v["aps"]["relevance-score"], 0.3);
        assert_eq!(v["aps"]["thread-id"], "backup");
    }

    #[test]
    fn the_caller_chooses_the_category() {
        let data = data();
        let v = parse(&payload(&Notification {
            category: Some("backup.retry"),
            ..note(&data)
        }));
        assert_eq!(v["aps"]["category"], "backup.retry");
    }

    #[test]
    fn data_is_strings_with_id_as_item_and_context_kept() {
        let data = json!({"id": 4182, "kind": "ship.done", "key": null, "collapse_id": "s1", "context": "deploy", "n": 2.5, "ok": true});
        let v = parse(&payload(&note(&data)));
        assert_eq!(
            v["data"],
            json!({"item": "4182", "kind": "ship.done", "collapse_id": "s1", "context": "deploy", "n": "2.5", "ok": "true"})
        );
        let none = Value::Null;
        assert!(parse(&payload(&note(&none))).get("data").is_none());
    }

    #[test]
    fn relevance_is_clamped() {
        let data = data();
        assert_eq!(
            parse(&payload(&Notification {
                relevance: 7.0,
                ..note(&data)
            }))["aps"]["relevance-score"],
            1.0
        );
        assert!(
            parse(&payload(&Notification {
                relevance: f64::NAN,
                ..note(&data)
            }))["aps"]
                .get("relevance-score")
                .is_none()
        );
    }

    #[test]
    fn a_long_body_is_cut_first_with_an_ellipsis() {
        let data = data();
        let long = "x".repeat(5000);
        let out = payload(&Notification {
            body: &long,
            ..note(&data)
        });
        assert!(out.len() <= MAX_PAYLOAD_BYTES);
        assert!(
            out.len() > MAX_PAYLOAD_BYTES - 8,
            "cut no more than it has to: {}",
            out.len()
        );
        let v = parse(&out);
        let body = v["aps"]["alert"]["body"].as_str().unwrap();
        assert!(body.ends_with('…'));
        assert!(long.starts_with(body.trim_end_matches('…')));
        assert_eq!(v["url"], "/system", "url kept while the body can give");
        assert_eq!(v["aps"]["alert"]["title"], "Backup failed");
        assert_eq!(v["aps"]["category"], "backup.failed");
        assert_eq!(v["data"]["item"], "4182");
    }

    #[test]
    fn multi_byte_text_is_not_split() {
        let data = data();
        for unit in ["é", "€", "🔥", "a🔥"] {
            let long = unit.repeat(3000);
            let out = payload(&Notification {
                body: &long,
                ..note(&data)
            });
            assert!(out.len() <= MAX_PAYLOAD_BYTES, "{unit}");
            assert!(
                out.len() > MAX_PAYLOAD_BYTES - 8,
                "{unit}: cut only what it must: {}",
                out.len()
            );
            let v = parse(&out); // parses: valid UTF-8 throughout
            let body = v["aps"]["alert"]["body"].as_str().unwrap();
            assert!(body.ends_with('…'));
            assert!(long.starts_with(body.strip_suffix('…').unwrap()), "{unit}");
        }
    }

    #[test]
    fn escaping_is_counted() {
        let data = data();
        for unit in ["\"", "\\", "\n", "\u{1}"] {
            let long = unit.repeat(3000);
            let out = payload(&Notification {
                body: &long,
                ..note(&data)
            });
            assert!(out.len() <= MAX_PAYLOAD_BYTES, "{unit:?}: {}", out.len());
            assert!(
                parse(&out)["aps"]["alert"]["body"]
                    .as_str()
                    .unwrap()
                    .ends_with('…')
            );
        }
    }

    #[test]
    fn then_the_url_goes_then_the_title_is_cut() {
        let data = data();
        // Body gives all it has, then the url goes; the title fits after that.
        let url = format!("/{}", "u".repeat(3000));
        let title = "T".repeat(900);
        let v = parse(&payload(&Notification {
            title: &title,
            url: Some(&url),
            ..note(&data)
        }));
        assert_eq!(v["aps"]["alert"]["body"], "");
        assert!(v.get("url").is_none());
        assert_eq!(v["aps"]["alert"]["title"], title.as_str());

        // A title too long on its own is cut to 40 characters.
        let title = "é".repeat(3000);
        let out = payload(&Notification {
            title: &title,
            ..note(&data)
        });
        assert!(out.len() <= MAX_PAYLOAD_BYTES);
        let v = parse(&out);
        assert_eq!(v["aps"]["alert"]["title"], "é".repeat(40));
        assert_eq!(v["aps"]["alert"]["body"], "");
        assert!(v.get("url").is_none());
        assert_eq!(v["aps"]["category"], "backup.failed");
        assert_eq!(v["aps"]["thread-id"], "backup");
        assert_eq!(v["data"]["collapse_id"], "b1-29fd692930019189");
    }

    #[test]
    fn a_short_payload_is_untouched() {
        let data = data();
        let v = parse(&payload(&note(&data)));
        assert_eq!(
            v["aps"]["alert"]["body"],
            "The disk is full. The last good backup was 02:00."
        );
    }

    fn activity() -> LiveActivity {
        LiveActivity {
            event: "update".into(),
            content_state: json!({"step": "Reading the logs", "progress": 0.4}),
            timestamp: 1_794_571_200,
            dismissal_date: None,
            stale_date: None,
            alert: None,
            priority: 10,
        }
    }

    #[test]
    fn activity_payload_shape() {
        let v = parse(&activity_payload(&activity()).unwrap());
        assert_eq!(
            v,
            json!({"aps": {"timestamp": 1_794_571_200, "event": "update", "content-state": {"step": "Reading the logs", "progress": 0.4}}})
        );
        let full = LiveActivity {
            dismissal_date: Some(1_794_574_800),
            stale_date: Some(1_794_572_100),
            alert: Some(("Still working".into(), "Two steps left".into())),
            event: "end".into(),
            ..activity()
        };
        let v = parse(&activity_payload(&full).unwrap());
        assert_eq!(v["aps"]["dismissal-date"], 1_794_574_800);
        assert_eq!(v["aps"]["stale-date"], 1_794_572_100);
        assert_eq!(v["aps"]["event"], "end");
        assert_eq!(
            v["aps"]["alert"],
            json!({"title": "Still working", "body": "Two steps left", "sound": "default"})
        );
    }

    #[test]
    fn an_activity_over_4kb_is_refused() {
        let big = LiveActivity {
            content_state: json!({"log": "x".repeat(5000)}),
            ..activity()
        };
        let p = activity_payload(&big).unwrap_err();
        assert_eq!(p.at, "live activity");
        assert!(p.message.contains("over Apple's 4096"), "{p}");
        assert!(p.fix.contains("never trimmed"), "{p}");
    }
}
