//! Loading definition files at startup. Every file is checked, against the
//! tools and each other, before any of them is saved, so one bad file changes
//! nothing and stops the start.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use chrono::{DateTime, Utc};
use schema::{Validator, bounded};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use store::DefinitionKind;
use work::{DefinitionError, DefinitionFile, Schedule, read_definition_files, save_definitions};
use workflow::{Agent, Workflow, check_agent, check_agent_sources, check_workflow};

use crate::Runtime;
use crate::jobs::{WORKFLOW_KIND, WorkflowArgs};

const DIRECTORIES: [(DefinitionKind, &str); 3] = [
    (DefinitionKind::Agent, "agents"),
    (DefinitionKind::Workflow, "workflows"),
    (DefinitionKind::Schedule, "schedules"),
];

fn invalid(file: &DefinitionFile, problem: impl std::fmt::Display) -> DefinitionError {
    DefinitionError::Invalid {
        name: file.relative.clone(),
        problem: bounded(&problem.to_string()),
    }
}

fn parse<T: DeserializeOwned + JsonSchema>(file: &DefinitionFile) -> Result<T, DefinitionError> {
    Validator::for_type::<T>()
        .map_err(|error| invalid(file, error))?
        .parse(file.body.clone())
        .map_err(|refusal| invalid(file, refusal))
}

/// A scheduled workflow has to exist, and be given inputs that fit.
fn check_scheduled_workflow(
    file: &DefinitionFile,
    schedule: &Schedule,
    workflows: &BTreeMap<&str, Workflow>,
) -> Result<(), DefinitionError> {
    let args: WorkflowArgs =
        serde_json::from_value(schedule.payload.clone()).map_err(|error| invalid(file, error))?;
    let Some(workflow) = workflows.get(args.workflow.as_str()) else {
        return Err(invalid(
            file,
            format!("there is no workflow {}", args.workflow),
        ));
    };
    Validator::from_schema(&workflow.inputs)
        .map_err(|error| invalid(file, error))?
        .check(&args.inputs)
        .map_err(|refusal| invalid(file, format!("its inputs: {refusal}")))
}

/// An agent's `role` must be one `models.yaml` defines.
fn check_agent_role(agent: &Agent, catalog: &model::Catalog) -> Result<(), String> {
    match &agent.role {
        Some(role) if catalog.role(role).is_none() => {
            let known: Vec<&str> = catalog.role_names().collect();
            Err(if known.is_empty() {
                format!("its role {role} isn't defined: models.yaml has no roles")
            } else {
                format!(
                    "its role {role} isn't defined; models.yaml's roles are {}",
                    known.join(", ")
                )
            })
        }
        _ => Ok(()),
    }
}

/// The most tools a route's profile may have: the choice among them is one
/// small model's.
pub(crate) const MAX_PROFILE_TOOLS: usize = 6;

fn check_role(
    role: Option<&String>,
    setting: &str,
    catalog: &model::Catalog,
) -> Result<(), String> {
    match role {
        Some(role) if catalog.role(role).is_none() => Err(format!(
            "{setting} names the role {role}, and models.yaml doesn't define it"
        )),
        _ => Ok(()),
    }
}

