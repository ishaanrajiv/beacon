//! A source's declarations: the kinds of notification it sends, each with its class, push level
//! and copy. One `kinds.toml` per source; `spec/declarations.md` documents the format and every
//! rule. [`parse`] reads the TOML and fills in defaults, [`validate`] runs every check on the typed
//! model, and every problem says where it is and how to fix it.
//!
//! The typed model round-trips through JSON with every default filled in: that is what
//! `beacon publish` sends to `PUT /v1/registry/{source}`, and what the server hands to clients.

use crate::duration::{Duration, MINUTE};
use crate::model::{BuiltinAction, Class, Icon, PushLevel};
use crate::names::{RESERVED_SOURCES, is_ident, is_kind_name, is_source, secret_word};
use crate::problem::{Problem, closest};
use crate::template::Template;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

/// The file every declarations folder holds.
pub const FILE: &str = "kinds.toml";

/// Most characters in a rendered title.
pub const TITLE_MAX: usize = 40;
/// Most characters in a rendered body.
pub const BODY_MAX: usize = 150;
/// Most characters in a hidden-preview text.
pub const PREVIEW_MAX: usize = 40;
/// Most characters in a source's display name.
pub const NAME_MAX: usize = 24;
/// Most characters in a custom action's label.
pub const LABEL_MAX: usize = 20;
/// Most characters a slot may declare.
pub const SLOT_MAX: u32 = 1000;
/// Most characters in a string fact's value.
pub const FACT_STRING_MAX: usize = 200;
/// Most actions a kind may offer.
pub const ACTIONS_MAX: usize = 4;
/// Most entries in `repeat`.
pub const REPEATS_MAX: usize = 8;
/// The shortest repeat interval.
pub const REPEAT_MIN: i64 = 5 * MINUTE;
/// The shortest throttle.
pub const THROTTLE_MIN: i64 = MINUTE;
/// The throttle a kind gets when it declares none.
pub const THROTTLE_DEFAULT: i64 = 10 * MINUTE;
/// Most characters in an `open` link or a custom action's `url`.
pub const LINK_MAX: usize = 512;
/// Most characters in a description.
pub const DESCRIPTION_MAX: usize = 200;
/// The body a recovery gets when the kind declares no `recovered_body`.
pub const RECOVERED_BODY_DEFAULT: &str = "Lasted {lasted}.";
/// The placeholder for how long an alert was open, usable in `recovered` and `recovered_body` only.
pub const LASTED: &str = "lasted";
/// Names a fact or slot can't take: the generated emitters' own arguments (`spec/clients.md`).
pub const RESERVED_VALUE_NAMES: [&str; 5] = ["key", "labels", "url", "context", "category"];
/// Most notification categories a kind declares.
pub const CATEGORIES_MAX: usize = 4;

// ---------- the typed model ----------

/// Everything one source declares.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declarations {
    pub source: String,
    /// What people see as the source's name ("Backups").
    pub name: String,
    pub icon: Icon,
    pub kinds: BTreeMap<String, Kind>,
}

/// One kind of notification. Every field is filled in: defaults are applied when the TOML is read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Kind {
    pub class: Class,
    pub push: PushLevel,
    #[serde(default)]
    pub description: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub facts: BTreeMap<String, FactType>,
    /// Slot name to its most characters.
    #[serde(default)]
    pub slots: BTreeMap<String, u32>,
    #[serde(default)]
    pub open: Option<String>,
    #[serde(default)]
    pub actions: Vec<Action>,
    #[serde(default)]
    pub time_bound: bool,
    #[serde(default)]
    pub repeat: Vec<Duration>,
    /// None: never throttled (`throttle = "none"`).
    #[serde(default = "default_throttle")]
    pub throttle: Option<Duration>,
    #[serde(default)]
    pub preview: Option<String>,
    #[serde(default)]
    pub recovered: Option<String>,
    #[serde(default)]
    pub recovered_body: Option<String>,
    #[serde(default)]
    pub context: bool,
    #[serde(default)]
    pub one_off: bool,
    /// The app's notification categories this kind's pushes may carry; the first is the default.
    /// Empty: the kind's name is the category.
    #[serde(default)]
    pub categories: Vec<String>,
}

fn default_throttle() -> Option<Duration> {
    Duration::from_ms(THROTTLE_DEFAULT)
}

/// A fact's type. Written `"int"`, or `"int?"` for an optional fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct FactType {
    pub base: FactBase,
    pub optional: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactBase {
    /// A short piece of text: a name, an id. At most 200 characters.
    String,
    /// A whole number.
    Int,
    /// Any number.
    Float,
    /// A length of time, sent as whole milliseconds.
    Duration,
    /// A moment, sent as milliseconds since 1970 UTC.
    Time,
}

impl FactBase {
    pub fn name(self) -> &'static str {
        match self {
            FactBase::String => "string",
            FactBase::Int => "int",
            FactBase::Float => "float",
            FactBase::Duration => "duration",
            FactBase::Time => "time",
        }
    }

    /// A measurement or a moment: a value that changes while the thing it describes stays the
    /// same. These don't belong in a title.
    pub fn is_live(self) -> bool {
        matches!(self, FactBase::Float | FactBase::Duration | FactBase::Time)
    }
}

impl FactType {
    pub const TYPE_NAMES: &'static str = "string, int, float, duration or time; add ? for optional";

    pub fn parse(text: &str) -> Result<FactType, String> {
        let (name, optional) = match text.strip_suffix('?') {
            Some(name) => (name, true),
            None => (text, false),
        };
        let base = match name {
            "string" => FactBase::String,
            "int" => FactBase::Int,
            "float" => FactBase::Float,
            "duration" => FactBase::Duration,
            "time" => FactBase::Time,
            _ => return Err(format!("`{text}` is not a fact type")),
        };
        Ok(FactType { base, optional })
    }
}

