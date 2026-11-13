//! `beacon check`, `beacon gen` and `beacon publish`: a service's declarations, from TOML to
//! generated code and to the server.

use crate::args::Args;
use crate::http::Http;
use crate::{Done, Fail, out};
use beacon_codegen::Language;
use beacon_core::Problem;
use beacon_core::decl::{self, Check, Declarations};
use std::collections::BTreeMap;
use std::path::Path;

/// Loads a folder (or a kinds.toml) and runs every check, the generator's included.
fn load(folder: &str) -> Result<Declarations, Vec<Problem>> {
    let decl = decl::load(Path::new(folder), &Check::default()).map_err(|p| in_folder(folder, p))?;
    let problems = beacon_codegen::check(&decl);
    if problems.is_empty() {
        Ok(decl)
    } else {
        Err(in_folder(folder, problems))
    }
}

/// Puts the folder in front of each problem's place, so two folders' problems can be told apart.
fn in_folder(folder: &str, problems: Vec<Problem>) -> Vec<Problem> {
    problems
        .into_iter()
        .map(|p| {
            let at = if p.at.starts_with(folder) {
                p.at
            } else {
                format!("{}/{}", folder.trim_end_matches('/'), p.at)
            };
            Problem::new(at, p.message, p.fix)
        })
        .collect()
}

fn summary(decl: &Declarations) -> String {
    let count = decl.kinds.len();
    format!(
        "{}: {count} {}",
        decl.source,
        if count == 1 { "kind" } else { "kinds" }
    )
}

/// Loads every folder; a source may appear once among them.
fn load_all(folders: &[String]) -> Result<Vec<(String, Declarations)>, Fail> {
    let mut loaded = Vec::new();
    let mut problems = Vec::new();
    let mut sources: BTreeMap<String, String> = BTreeMap::new();
    for folder in folders {
        match load(folder) {
            Err(found) => problems.extend(found),
            Ok(decl) => {
                if let Some(first) = sources.get(&decl.source) {
                    problems.push(Problem::new(
                        format!("{}/{}", folder.trim_end_matches('/'), decl::FILE),
                        format!("source `{}` is also declared in {first}", decl.source),
                        "a source keeps all its kinds in one kinds.toml: merge the two, or rename one source",
                    ));
                } else {
                    sources.insert(decl.source.clone(), folder.clone());
                    loaded.push((folder.clone(), decl));
                }
            }
        }
    }
    if problems.is_empty() {
        Ok(loaded)
    } else {
        Err(Fail::refused(problems))
    }
}

/// `beacon check <folder>...`
pub fn check(raw: Vec<String>) -> Done {
    let args = Args::parse("check", raw, &[], &[])?;
    args.need("check", &["folder"])?;
    for (_, decl) in load_all(&args.positional)? {
        out(&format!("ok  {}", summary(&decl)));
    }
    Ok(())
}

/// `beacon gen <folder> --lang python|typescript [--out FILE] [--check]`
pub fn generate(raw: Vec<String>) -> Done {
    let args = Args::parse("gen", raw, &["lang", "out"], &["check"])?;
    args.need("gen", &["folder"])?;
    args.at_most("gen", 1)?;
    let folder = &args.positional[0];
    let names = Language::ALL.map(Language::name).join(" or ");
    let language = args.value("lang").and_then(Language::parse).ok_or_else(|| {
        Fail::usage(Problem::new(
            "beacon gen --lang",
            match args.value("lang") {
                Some(given) => format!("`{given}` is not a language Beacon generates"),
                None => "which language?".to_string(),
            },
            format!("add `--lang {names}`"),
        ))
    })?;
    let decl = load(folder).map_err(Fail::refused)?;
    let text = beacon_codegen::generate(&decl, language);
    let Some(file) = args.value("out") else {
        if args.flag("check") {
            return Err(Fail::usage(Problem::new(
                "beacon gen --check",
                "`--check` needs the file to compare with",
                "add `--out <file>`",
            )));
        }
        out(text.trim_end_matches('\n'));
        return Ok(());
    };
    let current = std::fs::read_to_string(file).ok();
    if current.as_deref() == Some(text.as_str()) {
        if args.flag("check") {
            out(&format!("ok  {file} is current"));
        }
        return Ok(());
    }
    if args.flag("check") {
        let why = match current.as_deref().and_then(beacon_codegen::hash_in) {
            None if current.is_none() => "this generated file does not exist".to_string(),
            Some(hash) if hash != beacon_codegen::hash(&decl) => {
                format!(
                    "this file was generated from other declarations of `{}`",
                    decl.source
                )
            }
            _ => "this file differs from what `beacon gen` writes now".to_string(),
        };
        return Err(Fail::refused(vec![Problem::new(
            file,
            why,
            format!(
                "run `beacon gen {folder} --lang {} --out {file}` and commit the result",
                language.name()
            ),
        )]));
    }
    if let Some(parent) = Path::new(file).parent().filter(|p| !p.as_os_str().is_empty()) {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(file, text).map_err(|e| {
        Fail::refused(vec![Problem::new(
            file,
            format!("can't write it: {e}"),
            "check the folder exists and is writable",
        )])
    })?;
    out(&format!("wrote {file}"));
    Ok(())
}

/// `beacon publish <folder>... [--server URL]`
pub fn publish(raw: Vec<String>) -> Done {
    let args = Args::parse("publish", raw, &["server"], &[])?;
    args.need("publish", &["folder"])?;
    // Everything is checked before anything is sent.
    let loaded = load_all(&args.positional)?;
    let http = Http::new(args.server());
    let mut problems = Vec::new();
    for (folder, decl) in loaded {
        let path = format!("/v1/registry/{}", decl.source);
        let answer = http.request("PUT", &path, Some(&decl.to_json()))?;
        if answer.ok() {
            out(&format!("published  {}", summary(&decl)));
        } else if answer.status >= 500 {
            return Err(Fail::unreachable(answer.problems()));
        } else {
            problems.extend(in_folder(&folder, answer.problems()));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Fail::refused(problems))
    }
}
