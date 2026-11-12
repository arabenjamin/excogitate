//! Generates `schemas/` from the Rust types. With no argument it checks that
//! the committed files are current and strict; with `--write` it rewrites
//! them. With `--against <dir> --since <tag>` it checks that no schema
//! breaks what was built against the ones in `<dir>`, unless this version
//! is past `<tag>`'s. To publish a new schema, add its type to
//! [`generated`] and its reader to [`role`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::{fs, io};

use schema::{Role, Violation, breaking_changes, check_strict, schema_of};
use serde_json::Value;

/// Every published schema, by file stem.
fn generated() -> BTreeMap<String, Value> {
    let mut schemas: BTreeMap<String, Value> = fixed()
        .into_iter()
        .map(|(stem, schema)| (stem.to_owned(), schema))
        .collect();
    for (tool, input, output) in runtime::bundled_tool_schemas() {
        schemas.insert(format!("tool_{tool}_input"), input);
        schemas.insert(format!("tool_{tool}_output"), output);
    }
    schemas
}

/// The schemas with a type of their own.
fn fixed() -> BTreeMap<&'static str, Value> {
    use plugin_api::wire;
    BTreeMap::from([
        ("capability_request", schema_of::<wire::CapabilityRequest>()),
        ("capability_record", schema_of::<wire::WireRecord>()),
        ("capability_page", schema_of::<wire::WirePage>()),
        (
            "capability_http_response",
            schema_of::<wire::WireHttpResponse>(),
        ),
        (
            "capability_model_reply",
            schema_of::<wire::WireModelReply>(),
        ),
        ("tool_call_context", schema_of::<wire::WireContext>()),
        (
            "capability_job_started",
            schema_of::<wire::WireJobStarted>(),
        ),
        ("capability_job_state", schema_of::<wire::WireJobState>()),
        (
            "capability_note_stored",
            schema_of::<wire::WireNoteStored>(),
        ),
        ("source_search", schema_of::<wire::WireSourceSearch>()),
        ("source_get", schema_of::<wire::WireSourceGet>()),
        ("source_found", schema_of::<wire::WireSourceFound>()),
        ("source_got", schema_of::<wire::WireSourceGot>()),
        ("config", schema_of::<runtime::Config>()),
        ("budget", schema_of::<dispatch::Budget>()),
        ("change_list", schema_of::<runtime::ChangeList>()),
        ("change_view", schema_of::<runtime::ChangeView>()),
        ("hold_list", schema_of::<runtime::HoldList>()),
        ("lesson_list", schema_of::<runtime::LessonList>()),
        ("hold_request", schema_of::<runtime::HoldRequest>()),
        ("http_config", schema_of::<tools::HttpConfig>()),
        ("models_config", schema_of::<model::ModelsConfig>()),
        ("plugin_manifest", schema_of::<plugin_api::PluginManifest>()),
        ("plugins_config", schema_of::<plugin_host::PluginsConfig>()),
        ("receipt", schema_of::<dispatch::Receipt>()),
        ("record_decls", schema_of::<store::RecordDecls>()),
        ("agent", schema_of::<workflow::Agent>()),
        ("api_error", schema_of::<runtime::ApiError>()),
        ("attempt_list", schema_of::<runtime::AttemptList>()),
        ("attempt_view", schema_of::<runtime::AttemptView>()),
        ("improve_request", schema_of::<harness::Request>()),
        ("pick_result", schema_of::<runtime::Picked>()),
        ("definition_list", schema_of::<runtime::DefinitionList>()),
        ("job_list", schema_of::<runtime::JobList>()),
        ("job_view", schema_of::<runtime::JobView>()),
        ("openai_chat_request", schema_of::<runtime::OpenAiRequest>()),
        ("run_list", schema_of::<runtime::RunList>()),
        ("run_request", schema_of::<runtime::RunRequest>()),
        ("run_started", schema_of::<runtime::RunStarted>()),
        ("schedule", schema_of::<work::Schedule>()),
        ("trajectory", schema_of::<dispatch::Trajectory>()),
        ("workflow", schema_of::<workflow::Workflow>()),
        ("workflow_run", schema_of::<workflow::WorkflowRun>()),
        ("export_manifest", schema_of::<runtime::Manifest>()),
        ("export_note", schema_of::<runtime::NoteLine>()),
        ("export_chunk", schema_of::<runtime::ChunkLine>()),
        ("export_task", schema_of::<runtime::TaskLine>()),
        ("export_working", schema_of::<runtime::WorkingLine>()),
        ("export_job", schema_of::<runtime::JobLine>()),
        ("export_definition", schema_of::<runtime::DefinitionLine>()),
        ("export_record", schema_of::<runtime::RecordLine>()),
        ("export_link", schema_of::<runtime::LinkLine>()),
        ("export_spend", schema_of::<runtime::SpendLine>()),
    ])
}

