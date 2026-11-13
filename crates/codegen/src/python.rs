//! The Python emitter: the client's runtime, then one function per kind with keyword-only
//! arguments. Standard library only, Python 3.10+, clean under pyright's strict mode.

use crate::{PYTHON_CLIENT, header_lines, lifecycle_kinds, quote, runtime};
use beacon_core::decl::{Declarations, FactBase, FactType, Kind};
use std::fmt::Write;

/// Python's keywords that are valid snake_case names: a fact or slot can't be called one of these.
pub const KEYWORDS: [&str; 32] = [
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else",
    "except", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "nonlocal", "not",
    "or", "pass", "raise", "return", "try", "while", "with", "yield",
];

/// A kind's function name: `backup.failed` → `backup_failed`.
pub fn function(kind: &str) -> String {
    kind.replace(['.', '-'], "_")
}

fn annotation(fact: FactType) -> &'static str {
    match fact.base {
        FactBase::String => "str",
        FactBase::Int => "int",
        FactBase::Float => "float",
        FactBase::Duration => "int | timedelta",
        FactBase::Time => "int | datetime",
    }
}

/// The value sent for a fact argument.
fn value(name: &str, fact: FactType) -> String {
    match fact.base {
        FactBase::Duration => format!("_beacon.duration_ms({name})"),
        FactBase::Time => format!("_beacon.time_ms({name})"),
        FactBase::String | FactBase::Int | FactBase::Float => name.to_string(),
    }
}

/// The keyword-only parameters of a kind's function (and of its `_entry`).
fn parameters(kind: &Kind) -> Vec<String> {
    let mut params = Vec::new();
    for (name, fact) in kind.facts.iter().filter(|(_, f)| !f.optional) {
        params.push(format!("{name}: {}", annotation(*fact)));
    }
    for name in kind.slots.keys() {
        params.push(format!("{name}: str"));
    }
    if kind.context {
        params.push("context: str".to_string());
    }
    for (name, fact) in kind.facts.iter().filter(|(_, f)| f.optional) {
        params.push(format!("{name}: {} | None = None", annotation(*fact)));
    }
    params.push("key: str = \"\"".to_string());
    params.push("labels: Mapping[str, str] | None = None".to_string());
    if kind.open.is_some() {
        params.push("url: str | None = None".to_string());
    }
    if !kind.categories.is_empty() {
        let names: Vec<String> = kind.categories.iter().map(|c| quote(c)).collect();
        params.push(format!("category: Literal[{}] | None = None", names.join(", ")));
    }
    params
}

/// The keyword arguments passed on to the runtime.
fn arguments(kind: &Kind) -> Vec<String> {
    let mut args = vec!["key=key".to_string()];
    if !kind.facts.is_empty() {
        let facts: Vec<String> = kind
            .facts
            .iter()
            .map(|(name, fact)| format!("{}: {}", quote(name), value(name, *fact)))
            .collect();
        args.push(format!("facts=_beacon.present({{{}}})", facts.join(", ")));
    }
    if !kind.slots.is_empty() {
        let slots: Vec<String> = kind
            .slots
            .keys()
            .map(|name| format!("{}: {name}", quote(name)))
            .collect();
        args.push(format!("slots={{{}}}", slots.join(", ")));
    }
    args.push("labels=labels".to_string());
    if kind.context {
        args.push("context=context".to_string());
    }
    if kind.open.is_some() {
        args.push("url=url".to_string());
    }
    if !kind.categories.is_empty() {
        args.push("category=category".to_string());
    }
    args
}

fn docstring(text: &str) -> String {
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
    format!("    \"\"\"{escaped}\"\"\"\n")
}

fn function_def(out: &mut String, name: &str, params: &[String], returns: &str) {
    let _ = writeln!(out, "def {name}(\n    *,");
    for param in params {
        let _ = writeln!(out, "    {param},");
    }
    let _ = writeln!(out, ") -> {returns}:");
}

fn call(out: &mut String, method: &str, kind_name: &str, args: &[String]) {
    let _ = writeln!(out, "    return _beacon.{method}(\n        {},", quote(kind_name));
    for arg in args {
        let _ = writeln!(out, "        {arg},");
    }
    out.push_str("    )\n");
}

pub fn generate(decl: &Declarations) -> String {
    let mut out = String::new();
    for line in header_lines(decl) {
        let _ = writeln!(out, "# {line}");
    }
    out.push_str("from __future__ import annotations\n\n");
    out.push_str(runtime(PYTHON_CLIENT));
    let _ = writeln!(
        out,
        "\n\n# ---------- the kinds `{}` declares ----------\n",
        decl.source
    );
    let _ = writeln!(out, "SOURCE = {}", quote(&decl.source));

    for (name, kind) in &decl.kinds {
        let function = function(name);
        let description = if kind.description.is_empty() {
            format!("Sends `{name}`.")
        } else {
            kind.description.clone()
        };
        out.push_str("\n\n");
        function_def(&mut out, &function, &parameters(kind), "Sent");
        out.push_str(&docstring(&description));
        call(&mut out, "event", name, &arguments(kind));

        if kind.class.has_lifecycle() {
            let mut params = Vec::new();
            let mut args = vec!["key=key".to_string()];
            if kind.context {
                params.push("context: str".to_string());
                args.push("context=context".to_string());
            }
            params.push("key: str = \"\"".to_string());
            args.push("state=\"resolved\"".to_string());
            out.push_str("\n\n");
            function_def(&mut out, &format!("{function}_resolved"), &params, "Sent");
            out.push_str(&docstring(&format!(
                "Resolves the open `{name}` with this key: it recovered, or was handled."
            )));
            call(&mut out, "event", name, &args);

            out.push_str("\n\n");
            function_def(&mut out, &format!("{function}_entry"), &parameters(kind), "Entry");
            out.push_str(&docstring(&format!("A firing `{name}`, for `snapshot`.")));
            call(&mut out, "entry", name, &arguments(kind));
        }
    }

    let covered = lifecycle_kinds(decl);
    let kinds_type = if covered.is_empty() {
        "str"
    } else {
        let names: Vec<String> = covered.iter().map(|k| quote(k)).collect();
        let _ = write!(out, "\n\nSnapshotKind = Literal[{}]\n", names.join(", "));
        "SnapshotKind"
    };
    let _ = write!(
        out,
        r#"

def snapshot(
    firing: Sequence[Entry],
    *,
    kinds: Sequence[{kinds_type}] | None = None,
    stale_after: str | None = None,
) -> Sent:
    """Everything of `{source}` that is firing now: whatever was open and isn't listed resolves."""
    return _beacon.snapshot(SOURCE, firing, kinds=kinds, stale_after=stale_after)


def unobserved(reason: str, *, stale_after: str | None = None) -> Sent:
    """`{source}` couldn't see what it watches: its alerts are muted until its next snapshot."""
    return _beacon.snapshot(SOURCE, [], stale_after=stale_after, reason=reason)
"#,
        source = decl.source,
    );
    out
}
