# Harness²

Harness² is how Excogitate improves its own agents from evidence. It follows
*Harness²: Recursive Agent Harnessing for an Open World* (arXiv 2610.14224).

It's being built in pieces. So far there are [trajectories](dispatch.md#trajectories),
the record every comparison cites, and probes.

## Probes

A probe runs an agent under a candidate harness on a task, with no side
effects, so a candidate can be judged before anything changes.

- **Every call is checked** as in a real run: offered tools only, schemas,
  budgets.
- **Writes never run.** They stop as `stubbed`, and the model is told the call
  would have worked, so it carries on as it would in a real run.
- **Reads run only if they're allowed.** Any other read is stubbed too.
- **A stubbed call still counts** against the budgets, so a probe hits the
  same limits a real run would.
- **A candidate is checked first** against the [agent schema](../schemas/v1/agent.schema.json),
  and every tool it names must exist.
- **Its trajectory is `probe:<name>`,** apart from the agent's real runs, and
  its harness id is the candidate's. A probe's purpose is `harness`, which is
  what a model's `paid_for` must list for a probe to pay.

## What comes next

- **The loop:** an editor writes candidates from a reference run's
  trajectory, each candidate is probed, and a composer compares the runs. Every
  decision cites a step in a trajectory.
- **Adoption:** when the evidence supports an edit, the system writes the
  agent's file itself, records the change with its evidence, tells the owner,
  and keeps what it replaced so the change can be reverted or overridden.
- **Judges measured** on known answers before they're trusted to adopt.