impl fmt::Display for FactType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.base.name(), if self.optional { "?" } else { "" })
    }
}

impl TryFrom<String> for FactType {
    type Error = String;
    fn try_from(text: String) -> Result<FactType, String> {
        FactType::parse(&text)
    }
}

impl From<FactType> for String {
    fn from(fact: FactType) -> String {
        fact.to_string()
    }
}

/// A button a kind offers: one of Beacon's own, or a custom one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Action {
    Builtin(BuiltinAction),
    Custom(CustomAction),
}

/// A custom button. With `url`, a link the client opens; without, a choice Beacon records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomAction {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub url: Option<String>,
}

impl Action {
    pub fn id(&self) -> &str {
        match self {
            Action::Builtin(action) => action.as_str(),
            Action::Custom(custom) => &custom.id,
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Action::Builtin(action) => action.label(),
            Action::Custom(custom) => &custom.label,
        }
    }
}

impl Kind {
    pub fn has_action(&self, action: BuiltinAction) -> bool {
        self.actions.contains(&Action::Builtin(action))
    }
}

/// What a check allows beyond the format itself.
#[derive(Debug, Clone, Default)]
pub struct Check {
    /// Words no copy may contain, matched whole and ignoring case (`banned_words` in beacon.toml).
    pub banned_words: Vec<String>,
    /// Beacon's own declarations (`crates/core/builtin/`): a reserved source, and titles that are
    /// only a slot, are allowed.
    pub builtin: bool,
}

impl Declarations {
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// Reads declarations sent as JSON and checks them like a file's.
    pub fn from_json(value: Value, check: &Check) -> Result<Declarations, Vec<Problem>> {
        let decl: Declarations = serde_json::from_value(value).map_err(|e| {
            vec![Problem::new(
                "declarations",
                e.to_string(),
                "publish with `beacon publish <folder>`, which builds this from kinds.toml",
            )]
        })?;
        let problems = validate(&decl, check);
        if problems.is_empty() {
            Ok(decl)
        } else {
            Err(problems)
        }
    }
}

// ---------- reading the TOML ----------

const FILE_FIELDS: [&str; 4] = ["source", "name", "icon", "kinds"];
const KIND_FIELDS: [&str; 18] = [
    "class",
    "push",
    "description",
    "title",
    "body",
    "facts",
    "slots",
    "open",
    "actions",
    "time_bound",
    "repeat",
    "throttle",
    "preview",
    "recovered",
    "recovered_body",
    "context",
    "one_off",
    "categories",
];
const CUSTOM_ACTION_FIELDS: [&str; 3] = ["id", "label", "url"];

/// Reads and checks a folder's `kinds.toml`, or the file itself when `path` is a file.
pub fn load(path: &Path, check: &Check) -> Result<Declarations, Vec<Problem>> {
    let file = if path.is_dir() {
        path.join(FILE)
    } else {
        path.to_path_buf()
    };
    match std::fs::read_to_string(&file) {
        Ok(text) => parse(&text, check),
        Err(e) => Err(vec![Problem::new(
            file.display().to_string(),
            format!("can't read it: {e}"),
            "create kinds.toml starting with `source = \"<service name>\"`; see spec/declarations.md",
        )]),
    }
}

/// Parses and checks the text of a `kinds.toml`, filling in defaults.
pub fn parse(text: &str, check: &Check) -> Result<Declarations, Vec<Problem>> {
    let mut problems = Vec::new();
    let table: toml::Table = match text.parse() {
        Ok(table) => table,
        Err(e) => {
            return Err(vec![Problem::new(
                FILE,
                e.to_string().trim_end().to_string(),
                "fix the TOML; spec/declarations.md shows every field",
            )]);
        }
    };
    unknown_fields(FILE, &table, &FILE_FIELDS, &mut problems);

    let source = match string(FILE, &table, "source", &mut problems) {
        Some(source) => source,
        None => {
            problems.push(Problem::new(
                FILE,
                "no source",
                "add `source = \"<service name>\"` at the top, like `source = \"backup\"`",
            ));
            String::new()
        }
    };
    let name = string(FILE, &table, "name", &mut problems).unwrap_or_default();
    let icon = match string(FILE, &table, "icon", &mut problems) {
        None => Icon::Bell,
        Some(text) => Icon::parse(&text).unwrap_or_else(|| {
            let names = Icon::ALL.map(Icon::as_str);
            problems.push(Problem::new(
                format!("{FILE}: icon"),
                format!("`{text}` is not one of Beacon's icons"),
                match closest(&text, names) {
                    Some(near) => format!("did you mean `{near}`? The icons are {}", names.join(", ")),
                    None => format!("use one of {}", names.join(", ")),
                },
            ));
            Icon::Bell
        }),
    };

    let mut kinds = BTreeMap::new();
    match table.get("kinds") {
        None => {}
        Some(toml::Value::Table(raw)) => {
            for (name, value) in raw {
                let at = format!("{FILE}: kinds.\"{name}\"");
                match value {
                    toml::Value::Table(fields) => {
                        if let Some(kind) = read_kind(&at, fields, &mut problems) {
                            kinds.insert(name.clone(), kind);
                        }
                    }
                    _ => problems.push(Problem::new(
                        &at,
                        "a kind must be a table",
                        format!("write it as `[kinds.\"{name}\"]` followed by its fields"),
                    )),
                }
            }
        }
        Some(_) => problems.push(Problem::new(
            format!("{FILE}: kinds"),
            "kinds must be a table of kinds",
            "write each kind as `[kinds.\"<source>.<name>\"]`",
        )),
    }

    let decl = Declarations {
        source,
        name,
        icon,
        kinds,
    };
    problems.extend(validate(&decl, check));
    if problems.is_empty() {
        Ok(decl)
    } else {
        Err(problems)
    }
}