/// Who reads each published schema: what the runtime accepts, or what it
/// produces. Every schema has one, so a new one can't skip the check.
fn role(stem: &str) -> Option<Role> {
    if stem.starts_with("tool_") && stem.ends_with("_input") {
        return Some(Role::Input);
    }
    if stem.starts_with("tool_") && stem.ends_with("_output") {
        return Some(Role::Output);
    }
    // An export is written by one version and read by a later one, so it's
    // held to what an import must keep accepting.
    if stem.starts_with("export_") {
        return Some(Role::Input);
    }
    match stem {
        "agent"
        | "budget"
        | "capability_request"
        | "config"
        | "hold_request"
        | "http_config"
        | "improve_request"
        | "models_config"
        | "openai_chat_request"
        | "plugin_manifest"
        | "plugins_config"
        | "record_decls"
        | "run_request"
        | "schedule"
        | "source_found"
        | "source_got"
        | "workflow" => Some(Role::Input),
        "api_error"
        | "attempt_list"
        | "attempt_view"
        | "capability_http_response"
        | "capability_job_started"
        | "capability_job_state"
        | "capability_model_reply"
        | "capability_note_stored"
        | "capability_page"
        | "capability_record"
        | "change_list"
        | "change_view"
        | "definition_list"
        | "hold_list"
        | "job_list"
        | "job_view"
        | "lesson_list"
        | "pick_result"
        | "receipt"
        | "run_list"
        | "run_started"
        | "source_get"
        | "source_search"
        | "tool_call_context"
        | "trajectory"
        | "workflow_run" => Some(Role::Output),
        _ => None,
    }
}

/// The part of a version that must change for a break: the minor before
/// 1.0, the major after.
fn compatibility_line(version: &str) -> Option<(u64, u64)> {
    let mut parts = version.trim_start_matches('v').split('.');
    let major: u64 = parts.next()?.parse().ok()?;
    let minor: u64 = parts.next()?.parse().ok()?;
    Some(if major == 0 { (0, minor) } else { (major, 0) })
}

/// Every break between the schemas in `baseline` and `schemas`.
fn breaks_since(baseline: &Path, schemas: &BTreeMap<String, Value>) -> io::Result<Vec<String>> {
    let mut found = Vec::new();
    let mut names: Vec<String> = fs::read_dir(baseline)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<io::Result<_>>()?;
    names.sort();
    for name in names {
        let Some(stem) = name.strip_suffix(".schema.json") else {
            continue;
        };
        let Some(new) = schemas.get(stem) else {
            found.push(format!("{name} was removed"));
            continue;
        };
        let Some(role) = role(stem) else {
            found.push(format!("{name} has no role in schema-gen"));
            continue;
        };
        let old: Value = serde_json::from_str(&fs::read_to_string(baseline.join(&name))?)?;
        for change in breaking_changes(&old, new, role) {
            found.push(format!("{name}: {change}"));
        }
    }
    Ok(found)
}

/// The compatibility check: breaks since `since` fail unless this version
/// is past its line. Prints what it found.
fn compatible(baseline: &Path, since: &str) -> io::Result<bool> {
    let found = breaks_since(baseline, &generated())?;
    let version = env!("CARGO_PKG_VERSION");
    if found.is_empty() {
        eprintln!("schema-gen: no schema breaks what {since} published");
        return Ok(true);
    }
    let declared = match (compatibility_line(version), compatibility_line(since)) {
        (Some(now), Some(then)) => now != then,
        _ => false,
    };
    for change in &found {
        eprintln!("schema-gen: breaking since {since}: {change}");
    }
    if declared {
        eprintln!("schema-gen: allowed, since {version} is past {since}'s version line");
    } else {
        eprintln!(
            "schema-gen: make the change compatible, or bump the version past {since} \
             and say so in the changelog"
        );
    }
    Ok(declared)
}

