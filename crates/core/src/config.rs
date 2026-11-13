//! `beacon.toml` (`spec/config.md`): every field optional, every unknown field an error with the
//! name you probably meant, every value checked against its range at start.

use crate::Problem;
use crate::delivery::Settings;
use crate::duration::{DAY, Duration, HOUR, MINUTE, SECOND};
use crate::problem::closest;
use crate::quiet::{ClockTime, QuietHours};
use jiff::tz::TimeZone;
use std::path::{Path, PathBuf};

/// The file `beacon serve --config` reads.
pub const FILE: &str = "beacon.toml";
/// Where Beacon listens with no `listen`.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8210";

/// Everything beacon.toml says, with defaults filled in.
#[derive(Debug, Clone)]
pub struct Config {
    /// `listen`: an address and port.
    pub listen: String,
    /// `data_dir`: holds beacon.db.
    pub data_dir: PathBuf,
    /// `devices_db`: the devices and their push tokens. Default `data_dir/devices.db`.
    pub devices_db: PathBuf,
    /// `timezone`, as written.
    pub timezone: String,
    /// `origins`: Origin values a browser may send mutations from, beside the request's own host.
    pub origins: Vec<String>,
    pub banned_words: Vec<String>,
    /// `limits.body_bytes`.
    pub body_bytes: usize,
    /// `presence.heartbeat`, in ms: what `POST /v1/presence` tells clients.
    pub heartbeat: i64,
    /// The numbers the store decides with.
    pub settings: Settings,
    /// `[apns]`, when `key_path` is set.
    pub apns: Option<Apns>,
    /// `apns.retry`, in ms: the waits between attempts.
    pub retry: Vec<i64>,
    /// `inbox.page`: the default activity page.
    pub inbox_page: u32,
    /// `trail.open`: the link every `trail.*` item opens.
    pub trail_open: String,
    /// `[sink]`, when `trail_url` is set.
    pub sink: Option<Sink>,
    /// `auth.producer_token`.
    pub producer_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Apns {
    pub key_path: PathBuf,
    /// None: from the key's file name, `AuthKey_<key id>.p8`.
    pub key_id: Option<String>,
    pub team_id: String,
    pub topic: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sink {
    pub trail_url: String,
    /// The Trail source the copies are recorded under.
    pub source: String,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            listen: DEFAULT_LISTEN.into(),
            data_dir: PathBuf::from("./beacon-data"),
            devices_db: PathBuf::from("./beacon-data/devices.db"),
            timezone: "UTC".into(),
            origins: Vec::new(),
            banned_words: Vec::new(),
            body_bytes: 1_048_576,
            heartbeat: 30 * SECOND,
            settings: Settings::default(),
            apns: None,
            retry: vec![10 * SECOND, 30 * SECOND, 2 * MINUTE, 5 * MINUTE],
            inbox_page: 50,
            trail_open: "/".into(),
            sink: None,
            producer_token: None,
        }
    }
}

const TOP: [&str; 18] = [
    "listen",
    "data_dir",
    "devices_db",
    "timezone",
    "push",
    "origins",
    "banned_words",
    "retention",
    "idempotency",
    "limits",
    "presence",
    "quiet_hours",
    "delivery",
    "apns",
    "inbox",
    "trail",
    "sink",
    "auth",
];
const LIMITS: [&str; 1] = ["body_bytes"];
const PRESENCE: [&str; 2] = ["heartbeat", "active_window"];
const QUIET: [&str; 2] = ["start", "end"];
const DELIVERY: [&str; 10] = [
    "hold",
    "alert_hold",
    "flap_window",
    "flap_openings",
    "flap_stable",
    "budget",
    "snooze",
    "snapshot_stale_after",
    "late",
    "give_up",
];
const APNS: [&str; 6] = ["key_path", "key_id", "team_id", "topic", "retry", "give_up"];
const INBOX: [&str; 3] = ["resolved_days", "page", "since_last_visit_items"];
const TRAIL: [&str; 2] = ["open", "stale_after"];
const SINK: [&str; 2] = ["trail_url", "source"];
const AUTH: [&str; 1] = ["producer_token"];