fn read_kind(at: &str, fields: &toml::Table, problems: &mut Vec<Problem>) -> Option<Kind> {
    unknown_fields(at, fields, &KIND_FIELDS, problems);
    let before = problems.len();

    let class = match string(at, fields, "class", problems) {
        None => {
            problems.push(Problem::new(
                format!("{at}.class"),
                "no class",
                "add `class = \"update\"`: alert (broken), action (needs a decision), update (worth knowing) or record (audit only)",
            ));
            None
        }
        Some(text) => Class::parse(&text).or_else(|| {
            problems.push(Problem::new(
                format!("{at}.class"),
                format!("`{text}` is not a class"),
                hint(&text, Class::ALL.map(Class::as_str)),
            ));
            None
        }),
    };
    let push = match string(at, fields, "push", problems) {
        None if class == Some(Class::Record) => Some(PushLevel::None),
        None => {
            problems.push(Problem::new(
                format!("{at}.push"),
                "no push level",
                "add `push = \"silent\"`, or urgent (alerts only), normal or none",
            ));
            None
        }
        Some(text) => PushLevel::parse(&text).or_else(|| {
            problems.push(Problem::new(
                format!("{at}.push"),
                format!("`{text}` is not a push level"),
                hint(&text, PushLevel::ALL.map(PushLevel::as_str)),
            ));
            None
        }),
    };
    let title = string(at, fields, "title", problems).unwrap_or_default();
    let description = string(at, fields, "description", problems).unwrap_or_default();
    let body = string(at, fields, "body", problems).unwrap_or_default();
    let open = string(at, fields, "open", problems);
    let preview = string(at, fields, "preview", problems);
    let recovered = string(at, fields, "recovered", problems);
    let recovered_body = string(at, fields, "recovered_body", problems);
    let time_bound = boolean(at, fields, "time_bound", problems);
    let context = boolean(at, fields, "context", problems);
    let one_off = boolean(at, fields, "one_off", problems);

    let mut facts = BTreeMap::new();
    for (name, value) in table(at, fields, "facts", problems) {
        let parsed = match value {
            toml::Value::String(text) => FactType::parse(text),
            _ => Err("a fact's type must be a string".to_string()),
        };
        match parsed {
            Ok(fact) => {
                facts.insert(name.clone(), fact);
            }
            Err(message) => problems.push(Problem::new(
                format!("{at}.facts.{name}"),
                message,
                format!("write one of: {}", FactType::TYPE_NAMES),
            )),
        }
    }

    let mut slots = BTreeMap::new();
    for (name, value) in table(at, fields, "slots", problems) {
        match value.as_integer().and_then(|n| u32::try_from(n).ok()) {
            Some(max) if (1..=SLOT_MAX).contains(&max) => {
                slots.insert(name.clone(), max);
            }
            _ => problems.push(Problem::new(
                format!("{at}.slots.{name}"),
                format!("`{value}` is not a slot length"),
                format!("write the most characters the slot may hold, a whole number from 1 to {SLOT_MAX}, like `{name} = 80`"),
            )),
        }
    }

    let mut actions = Vec::new();
    for (i, value) in array(at, fields, "actions", problems).iter().enumerate() {
        let here = format!("{at}.actions[{i}]");
        match value {
            toml::Value::String(text) => match BuiltinAction::parse(text) {
                Some(action) => actions.push(Action::Builtin(action)),
                None => problems.push(Problem::new(
                    &here,
                    format!("`{text}` is not one of Beacon's actions"),
                    format!(
                        "{}; or declare a custom one as {{ id = \"review\", label = \"Review\", url = \"/review\" }}",
                        hint(text, BuiltinAction::ALL.map(BuiltinAction::as_str))
                    ),
                )),
            },
            toml::Value::Table(custom) => {
                unknown_fields(&here, custom, &CUSTOM_ACTION_FIELDS, problems);
                let id = string(&here, custom, "id", problems).unwrap_or_default();
                let label = string(&here, custom, "label", problems).unwrap_or_default();
                let url = string(&here, custom, "url", problems);
                actions.push(Action::Custom(CustomAction { id, label, url }));
            }
            _ => problems.push(Problem::new(
                &here,
                "an action is a name or a table",
                "write `\"acknowledge\"`, or { id = \"review\", label = \"Review\", url = \"/review\" }",
            )),
        }
    }

    let mut repeat = Vec::new();
    for (i, value) in array(at, fields, "repeat", problems).iter().enumerate() {
        match value.as_str().map(Duration::parse) {
            Some(Ok(duration)) => repeat.push(duration),
            Some(Err(message)) => problems.push(Problem::new(
                format!("{at}.repeat[{i}]"),
                message,
                format!("write {}", Duration::FORMS),
            )),
            None => problems.push(Problem::new(
                format!("{at}.repeat[{i}]"),
                "a repeat interval must be a string",
                "write durations, like repeat = [\"30m\", \"2h\"]",
            )),
        }
    }

    let mut categories = Vec::new();
    for (i, value) in array(at, fields, "categories", problems).iter().enumerate() {
        match value.as_str() {
            Some(text) => categories.push(text.to_string()),
            None => problems.push(Problem::new(
                format!("{at}.categories[{i}]"),
                "a category must be a string",
                "write names the app registered, like categories = [\"APPROVAL\"]",
            )),
        }
    }

    let throttle = match string(at, fields, "throttle", problems).as_deref() {
        None => default_throttle(),
        Some("none") => None,
        Some(text) => match Duration::parse(text) {
            Ok(duration) => Some(duration),
            Err(message) => {
                problems.push(Problem::new(
                    format!("{at}.throttle"),
                    message,
                    format!("write {}, or \"none\"", Duration::FORMS),
                ));
                default_throttle()
            }
        },
    };

    if problems.len() > before {
        return None;
    }
    let (class, push) = (class?, push?);
    let preview = preview.or_else(|| default_preview(push, &title));
    let alert_with_recovery = class == Class::Alert && !one_off;
    let recovered = recovered.or_else(|| alert_with_recovery.then(|| default_recovered(&title)).flatten());
    let recovered_body =
        recovered_body.or_else(|| alert_with_recovery.then(|| RECOVERED_BODY_DEFAULT.to_string()));
    Some(Kind {
        class,
        push,
        description,
        title,
        body,
        facts,
        slots,
        open,
        actions,
        time_bound,
        repeat,
        throttle,
        preview,
        recovered,
        recovered_body,
        context,
        one_off,
        categories,
    })
}