/// Where the current version's schemas live.
fn schema_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/v1")
}

fn file_name(stem: &str) -> String {
    format!("{stem}.schema.json")
}

fn render(schema: &Value) -> Result<String, serde_json::Error> {
    Ok(serde_json::to_string_pretty(schema)? + "\n")
}

/// Every schema that isn't strict, and why.
fn strictness_problems(schemas: &BTreeMap<String, Value>) -> Vec<(String, Violation)> {
    schemas
        .iter()
        .flat_map(|(stem, schema)| {
            check_strict(schema)
                .into_iter()
                .map(|violation| (stem.clone(), violation))
        })
        .collect()
}

/// What differs between the generated schemas and the files in `dir`.
fn drift(dir: &Path, schemas: &BTreeMap<String, Value>) -> io::Result<Vec<String>> {
    let mut found = Vec::new();
    for (stem, schema) in schemas {
        let name = file_name(stem);
        let want = render(schema)?;
        match fs::read_to_string(dir.join(&name)) {
            Ok(have) if have == want => {}
            Ok(_) => found.push(format!("{name} is out of date")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                found.push(format!("{name} is missing"));
            }
            Err(error) => return Err(error),
        }
    }
    let known: Vec<String> = schemas.keys().map(|stem| file_name(stem)).collect();
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if !known.contains(&name) {
            found.push(format!("{name} has no type behind it"));
        }
    }
    Ok(found)
}

fn write_all(dir: &Path, schemas: &BTreeMap<String, Value>) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    for (stem, schema) in schemas {
        fs::write(dir.join(file_name(stem)), render(schema)?)?;
    }
    Ok(())
}

fn run(write: bool) -> io::Result<Vec<String>> {
    let schemas = generated();
    let mut problems: Vec<String> = strictness_problems(&schemas)
        .into_iter()
        .map(|(stem, violation)| format!("{stem} isn't strict at {violation}"))
        .collect();
    if write && problems.is_empty() {
        write_all(&schema_dir(), &schemas)?;
    } else if !write {
        problems.extend(drift(&schema_dir(), &schemas)?);
    }
    Ok(problems)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value_of = |flag: &str| {
        args.iter()
            .position(|arg| arg == flag)
            .and_then(|at| args.get(at + 1))
    };
    if let Some(baseline) = value_of("--against") {
        let Some(since) = value_of("--since") else {
            eprintln!("schema-gen: --against needs --since <tag>");
            return ExitCode::FAILURE;
        };
        return match compatible(Path::new(baseline), since) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::FAILURE,
            Err(error) => {
                eprintln!("schema-gen: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let write = args.iter().any(|arg| arg == "--write");
    match run(write) {
        Ok(problems) if problems.is_empty() => ExitCode::SUCCESS,
        Ok(problems) => {
            for problem in problems {
                eprintln!("schema-gen: {problem}");
            }
            eprintln!("schema-gen: fix the type, or run with --write if the change is intended");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("schema-gen: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_schemas_are_current_and_strict() {
        assert_eq!(run(false).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn every_schema_says_who_reads_it() {
        let missing: Vec<String> = generated()
            .into_keys()
            .filter(|stem| role(stem).is_none())
            .collect();
        assert_eq!(missing, Vec::<String>::new());
    }

    #[test]
    fn the_committed_schemas_break_nothing_against_themselves() {
        assert_eq!(
            breaks_since(&schema_dir(), &generated()).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_break_is_declared_by_the_version_line() {
        assert_eq!(compatibility_line("v0.4.0"), Some((0, 4)));
        assert_eq!(compatibility_line("0.4.7"), Some((0, 4)));
        assert_eq!(compatibility_line("1.2.0"), Some((1, 0)));
        assert_eq!(compatibility_line("main"), None);
    }
}
