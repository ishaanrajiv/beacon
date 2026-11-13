//! Turning an event's facts and slots into the text people see, from its kind's templates
//! (`spec/declarations.md`, "Rendering"). [`check_values`] says what is wrong with the values an
//! event sent; [`render`] and [`render_recovery`] write the title, body and link.

use crate::decl::{BODY_MAX, FACT_STRING_MAX, FactBase, Kind, LASTED, TITLE_MAX};
use crate::duration::{DAY, HOUR, MINUTE};
use crate::problem::{Problem, closest};
use crate::template::{Template, clean, percent_encode, truncate_words};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde_json::{Map, Value};

/// The moment rendering happens and the timezone times are shown in. Times are written relative to
/// `now` ("yesterday 02:00"), so the same event renders the same way only at the same moment.
#[derive(Debug, Clone)]
pub struct Clock {
    pub now_ms: i64,
    pub tz: TimeZone,
}

/// An event's rendered text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub title: String,
    pub body: String,
    /// The kind's `open` link with facts filled in. None when the kind declares none.
    pub url: Option<String>,
}

/// Everything wrong with the facts and slots an event sent for a kind. Empty when they are fine.
/// `at` prefixes each problem's place (`event`, or `snapshot firing[2]`).
pub fn check_values(
    at: &str,
    kind_name: &str,
    kind: &Kind,
    facts: &Map<String, Value>,
    slots: &Map<String, Value>,
) -> Vec<Problem> {
    let mut problems = Vec::new();
    for (name, fact) in &kind.facts {
        let here = format!("{at} facts.{name}");
        match facts.get(name) {
            None | Some(Value::Null) if fact.optional => {}
            None | Some(Value::Null) => problems.push(Problem::new(
                here,
                format!("{kind_name} needs the fact `{name}`"),
                format!("send facts.{name} as {}", describe(fact.base)),
            )),
            Some(value) => {
                if let Err(message) = fact_value(fact.base, value) {
                    problems.push(Problem::new(
                        here,
                        message,
                        format!("send {}", describe(fact.base)),
                    ));
                }
            }
        }
    }
    for name in facts.keys() {
        if !kind.facts.contains_key(name) {
            problems.push(unknown(at, "facts", name, kind_name, kind.facts.keys()));
        }
    }
    for name in kind.slots.keys() {
        match slots.get(name) {
            Some(Value::String(_)) => {}
            None | Some(Value::Null) => problems.push(Problem::new(
                format!("{at} slots.{name}"),
                format!("{kind_name} needs the slot `{name}`"),
                format!("send slots.{name} as a string, \"\" when there is nothing to say"),
            )),
            Some(other) => problems.push(Problem::new(
                format!("{at} slots.{name}"),
                format!("a slot is text, and this is {}", json_type(other)),
                format!("send slots.{name} as a string"),
            )),
        }
    }
    for name in slots.keys() {
        if !kind.slots.contains_key(name) {
            problems.push(unknown(at, "slots", name, kind_name, kind.slots.keys()));
        }
    }
    problems
}

fn unknown<'a>(
    at: &str,
    field: &str,
    name: &str,
    kind_name: &str,
    known: impl Iterator<Item = &'a String> + Clone,
) -> Problem {
    let all: Vec<&str> = known.map(String::as_str).collect();
    Problem::new(
        format!("{at} {field}.{name}"),
        format!("{kind_name} declares no {} `{name}`", &field[..field.len() - 1]),
        match closest(name, all.iter().copied()) {
            Some(near) => format!("did you mean `{near}`?"),
            None if all.is_empty() => format!("send no {field}, or declare it in kinds.toml and publish"),
            None => format!("its {field} are {}", all.join(", ")),
        },
    )
}

