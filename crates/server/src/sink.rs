//! The copy to Trail (`spec/config.md`, "The copy to Trail"): every item change, in order, as a
//! `<sink.source>.notification.recorded` event, from a cursor kept in beacon.db. Outside the
//! notification's path: a Trail outage never delays a push, and the copy catches up when Trail
//! answers again. Each event's id is `beacon-<history row>`, so a resend is stored once.

use crate::App;
use beacon_core::config::Sink;
use beacon_store::SinkRecord;
use serde_json::{Map, Value, json};
use std::sync::atomic::Ordering;
use std::time::Duration;

const BATCH: u32 = 200;
/// How often to look for changes while there are none.
const IDLE: Duration = Duration::from_secs(2);
/// The longest wait between tries while Trail doesn't answer.
const BACKOFF_MAX: Duration = Duration::from_secs(300);

/// One record as Trail's event. The item's source goes in `item_source`: Trail reserves `source`.
pub(crate) fn event(sink: &Sink, record: &SinkRecord) -> Value {
    let mut props = Map::new();
    props.insert("item".into(), json!(record.item));
    props.insert("what".into(), json!(record.what));
    props.insert("kind".into(), json!(record.kind));
    props.insert("class".into(), json!(record.class));
    props.insert("item_source".into(), json!(record.source));
    if let Some(state) = &record.state {
        props.insert("state".into(), json!(state));
    }
    props.insert("title".into(), json!(record.title));
    props.insert("push_level".into(), json!(record.push_level));
    props.insert("push_status".into(), json!(record.push_status));
    if let Some(reason) = &record.push_reason {
        props.insert("push_reason".into(), json!(reason));
    }
    json!({
        "id": format!("beacon-{}", record.id),
        "name": format!("{}.notification.recorded", sink.source),
        "ts": record.at,
        "props": props,
    })
}

/// Sends batches until the server stops.
pub(crate) fn run(app: &App, sink: &Sink) {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        .build()
        .new_agent();
    let url = format!("{}/v1/ingest", sink.trail_url);
    let mut backoff = Duration::from_secs(1);
    while !app.stop.load(Ordering::Relaxed) {
        let batch = match app.store.sink_batch(BATCH) {
            Ok(batch) => batch,
            Err(e) => {
                eprintln!("beacon: the copy to Trail can't read beacon.db: {e}");
                std::thread::sleep(IDLE);
                continue;
            }
        };
        let Some(last) = batch.last().map(|r| r.id) else {
            std::thread::sleep(IDLE);
            continue;
        };
        let events: Vec<Value> = batch.iter().map(|r| event(sink, r)).collect();
        let body = json!({"source": sink.source, "version": crate::VERSION, "events": events});
        let sent = agent
            .post(&url)
            .header("Content-Type", "application/json")
            .send(body.to_string());
        match sent {
            Ok(response) if response.status().is_success() => {
                backoff = Duration::from_secs(1);
                if let Err(e) = app.store.sink_done(last, app.now()) {
                    eprintln!("beacon: the copy to Trail can't move its cursor: {e}");
                }
                if batch.len() < BATCH as usize {
                    std::thread::sleep(IDLE);
                }
            }
            outcome => {
                let why = match outcome {
                    Ok(response) => format!("HTTP {}", response.status()),
                    Err(e) => e.to_string(),
                };
                eprintln!(
                    "beacon: Trail didn't take the copy ({why}); trying again in {}s",
                    backoff.as_secs()
                );
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_items_source_is_item_source() {
        let sink = Sink {
            trail_url: "http://127.0.0.1:8200".into(),
            source: "beacon".into(),
        };
        let record = SinkRecord {
            id: 7,
            at: 1,
            item: 3,
            what: "opened".into(),
            kind: "backup.failed".into(),
            class: "alert".into(),
            source: "backup".into(),
            state: None,
            title: "Backup failed".into(),
            push_level: "urgent".into(),
            push_status: "held".into(),
            push_reason: Some("active".into()),
        };
        let e = event(&sink, &record);
        assert_eq!(e["id"], "beacon-7");
        assert_eq!(e["name"], "beacon.notification.recorded");
        assert_eq!(e["props"]["item_source"], "backup");
        assert!(e["props"].get("source").is_none());
        assert!(e["props"].get("state").is_none());
        assert_eq!(e["props"]["push_reason"], "active");
    }
}