/// A kind that pushes and has a fixed title previews as its title.
fn default_preview(push: PushLevel, title: &str) -> Option<String> {
    if push == PushLevel::None || title.contains('{') || title.is_empty() {
        return None;
    }
    Some(title.to_string())
}

/// "Dashboard is down" recovers as "Dashboard is back"; "Backups are overdue" as "Backups are back".
pub fn default_recovered(title: &str) -> Option<String> {
    [" is ", " are "].into_iter().find_map(|verb| {
        let (subject, _) = title.split_once(verb)?;
        (!subject.trim().is_empty()).then(|| format!("{subject}{verb}back"))
    })
}

fn hint<'a>(word: &str, names: impl IntoIterator<Item = &'a str> + Clone) -> String {
    let all: Vec<&str> = names.clone().into_iter().collect();
    match closest(word, names) {
        Some(near) => format!("did you mean `{near}`? Write one of {}", all.join(", ")),
        None => format!("write one of {}", all.join(", ")),
    }
}

fn unknown_fields(at: &str, table: &toml::Table, known: &[&str], problems: &mut Vec<Problem>) {
    for key in table.keys() {
        if !known.contains(&key.as_str()) {
            problems.push(Problem::new(
                format!("{at}.{key}"),
                format!("`{key}` is not a field here"),
                match closest(key, known.iter().copied()) {
                    Some(near) => format!("did you mean `{near}`?"),
                    None => format!("the fields are {}", known.join(", ")),
                },
            ));
        }
    }
}

fn string(at: &str, table: &toml::Table, key: &str, problems: &mut Vec<Problem>) -> Option<String> {
    match table.get(key)? {
        toml::Value::String(text) => Some(text.clone()),
        other => {
            problems.push(Problem::new(
                format!("{at}.{key}"),
                format!("{key} must be a string, not {}", other.type_str()),
                format!("write it in quotes: {key} = \"...\""),
            ));
            None
        }
    }
}

fn boolean(at: &str, table: &toml::Table, key: &str, problems: &mut Vec<Problem>) -> bool {
    match table.get(key) {
        None => false,
        Some(toml::Value::Boolean(flag)) => *flag,
        Some(other) => {
            problems.push(Problem::new(
                format!("{at}.{key}"),
                format!("{key} must be true or false, not {}", other.type_str()),
                format!("write {key} = true"),
            ));
            false
        }
    }
}

fn table<'a>(
    at: &str,
    fields: &'a toml::Table,
    key: &str,
    problems: &mut Vec<Problem>,
) -> Vec<(&'a String, &'a toml::Value)> {
    match fields.get(key) {
        None => Vec::new(),
        Some(toml::Value::Table(table)) => table.iter().collect(),
        Some(other) => {
            problems.push(Problem::new(
                format!("{at}.{key}"),
                format!("{key} must be a table, not {}", other.type_str()),
                format!("write {key} = {{ name = ... }}"),
            ));
            Vec::new()
        }
    }
}

fn array<'a>(at: &str, fields: &'a toml::Table, key: &str, problems: &mut Vec<Problem>) -> &'a [toml::Value] {
    match fields.get(key) {
        None => &[],
        Some(toml::Value::Array(items)) => items,
        Some(other) => {
            problems.push(Problem::new(
                format!("{at}.{key}"),
                format!("{key} must be a list, not {}", other.type_str()),
                format!("write {key} = [ ... ]"),
            ));
            &[]
        }
    }
}

// ---------- the checks ----------

/// Every check on a source's declarations. Empty when they are fine.
pub fn validate(decl: &Declarations, check: &Check) -> Vec<Problem> {
    let mut problems = Vec::new();
    if !decl.source.is_empty() && !is_source(&decl.source) {
        problems.push(Problem::new(
            format!("{FILE}: source"),
            format!("`{}` is not a source name", decl.source),
            "use lowercase letters, digits and hyphens, starting with a letter, at most 32 characters, like `backup`",
        ));
    }
    if !check.builtin && RESERVED_SOURCES.contains(&decl.source.as_str()) {
        problems.push(Problem::new(
            format!("{FILE}: source"),
            format!("`{}` is Beacon's own source", decl.source),
            "name the source after your service",
        ));
    }
    let name_len = decl.name.trim().chars().count();
    if name_len == 0 {
        problems.push(Problem::new(
            format!("{FILE}: name"),
            "no display name",
            "add `name = \"Backups\"`: what people see as this source's name",
        ));
    } else if name_len > NAME_MAX {
        problems.push(Problem::new(
            format!("{FILE}: name"),
            format!("the name is {name_len} characters; at most {NAME_MAX} fit beside an item"),
            "shorten it",
        ));
    }
    if decl.kinds.is_empty() {
        problems.push(Problem::new(
            FILE,
            "no kinds are declared",
            format!(
                "add one: [kinds.\"{}.failed\"] with class, push and title",
                if decl.source.is_empty() {
                    "backup"
                } else {
                    &decl.source
                }
            ),
        ));
    }
    for (name, kind) in &decl.kinds {
        check_kind(decl, name, kind, check, &mut problems);
    }
    problems
}