/// Reads beacon.toml at `path`; None reads no file and keeps every default.
pub fn load(path: Option<&Path>) -> Result<Config, Vec<Problem>> {
    let Some(path) = path else {
        return parse("");
    };
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).map_err(|problems| {
            problems
                .into_iter()
                .map(|p| Problem::new(format!("{}: {}", path.display(), p.at), p.message, p.fix))
                .collect()
        }),
        Err(e) => Err(vec![Problem::new(
            path.display().to_string(),
            format!("can't read it: {e}"),
            "check the path given to --config, or leave --config out to run with the defaults",
        )]),
    }
}

/// Parses beacon.toml's text.
pub fn parse(text: &str) -> Result<Config, Vec<Problem>> {
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| {
        vec![Problem::new(
            FILE,
            format!("not valid TOML: {}", e.message()),
            "fix the line it names; spec/config.md shows every field",
        )]
    })?;
    let mut r = Reader::default();
    let mut c = Config::default();
    r.unknown("", &table, &TOP);

    if let Some(listen) = r.string(&table, "", "listen") {
        if listen.parse::<std::net::SocketAddr>().is_ok() {
            c.listen = listen;
        } else {
            r.problem(
                "listen",
                format!("`{listen}` is not an address and port"),
                "write an IP address and a port, like \"127.0.0.1:8210\"",
            );
        }
    }
    if let Some(dir) = r.string(&table, "", "data_dir") {
        c.data_dir = PathBuf::from(dir);
    }
    c.devices_db = match r.string(&table, "", "devices_db") {
        Some(path) => PathBuf::from(path),
        None => c.data_dir.join("devices.db"),
    };
    if let Some(zone) = r.string(&table, "", "timezone") {
        match TimeZone::get(&zone) {
            Ok(tz) => {
                c.settings.tz = tz;
                c.timezone = zone;
            }
            Err(_) => {
                let fix = match closest(
                    &zone,
                    ["UTC", "Europe/London", "America/New_York", "Asia/Kolkata"],
                ) {
                    Some(near) => format!("did you mean `{near}`? Write an IANA zone name"),
                    None => "write an IANA zone name, like \"Europe/London\", or \"UTC\"".into(),
                };
                r.problem(
                    "timezone",
                    format!("`{zone}` is not a time zone Beacon can load"),
                    fix,
                );
            }
        }
    }
    if let Some(push) = r.boolean(&table, "", "push") {
        c.settings.push = push;
    }
    for origin in r.strings(&table, "", "origins") {
        if is_origin(&origin) {
            c.origins.push(origin);
        } else {
            r.problem(
                "origins",
                format!("`{origin}` is not an origin"),
                "write a scheme, a host and an optional port, with no path, like \"https://lab.example.com\"",
            );
        }
    }
    for word in r.strings(&table, "", "banned_words") {
        if word.is_empty() || word.chars().any(char::is_whitespace) {
            r.problem(
                "banned_words",
                format!("`{word}` is not a single word"),
                "list single words, like [\"jarvis\"]",
            );
        } else {
            c.banned_words.push(word);
        }
    }
    if let Some(ms) = r.duration(&table, "", "retention", DAY, 3650 * DAY) {
        c.settings.retention = ms;
    }
    if let Some(ms) = r.duration(&table, "", "idempotency", HOUR, 7 * DAY) {
        c.settings.idempotency = ms;
    }

    let limits = r.section(&table, "limits", &LIMITS);
    if let Some(n) = r.integer(&limits, "limits", "body_bytes", 1024, 16 * 1024 * 1024) {
        c.body_bytes = usize::try_from(n).unwrap_or(c.body_bytes);
    }

    let presence = r.section(&table, "presence", &PRESENCE);
    if let Some(ms) = r.duration(&presence, "presence", "heartbeat", 5 * SECOND, 5 * MINUTE) {
        c.heartbeat = ms;
    }
    if let Some(ms) = r.duration(&presence, "presence", "active_window", 10 * SECOND, 15 * MINUTE) {
        c.settings.active_window = ms;
    }
    if c.heartbeat >= c.settings.active_window {
        r.problem(
            "presence.heartbeat",
            "the heartbeat must be shorter than active_window, or a device goes inactive between beats",
            "keep active_window at least twice the heartbeat, like heartbeat = \"30s\", active_window = \"75s\"",
        );
    }

    let quiet = r.section(&table, "quiet_hours", &QUIET);
    let start = r.clock(&quiet, "quiet_hours", "start");
    let end = r.clock(&quiet, "quiet_hours", "end");
    c.settings.quiet_hours = QuietHours {
        start: start.unwrap_or(c.settings.quiet_hours.start),
        end: end.unwrap_or(c.settings.quiet_hours.end),
    };

    let d = r.section(&table, "delivery", &DELIVERY);
    let s = &mut c.settings;
    let set = |slot: &mut i64, value: Option<i64>| {
        if let Some(v) = value {
            *slot = v;
        }
    };
    set(
        &mut s.hold,
        r.duration(&d, "delivery", "hold", SECOND, 30 * MINUTE),
    );
    set(
        &mut s.alert_hold,
        r.duration(&d, "delivery", "alert_hold", SECOND, 30 * MINUTE),
    );
    set(
        &mut s.flap_window,
        r.duration(&d, "delivery", "flap_window", MINUTE, DAY),
    );
    set(
        &mut s.flap_stable,
        r.duration(&d, "delivery", "flap_stable", MINUTE, DAY),
    );
    if let Some(n) = r.integer(&d, "delivery", "flap_openings", 1, 100) {
        s.flap_openings = u32::try_from(n).unwrap_or(s.flap_openings);
    }
    if let Some(n) = r.integer(&d, "delivery", "budget", 0, 1000) {
        s.budget = u32::try_from(n).unwrap_or(s.budget);
    }
    set(
        &mut s.snooze,
        r.duration(&d, "delivery", "snooze", MINUTE, 30 * DAY),
    );
    set(
        &mut s.snapshot_stale_after,
        r.duration(&d, "delivery", "snapshot_stale_after", MINUTE, 7 * DAY),
    );
    set(&mut s.late, r.duration(&d, "delivery", "late", MINUTE, 7 * DAY));
    // Older files put give_up under [delivery]; spec/config.md has it under [apns].
    set(&mut s.give_up, r.duration(&d, "delivery", "give_up", MINUTE, DAY));

    let a = r.section(&table, "apns", &APNS);
    set(
        &mut c.settings.give_up,
        r.duration(&a, "apns", "give_up", MINUTE, DAY),
    );
    if a.contains_key("retry") {
        let mut retry = Vec::new();
        for (i, text) in r.strings(&a, "apns", "retry").iter().enumerate() {
            match Duration::parse(text) {
                Ok(d) if (SECOND..=HOUR).contains(&d.ms()) => retry.push(d.ms()),
                Ok(_) => r.problem(
                    &format!("apns.retry[{i}]"),
                    format!("`{text}` is outside 1s to 1h"),
                    "write waits from 1s to 1h, like [\"10s\", \"30s\", \"2m\", \"5m\"]",
                ),
                Err(message) => r.problem(
                    &format!("apns.retry[{i}]"),
                    message,
                    format!("write {}", Duration::FORMS),
                ),
            }
        }
        if retry.len() > 10 {
            r.problem(
                "apns.retry",
                format!("{} waits; at most 10", retry.len()),
                "keep the list short: a push older than give_up is dropped anyway",
            );
        }
        c.retry = retry;
    }
    let key_path = r.string(&a, "apns", "key_path");
    let key_id = r.string(&a, "apns", "key_id");
    let team_id = r.string(&a, "apns", "team_id");
    let topic = r.string(&a, "apns", "topic");
    if let Some(key_path) = key_path {
        if let Some(id) = &key_id
            && !is_apple_id(id)
        {
            r.problem(
                "apns.key_id",
                format!("`{id}` is not a key id"),
                "write the 10-character id Apple shows beside the key, like \"ABC123DEFG\"",
            );
        }
        match &team_id {
            Some(id) if is_apple_id(id) => {}
            Some(id) => r.problem(
                "apns.team_id",
                format!("`{id}` is not a team id"),
                "write your 10-character Apple team id, like \"TEAM123456\"",
            ),
            None => r.problem(
                "apns.team_id",
                "key_path is set but team_id isn't",
                "add team_id = \"<your 10-character Apple team id>\"",
            ),
        }
        match &topic {
            Some(t) if !t.is_empty() && !t.contains(char::is_whitespace) => {}
            Some(t) => r.problem(
                "apns.topic",
                format!("`{t}` is not a bundle id"),
                "write the app's bundle id, like \"com.example.lab\"",
            ),
            None => r.problem(
                "apns.topic",
                "key_path is set but topic isn't",
                "add topic = \"<the app's bundle id>\", like \"com.example.lab\"",
            ),
        }
        c.apns = Some(Apns {
            key_path: PathBuf::from(key_path),
            key_id,
            team_id: team_id.unwrap_or_default(),
            topic: topic.unwrap_or_default(),
        });
    } else if key_id.is_some() || team_id.is_some() || topic.is_some() {
        r.problem(
            "apns.key_path",
            "key_id, team_id or topic is set but key_path isn't, so Beacon can't push",
            "add key_path = \"/etc/beacon/AuthKey_<key id>.p8\", or remove the [apns] fields",
        );
    }

    let inbox = r.section(&table, "inbox", &INBOX);
    if let Some(n) = r.integer(&inbox, "inbox", "resolved_days", 1, 90) {
        c.settings.resolved_for = n * DAY;
    }
    if let Some(n) = r.integer(&inbox, "inbox", "page", 1, 200) {
        c.inbox_page = u32::try_from(n).unwrap_or(c.inbox_page);
    }
    if let Some(n) = r.integer(&inbox, "inbox", "since_last_visit_items", 0, 50) {
        c.settings.since_last_visit_items = u32::try_from(n).unwrap_or(5);
    }

    let trail = r.section(&table, "trail", &TRAIL);
    if let Some(open) = r.string(&trail, "trail", "open") {
        if open.starts_with('/') || open.starts_with("https://") {
            c.trail_open = open;
        } else {
            r.problem(
                "trail.open",
                format!("`{open}` is not a link Beacon can send"),
                "start it with / (a page of your app) or https://",
            );
        }
    }
    if let Some(ms) = r.duration(&trail, "trail", "stale_after", MINUTE, 7 * DAY) {
        c.settings.trail_stale_after = ms;
    }

    let sink = r.section(&table, "sink", &SINK);
    let source = r
        .string(&sink, "sink", "source")
        .unwrap_or_else(|| "beacon".into());
    if !crate::names::is_source(&source) {
        r.problem(
            "sink.source",
            format!("`{source}` is not a Trail source name"),
            "use lowercase letters, digits and hyphens, like \"beacon\"",
        );
    }
    if let Some(url) = r.string(&sink, "sink", "trail_url") {
        if url.starts_with("http://") || url.starts_with("https://") {
            c.sink = Some(Sink {
                trail_url: url.trim_end_matches('/').to_string(),
                source,
            });
        } else {
            r.problem(
                "sink.trail_url",
                format!("`{url}` is not an http:// or https:// URL"),
                "write Trail's address, like \"http://127.0.0.1:8200\"",
            );
        }
    }

    let auth = r.section(&table, "auth", &AUTH);
    if let Some(token) = r.string(&auth, "auth", "producer_token") {
        if token.len() < 16 || !token.bytes().all(|b| b.is_ascii_graphic()) {
            r.problem(
                "auth.producer_token",
                "a producer token is at least 16 printable characters with no spaces",
                "make one with `openssl rand -hex 24`",
            );
        } else {
            c.producer_token = Some(token);
        }
    }

    if r.problems.is_empty() {
        Ok(c)
    } else {
        Err(r.problems)
    }
}

