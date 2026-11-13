//! The TypeScript emitter: the client's runtime, then one exported function per kind taking one
//! object argument. No dependencies, an ES module, clean under `tsc --strict`.

use crate::{TYPESCRIPT_CLIENT, header_lines, lifecycle_kinds, quote, runtime};
use beacon_core::decl::{Declarations, FactBase, FactType, Kind};
use std::fmt::Write;

/// A kind's function name: `assistant.task_result` → `assistantTaskResult`.
pub fn function(kind: &str) -> String {
    let mut out = String::new();
    for (i, part) in kind.split(['.', '-', '_']).filter(|p| !p.is_empty()).enumerate() {
        if i == 0 {
            out.push_str(part);
        } else {
            let mut chars = part.chars();
            if let Some(first) = chars.next() {
                out.extend(first.to_uppercase());
                out.push_str(chars.as_str());
            }
        }
    }
    out
}

fn annotation(fact: FactType) -> &'static str {
    match fact.base {
        FactBase::String => "string",
        FactBase::Int | FactBase::Float | FactBase::Duration => "number",
        FactBase::Time => "number | Date",
    }
}

fn value(name: &str, fact: FactType) -> String {
    match fact.base {
        FactBase::Time => format!("beacon.timeMs(args.{name})"),
        _ => format!("args.{name}"),
    }
}

/// The fields of a kind's argument object, beside `Common`.
fn fields(kind: &Kind) -> String {
    let mut fields = Vec::new();
    for (name, fact) in &kind.facts {
        let optional = if fact.optional { "?" } else { "" };
        fields.push(format!("{name}{optional}: {}", annotation(*fact)));
    }
    for name in kind.slots.keys() {
        fields.push(format!("{name}: string"));
    }
    if kind.context {
        fields.push("context: string".to_string());
    }
    if kind.open.is_some() {
        fields.push("url?: string".to_string());
    }
    if !kind.categories.is_empty() {
        let names: Vec<String> = kind.categories.iter().map(|c| quote(c)).collect();
        fields.push(format!("category?: {}", names.join(" | ")));
    }
    if fields.is_empty() {
        "Common".to_string()
    } else {
        format!("{{ {} }} & Common", fields.join("; "))
    }
}

/// The options passed on to the runtime.
fn options(kind: &Kind) -> Vec<String> {
    let mut options = vec!["key: args.key".to_string()];
    if !kind.facts.is_empty() {
        let facts: Vec<String> = kind
            .facts
            .iter()
            .map(|(name, fact)| format!("{name}: {}", value(name, *fact)))
            .collect();
        options.push(format!("facts: beacon.present({{ {} }})", facts.join(", ")));
    }
    if !kind.slots.is_empty() {
        let slots: Vec<String> = kind
            .slots
            .keys()
            .map(|name| format!("{name}: args.{name}"))
            .collect();
        options.push(format!("slots: {{ {} }}", slots.join(", ")));
    }
    options.push("labels: args.labels".to_string());
    if kind.context {
        options.push("context: args.context".to_string());
    }
    if kind.open.is_some() {
        options.push("url: args.url".to_string());
    }
    if !kind.categories.is_empty() {
        options.push("category: args.category".to_string());
    }
    options
}

fn doc(text: &str) -> String {
    format!("/** {} */\n", text.replace("*/", "*\\/"))
}

fn body(out: &mut String, method: &str, kind_name: &str, options: &[String]) {
    let _ = writeln!(out, "  return beacon.{method}({}, {{", quote(kind_name));
    for option in options {
        let _ = writeln!(out, "    {option},");
    }
    out.push_str("  });\n}\n");
}

pub fn generate(decl: &Declarations) -> String {
    let mut out = String::new();
    for line in header_lines(decl) {
        let _ = writeln!(out, "// {line}");
    }
    out.push('\n');
    out.push_str(runtime(TYPESCRIPT_CLIENT));
    let _ = writeln!(
        out,
        "\n// ---------- the kinds `{}` declares ----------\n",
        decl.source
    );
    let _ = writeln!(out, "export const SOURCE = {};\n", quote(&decl.source));
    out.push_str("/** What every kind's function also takes: which alert or action this is, and labels silences match. */\n");
    out.push_str("export interface Common {\n  key?: string;\n  labels?: Record<string, string>;\n}\n");

    for (name, kind) in &decl.kinds {
        let function = function(name);
        let description = if kind.description.is_empty() {
            format!("Sends `{name}`.")
        } else {
            kind.description.clone()
        };
        let fields = fields(kind);
        out.push('\n');
        out.push_str(&doc(&description));
        let default = if fields == "Common" { " = {}" } else { "" };
        let _ = writeln!(
            out,
            "export function {function}(args: {fields}{default}): Promise<Sent> {{"
        );
        body(&mut out, "event", name, &options(kind));

        if kind.class.has_lifecycle() {
            out.push('\n');
            out.push_str(&doc(&format!(
                "Resolves the open `{name}` with this key: it recovered, or was handled."
            )));
            if kind.context {
                let _ = writeln!(
                    out,
                    "export function {function}Resolved(args: {{ context: string; key?: string }}): Promise<Sent> {{"
                );
                body(
                    &mut out,
                    "event",
                    name,
                    &[
                        "key: args.key".to_string(),
                        "context: args.context".to_string(),
                        "state: \"resolved\"".to_string(),
                    ],
                );
            } else {
                let _ = writeln!(
                    out,
                    "export function {function}Resolved(args: {{ key?: string }} = {{}}): Promise<Sent> {{"
                );
                body(
                    &mut out,
                    "event",
                    name,
                    &["key: args.key".to_string(), "state: \"resolved\"".to_string()],
                );
            }

            out.push('\n');
            out.push_str(&doc(&format!("A firing `{name}`, for `snapshot`.")));
            let _ = writeln!(
                out,
                "export function {function}Entry(args: {fields}{default}): Entry {{"
            );
            body(&mut out, "entry", name, &options(kind));
        }
    }

    let covered = lifecycle_kinds(decl);
    let kinds_type = if covered.is_empty() {
        "string"
    } else {
        let names: Vec<String> = covered.iter().map(|k| quote(k)).collect();
        let _ = write!(
            out,
            "\n/** The kinds a snapshot of `{}` can cover. */\nexport type SnapshotKind = {};\n",
            decl.source,
            names.join(" | ")
        );
        "SnapshotKind"
    };
    let _ = write!(
        out,
        r#"
/** Everything of `{source}` that is firing now: whatever was open and isn't listed resolves. */
export function snapshot(
  firing: readonly Entry[],
  options: {{ kinds?: readonly {kinds_type}[]; stale_after?: string }} = {{}},
): Promise<Sent> {{
  return beacon.snapshot(SOURCE, firing, options);
}}

/** `{source}` couldn't see what it watches: its alerts are muted until its next snapshot. */
export function unobserved(reason: string, options: {{ stale_after?: string }} = {{}}): Promise<Sent> {{
  return beacon.snapshot(SOURCE, [], {{ ...options, reason }});
}}
"#,
        source = decl.source,
    );
    out
}