/// Which placeholders a template may use.
#[derive(Clone, Copy, PartialEq)]
enum Allowed {
    /// title and body: facts and slots.
    Text,
    /// open and custom action urls: facts only.
    Link,
    /// recovered and recovered_body: facts, slots and `lasted`.
    Recovery,
}

fn check_kind(decl: &Declarations, name: &str, kind: &Kind, check: &Check, problems: &mut Vec<Problem>) {
    let at = format!("{FILE}: kinds.\"{name}\"");
    if !decl.source.is_empty() && !is_kind_name(&decl.source, name) {
        problems.push(Problem::new(
            &at,
            format!("`{name}` is not a kind name of source `{}`", decl.source),
            format!(
                "name it `{}.<what>` in snake_case, like `{}.failed`",
                decl.source, decl.source
            ),
        ));
    }
    let (class, push) = (kind.class, kind.push);

    // Class and push.
    if push == PushLevel::Urgent && class != Class::Alert {
        problems.push(Problem::new(
            format!("{at}.push"),
            format!("only alerts may push urgent, and this is an {}", class.as_str()),
            "use normal; urgent is for something broken you'd want to know about at 3am",
        ));
    }
    if class == Class::Record && push != PushLevel::None {
        problems.push(Problem::new(
            format!("{at}.push"),
            "a record never pushes",
            "remove push, or make it an update if it is worth knowing",
        ));
    }
    if class == Class::Alert && push == PushLevel::None {
        problems.push(Problem::new(
            format!("{at}.push"),
            "an alert must push",
            "use urgent, normal or silent; or make it an update if nobody needs to know at once",
        ));
    }
    if kind.description.chars().count() > DESCRIPTION_MAX {
        problems.push(Problem::new(
            format!("{at}.description"),
            format!("the description is over {DESCRIPTION_MAX} characters"),
            "keep it to one sentence",
        ));
    }

    // Facts and slots.
    for fact in kind.facts.keys() {
        check_value_name(&format!("{at}.facts.{fact}"), fact, problems);
    }
    for (slot, max) in &kind.slots {
        check_value_name(&format!("{at}.slots.{slot}"), slot, problems);
        if kind.facts.contains_key(slot) {
            problems.push(Problem::new(
                format!("{at}.slots.{slot}"),
                format!("`{slot}` is both a fact and a slot"),
                "give the slot another name",
            ));
        }
        if !(1..=SLOT_MAX).contains(max) {
            problems.push(Problem::new(
                format!("{at}.slots.{slot}"),
                format!("`{max}` is not a slot length"),
                format!("write a whole number from 1 to {SLOT_MAX}"),
            ));
        }
    }

    // Templates.
    let mut used = BTreeSet::new();
    let title = template(
        &at,
        "title",
        &kind.title,
        Allowed::Text,
        kind,
        &mut used,
        problems,
    );
    let body = template(&at, "body", &kind.body, Allowed::Text, kind, &mut used, problems);
    if kind.title.trim().is_empty() {
        problems.push(Problem::new(
            format!("{at}.title"),
            "no title",
            "add a title: the thing and its state, like \"Backup failed\" or \"Dashboard is down\"",
        ));
    } else if let Some(title) = &title {
        check_title(&at, "title", title, kind, check, problems);
    }
    if let Some(body) = &body {
        let longest = longest(body, kind);
        if longest > BODY_MAX {
            problems.push(Problem::new(
                format!("{at}.body"),
                format!(
                    "the body can reach {longest} characters with its slots full; at most {BODY_MAX} show"
                ),
                "shorten the text or lower a slot's length",
            ));
        }
        copy_rules(&format!("{at}.body"), &body.literal(), check, problems);
    }

    if let Some(open) = &kind.open {
        template(&at, "open", open, Allowed::Link, kind, &mut used, problems);
        check_link(&format!("{at}.open"), open, problems);
    }

    // Preview.
    match &kind.preview {
        None if push != PushLevel::None => problems.push(Problem::new(
            format!("{at}.preview"),
            "a kind that pushes and has placeholders in its title needs a preview",
            "add `preview = \"Approval needed\"`: what a locked phone shows instead of the text",
        )),
        None => {}
        Some(preview) => {
            let len = preview.chars().count();
            if preview.trim().is_empty() {
                problems.push(Problem::new(
                    format!("{at}.preview"),
                    "the preview is empty",
                    "write what a locked phone shows, like \"Approval needed\"",
                ));
            } else if len > PREVIEW_MAX {
                problems.push(Problem::new(
                    format!("{at}.preview"),
                    format!("the preview is {len} characters; at most {PREVIEW_MAX} show"),
                    "shorten it",
                ));
            }
            if preview.contains('{') || preview.contains('}') {
                problems.push(Problem::new(
                    format!("{at}.preview"),
                    "a preview can't have placeholders: it shows when the text is hidden",
                    "write fixed text, like \"Approval needed\"",
                ));
            }
            copy_rules(&format!("{at}.preview"), preview, check, problems);
        }
    }

    // Recovery.
    let recovers = class == Class::Alert && !kind.one_off;
    for (field, value) in [
        ("recovered", &kind.recovered),
        ("recovered_body", &kind.recovered_body),
    ] {
        match value {
            Some(_) if !recovers => problems.push(Problem::new(
                format!("{at}.{field}"),
                if class == Class::Alert {
                    "a one-off alert has no recovery".to_string()
                } else {
                    format!("only alerts recover, and this is an {}", class.as_str())
                },
                format!("remove {field}"),
            )),
            Some(text) => {
                if let Some(parsed) = template(&at, field, text, Allowed::Recovery, kind, &mut used, problems) {
                    if field == "recovered" {
                        check_title(&at, field, &parsed, kind, check, problems);
                    } else {
                        let longest = longest(&parsed, kind);
                        if longest > BODY_MAX {
                            problems.push(Problem::new(
                                format!("{at}.{field}"),
                                format!("it can reach {longest} characters with its slots full; at most {BODY_MAX} show"),
                                "shorten the text or lower a slot's length",
                            ));
                        }
                        copy_rules(&format!("{at}.{field}"), &parsed.literal(), check, problems);
                    }
                }
            }
            None if recovers && field == "recovered" => problems.push(Problem::new(
                format!("{at}.recovered"),
                "no recovery title, and the title isn't \"<thing> is <state>\" to make one from",
                "add `recovered = \"Backups are working again\"`: the title once it is fixed; or `one_off = true` if nothing reports recovery",
            )),
            None => {}
        }
    }

    // Actions.
    check_actions(&at, kind, &mut used, problems);

    // Lifecycle fields.
    if kind.time_bound && !matches!(class, Class::Action | Class::Update) {
        problems.push(Problem::new(
            format!("{at}.time_bound"),
            format!(
                "only actions and updates are time-bound, and this is an {}",
                class.as_str()
            ),
            if class == Class::Alert {
                "remove it; an alert passes quiet hours only with push = \"urgent\""
            } else {
                "remove it"
            },
        ));
    }
    if kind.context && class == Class::Record {
        problems.push(Problem::new(
            format!("{at}.context"),
            "a record is never shown in a context",
            "remove context",
        ));
    }
    if kind.one_off && class != Class::Alert {
        problems.push(Problem::new(
            format!("{at}.one_off"),
            format!("only alerts are one-off, and this is an {}", class.as_str()),
            "remove one_off",
        ));
    }
    if kind.one_off
        && class == Class::Alert
        && !kind.has_action(BuiltinAction::Acknowledge)
        && !kind.has_action(BuiltinAction::Dismiss)
    {
        problems.push(Problem::new(
            format!("{at}.actions"),
            "a one-off alert closes only when you acknowledge or dismiss it, and it offers neither",
            "add \"acknowledge\" or \"dismiss\" to actions",
        ));
    }
    check_repeat(&at, kind, problems);
    check_categories(&at, kind, problems);
    if let Some(throttle) = kind.throttle
        && throttle.ms() < THROTTLE_MIN
    {
        problems.push(Problem::new(
            format!("{at}.throttle"),
            format!("a throttle of {throttle} is shorter than a minute"),
            "use 1m or more, or \"none\"",
        ));
    }

    // Everything declared is used somewhere.
    for (field, names) in [
        ("facts", kind.facts.keys().collect::<Vec<_>>()),
        ("slots", kind.slots.keys().collect::<Vec<_>>()),
    ] {
        for value in names {
            if !used.contains(value.as_str()) {
                problems.push(Problem::new(
                    format!("{at}.{field}.{value}"),
                    format!("`{value}` is declared but no template uses it"),
                    format!("put {{{value}}} in the title, body, link or recovery text, or remove it"),
                ));
            }
        }
    }
}