fn describe(base: FactBase) -> &'static str {
    match base {
        FactBase::String => "a string of at most 200 characters",
        FactBase::Int => "a whole number",
        FactBase::Float => "a number",
        FactBase::Duration => "whole milliseconds, 0 or more",
        FactBase::Time => "milliseconds since 1970 UTC",
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

/// Checks one fact's value against its type.
fn fact_value(base: FactBase, value: &Value) -> Result<(), String> {
    let whole = value.as_i64();
    match (base, value) {
        (FactBase::String, Value::String(text)) => {
            let len = text.chars().count();
            if len > FACT_STRING_MAX {
                Err(format!(
                    "this is {len} characters; a fact is a short value of at most {FACT_STRING_MAX}, and longer text goes in a slot"
                ))
            } else {
                Ok(())
            }
        }
        (FactBase::Int, Value::Number(_)) if whole.is_some() => Ok(()),
        (FactBase::Float, Value::Number(n)) if n.as_f64().is_some_and(f64::is_finite) => Ok(()),
        (FactBase::Duration, Value::Number(_)) if whole.is_some_and(|ms| ms >= 0) => Ok(()),
        (FactBase::Time, Value::Number(_))
            if whole.is_some_and(|ms| Timestamp::from_millisecond(ms).is_ok()) =>
        {
            Ok(())
        }
        (_, other) => {
            let article = if base == FactBase::Int { "an" } else { "a" };
            Err(format!(
                "{article} {} fact can't be {}",
                base.name(),
                describe_value(other)
            ))
        }
    }
}

fn describe_value(value: &Value) -> String {
    match value {
        Value::Number(n) => format!("{n}"),
        other => json_type(other).to_string(),
    }
}

/// Renders an event's title, body and link. The values must have passed [`check_values`]; one that
/// didn't renders as empty text rather than failing.
pub fn render(
    kind: &Kind,
    facts: &Map<String, Value>,
    slots: &Map<String, Value>,
    clock: &Clock,
) -> Rendered {
    let values = Values {
        kind,
        facts,
        slots,
        lasted: None,
        clock,
    };
    Rendered {
        title: finish(&kind.title, &values, TITLE_MAX),
        body: finish(&kind.body, &values, BODY_MAX),
        url: kind.open.as_deref().map(|open| link(open, kind, facts, clock)),
    }
}

/// Renders a custom action's link, or an `open` link: facts percent-encoded.
pub fn link(template: &str, kind: &Kind, facts: &Map<String, Value>, clock: &Clock) -> String {
    let empty = Map::new();
    let values = Values {
        kind,
        facts,
        slots: &empty,
        lasted: None,
        clock,
    };
    match Template::parse(template) {
        Ok(parsed) => parsed.render(|name| percent_encode(&values.get(name))),
        Err(_) => String::new(),
    }
}

/// Renders the title and body an alert shows once it recovers, after `lasted_ms` open. None for a
/// kind with no recovery (not an alert, or one-off).
pub fn render_recovery(
    kind: &Kind,
    facts: &Map<String, Value>,
    slots: &Map<String, Value>,
    lasted_ms: i64,
    clock: &Clock,
) -> Option<(String, String)> {
    let values = Values {
        kind,
        facts,
        slots,
        lasted: Some(lasted_ms),
        clock,
    };
    let title = finish(kind.recovered.as_deref()?, &values, TITLE_MAX);
    let body = kind
        .recovered_body
        .as_deref()
        .map(|body| finish(body, &values, BODY_MAX))
        .unwrap_or_default();
    Some((title, body))
}

struct Values<'a> {
    kind: &'a Kind,
    facts: &'a Map<String, Value>,
    slots: &'a Map<String, Value>,
    lasted: Option<i64>,
    clock: &'a Clock,
}

impl Values<'_> {
    fn get(&self, name: &str) -> String {
        if name == LASTED {
            return self.lasted.map(format_duration).unwrap_or_default();
        }
        if let Some(max) = self.kind.slots.get(name) {
            let text = self.slots.get(name).and_then(Value::as_str).unwrap_or_default();
            return truncate_words(&clean(text), *max as usize);
        }
        let (Some(fact), Some(value)) = (self.kind.facts.get(name), self.facts.get(name)) else {
            return String::new();
        };
        match (fact.base, value) {
            (FactBase::String, Value::String(text)) => clean(text),
            (FactBase::Int, Value::Number(n)) => n.as_i64().map(|n| n.to_string()).unwrap_or_default(),
            (FactBase::Float, Value::Number(n)) => n.as_f64().map(format_float).unwrap_or_default(),
            (FactBase::Duration, Value::Number(n)) => n.as_i64().map(format_duration).unwrap_or_default(),
            (FactBase::Time, Value::Number(n)) => n
                .as_i64()
                .map(|ms| format_time(ms, self.clock))
                .unwrap_or_default(),
            _ => String::new(),
        }
    }
}

/// Renders a template, then tidies what empty values left behind and cuts it to `max`.
fn finish(template: &str, values: &Values<'_>, max: usize) -> String {
    let text = match Template::parse(template) {
        Ok(parsed) => parsed.render(|name| values.get(name)),
        Err(_) => String::new(),
    };
    truncate_words(&clean(&text), max)
}

/// `95.13` as "95.1", `150.4` as "150", `2.0` as "2".
pub fn format_float(value: f64) -> String {
    let text = if value.abs() >= 100.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    };
    let text = text.strip_suffix(".0").unwrap_or(&text).to_string();
    if text == "-0" { "0".to_string() } else { text }
}

/// "under a minute", "4 min", "2 h", "1 h 5 min", "3 days".
pub fn format_duration(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < MINUTE {
        "under a minute".to_string()
    } else if ms < HOUR {
        format!("{} min", ms / MINUTE)
    } else if ms < DAY {
        let (hours, minutes) = (ms / HOUR, ms % HOUR / MINUTE);
        if minutes == 0 {
            format!("{hours} h")
        } else {
            format!("{hours} h {minutes} min")
        }
    } else {
        let days = ms / DAY;
        if days == 1 {
            "1 day".to_string()
        } else {
            format!("{days} days")
        }
    }
}

/// A moment, as a person reads it beside `now`: "16:45" today, "yesterday 16:45", "tomorrow 09:00",
/// "Tue 16:45" within six days either way, else "12 Nov", with the year when it isn't this one.
pub fn format_time(ms: i64, clock: &Clock) -> String {
    let (Ok(at), Ok(now)) = (
        Timestamp::from_millisecond(ms),
        Timestamp::from_millisecond(clock.now_ms),
    ) else {
        return String::new();
    };
    let at = at.to_zoned(clock.tz.clone());
    let today = now.to_zoned(clock.tz.clone()).date();
    let days = today
        .until(at.date())
        .map(|span| span.get_days())
        .unwrap_or(i32::MAX);
    let clock_time = at.strftime("%H:%M").to_string();
    match days {
        0 => clock_time,
        -1 => format!("yesterday {clock_time}"),
        1 => format!("tomorrow {clock_time}"),
        -6..=6 => format!("{} {clock_time}", at.strftime("%a")),
        _ if at.year() == today.year() => at.strftime("%-d %b").to_string(),
        _ => at.strftime("%-d %b %Y").to_string(),
    }
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
