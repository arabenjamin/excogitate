//! Harness²'s routed probe: the routed chat as it runs now, with one profile
//! or the writer swapped for a candidate, writes stubbed.

use async_trait::async_trait;
use dispatch::Probe;
use harness::{PROFILE_PART, RouteProbe, WRITER_PART};
use workflow::{Agent, ProbeRun};

use crate::Runtime;
use crate::mcp::ChatInput;

#[async_trait]
impl RouteProbe for Runtime {
    async fn part_of(&self, name: &str) -> Result<&'static str, String> {
        let line = self
            .current_line()
            .await
            .map_err(|failure| failure.to_string())?;
        if line.writer.0 == name {
            Ok(WRITER_PART)
        } else if line.profiles.iter().any(|(profile, _)| profile == name) {
            Ok(PROFILE_PART)
        } else {
            Err(format!(
                "{name} is neither a profile of the routed chat nor its writer"
            ))
        }
    }

    async fn probe_routed(
        &self,
        name: &str,
        agent: &Agent,
        task: &str,
    ) -> Result<ProbeRun, String> {
        self.workflows.check_candidate(agent)?;
        let mut line = self
            .current_line()
            .await
            .map_err(|failure| failure.to_string())?;
        if line.writer.0 == name {
            line.writer.1 = agent.clone();
        } else if let Some(slot) = line
            .profiles
            .iter_mut()
            .find(|(profile, _)| profile == name)
        {
            slot.1 = agent.clone();
        } else {
            return Err(format!(
                "{name} is neither a profile of the routed chat nor its writer"
            ));
        }
        // Every read the profiles make runs, so the writer writes from real
        // results, except those the owner listed as costing money or quota.
        let skips = &self.config.harness.routed_probe_skips;
        let reads = line
            .profiles
            .iter()
            .flat_map(|(_, profile)| profile.tools.iter())
            .filter(|tool| !skips.contains(tool))
            .cloned()
            .collect();
        let input = ChatInput {
            extra_system: None,
            history: Vec::new(),
            question: task.to_owned(),
            temperature: None,
            max_output_tokens: self.config.max_output_tokens,
            time_cap_ms: None,
            requested_by: Some("harness".to_owned()),
        };
        let result = self
            .routed_as(
                &line,
                input,
                harness::PURPOSE,
                &format!("probe:{name}"),
                Some(Probe { reads }),
            )
            .await
            .map_err(|failure| failure.to_string())?;
        Ok(ProbeRun {
            run: result.receipt.run.clone(),
            harness: self.workflows.agent_harness(agent),
            answer: result.answer,
            stopped_because: result.stopped_because,
            receipt: result.receipt,
        })
    }
}