fn check_value_name(at: &str, name: &str, problems: &mut Vec<Problem>) {
    if !is_ident(name) {
        problems.push(Problem::new(
            at,
            format!("`{name}` is not a name"),
            "use snake_case, starting with a letter, like `last_ok`",
        ));
    }
    if name == LASTED {
        problems.push(Problem::new(
            at,
            "`lasted` is Beacon's: how long an alert was open",
            "use another name",
        ));
    }
    if RESERVED_VALUE_NAMES.contains(&name) {
        problems.push(Problem::new(
            at,
            format!("`{name}` is an argument of every generated emitter"),
            format!("use a longer name, like `{name}_name` or `the_{name}`"),
        ));
    }
    if let Some(word) = secret_word(name) {
        problems.push(Problem::new(
            at,
            format!("`{name}` looks like it holds a {word}; notifications are kept for months and shown on a lock screen"),
            "send something that names it instead, never the secret itself",
        ));
    }
}

/// Parses a template and checks its placeholders, adding them to `used`.
fn template(
    at: &str,
    field: &str,
    text: &str,
    allowed: Allowed,
    kind: &Kind,
    used: &mut BTreeSet<String>,
    problems: &mut Vec<Problem>,
) -> Option<Template> {
    let here = format!("{at}.{field}");
    let parsed = match Template::parse(text) {
        Ok(parsed) => parsed,
        Err(message) => {
            problems.push(Problem::new(
                here,
                message,
                "write placeholders as {name}, and literal braces as {{ and }}",
            ));
            return None;
        }
    };
    for name in parsed.placeholders() {
        let is_fact = kind.facts.contains_key(name);
        let is_slot = kind.slots.contains_key(name);
        if name == LASTED && allowed == Allowed::Recovery {
            continue;
        }
        if name == LASTED {
            problems.push(Problem::new(
                &here,
                "{lasted} is known only once an alert recovers",
                "use it in recovered or recovered_body",
            ));
        } else if is_slot && allowed == Allowed::Link {
            problems.push(Problem::new(
                &here,
                format!("`{{{name}}}` is a slot; a link may use only facts"),
                format!("declare `{name}` as a fact, or leave it out of the link"),
            ));
        } else if !is_fact && !is_slot {
            let known = kind.facts.keys().chain(kind.slots.keys()).map(String::as_str);
            problems.push(Problem::new(
                &here,
                format!("`{{{name}}}` is not a declared fact or slot"),
                match closest(name, known) {
                    Some(near) => format!("did you mean `{{{near}}}`?"),
                    None => format!(
                        "declare it: facts = {{ {name} = \"string\" }}, or slots = {{ {name} = 80 }} for written text"
                    ),
                },
            ));
        }
        used.insert(name.to_string());
    }
    Some(parsed)
}