/// Apple's 10-character key and team ids.
fn is_apple_id(text: &str) -> bool {
    text.len() == 10 && text.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// `https://lab.example.com` or `http://127.0.0.1:8080`: a scheme, a host, an optional port.
pub fn is_origin(text: &str) -> bool {
    let rest = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"));
    let Some(rest) = rest else {
        return false;
    };
    let (host, port) = match rest.rsplit_once(':') {
        Some((host, port)) if !host.ends_with(']') || rest.starts_with('[') => (host, Some(port)),
        _ => (rest, None),
    };
    !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'[' | b']' | b':'))
        && port.is_none_or(|p| !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()))
}

#[derive(Default)]
struct Reader {
    problems: Vec<Problem>,
}

fn at(section: &str, key: &str) -> String {
    if section.is_empty() {
        key.to_string()
    } else {
        format!("{section}.{key}")
    }
}

impl Reader {
    fn problem(&mut self, at: &str, message: impl Into<String>, fix: impl Into<String>) {
        self.problems
            .push(Problem::new(format!("{FILE}: {at}"), message, fix));
    }

    fn unknown(&mut self, section: &str, table: &toml::Table, known: &[&str]) {
        for key in table.keys() {
            if known.contains(&key.as_str()) {
                continue;
            }
            let fix = match closest(key, known.iter().copied()) {
                Some(near) => format!("did you mean `{near}`?"),
                None if section.is_empty() => format!("the fields are {}", known.join(", ")),
                None => format!("[{section}] has {}", known.join(", ")),
            };
            self.problem(
                &at(section, key),
                format!("`{key}` is not a field Beacon reads"),
                fix,
            );
        }
    }

