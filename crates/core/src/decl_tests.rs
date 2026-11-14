use super::*;
use crate::duration::HOUR;
use crate::model::{BuiltinAction, Class, Icon, PushLevel};

fn problems_with(text: &str, check: &Check) -> String {
    match parse(text, check) {
        Ok(_) => String::new(),
        Err(problems) => problems
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn problems(text: &str) -> String {
    problems_with(text, &Check::default())
}

/// A file with one kind, `backup.thing`, made of `fields`.
fn one_kind(fields: &str) -> String {
    format!("source = \"backup\"\nname = \"Backups\"\n\n[kinds.\"backup.thing\"]\n{fields}\n")
}

const UPDATE: &str = "class = \"update\"\npush = \"silent\"\ntitle = \"Backup finished\"";
const ALERT: &str = "class = \"alert\"\npush = \"urgent\"\ntitle = \"Backup is failing\"";
const ACTION: &str = "class = \"action\"\npush = \"normal\"\ntitle = \"Approval needed\"";
const RECORD: &str = "class = \"record\"\ntitle = \"Backup finished\"";

fn example(name: &str) -> Declarations {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/homelab-ish")
        .join(name)
        .join("beacon");
    load(&dir, &Check::default()).unwrap_or_else(|p| panic!("{name}: {p:?}"))
}

#[test]
fn the_examples_pass() {
    let backup = example("backup");
    assert_eq!(backup.source, "backup");
    assert_eq!(backup.name, "Backups");
    assert_eq!(backup.icon, Icon::Archive);
    assert_eq!(backup.kinds.len(), 4);
    assert_eq!(example("assistant").kinds.len(), 7);
    assert_eq!(example("ship").kinds.len(), 4);
}

#[test]
fn the_base_kinds_pass() {
    for base in [UPDATE, ALERT, ACTION, RECORD] {
        assert_eq!(problems(&one_kind(base)), "", "{base}");
    }
}

#[test]
fn defaults_are_filled_in() {
    let backup = example("backup");
    let stale = &backup.kinds["backup.stale"];
    assert_eq!(stale.recovered.as_deref(), Some("Backups are back"));
    assert_eq!(stale.recovered_body.as_deref(), Some(RECOVERED_BODY_DEFAULT));
    assert_eq!(stale.preview.as_deref(), Some("Backups are overdue"));
    assert_eq!(stale.throttle.map(Duration::ms), Some(10 * MINUTE));
    let finished = &backup.kinds["backup.finished"];
    assert_eq!(finished.push, PushLevel::None);
    assert_eq!(finished.preview, None);
    assert_eq!(finished.recovered, None);
    let failed = &backup.kinds["backup.failed"];
    assert_eq!(
        failed.repeat.iter().map(|d| d.ms()).collect::<Vec<_>>(),
        [2 * HOUR, 6 * HOUR]
    );
    assert!(failed.has_action(BuiltinAction::Acknowledge));

    let assistant = example("assistant");
    assert_eq!(assistant.kinds["assistant.reminder"].throttle, None);
    assert!(assistant.kinds["assistant.reminder"].time_bound);
    assert!(assistant.kinds["assistant.approval"].context);
    assert_eq!(assistant.kinds["assistant.approval"].class, Class::Action);
    assert_eq!(
        assistant.kinds["assistant.unavailable"].recovered.as_deref(),
        Some("The assistant is back")
    );

    let ship = example("ship");
    assert!(ship.kinds["ship.failed"].one_off);
    assert_eq!(ship.kinds["ship.failed"].recovered, None);
    assert_eq!(ship.kinds["ship.failed"].recovered_body, None);

    let plain = parse(&one_kind(UPDATE), &Check::default()).unwrap();
    assert_eq!(plain.icon, Icon::Bell);
}

#[test]
fn derives_a_recovery_title() {
    assert_eq!(
        default_recovered("Dashboard is down").as_deref(),
        Some("Dashboard is back")
    );
    assert_eq!(
        default_recovered("{app} is down").as_deref(),
        Some("{app} is back")
    );
    assert_eq!(
        default_recovered("Backups are overdue").as_deref(),
        Some("Backups are back")
    );
    assert_eq!(default_recovered("Backup failed"), None);
    assert_eq!(default_recovered(" is down"), None);
}

#[test]
fn round_trips_through_json() {
    for name in ["backup", "assistant", "ship"] {
        let decl = example(name);
        let json = decl.to_json();
        let back = Declarations::from_json(json.clone(), &Check::default()).unwrap();
        assert_eq!(back, decl);
        assert_eq!(back.to_json(), json);
    }
    let json = example("ship").to_json();
    assert_eq!(json["kinds"]["ship.pr_ready"]["actions"][0]["id"], "review");
    assert_eq!(json["kinds"]["ship.pr_ready"]["throttle"], "10m");
    assert_eq!(json["kinds"]["ship.pr_ready"]["facts"]["number"], "int");
    assert_eq!(
        example("assistant").to_json()["kinds"]["assistant.reminder"]["throttle"],
        Value::Null
    );
    assert_eq!(
        example("backup").to_json()["kinds"]["backup.failed"]["actions"][1],
        "silence_1h"
    );
}

#[test]
fn service_defaults_to_the_source_and_is_checked() {
    let backup = example("backup");
    assert_eq!((backup.service.as_str(), backup.service()), ("backup", "backup"));
    assert_eq!(backup.to_json()["service"], "backup");
    assert!(
        backup.to_publish_json().get("service").is_none(),
        "an older server takes it"
    );
    let named = parse(
        &format!("service = \"mail-worker\"\n{}", one_kind(UPDATE)),
        &Check::default(),
    )
    .unwrap();
    assert_eq!(named.service, "mail-worker");
    assert_eq!(named.to_publish_json()["service"], "mail-worker");
    let back = Declarations::from_json(named.to_publish_json(), &Check::default()).unwrap();
    assert_eq!(back, named);
    // Missing from JSON: the source.
    let back = Declarations::from_json(backup.to_publish_json(), &Check::default()).unwrap();
    assert_eq!(back.service, "backup");
    for bad in [
        "Mail",
        "mail worker",
        "mail--worker",
        "-mail",
        "mail_",
        &"m".repeat(65),
    ] {
        let text = format!("service = \"{bad}\"\n{}", one_kind(UPDATE));
        assert!(problems(&text).contains("is not a service name"), "{bad}");
    }
    for good in ["mail_worker", "mail-worker", "m2"] {
        assert!(is_service(good), "{good}");
    }
}

#[test]
fn trail_may_be_extended_with_kinds_of_its_own() {
    let trail = example("trail");
    assert_eq!(trail.source, "trail");
    assert_eq!(trail.kinds["trail.disk_full"].push, PushLevel::Urgent);
    assert_eq!(trail.kinds["trail.queue_backlog"].push, PushLevel::Normal);
    // Checked like any other kind: copy rules included.
    let text = "source = \"trail\"\nname = \"Monitoring\"\n[kinds.\"trail.hot\"]\nclass = \"alert\"\npush = \"urgent\"\ntitle = \"{title}\"\nslots = { title = 40 }\none_off = true\nactions = [\"acknowledge\"]\npreview = \"Hot\"\n";
    assert!(problems(text).contains("words of its own"), "{}", problems(text));
}

#[test]
fn stored_declarations_are_stamped_and_read_back() {
    for name in ["backup", "assistant", "ship"] {
        let decl = example(name);
        let stored = decl.to_stored();
        assert_eq!(stored["format"], STORED_FORMAT);
        let back = Declarations::from_stored(&stored).unwrap();
        assert_eq!(back.decl, decl);
        assert_eq!(back.format, Some(STORED_FORMAT));
        assert!(back.unreadable.is_empty());
    }
    let kind = &example("ship").kinds["ship.pr_ready"];
    let stored = kind.to_stored();
    assert_eq!(stored["format"], STORED_FORMAT);
    assert_eq!(&Kind::from_stored(&stored).unwrap(), kind);
}

#[test]
fn stored_declarations_from_another_version_still_read() {
    // A newer version's fields are ignored, at every level.
    let mut json = example("ship").to_stored();
    json["format"] = 7.into();
    json["colour"] = "teal".into();
    json["kinds"]["ship.pr_ready"]["escalate_after"] = "1h".into();
    json["kinds"]["ship.pr_ready"]["actions"][0]["style"] = "primary".into();
    let back = Declarations::from_stored(&json).unwrap();
    assert_eq!(back.format, Some(7));
    assert_eq!(back.decl, example("ship"));

    // An older version's: missing fields take their defaults.
    let json = serde_json::json!({
        "source": "backup",
        "kinds": {
            "backup.finished": {"class": "record", "title": "Backup finished"},
            "backup.failed": {"class": "alert", "push": "urgent", "title": "Backup failed", "recovered": "Backups are back"}
        }
    });
    let back = Declarations::from_stored(&json).unwrap();
    assert_eq!(back.format, None);
    assert_eq!(back.decl.name, "backup");
    assert_eq!(back.decl.icon, Icon::Bell);
    let finished = &back.decl.kinds["backup.finished"];
    assert_eq!((finished.class, finished.push), (Class::Record, PushLevel::None));
    assert_eq!(
        back.decl.kinds["backup.failed"].throttle.map(Duration::ms),
        Some(THROTTLE_DEFAULT)
    );

    // A kind that still can't be read is reported, and the others are kept.
    let mut json = example("backup").to_stored();
    json["kinds"]["backup.failed"]["class"] = "page".into();
    json["kinds"]["backup.space"]
        .as_object_mut()
        .unwrap()
        .remove("title");
    let back = Declarations::from_stored(&json).unwrap();
    assert_eq!(
        back.unreadable.keys().collect::<Vec<_>>(),
        ["backup.failed", "backup.space"]
    );
    assert!(
        back.unreadable["backup.space"].contains("title"),
        "{:?}",
        back.unreadable
    );
    assert_eq!(back.decl.kinds.len(), example("backup").kinds.len() - 2);

    // Only a source that isn't one at all can't be read.
    assert!(Declarations::from_stored(&Value::from("backup")).is_err());
    assert!(Declarations::from_stored(&serde_json::json!({"kinds": {}})).is_err());
    assert!(Declarations::from_stored(&serde_json::json!({"source": "x", "kinds": []})).is_err());
}

#[test]
fn json_is_checked_like_toml() {
    let mut json = example("backup").to_json();
    json["kinds"]["backup.failed"]["push"] = "loud".into();
    let problems = Declarations::from_json(json, &Check::default()).unwrap_err();
    assert!(problems[0].fix.contains("beacon publish"), "{problems:?}");

    let mut json = example("backup").to_json();
    json["kinds"]["backup.failed"]["title"] = "Backup failed.".into();
    let problems = Declarations::from_json(json, &Check::default()).unwrap_err();
    assert!(
        problems.iter().any(|p| p.message.contains("punctuation")),
        "{problems:?}"
    );

    // A missing throttle in JSON means the default; null means none.
    let mut json = example("ship").to_json();
    json["kinds"]["ship.deployed"]
        .as_object_mut()
        .unwrap()
        .remove("throttle");
    let decl = Declarations::from_json(json, &Check::default()).unwrap();
    assert_eq!(
        decl.kinds["ship.deployed"].throttle.map(Duration::ms),
        Some(THROTTLE_DEFAULT)
    );
}

#[test]
fn a_missing_file_says_how_to_start() {
    let problems = load(std::path::Path::new("/nonexistent/beacon"), &Check::default()).unwrap_err();
    assert!(problems[0].message.contains("can't read it"));
    assert!(problems[0].fix.contains("source ="));
}

/// Whole files that each break one file-level rule, and a phrase the problem must contain.
#[test]
fn file_level_problems() {
    let kind = "\n[kinds.\"backup.thing\"]\nclass = \"record\"\ntitle = \"Backup finished\"\n";
    let cases: Vec<(String, &str)> = vec![
        ("source = ".to_string(), "kinds.toml: "),
        (format!("sourse = \"backup\"\nname = \"Backups\"{kind}"), "did you mean `source`?"),
        (format!("name = \"Backups\"{kind}"), "no source"),
        (format!("source = 3\nname = \"Backups\"{kind}"), "source must be a string"),
        (
            "source = \"Backup\"\nname = \"Backups\"\n[kinds.\"Backup.thing\"]\nclass = \"record\"\ntitle = \"Backup finished\"".to_string(),
            "is not a source name",
        ),
        (
            "source = \"beacon\"\nname = \"Mine\"\n[kinds.\"beacon.thing\"]\nclass = \"record\"\ntitle = \"Thing happened\"".to_string(),
            "Beacon's own source",
        ),
        (
            "source = \"trail\"\nname = \"Monitoring\"\n[kinds.\"trail.warning\"]\nclass = \"record\"\ntitle = \"Thing happened\"".to_string(),
            "one of Beacon's own Trail kinds",
        ),
        (format!("source = \"backup\"{kind}"), "no display name"),
        (format!("source = \"backup\"\nname = \"Backups of every disk in the house\"{kind}"), "at most 24"),
        (format!("source = \"backup\"\nname = \"Backups\"\nicon = \"archiv\"{kind}"), "did you mean `archive`?"),
        (format!("source = \"backup\"\nname = \"Backups\"\nicon = \"rocket\"{kind}"), "use one of archive, bell"),
        ("source = \"backup\"\nname = \"Backups\"\n".to_string(), "no kinds are declared"),
        ("source = \"backup\"\nname = \"Backups\"\nkinds = 3\n".to_string(), "kinds must be a table"),
        ("source = \"backup\"\nname = \"Backups\"\nkinds = { \"backup.x\" = 3 }\n".to_string(), "a kind must be a table"),
        (
            "source = \"backup\"\nname = \"Backups\"\n[kinds.\"deploy.failed\"]\nclass = \"record\"\ntitle = \"Deploy failed\"".to_string(),
            "is not a kind name of source `backup`",
        ),
        (
            "source = \"backup\"\nname = \"Backups\"\n[kinds.\"backup.Failed\"]\nclass = \"record\"\ntitle = \"Backup failed\"".to_string(),
            "is not a kind name",
        ),
    ];
    for (text, expected) in cases {
        let found = problems(&text);
        assert!(
            found.contains(expected),
            "expected {expected:?} for\n{text}\ngot:\n{found}"
        );
    }
}

/// One kind's fields that each break one rule, and a phrase the problems must contain.
#[test]
fn kind_level_problems() {
    let cases: Vec<(String, &str)> = vec![
        // Shape and types.
        (format!("{UPDATE}\npushh = \"silent\""), "did you mean `push`?"),
        (format!("{UPDATE}\nwhatever = 1"), "the fields are class, push"),
        ("push = \"silent\"\ntitle = \"Backup finished\"".into(), "no class"),
        ("class = \"alrt\"\npush = \"urgent\"\ntitle = \"Backup failed\"".into(), "did you mean `alert`?"),
        ("class = \"update\"\ntitle = \"Backup finished\"".into(), "no push level"),
        ("class = \"update\"\npush = \"loud\"\ntitle = \"Backup finished\"".into(), "is not a push level"),
        ("class = \"update\"\npush = \"silent\"\ntitle = 3".into(), "title must be a string, not integer"),
        (format!("{UPDATE}\ntime_bound = \"yes\""), "time_bound must be true or false"),
        (format!("{UPDATE}\nfacts = \"x\""), "facts must be a table"),
        (format!("{UPDATE}\nactions = \"done\""), "actions must be a list"),
        (format!("{UPDATE}\nactions = [3]"), "an action is a name or a table"),
        (format!("{UPDATE}\nrepeat = 3"), "repeat must be a list"),
        // Class and push.
        ("class = \"update\"\npush = \"urgent\"\ntitle = \"Backup finished\"".into(), "only alerts may push urgent"),
        ("class = \"record\"\npush = \"silent\"\ntitle = \"Backup finished\"".into(), "a record never pushes"),
        ("class = \"alert\"\npush = \"none\"\ntitle = \"Backup is failing\"".into(), "an alert must push"),
        (format!("{UPDATE}\ndescription = \"{}\"", "x".repeat(201)), "over 200 characters"),
        // Facts and slots.
        (format!("{UPDATE}\nbody = \"{{lastOk}}\"\nfacts = {{ lastOk = \"time\" }}"), "is not a name"),
        (format!("{UPDATE}\nbody = \"{{lasted}}\"\nfacts = {{ lasted = \"duration\" }}"), "`lasted` is Beacon's"),
        (format!("{UPDATE}\nbody = \"{{url}}\"\nfacts = {{ url = \"string\" }}"), "an argument of every generated emitter"),
        (format!("{UPDATE}\nbody = \"{{reset_token}}\"\nfacts = {{ reset_token = \"string\" }}"), "holds a token"),
        (format!("{UPDATE}\nbody = \"{{x}}\"\nfacts = {{ x = \"number\" }}"), "`number` is not a fact type"),
        (format!("{UPDATE}\nbody = \"{{x}}\"\nfacts = {{ x = 3 }}"), "a fact's type must be a string"),
        (format!("{UPDATE}\nbody = \"{{x}}\"\nslots = {{ x = 0 }}"), "is not a slot length"),
        (format!("{UPDATE}\nbody = \"{{x}}\"\nslots = {{ x = 2000 }}"), "is not a slot length"),
        (format!("{UPDATE}\nbody = \"{{x}}\"\nslots = {{ x = \"long\" }}"), "is not a slot length"),
        (format!("{UPDATE}\nbody = \"{{x}}\"\nfacts = {{ x = \"string\" }}\nslots = {{ x = 20 }}"), "both a fact and a slot"),
        (format!("{UPDATE}\nfacts = {{ size = \"string\" }}"), "`size` is declared but no template uses it"),
        (format!("{UPDATE}\nslots = {{ note = 20 }}"), "`note` is declared but no template uses it"),
        // Templates.
        ("class = \"update\"\npush = \"silent\"".into(), "no title"),
        (format!("{UPDATE}\nbody = \"{{reason\""), "has no closing"),
        (format!("{UPDATE}\nbody = \"done }}\""), "closes nothing"),
        (format!("{UPDATE}\nbody = \"{{reasn}}\"\nslots = {{ reason = 80 }}"), "did you mean `{reason}`?"),
        (format!("{UPDATE}\nbody = \"{{zzz}}\""), "declare it: facts = { zzz = \"string\" }"),
        (format!("{UPDATE}\nbody = \"{{lasted}}\""), "{lasted} is known only once an alert recovers"),
        (format!("{UPDATE}\nopen = \"/x/{{note}}\"\nbody = \"{{note}}\"\nslots = {{ note = 20 }}"), "a link may use only facts"),
        (format!("{UPDATE}\nopen = \"system\""), "is not a link Beacon can open"),
        (format!("{UPDATE}\nopen = \"/{}\"", "x".repeat(600)), "the link is over 512"),
        // Titles.
        ("class = \"update\"\npush = \"silent\"\ntitle = \"Backup finished {note}\"\nslots = { note = 30 }".into(), "can reach 46 characters"),
        ("class = \"update\"\npush = \"silent\"\ntitle = \"Backup finished.\"".into(), "doesn't end with punctuation"),
        ("class = \"update\"\npush = \"silent\"\ntitle = \"Cleared: backup\"".into(), "no \"Cleared:\""),
        ("class = \"update\"\npush = \"silent\"\ntitle = \"backup finished\"".into(), "starts with a capital"),
        ("class = \"update\"\npush = \"silent\"\npreview = \"Backup\"\ntitle = \"{what}\"\nslots = { what = 30 }".into(), "words of its own"),
        ("class = \"update\"\npush = \"silent\"\npreview = \"Hot\"\ntitle = \"CPU at {temp}\"\nfacts = { temp = \"float\" }".into(), "is a float: a live value"),
        ("class = \"update\"\npush = \"silent\"\npreview = \"Soon\"\ntitle = \"Resets at {at}\"\nfacts = { at = \"time\" }".into(), "is a time: a live value"),
        ("class = \"alert\"\npush = \"urgent\"\ntitle = \"Dashboard is down\"\nrecovered = \"Back after {lasted}\"".into(), "is a duration: a live value"),
        // Copy rules.
        (format!("{UPDATE}\nbody = \"Run `restart` now.\""), "a command in backticks"),
        (format!("{UPDATE}\nbody = \"Try sudo reboot.\""), "a shell command"),
        (format!("{UPDATE}\nbody = \"Since 2026-11-13.\""), "an ISO timestamp"),
        (format!("{UPDATE}\nbody = \"At a1b2c3d4e5.\""), "looks like a commit id"),
        (format!("{UPDATE}\nbody = \"{}.\"", "x".repeat(151)), "can reach 152 characters"),
        // Preview.
        ("class = \"update\"\npush = \"silent\"\ntitle = \"{app} is deployed\"\nfacts = { app = \"string\" }".into(), "needs a preview"),
        (format!("{UPDATE}\npreview = \"{}\"", "x".repeat(41)), "at most 40 show"),
        (format!("{UPDATE}\npreview = \"About {{thing}}\""), "can't have placeholders"),
        (format!("{UPDATE}\npreview = \" \""), "the preview is empty"),
        // Recovery.
        (format!("{UPDATE}\nrecovered = \"Backup is back\""), "only alerts recover"),
        (format!("{UPDATE}\nrecovered_body = \"Fine.\""), "only alerts recover"),
        (format!("{ALERT}\none_off = true\nactions = [\"dismiss\"]\nrecovered = \"Backup is back\""), "a one-off alert has no recovery"),
        ("class = \"alert\"\npush = \"urgent\"\ntitle = \"Backup failed\"".into(), "no recovery title"),
        (format!("{ALERT}\nrecovered_body = \"{}.\"", "x".repeat(151)), "can reach 152 characters"),
        (format!("{ALERT}\nrecovered = \"Backup is back.\""), "doesn't end with punctuation"),
        // Actions.
        (format!("{ALERT}\nactions = [\"acknowlege\"]"), "did you mean `acknowledge`?"),
        (format!("{UPDATE}\nactions = [\"done\"]"), "`done` doesn't apply to an update"),
        (format!("{ALERT}\nactions = [\"snooze\"]"), "`snooze` doesn't apply to an alert"),
        (format!("{ACTION}\nactions = [\"acknowledge\"]"), "`acknowledge` doesn't apply to an action"),
        (format!("{ACTION}\nactions = [\"done\", \"done\"]"), "listed twice"),
        (format!("{RECORD}\nactions = [\"silence_1h\"]"), "a record has no buttons"),
        (
            format!("{ACTION}\nactions = [\"done\", \"snooze\", \"dismiss\", \"silence_1h\", {{ id = \"open\", label = \"Open\", url = \"/x\" }}]"),
            "at most 4 fit",
        ),
        (format!("{ACTION}\nactions = [{{ id = \"Open\", label = \"Open\" }}]"), "is not an action id"),
        (format!("{ACTION}\nactions = [{{ id = \"done\", label = \"Finish\" }}]"), "one of Beacon's own actions"),
        (format!("{ACTION}\nactions = [{{ id = \"open\", label = \"\" }}]"), "a button label is 1 to 20"),
        (format!("{ACTION}\nactions = [{{ id = \"open\", label = \"Open it in the other app now\" }}]"), "a button label is 1 to 20"),
        (format!("{ACTION}\nactions = [{{ id = \"open\", label = \"Open\", url = \"ftp://x\" }}]"), "is not a link Beacon can open"),
        (format!("{ACTION}\nactions = [{{ id = \"open\", label = \"Open\", link = \"/x\" }}]"), "`link` is not a field here"),
        (format!("{ACTION}\nactions = [{{ id = \"open\", label = \"Open\", url = \"/x/{{nope}}\" }}]"), "not a declared fact or slot"),
        // Lifecycle fields.
        (format!("{ALERT}\ntime_bound = true"), "an alert passes quiet hours only with push = \"urgent\""),
        (format!("{RECORD}\ntime_bound = true"), "only actions and updates are time-bound"),
        (format!("{RECORD}\ncontext = true"), "a record is never shown in a context"),
        (format!("{UPDATE}\none_off = true"), "only alerts are one-off"),
        (format!("{ALERT}\none_off = true"), "offers neither"),
        (format!("{UPDATE}\nrepeat = [\"30m\"]"), "only alerts repeat"),
        (format!("{ALERT}\none_off = true\nactions = [\"acknowledge\"]\nrepeat = [\"30m\"]"), "a one-off alert doesn't repeat"),
        ("class = \"alert\"\npush = \"silent\"\ntitle = \"Backup is failing\"\nactions = [\"acknowledge\"]\nrepeat = [\"30m\"]".into(), "a silent alert has nothing to repeat"),
        (format!("{ALERT}\nrepeat = [\"30m\"]"), "offers no acknowledge"),
        (format!("{ALERT}\nactions = [\"acknowledge\"]\nrepeat = [\"1m\"]"), "too soon to repeat"),
        (format!("{ALERT}\nactions = [\"acknowledge\"]\nrepeat = [\"2h\", \"30m\"]"), "shorter than the interval before it"),
        (format!("{ALERT}\nactions = [\"acknowledge\"]\nrepeat = [{}]", ["\"1h\""; 9].join(", ")), "at most 8"),
        (format!("{ALERT}\nactions = [\"acknowledge\"]\nrepeat = [\"soon\"]"), "is not a duration"),
        (format!("{ALERT}\nactions = [\"acknowledge\"]\nrepeat = [30]"), "a repeat interval must be a string"),
        (format!("{UPDATE}\nthrottle = \"often\""), "is not a duration"),
        (format!("{UPDATE}\nthrottle = \"30s\""), "shorter than a minute"),
    ];
    for (fields, expected) in cases {
        let text = one_kind(&fields);
        let found = problems(&text);
        assert!(
            found.contains(expected),
            "expected {expected:?} for\n{fields}\ngot:\n{found}"
        );
    }
}

#[test]
fn banned_words_come_from_the_check() {
    let text = one_kind(&format!("{UPDATE}\nbody = \"Acme saved it.\""));
    assert_eq!(problems(&text), "");
    let check = Check {
        banned_words: vec!["acme".to_string()],
        builtin: false,
    };
    assert!(problems_with(&text, &check).contains("the word `acme`, which beacon.toml bans"));
}

#[test]
fn every_problem_has_a_place_and_a_fix() {
    let text = one_kind(
        "class = \"update\"\npush = \"urgent\"\ntitle = \"x.\"\nbody = \"{nope}\"\nactions = [\"done\"]",
    );
    let found = parse(&text, &Check::default()).unwrap_err();
    assert!(found.len() >= 4, "{found:?}");
    for problem in found {
        assert!(problem.at.starts_with("kinds.toml"), "{problem:?}");
        assert!(!problem.fix.is_empty(), "{problem:?}");
    }
}

#[test]
fn categories_are_checked() {
    let ok = example_kind(&format!("{ACTION}\ncategories = [\"APPROVAL\", \"APPROVAL_2\"]"));
    assert_eq!(ok.categories, ["APPROVAL", "APPROVAL_2"]);
    assert_eq!(push_category("backup.thing", &ok, None), "APPROVAL");
    assert_eq!(
        push_category("backup.thing", &ok, Some("APPROVAL_2")),
        "APPROVAL_2"
    );
    let plain = example_kind(ACTION);
    assert!(plain.categories.is_empty());
    assert_eq!(push_category("backup.thing", &plain, None), "backup.thing");
    for (fields, expected) in [
        (
            format!("{ACTION}\ncategories = [\"approval\"]"),
            "is not a category name",
        ),
        (
            format!("{ACTION}\ncategories = [\"9X\"]"),
            "is not a category name",
        ),
        (
            format!("{ACTION}\ncategories = [\"{}\"]", "A".repeat(41)),
            "is not a category name",
        ),
        (
            format!("{ACTION}\ncategories = [\"A\", \"B\", \"C\", \"D\", \"E\"]"),
            "at most 4",
        ),
        (format!("{ACTION}\ncategories = [\"A\", \"A\"]"), "listed twice"),
        (
            format!("{ACTION}\ncategories = [1]"),
            "a category must be a string",
        ),
        (format!("{ACTION}\ncategories = \"A\""), "must be a list"),
        (format!("{RECORD}\ncategories = [\"A\"]"), "a record never pushes"),
        (format!("{ACTION}\ncategoris = [\"A\"]"), "categories"),
        (
            format!("{UPDATE}\nslots = {{ category = 20 }}\nbody = \"{{category}}\""),
            "category",
        ),
    ] {
        let found = problems(&one_kind(&fields));
        assert!(found.contains(expected), "{fields}: {found}");
    }
    assert!(is_category(&"A".repeat(40)));
}

fn example_kind(fields: &str) -> Kind {
    let decl = parse(&one_kind(fields), &Check::default()).unwrap_or_else(|p| panic!("{p:?}"));
    decl.kinds["backup.thing"].clone()
}