/// A route's profiles and writer must be agents it can use: a profile has
/// one to six tools and no delegation or guardrails, which a routed turn
/// doesn't run; the writer has no tools.
fn check_route(
    route: &crate::Route,
    agents: &BTreeMap<String, Agent>,
    catalog: &model::Catalog,
) -> Result<(), String> {
    let named = |name: &str| {
        agents
            .get(name)
            .ok_or_else(|| format!("it names the agent {name}, and there's no such file"))
    };
    if route.profiles.is_empty() || route.profiles.len() > crate::route::MAX_PROFILES {
        return Err(format!(
            "it needs one to {} profiles",
            crate::route::MAX_PROFILES
        ));
    }
    if !(1..=6).contains(&route.max_calls) {
        return Err("its max_calls must be 1 to 6".to_owned());
    }
    if !(1..=4).contains(&route.max_parts) {
        return Err("its max_parts must be 1 to 4".to_owned());
    }
    for name in &route.profiles {
        if name == crate::route::SMALL_TALK {
            return Err(format!(
                "{name} is the route's own choice, not a profile's name"
            ));
        }
        let profile = named(name)?;
        if profile.tools.is_empty() || profile.tools.len() > MAX_PROFILE_TOOLS {
            return Err(format!(
                "the profile {name} has {} tools; a profile has one to {MAX_PROFILE_TOOLS}",
                profile.tools.len()
            ));
        }
        if !profile.guardrails.is_empty() {
            return Err(format!(
                "the profile {name} has guardrails, which a routed turn doesn't run yet"
            ));
        }
        if profile.description.trim().is_empty() {
            return Err(format!(
                "the profile {name} has no description, and the choice reads only that"
            ));
        }
    }
    let writer = named(&route.writer)?;
    if !writer.tools.is_empty() {
        return Err(format!(
            "the writer {} has tools; it answers from the results alone",
            route.writer
        ));
    }
    for rule in &route.only_when_asked {
        for profile in [&rule.profile, &rule.otherwise] {
            if !route.profiles.contains(profile) {
                return Err(format!(
                    "only_when_asked names {profile}, which isn't one of its profiles"
                ));
            }
        }
        regex::RegexBuilder::new(&rule.asked)
            .size_limit(1 << 20)
            .build()
            .map_err(|error| format!("only_when_asked's pattern for {}: {error}", rule.profile))?;
    }
    check_role(route.choice_role.as_ref(), "choice_role", catalog)?;
    check_role(route.arguments_role.as_ref(), "arguments_role", catalog)
}

fn check_files(files: &[DefinitionFile], runtime: &Runtime) -> Result<(), DefinitionError> {
    let of_kind = |kind| files.iter().filter(move |file| file.kind == kind);
    let tools: BTreeMap<_, _> = runtime
        .registry
        .definitions()
        .into_iter()
        .map(|definition| (definition.name.clone(), definition))
        .collect();
    let mut agents = BTreeSet::new();
    let mut parsed = BTreeMap::new();
    for file in of_kind(DefinitionKind::Agent) {
        let agent = parse::<Agent>(file)?;
        check_agent(&agent, &tools)
            .and_then(|()| check_agent_sources(&agent, &runtime.sources))
            .and_then(|()| check_agent_role(&agent, runtime.gateway.catalog()))
            .map_err(|problem| invalid(file, problem))?;
        agents.insert(file.name.clone());
        parsed.insert(file.name.clone(), agent);
    }
    if let Some(route) = runtime.config.route() {
        check_route(route, &parsed, runtime.gateway.catalog()).map_err(|problem| {
            DefinitionError::Invalid {
                name: "front_door".to_owned(),
                problem,
            }
        })?;
    }
    let mut workflows = BTreeMap::new();
    for file in of_kind(DefinitionKind::Workflow) {
        let workflow: Workflow = parse(file)?;
        check_workflow(&workflow, &tools, &agents).map_err(|problem| invalid(file, problem))?;
        workflows.insert(file.name.as_str(), workflow);
    }
    let payloads = runtime
        .job_handlers()
        .iter()
        .map(|handler| (handler.kind().to_owned(), handler.payload_schema()))
        .collect();
    for file in of_kind(DefinitionKind::Schedule) {
        let schedule = Schedule::from_file(file, &payloads)?;
        if schedule.kind == WORKFLOW_KIND {
            check_scheduled_workflow(file, &schedule, &workflows)?;
        }
    }
    Ok(())
}

