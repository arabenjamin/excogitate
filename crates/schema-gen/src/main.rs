//! Generates `schemas/` from the Rust types. With no argument it checks that
//! the committed files are current and strict; with `--write` it rewrites
//! them. To publish a new schema, add its type to [`generated`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::{fs, io};

use schema::{Violation, check_strict, schema_of};
use serde_json::Value;

/// Every published schema, by file stem.
fn generated() -> BTreeMap<&'static str, Value> {
    BTreeMap::from([
        ("brain_config", schema_of::<brain::BrainConfig>()),
        ("budget", schema_of::<dispatch::Budget>()),
        ("http_config", schema_of::<tools::HttpConfig>()),
        ("models_config", schema_of::<model::ModelsConfig>()),
        ("plugin_manifest", schema_of::<plugin_api::PluginManifest>()),
        ("plugins_config", schema_of::<plugin_host::PluginsConfig>()),
        ("receipt", schema_of::<dispatch::Receipt>()),
        ("record_decls", schema_of::<store::RecordDecls>()),
        ("agent", schema_of::<workflow::Agent>()),
        ("api_error", schema_of::<brain::ApiError>()),
        ("definition_list", schema_of::<brain::DefinitionList>()),
        ("job_list", schema_of::<brain::JobList>()),
        ("job_view", schema_of::<brain::JobView>()),
        ("openai_chat_request", schema_of::<brain::OpenAiRequest>()),
        ("run_request", schema_of::<brain::RunRequest>()),
        ("run_started", schema_of::<brain::RunStarted>()),
        ("schedule", schema_of::<work::Schedule>()),
        ("workflow", schema_of::<workflow::Workflow>()),
        ("workflow_run", schema_of::<workflow::WorkflowRun>()),
    ])
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
fn strictness_problems(schemas: &BTreeMap<&'static str, Value>) -> Vec<(String, Violation)> {
    schemas
        .iter()
        .flat_map(|(stem, schema)| {
            check_strict(schema)
                .into_iter()
                .map(|violation| ((*stem).to_owned(), violation))
        })
        .collect()
}

/// What differs between the generated schemas and the files in `dir`.
fn drift(dir: &Path, schemas: &BTreeMap<&'static str, Value>) -> io::Result<Vec<String>> {
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

fn write_all(dir: &Path, schemas: &BTreeMap<&'static str, Value>) -> io::Result<()> {
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
    let write = std::env::args().any(|arg| arg == "--write");
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
}