    /// A `[section]` table, checked for unknown fields; empty when absent.
    fn section(&mut self, table: &toml::Table, name: &str, known: &[&str]) -> toml::Table {
        match table.get(name) {
            None => toml::Table::new(),
            Some(toml::Value::Table(t)) => {
                self.unknown(name, t, known);
                t.clone()
            }
            Some(other) => {
                self.problem(
                    name,
                    format!("{name} must be a table, not {}", other.type_str()),
                    format!("write it as a [{name}] section"),
                );
                toml::Table::new()
            }
        }
    }

    fn string(&mut self, table: &toml::Table, section: &str, key: &str) -> Option<String> {
        match table.get(key)? {
            toml::Value::String(text) => Some(text.clone()),
            other => {
                self.problem(
                    &at(section, key),
                    format!("{key} must be a string, not {}", other.type_str()),
                    format!("write it in quotes: {key} = \"...\""),
                );
                None
            }
        }
    }

    fn strings(&mut self, table: &toml::Table, section: &str, key: &str) -> Vec<String> {
        match table.get(key) {
            None => Vec::new(),
            Some(toml::Value::Array(items)) => {
                let mut out = Vec::new();
                for (i, item) in items.iter().enumerate() {
                    match item.as_str() {
                        Some(text) => out.push(text.to_string()),
                        None => self.problem(
                            &format!("{}[{i}]", at(section, key)),
                            format!("each of {key} must be a string"),
                            format!("write {key} = [\"...\"]"),
                        ),
                    }
                }
                out
            }
            Some(other) => {
                self.problem(
                    &at(section, key),
                    format!("{key} must be a list, not {}", other.type_str()),
                    format!("write {key} = [\"...\"]"),
                );
                Vec::new()
            }
        }
    }