impl Runtime {
    /// Reads and checks every definition file under `root`, then makes the
    /// store match them, all at once. Returns how many there are.
    ///
    /// # Errors
    ///
    /// [`DefinitionError`] naming the file and what's wrong with it.
    pub async fn load_definitions(
        &self,
        root: &Path,
        now: DateTime<Utc>,
    ) -> Result<usize, DefinitionError> {
        let files = read_definition_files(root, &DIRECTORIES)?;
        check_files(&files, self)?;
        let kinds = DIRECTORIES.map(|(kind, _)| kind);
        save_definitions(self.workflows.store.as_ref(), &files, &kinds, now).await
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use events::MemorySink;
    use model::Catalog;
    use protocol::Actor;
    use serde_json::{Value, json};
    use store::{DefinitionReader, Job, JobId, JobStatus, PageRequest};
    use store_memory::MemoryStore;
    use work::JobHandler;

    use super::*;
    use crate::Config;
    use crate::jobs::{ChatJob, WorkflowJob};

    const CATALOG: &str = "version: 1\nproviders:\n  - {name: c, base_url: 'http://127.0.0.1:9', no_log: true, timeout_secs: 1}\n\
         models:\n  - {name: c, provider: c, wire_model: x, capabilities: [chat, tools], context_tokens: 1000, max_output_tokens: 100}\n";
    const AGENT: &str = "system_prompt: You help.\ntools: [memory]\n";
    const WORKFLOW: &str = "inputs:\n  type: object\n  additionalProperties: false\n  required: [text]\n  properties:\n    text: {type: string, maxLength: 100}\n\
         steps:\n  - type: tool\n    id: keep\n    tool: memory\n    arguments: {operation: store, text: '{{inputs.text}}', source_url: 'http://example.com/'}\n\
         output: done\n";
    const SCHEDULE: &str =
        "cron: '0 9 * * *'\nkind: workflow\npayload: {workflow: flow, inputs: {text: hello}}\n";

    struct Fixture {
        root: PathBuf,
        store: Arc<MemoryStore>,
        runtime: Runtime,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("definitions-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            for directory in DIRECTORIES.map(|(_, directory)| directory) {
                std::fs::create_dir_all(root.join(directory)).unwrap();
            }
            let store = Arc::new(MemoryStore::new(3));
            let runtime = Runtime::assemble(
                Arc::clone(&store) as Arc<dyn store::Store>,
                Arc::new(MemorySink::new()),
                Catalog::from_yaml(CATALOG, |_| None).unwrap(),
                Config::default(),
                &|_| None,
            )
            .unwrap();
            Self {
                root,
                store,
                runtime,
            }
        }

        fn write(&self, path: &str, text: &str) {
            std::fs::write(self.root.join(path), text).unwrap();
        }

        async fn load(&self) -> Result<usize, DefinitionError> {
            self.runtime.load_definitions(&self.root, Utc::now()).await
        }

        async fn stored(&self, kind: DefinitionKind) -> usize {
            self.store
                .list_definitions(kind, PageRequest::new(100, 0).unwrap())
                .await
                .unwrap()
                .items
                .len()
        }

        fn job(payload: Value) -> Job {
            Job {
                id: JobId::generate(),
                kind: WORKFLOW_KIND.to_owned(),
                payload,
                priority: 0,
                status: JobStatus::Claimed,
                requested_by: Actor::new("test"),
                run_at: Utc::now(),
                attempts: 1,
                max_attempts: 1,
                lease: None,
                last_error: None,
                result: None,
                finished_at: None,
            }
        }

        async fn run_job(&self, payload: Value) -> Result<Value, String> {
            WorkflowJob(self.runtime.clone())
                .run(&Self::job(payload))
                .await
                .map_err(|failure| failure.message)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn assert_invalid(result: Result<usize, DefinitionError>, expected: &str) {
        let error = result.unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
    }

    #[tokio::test]
    async fn every_kind_loads_and_is_marked_with_its_file() {
        let fixture = Fixture::new("all");
        fixture.write("agents/helper.yaml", AGENT);
        fixture.write("workflows/flow.yaml", WORKFLOW);
        fixture.write("schedules/morning.yaml", SCHEDULE);
        assert_eq!(fixture.load().await.unwrap(), 3);
        let page = fixture
            .store
            .list_definitions(DefinitionKind::Workflow, PageRequest::new(10, 0).unwrap())
            .await
            .unwrap();
        assert_eq!(page.items[0].source, "file:workflows/flow.yaml");
    }

    #[tokio::test]
    async fn one_bad_file_saves_nothing() {
        let fixture = Fixture::new("atomic");
        fixture.write("agents/helper.yaml", AGENT);
        fixture.write("workflows/flow.yaml", "inputs: {}\nsteps: []\n");
        assert_invalid(fixture.load().await, "workflows/flow.yaml");
        assert_eq!(fixture.stored(DefinitionKind::Agent).await, 0);
    }

    #[tokio::test]
    async fn a_removed_file_removes_its_definition() {
        let fixture = Fixture::new("removed");
        fixture.write("agents/helper.yaml", AGENT);
        fixture.load().await.unwrap();
        std::fs::remove_file(fixture.root.join("agents/helper.yaml")).unwrap();
        fixture.load().await.unwrap();
        assert_eq!(fixture.stored(DefinitionKind::Agent).await, 0);
    }

    #[tokio::test]
    async fn an_agent_needs_tools_that_exist() {
        let fixture = Fixture::new("agent-tool");
        fixture.write("agents/helper.yaml", "system_prompt: x\ntools: [nope]\n");
        assert_invalid(fixture.load().await, "no tool nope");
    }

    #[tokio::test]
    async fn an_agent_can_name_only_sources_that_exist() {
        let fixture = Fixture::new("agent-source");
        fixture.write(
            "agents/helper.yaml",
            "system_prompt: x\ntools: [memory]\nmemory: {sources: [lake.notes]}\n",
        );
        assert_invalid(fixture.load().await, "no plugin offers");
        fixture.write(
            "agents/helper.yaml",
            "system_prompt: x\ntools: [memory]\nmemory: {sources: []}\n",
        );
        assert_eq!(fixture.load().await.unwrap(), 1, "none at all is a choice");
    }

    #[tokio::test]
    async fn skills_and_guardrails_are_checked_when_loaded() {
        let fixture = Fixture::new("agent-rules");
        fixture.write(
            "agents/helper.yaml",
            "system_prompt: x\ntools: [memory]\n\
             skills: [{name: s, when: '(', instructions: Look.}]\n",
        );
        assert_invalid(fixture.load().await, "the skill s");
        fixture.write(
            "agents/helper.yaml",
            "system_prompt: x\ntools: [memory]\n\
             guardrails: [{name: g, tool: http_request, because: Look first., required: true}]\n",
        );
        assert_invalid(fixture.load().await, "doesn't have");
        fixture.write(
            "agents/helper.yaml",
            "system_prompt: x\ntools: [memory]\n\
             skills: [{name: s, when: notes, instructions: Keep them short.}]\n\
             guardrails: [{name: g, tool: memory, because: Look first., required: true}]\n",
        );
        assert_eq!(fixture.load().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_workflow_can_use_an_agent_defined_alongside_it_only() {
        let step = "inputs: {type: object, additionalProperties: false, properties: {}}\n\
             steps:\n  - {type: agent, id: a, agent: helper, message: hi}\n";
        let fixture = Fixture::new("agent-ref");
        fixture.write("workflows/flow.yaml", step);
        assert_invalid(fixture.load().await, "no agent helper");
        fixture.write("agents/helper.yaml", AGENT);
        fixture.load().await.unwrap();
    }

    #[tokio::test]
    async fn a_schedule_needs_a_workflow_that_exists() {
        let fixture = Fixture::new("schedule-ref");
        fixture.write("schedules/morning.yaml", SCHEDULE);
        assert_invalid(fixture.load().await, "no workflow flow");
    }

    #[tokio::test]
    async fn a_schedule_must_give_inputs_that_fit() {
        let fixture = Fixture::new("schedule-inputs");
        fixture.write("workflows/flow.yaml", WORKFLOW);
        fixture.write(
            "schedules/morning.yaml",
            "cron: '0 9 * * *'\nkind: workflow\npayload: {workflow: flow, inputs: {nope: 1}}\n",
        );
        assert_invalid(fixture.load().await, "its inputs");
    }

    #[tokio::test]
    async fn a_chat_schedule_isnt_held_to_workflow_rules() {
        let fixture = Fixture::new("chat");
        fixture.write(
            "schedules/morning.yaml",
            "cron: '0 9 * * *'\nkind: chat\npayload: {message: hi}\n",
        );
        assert_eq!(fixture.load().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn the_example_files_load_against_the_real_tools() {
        let fixture = Fixture::new("examples");
        fixture.write(
            "agents/summarizer.yaml",
            include_str!("../../../definitions/agents/summarizer.yaml.example"),
        );
        fixture.write(
            "workflows/read-and-remember.yaml",
            include_str!("../../../definitions/workflows/read-and-remember.yaml.example"),
        );
        fixture.write(
            "schedules/improve-agents.yaml",
            include_str!("../../../definitions/schedules/improve-agents.yaml.example"),
        );
        fixture.write(
            "schedules/review-answers.yaml",
            include_str!("../../../definitions/schedules/review-answers.yaml.example"),
        );
        assert_eq!(fixture.load().await.unwrap(), 4);
    }

    #[tokio::test]
    async fn the_quickstart_agents_load_and_make_the_default_route() {
        let fixture = Fixture::new("quickstart");
        for agent in ["reader", "writer"] {
            let text = match agent {
                "reader" => include_str!("../../../definitions/agents/reader.yaml.example"),
                _ => include_str!("../../../definitions/agents/writer.yaml.example"),
            };
            fixture.write(&format!("agents/{agent}.yaml"), text);
        }
        assert_eq!(fixture.load().await.unwrap(), 2);
        let line = fixture.runtime.default_line().await.unwrap();
        let profiles: Vec<&str> = line
            .profiles
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(profiles, ["reader"]);
        assert!(line.profiles[0].1.chat_writes);
        assert_eq!(line.writer.0, "writer");
    }

    #[tokio::test]
    async fn a_job_runs_a_workflow_and_keeps_its_result() {
        let fixture = Fixture::new("job");
        fixture.write("workflows/flow.yaml", WORKFLOW);
        fixture.load().await.unwrap();
        let result = fixture
            .run_job(json!({ "workflow": "flow", "inputs": { "text": "otters" } }))
            .await
            .unwrap();
        assert_eq!(result.get("output"), Some(&json!("done")));
        assert_eq!(result.get("outcome"), Some(&json!("completed")));
    }

    #[tokio::test]
    async fn a_chat_with_no_answer_is_a_failed_job_that_keeps_its_receipt() {
        let fixture = Fixture::new("chat-failed");
        let mut job = Fixture::job(json!({ "message": "hi" }));
        job.kind = "chat".to_owned();
        let failure = ChatJob(fixture.runtime.clone())
            .run(&job)
            .await
            .unwrap_err();
        let receipt = failure.result.unwrap();
        assert_eq!(
            receipt.pointer("/receipt/outcome"),
            Some(&json!("model_failed"))
        );
    }

    #[tokio::test]
    async fn a_workflow_that_fails_is_a_failed_job_naming_the_step() {
        let fixture = Fixture::new("job-failed");
        fixture.write(
            "workflows/flow.yaml",
            "inputs: {type: object, additionalProperties: false, properties: {}}\n\
             steps:\n  - {type: tool, id: fetch, tool: http_request, arguments: {url: 'http://127.0.0.1:9/'}}\n",
        );
        fixture.load().await.unwrap();
        let error = fixture
            .run_job(json!({ "workflow": "flow", "inputs": {} }))
            .await
            .unwrap_err();
        assert!(error.starts_with("step fetch:"), "{error}");
    }
}