/// The most characters a template can render to when every slot is full. Facts count zero: their
/// length isn't known, which is why rendering also cuts the result to fit.
fn longest(template: &Template, kind: &Kind) -> usize {
    let slots: usize = template
        .placeholders()
        .filter_map(|name| kind.slots.get(name))
        .map(|max| *max as usize)
        .sum();
    template.literal().chars().count() + slots
}

fn check_title(
    at: &str,
    field: &str,
    title: &Template,
    kind: &Kind,
    check: &Check,
    problems: &mut Vec<Problem>,
) {
    let here = format!("{at}.{field}");
    let literal = title.literal();
    let longest = longest(title, kind);
    if longest > TITLE_MAX {
        problems.push(Problem::new(
            &here,
            format!("the title can reach {longest} characters with its slots full; at most {TITLE_MAX} show"),
            "shorten the text or lower a slot's length: a title is the thing and its state",
        ));
    }
    let trimmed = literal.trim_end();
    if title
        .segments
        .last()
        .is_some_and(|s| matches!(s, crate::template::Segment::Text(_)))
        && trimmed.ends_with(['.', '!', '?', ';', ','])
    {
        problems.push(Problem::new(
            &here,
            "a title doesn't end with punctuation",
            "remove the last character",
        ));
    }
    if literal.contains(':') {
        problems.push(Problem::new(
            &here,
            "a title has no colon: no \"Cleared:\" or \"Backup:\" prefix",
            "write the thing and its state, like \"Backup failed\"",
        ));
    }
    if let Some(crate::template::Segment::Text(first)) = title.segments.first()
        && first.chars().next().is_some_and(char::is_lowercase)
    {
        problems.push(Problem::new(
            &here,
            "a title starts with a capital letter",
            "capitalise the first word",
        ));
    }
    if !check.builtin && !literal.chars().any(char::is_alphabetic) {
        problems.push(Problem::new(
            &here,
            "a title needs words of its own, not only placeholders",
            "write the thing and its state around the values, like \"{app} is down\"",
        ));
    }
    for name in title.placeholders() {
        let live = if name == LASTED {
            Some("duration")
        } else {
            kind.facts
                .get(name)
                .filter(|f| f.base.is_live())
                .map(|f| f.base.name())
        };
        if let Some(kind_name) = live {
            problems.push(Problem::new(
                &here,
                format!("`{{{name}}}` is a {kind_name}: a live value doesn't belong in a title"),
                "move it to the body",
            ));
        }
    }
    copy_rules(&here, &literal, check, problems);
}

/// The copy rules that can be checked on fixed text: no commands, ids, timestamps or banned words.
fn copy_rules(at: &str, text: &str, check: &Check, problems: &mut Vec<Problem>) {
    let fix =
        "write it for a person: what happened and what to do; leave ids, commands and timestamps to the link";
    let mut found = Vec::new();
    if text.contains('`') {
        found.push("a command in backticks".to_string());
    }
    let lower = text.to_lowercase();
    if ["sudo ", "systemctl ", "$ ", "&& ", "| grep"]
        .iter()
        .any(|c| lower.contains(c))
    {
        found.push("a shell command".to_string());
    }
    if has_iso_date(text) {
        found.push("an ISO timestamp".to_string());
    }
    for word in text.split(|c: char| !c.is_alphanumeric()) {
        if word.len() >= 7
            && word.chars().all(|c| c.is_ascii_hexdigit())
            && word.chars().any(|c| c.is_ascii_digit())
            && word.chars().any(|c| c.is_ascii_alphabetic())
        {
            found.push(format!("`{word}`, which looks like a commit id"));
        }
        if let Some(banned) = check.banned_words.iter().find(|b| b.eq_ignore_ascii_case(word)) {
            found.push(format!("the word `{banned}`, which beacon.toml bans"));
        }
    }
    for what in found {
        problems.push(Problem::new(at, format!("this has {what}"), fix));
    }
}

fn has_iso_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.windows(10).any(|w| {
        w[..4].iter().all(u8::is_ascii_digit)
            && w[4] == b'-'
            && w[5..7].iter().all(u8::is_ascii_digit)
            && w[7] == b'-'
            && w[8..].iter().all(u8::is_ascii_digit)
    })
}

fn check_link(at: &str, link: &str, problems: &mut Vec<Problem>) {
    if !(link.starts_with('/') || link.starts_with("https://")) {
        problems.push(Problem::new(
            at,
            format!("`{link}` is not a link Beacon can open"),
            "start it with / for a page of your app (\"/system\"), or with https://",
        ));
    }
    if link.chars().count() > LINK_MAX {
        problems.push(Problem::new(
            at,
            format!("the link is over {LINK_MAX} characters"),
            "shorten it",
        ));
    }
}