    fn boolean(&mut self, table: &toml::Table, section: &str, key: &str) -> Option<bool> {
        match table.get(key)? {
            toml::Value::Boolean(flag) => Some(*flag),
            other => {
                self.problem(
                    &at(section, key),
                    format!("{key} must be true or false, not {}", other.type_str()),
                    format!("write {key} = true or {key} = false"),
                );
                None
            }
        }
    }

    fn integer(&mut self, table: &toml::Table, section: &str, key: &str, min: i64, max: i64) -> Option<i64> {
        match table.get(key)? {
            toml::Value::Integer(n) if (min..=max).contains(n) => Some(*n),
            toml::Value::Integer(n) => {
                self.problem(
                    &at(section, key),
                    format!("{n} is outside {min} to {max}"),
                    format!("write a whole number from {min} to {max}"),
                );
                None
            }
            other => {
                self.problem(
                    &at(section, key),
                    format!("{key} must be a whole number, not {}", other.type_str()),
                    format!("write a whole number from {min} to {max}, without quotes"),
                );
                None
            }
        }
    }

    /// A duration string within `[min, max]`, in ms.
    fn duration(&mut self, table: &toml::Table, section: &str, key: &str, min: i64, max: i64) -> Option<i64> {
        let text = self.string(table, section, key)?;
        let range = format!(
            "{} to {}",
            Duration::from_ms(min).map(|d| d.to_string()).unwrap_or_default(),
            Duration::from_ms(max).map(|d| d.to_string()).unwrap_or_default()
        );
        match Duration::parse(&text) {
            Ok(d) if (min..=max).contains(&d.ms()) => Some(d.ms()),
            Ok(_) => {
                self.problem(
                    &at(section, key),
                    format!("`{text}` is outside {range}"),
                    format!("write a duration from {range}"),
                );
                None
            }
            Err(message) => {
                self.problem(&at(section, key), message, format!("write {}", Duration::FORMS));
                None
            }
        }
    }