fn check_actions(at: &str, kind: &Kind, used: &mut BTreeSet<String>, problems: &mut Vec<Problem>) {
    let class = kind.class;
    if class == Class::Record && !kind.actions.is_empty() {
        problems.push(Problem::new(
            format!("{at}.actions"),
            "a record has no buttons",
            "remove actions, or make it an update",
        ));
        return;
    }
    if kind.actions.len() > ACTIONS_MAX {
        problems.push(Problem::new(
            format!("{at}.actions"),
            format!(
                "{} actions; at most {ACTIONS_MAX} fit on a notification",
                kind.actions.len()
            ),
            "keep the ones people use",
        ));
    }
    let mut seen = BTreeSet::new();
    for (i, action) in kind.actions.iter().enumerate() {
        let here = format!("{at}.actions[{i}]");
        if !seen.insert(action.id().to_string()) {
            problems.push(Problem::new(
                &here,
                format!("`{}` is listed twice", action.id()),
                "list each action once",
            ));
        }
        match action {
            Action::Builtin(builtin) => {
                if !builtin.allowed_on().contains(&class) {
                    let on: Vec<&str> = builtin.allowed_on().iter().map(|c| c.as_str()).collect();
                    problems.push(Problem::new(
                        &here,
                        format!("`{}` doesn't apply to an {}", builtin.as_str(), class.as_str()),
                        format!("it is for {} kinds; remove it", on.join(" and ")),
                    ));
                }
            }
            Action::Custom(custom) => {
                if !is_ident(&custom.id) {
                    problems.push(Problem::new(
                        format!("{here}.id"),
                        format!("`{}` is not an action id", custom.id),
                        "use snake_case, like `review`",
                    ));
                } else if BuiltinAction::parse(&custom.id).is_some() || custom.id == "silence" {
                    problems.push(Problem::new(
                        format!("{here}.id"),
                        format!("`{}` is one of Beacon's own actions", custom.id),
                        format!(
                            "write \"{}\" on its own, or give the custom action another id",
                            custom.id
                        ),
                    ));
                }
                let len = custom.label.trim().chars().count();
                if len == 0 || len > LABEL_MAX {
                    problems.push(Problem::new(
                        format!("{here}.label"),
                        format!("a button label is 1 to {LABEL_MAX} characters, and this is {len}"),
                        "write a short verb, like \"Review\" or \"Sign in\"",
                    ));
                }
                if let Some(url) = &custom.url {
                    template(&here, "url", url, Allowed::Link, kind, used, problems);
                    check_link(&format!("{here}.url"), url, problems);
                }
            }
        }
    }
}

/// Whether `text` can be a notification category: `^[A-Z][A-Z0-9_]{0,39}$`.
pub fn is_category(text: &str) -> bool {
    let mut bytes = text.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_uppercase())
        && text.len() <= 40
        && bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// The category a push of `kind` (named `name`) carries: `chosen` when the event picked one,
/// else the first declared, else the kind's name.
pub fn push_category<'a>(name: &'a str, kind: &'a Kind, chosen: Option<&'a str>) -> &'a str {
    chosen
        .or_else(|| kind.categories.first().map(String::as_str))
        .unwrap_or(name)
}

fn check_categories(at: &str, kind: &Kind, problems: &mut Vec<Problem>) {
    if kind.categories.is_empty() {
        return;
    }
    let here = format!("{at}.categories");
    if kind.class == Class::Record {
        problems.push(Problem::new(
            &here,
            "a record never pushes, so it has no notification category",
            "remove categories",
        ));
        return;
    }
    if kind.categories.len() > CATEGORIES_MAX {
        problems.push(Problem::new(
            &here,
            format!("{} categories; at most {CATEGORIES_MAX}", kind.categories.len()),
            "keep the ones the app registered for this kind",
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for (i, name) in kind.categories.iter().enumerate() {
        if !is_category(name) {
            problems.push(Problem::new(
                format!("{here}[{i}]"),
                format!("`{name}` is not a category name"),
                "use 1 to 40 of A-Z, 0-9 and _, starting with a letter, like `APPROVAL`",
            ));
        } else if !seen.insert(name.as_str()) {
            problems.push(Problem::new(
                format!("{here}[{i}]"),
                format!("`{name}` is listed twice"),
                "list each category once",
            ));
        }
    }
}

fn check_repeat(at: &str, kind: &Kind, problems: &mut Vec<Problem>) {
    if kind.repeat.is_empty() {
        return;
    }
    let here = format!("{at}.repeat");
    if kind.class != Class::Alert {
        problems.push(Problem::new(
            &here,
            format!("only alerts repeat, and this is an {}", kind.class.as_str()),
            "remove repeat",
        ));
        return;
    }
    if kind.one_off {
        problems.push(Problem::new(
            &here,
            "a one-off alert doesn't repeat",
            "remove repeat",
        ));
    }
    if !matches!(kind.push, PushLevel::Urgent | PushLevel::Normal) {
        problems.push(Problem::new(
            &here,
            "a silent alert has nothing to repeat",
            "remove repeat, or push urgent or normal",
        ));
    }
    if !kind.has_action(BuiltinAction::Acknowledge) {
        problems.push(Problem::new(
            &here,
            "repeats stop when you acknowledge, and this kind offers no acknowledge",
            "add \"acknowledge\" to actions",
        ));
    }
    if kind.repeat.len() > REPEATS_MAX {
        problems.push(Problem::new(
            &here,
            format!("{} repeats; at most {REPEATS_MAX}", kind.repeat.len()),
            "keep the first few: an alert nobody acknowledges after that won't be fixed by more",
        ));
    }
    for (i, interval) in kind.repeat.iter().enumerate() {
        if interval.ms() < REPEAT_MIN {
            problems.push(Problem::new(
                format!("{here}[{i}]"),
                format!("{interval} is too soon to repeat"),
                "repeat after 5m or more",
            ));
        }
        if i > 0 && *interval < kind.repeat[i - 1] {
            problems.push(Problem::new(
                format!("{here}[{i}]"),
                format!("{interval} is shorter than the interval before it"),
                "make each interval at least as long as the one before, like [\"30m\", \"2h\", \"6h\"]",
            ));
        }
    }
}

#[cfg(test)]
#[path = "decl_tests.rs"]
mod tests;