    fn clock(&mut self, table: &toml::Table, section: &str, key: &str) -> Option<ClockTime> {
        let text = self.string(table, section, key)?;
        match ClockTime::parse(&text) {
            Ok(t) => Some(t),
            Err(message) => {
                self.problem(
                    &at(section, key),
                    message,
                    "write a 24-hour local time, like \"23:00\"",
                );
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn problems(text: &str) -> String {
        match parse(text) {
            Ok(_) => String::new(),
            Err(p) => p.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n"),
        }
    }

    #[test]
    fn no_file_keeps_every_default() {
        let c = parse("").unwrap();
        assert_eq!(c.listen, "127.0.0.1:8210");
        assert_eq!(c.data_dir, PathBuf::from("./beacon-data"));
        assert_eq!(c.devices_db, PathBuf::from("./beacon-data/devices.db"));
        assert!(c.apns.is_none() && c.sink.is_none() && c.producer_token.is_none());
        assert_eq!(c.retry, [10_000, 30_000, 120_000, 300_000]);
        assert_eq!(c.heartbeat, 30_000);
        assert_eq!(c.body_bytes, 1_048_576);
        assert_eq!(c.inbox_page, 50);
        assert_eq!(c.settings.give_up, HOUR);
        assert!(c.settings.push);
        assert!(load(None).is_ok());
    }

    #[test]
    fn the_spec_example_reads() {
        let text = r#"
listen = "127.0.0.1:8300"
data_dir = "/var/lib/beacon"
timezone = "Europe/London"
push = false
origins = ["https://lab.example.com", "http://127.0.0.1:8080"]
banned_words = ["example"]
retention = "30d"
idempotency = "2h"
[limits]
body_bytes = 2048
[presence]
heartbeat = "20s"
active_window = "60s"
[quiet_hours]
start = "22:30"
end = "07:00"
[delivery]
hold = "60s"
alert_hold = "90s"
flap_window = "20m"
flap_openings = 4
flap_stable = "10m"
budget = 8
snooze = "2h"
snapshot_stale_after = "20m"
late = "30m"
[apns]
key_path = "/etc/beacon/AuthKey_ABC123DEFG.p8"
team_id = "TEAM123456"
topic = "com.example.lab"
retry = ["5s", "1m"]
give_up = "30m"
[inbox]
resolved_days = 3
page = 20
since_last_visit_items = 2
[trail]
open = "/trail"
stale_after = "12m"
[sink]
trail_url = "http://127.0.0.1:8200/"
source = "beacon"
[auth]
producer_token = "0123456789abcdef0123"
"#;
        let c = parse(text).unwrap_or_else(|p| panic!("{p:?}"));
        assert_eq!(c.listen, "127.0.0.1:8300");
        assert_eq!(c.devices_db, PathBuf::from("/var/lib/beacon/devices.db"));
        assert_eq!(c.timezone, "Europe/London");
        assert!(!c.settings.push);
        assert_eq!(c.origins.len(), 2);
        assert_eq!(c.settings.retention, 30 * DAY);
        assert_eq!(c.settings.idempotency, 2 * HOUR);
        assert_eq!(c.body_bytes, 2048);
        assert_eq!(
            (c.heartbeat, c.settings.active_window),
            (20 * SECOND, 60 * SECOND)
        );
        assert_eq!(c.settings.quiet_hours.start.to_string(), "22:30");
        assert_eq!(c.settings.hold, 60 * SECOND);
        assert_eq!(c.settings.flap_openings, 4);
        assert_eq!(c.settings.budget, 8);
        assert_eq!(c.settings.late, 30 * MINUTE);
        let apns = c.apns.unwrap();
        assert_eq!(apns.key_id, None);
        assert_eq!(apns.topic, "com.example.lab");
        assert_eq!(c.retry, [5 * SECOND, MINUTE]);
        assert_eq!(c.settings.give_up, 30 * MINUTE);
        assert_eq!(c.settings.resolved_for, 3 * DAY);
        assert_eq!(c.inbox_page, 20);
        assert_eq!(c.settings.since_last_visit_items, 2);
        assert_eq!(c.trail_open, "/trail");
        assert_eq!(c.settings.trail_stale_after, 12 * MINUTE);
        assert_eq!(c.sink.unwrap().trail_url, "http://127.0.0.1:8200");
        assert!(c.producer_token.is_some());
    }

    #[test]
    fn every_bad_value_names_its_field_and_a_fix() {
        for (text, expected) in [
            ("lisen = \"x\"", "did you mean `listen`?"),
            ("[delivery]\nhodl = \"1s\"", "did you mean `hold`?"),
            ("[apns]\ntopik = \"x\"", "did you mean `topic`?"),
            ("[nothing]\nx = 1", "the fields are"),
            ("listen = \"localhost\"", "is not an address and port"),
            ("timezone = \"Mars/Base\"", "is not a time zone"),
            ("push = \"yes\"", "must be true or false"),
            ("origins = [\"lab.example.com\"]", "is not an origin"),
            ("origins = [\"https://lab.example.com/path\"]", "is not an origin"),
            ("banned_words = [\"two words\"]", "is not a single word"),
            ("retention = \"1h\"", "outside 1d to 3650d"),
            ("idempotency = \"8d\"", "outside 1h to 7d"),
            ("[limits]\nbody_bytes = 10", "outside 1024 to"),
            ("[limits]\nbody_bytes = \"1MB\"", "must be a whole number"),
            (
                "[presence]\nheartbeat = \"2m\"\nactive_window = \"1m\"",
                "shorter than active_window",
            ),
            ("[quiet_hours]\nstart = \"25:00\"", "quiet_hours.start"),
            ("[delivery]\nhold = \"31m\"", "outside 1s to 30m"),
            ("[delivery]\nflap_openings = 0", "outside 1 to 100"),
            ("[delivery]\nbudget = 1001", "outside 0 to 1000"),
            ("[delivery]\nsnooze = \"soon\"", "is not a duration"),
            ("[apns]\nkey_path = \"/k.p8\"", "team_id isn't"),
            (
                "[apns]\nkey_path = \"/k.p8\"\nteam_id = \"TEAM123456\"",
                "topic isn't",
            ),
            (
                "[apns]\nkey_path = \"/k.p8\"\nteam_id = \"team\"\ntopic = \"a.b\"",
                "is not a team id",
            ),
            (
                "[apns]\nkey_path = \"/k.p8\"\nkey_id = \"x\"\nteam_id = \"TEAM123456\"\ntopic = \"a.b\"",
                "is not a key id",
            ),
            ("[apns]\nteam_id = \"TEAM123456\"", "key_path isn't"),
            ("[apns]\nretry = [\"2h\"]", "outside 1s to 1h"),
            (
                "[apns]\nretry = [\"1s\",\"1s\",\"1s\",\"1s\",\"1s\",\"1s\",\"1s\",\"1s\",\"1s\",\"1s\",\"1s\"]",
                "at most 10",
            ),
            ("[apns]\ngive_up = \"2d\"", "outside 1m to 1d"),
            ("[inbox]\nresolved_days = 91", "outside 1 to 90"),
            ("[inbox]\npage = 0", "outside 1 to 200"),
            ("[trail]\nopen = \"http://x\"", "is not a link"),
            ("[sink]\ntrail_url = \"127.0.0.1:8200\"", "http:// or https://"),
            ("[sink]\nsource = \"Bea con\"", "not a Trail source name"),
            ("[auth]\nproducer_token = \"short\"", "at least 16"),
            ("limits = 3", "must be a table"),
            ("not toml", "not valid TOML"),
        ] {
            let found = problems(text);
            assert!(found.contains(expected), "{text}: {found}");
            assert!(found.contains("fix:"), "{text}: {found}");
        }
    }

    #[test]
    fn origins_are_scheme_host_and_port() {
        assert!(is_origin("https://lab.example.com"));
        assert!(is_origin("http://127.0.0.1:8080"));
        assert!(!is_origin("https://lab.example.com/"));
        assert!(!is_origin("ftp://lab.example.com"));
        assert!(!is_origin("https://"));
    }

    #[test]
    fn a_missing_file_says_so() {
        let err = load(Some(Path::new("/nonexistent/beacon.toml"))).unwrap_err();
        assert!(err[0].message.contains("can't read it"));
    }
}
