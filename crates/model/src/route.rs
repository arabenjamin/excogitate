//! Choosing a model: privacy first, then capability, then budget. Cheapest
//! comes first, so a free or local model wins when it can do the work.

use std::fmt;
use std::sync::Arc;

use protocol::DataClass;

use crate::{Capability, Catalog, Model};

/// What a call needs from a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutePolicy {
    /// What the model must be able to do, beyond what the call itself implies.
    pub capabilities: Vec<Capability>,
    /// Who may see the request. Private goes only to providers keeping no logs.
    pub data_class: DataClass,
    /// What this work is for, checked against a paid provider's allowlist.
    pub purpose: String,
    /// What's left of the run's money budget, if it has one.
    pub budget_usd_micros: Option<u64>,
    /// The vector width an embedding must have, when embedding.
    pub embedding_dimension: Option<usize>,
    /// The one model this call may use, by name, when it must be that one:
    /// a test of a model has to reach the model it tests.
    pub only_model: Option<String>,
    /// The role whose models this call tries, in the role's order instead
    /// of cheapest first. A model outside it isn't used.
    pub role: Option<String>,
}

/// How many tokens a call could use, at most.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenEstimate {
    /// Tokens read and paid for, in total.
    pub input: u32,
    /// The most tokens read in one piece, which has to fit the context. Only
    /// a batch of embeddings has this differ from `input`.
    pub largest_input: u32,
    /// Tokens written.
    pub output: u32,
}

/// Why a model wasn't used. A closed list, so a failure to route is
/// explainable model by model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// The work is private and the provider may keep logs.
    NotPrivate,
    /// It can't do something the call needs.
    Lacks(Capability),
    /// The request plus its reply wouldn't fit.
    TooSmall {
        /// Tokens the call needs.
        needed: u64,
        /// Tokens the model has.
        has: u32,
    },
    /// Its embeddings are the wrong width.
    WrongWidth,
    /// It's priced, and this purpose isn't allowed to pay.
    PaidNotAllowed,
    /// The call could cost more than the run has left.
    OverRunBudget,
    /// The provider's daily cap can't cover the call.
    OverDailyCap,
    /// The call is pinned to another model.
    NotPinned,
    /// The call asks for a role this model isn't in, or that isn't defined.
    NotInRole(String),
    /// It failed or was rate-limited lately, and is left alone until then.
    Cooling {
        /// When it may be tried again.
        until: chrono::DateTime<chrono::Utc>,
    },
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotPrivate => f.write_str("work is private and the provider may keep logs"),
            Self::Lacks(capability) => write!(f, "lacks {capability:?}"),
            Self::TooSmall { needed, has } => {
                write!(f, "needs {needed} tokens and the model has {has}")
            }
            Self::WrongWidth => f.write_str("embeddings are the wrong width"),
            Self::PaidNotAllowed => f.write_str("priced, and this purpose may not pay"),
            Self::OverRunBudget => f.write_str("could cost more than the run has left"),
            Self::OverDailyCap => f.write_str("the provider's daily cap can't cover it"),
            Self::NotPinned => f.write_str("the call is pinned to another model"),
            Self::NotInRole(role) => write!(f, "not in the role {role}"),
            Self::Cooling { until } => write!(
                f,
                "failed or rate-limited lately, left alone until {}",
                until.format("%H:%M:%S UTC")
            ),
        }
    }
}

/// No model could take the call, and why not, model by model.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("no model can take this call: {}", self.explain())]
pub struct NoRoute {
    /// Every model considered, with why it wasn't used.
    pub rejected: Vec<(String, Rejection)>,
}

impl NoRoute {
    fn explain(&self) -> String {
        if self.rejected.is_empty() {
            return "none are configured".to_owned();
        }
        let parts: Vec<String> = self
            .rejected
            .iter()
            .map(|(model, why)| format!("{model}: {why}"))
            .collect();
        parts.join("; ")
    }
}

/// A model that can take the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The model.
    pub model: Arc<Model>,
    /// The output tokens it would be allowed.
    pub output_tokens: u32,
    /// The most the call could cost on it.
    pub worst_case_usd_micros: u64,
    /// What the prompt alone costs: the least a call that reached the model
    /// is billed.
    pub input_usd_micros: u64,
}

fn check(
    model: &Arc<Model>,
    required: &[Capability],
    policy: &RoutePolicy,
    estimate: TokenEstimate,
) -> Result<Candidate, Rejection> {
    // Privacy before everything: no other reason can let private work through.
    if policy.data_class == DataClass::Private && !model.provider.no_log {
        return Err(Rejection::NotPrivate);
    }
    if policy
        .only_model
        .as_ref()
        .is_some_and(|only| *only != model.name)
    {
        return Err(Rejection::NotPinned);
    }
    if let Some(missing) = required.iter().find(|needed| !model.can(**needed)) {
        return Err(Rejection::Lacks(*missing));
    }
    if policy
        .embedding_dimension
        .is_some_and(|width| model.embedding_dimension != Some(width))
    {
        return Err(Rejection::WrongWidth);
    }
    let output_tokens = estimate.output.min(model.max_output_tokens);
    let needed = u64::from(estimate.largest_input) + u64::from(output_tokens);
    if needed > u64::from(model.context_tokens) {
        return Err(Rejection::TooSmall {
            needed,
            has: model.context_tokens,
        });
    }
    if model.is_paid() && !model.provider.paid_for.contains(&policy.purpose) {
        return Err(Rejection::PaidNotAllowed);
    }
    let worst_case_usd_micros = model.price.cost(estimate.input, output_tokens);
    if policy
        .budget_usd_micros
        .is_some_and(|left| worst_case_usd_micros > left)
    {
        return Err(Rejection::OverRunBudget);
    }
    Ok(Candidate {
        model: Arc::clone(model),
        output_tokens,
        worst_case_usd_micros,
        input_usd_micros: model.price.cost(estimate.input, 0),
    })
}

/// The models that can take a call, cheapest first; among equal costs, in
/// configured order. A call that asks for a role takes only that role's
/// models, in the role's order.
///
/// # Errors
///
/// [`NoRoute`] if none can, saying why for each.
pub fn plan(
    catalog: &Catalog,
    required: &[Capability],
    policy: &RoutePolicy,
    estimate: TokenEstimate,
) -> Result<Vec<Candidate>, NoRoute> {
    let mut candidates = Vec::new();
    let mut rejected = Vec::new();
    let in_role = policy
        .role
        .as_deref()
        .map(|role| (role, catalog.role(role)));
    let ordered: Vec<&Arc<Model>> = match in_role {
        Some((_, Some(models))) => models.iter().collect(),
        Some((role, None)) => {
            let rejected = catalog
                .models()
                .iter()
                .map(|model| (model.name.clone(), Rejection::NotInRole(role.to_owned())))
                .collect();
            return Err(NoRoute { rejected });
        }
        None => catalog.models().iter().collect(),
    };
    for model in ordered {
        match check(model, required, policy, estimate) {
            Ok(candidate) => candidates.push(candidate),
            Err(why) => rejected.push((model.name.clone(), why)),
        }
    }
    if candidates.is_empty() {
        return Err(NoRoute { rejected });
    }
    if in_role.is_none() {
        candidates.sort_by_key(|candidate| candidate.worst_case_usd_micros);
    }
    Ok(candidates)
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::catalog::test_support::catalog;

    const YAML: &str = "
version: 1
providers:
  - {name: local, base_url: 'http://l/v1', no_log: true, timeout_secs: 5}
  - {name: cloud, base_url: 'https://c/v1', no_log: false, timeout_secs: 5,
     spend_cap_usd_micros_per_day: 1000000, paid_for: [allowed]}
  - {name: free, base_url: 'https://f/v1', no_log: false, timeout_secs: 5}
models:
  - {name: paid, provider: cloud, wire_model: a, capabilities: [chat, tools, investigative],
     context_tokens: 1000, max_output_tokens: 100,
     price: {input_usd_micros_per_mtok: 1000000, output_usd_micros_per_mtok: 1000000}}
  - {name: free-cloud, provider: free, wire_model: b, capabilities: [chat],
     context_tokens: 1000, max_output_tokens: 100}
  - {name: small, provider: local, wire_model: c, capabilities: [chat],
     context_tokens: 100, max_output_tokens: 50}
  - {name: embed, provider: local, wire_model: d, capabilities: [embeddings],
     context_tokens: 100, max_output_tokens: 1, embedding_dimension: 3}
";

    fn policy(data_class: DataClass) -> RoutePolicy {
        RoutePolicy {
            capabilities: vec![],
            data_class,
            purpose: "allowed".to_owned(),
            budget_usd_micros: None,
            embedding_dimension: None,
            only_model: None,
            role: None,
        }
    }

    const SMALL_CALL: TokenEstimate = TokenEstimate {
        input: 10,
        largest_input: 10,
        output: 10,
    };

    fn names(result: &Result<Vec<Candidate>, NoRoute>) -> Vec<&str> {
        result
            .as_ref()
            .unwrap()
            .iter()
            .map(|c| c.model.name.as_str())
            .collect()
    }

    fn why(result: Result<Vec<Candidate>, NoRoute>, model: &str) -> Rejection {
        let no_route = result.unwrap_err();
        let found = no_route.rejected.iter().find(|(name, _)| name == model);
        found.unwrap().1.clone()
    }

    #[test]
    fn a_role_goes_in_its_own_order_not_cheapest_first() {
        let yaml = format!("{YAML}roles:\n  - {{name: writer, models: [paid, small]}}\n");
        let c = catalog(&yaml);
        let mut asked = policy(DataClass::Public);
        assert_eq!(
            names(&plan(&c, &[Capability::Chat], &asked, SMALL_CALL)),
            ["free-cloud", "small", "paid"],
            "without a role, cheapest first"
        );
        asked.role = Some("writer".to_owned());
        // The priced model first, as the role says; free-cloud isn't in it.
        assert_eq!(
            names(&plan(&c, &[Capability::Chat], &asked, SMALL_CALL)),
            ["paid", "small"]
        );
        // Every other check still holds: private work skips the one that
        // keeps logs, and the role's next model takes it.
        asked.data_class = DataClass::Private;
        assert_eq!(
            names(&plan(&c, &[Capability::Chat], &asked, SMALL_CALL)),
            ["small"]
        );
    }

    #[test]
    fn a_role_thats_not_defined_routes_nowhere_and_says_so() {
        let c = catalog(YAML);
        let mut asked = policy(DataClass::Public);
        asked.role = Some("judge".to_owned());
        let result = plan(&c, &[Capability::Chat], &asked, SMALL_CALL);
        assert_eq!(
            why(result, "small"),
            Rejection::NotInRole("judge".to_owned())
        );
    }

    #[test]
    fn private_work_only_goes_where_no_logs_are_kept() {
        let c = catalog(YAML);
        let plan = plan(
            &c,
            &[Capability::Chat],
            &policy(DataClass::Private),
            SMALL_CALL,
        );
        assert_eq!(names(&plan), ["small"]);
    }

    #[test]
    fn private_work_fails_rather_than_leak() {
        let c = catalog(YAML);
        let needs = [Capability::Investigative];
        let result = plan(&c, &needs, &policy(DataClass::Private), SMALL_CALL);
        assert_eq!(why(result, "paid"), Rejection::NotPrivate);
    }

    #[test]
    fn a_pinned_call_goes_only_to_its_model() {
        let c = catalog(YAML);
        let mut pinned = policy(DataClass::Public);
        pinned.only_model = Some("small".to_owned());
        let result = plan(&c, &[Capability::Chat], &pinned, SMALL_CALL);
        assert_eq!(names(&result), ["small"]);
        // Privacy still comes first: a pin can't send private work to a
        // provider that keeps logs.
        let mut leaky = policy(DataClass::Private);
        leaky.only_model = Some("free-cloud".to_owned());
        let result = plan(&c, &[Capability::Chat], &leaky, SMALL_CALL);
        assert_eq!(why(result, "free-cloud"), Rejection::NotPrivate);
        let mut gone = policy(DataClass::Public);
        gone.only_model = Some("gone".to_owned());
        let result = plan(&c, &[Capability::Chat], &gone, SMALL_CALL);
        assert_eq!(why(result, "small"), Rejection::NotPinned);
    }

    #[test]
    fn the_cheapest_goes_first_then_configured_order() {
        let c = catalog(YAML);
        let plan = plan(
            &c,
            &[Capability::Chat],
            &policy(DataClass::Public),
            SMALL_CALL,
        );
        assert_eq!(names(&plan), ["free-cloud", "small", "paid"]);
    }

    #[test]
    fn a_purpose_off_the_allowlist_cannot_pay() {
        let c = catalog(YAML);
        let mut p = policy(DataClass::Public);
        p.purpose = "background".to_owned();
        let needs = [Capability::Investigative];
        assert_eq!(
            why(plan(&c, &needs, &p, SMALL_CALL), "paid"),
            Rejection::PaidNotAllowed
        );
    }

    #[test]
    fn the_run_budget_bounds_the_worst_case() {
        let c = catalog(YAML);
        let mut p = policy(DataClass::Public);
        let needs = [Capability::Investigative];
        p.budget_usd_micros = Some(19);
        assert_eq!(
            why(plan(&c, &needs, &p, SMALL_CALL), "paid"),
            Rejection::OverRunBudget
        );
        p.budget_usd_micros = Some(20);
        assert_eq!(names(&plan(&c, &needs, &p, SMALL_CALL)), ["paid"]);
    }

    #[test]
    fn a_call_must_fit_the_context() {
        let c = catalog(YAML);
        let big = TokenEstimate {
            input: 80,
            largest_input: 80,
            output: 50,
        };
        let plan_result = plan(&c, &[Capability::Chat], &policy(DataClass::Private), big);
        assert_eq!(
            why(plan_result, "small"),
            Rejection::TooSmall {
                needed: 130,
                has: 100
            }
        );
    }

    #[test]
    fn output_is_cut_to_the_model_limit() {
        let c = catalog(YAML);
        let ask = TokenEstimate {
            input: 1,
            largest_input: 1,
            output: 90,
        };
        let plan = plan(&c, &[Capability::Chat], &policy(DataClass::Private), ask).unwrap();
        assert_eq!(plan[0].output_tokens, 50);
    }

    #[test]
    fn embeddings_must_be_the_stores_width() {
        let c = catalog(YAML);
        let mut p = policy(DataClass::Private);
        p.embedding_dimension = Some(4);
        let result = plan(&c, &[Capability::Embeddings], &p, SMALL_CALL);
        assert_eq!(why(result, "embed"), Rejection::WrongWidth);
        p.embedding_dimension = Some(3);
        assert_eq!(
            names(&plan(&c, &[Capability::Embeddings], &p, SMALL_CALL)),
            ["embed"]
        );
    }

    #[test]
    fn no_route_names_every_model() {
        let c = catalog(YAML);
        let needs = [Capability::Tools];
        let message = plan(&c, &needs, &policy(DataClass::Private), SMALL_CALL)
            .unwrap_err()
            .to_string();
        for model in ["paid", "free-cloud", "small", "embed"] {
            assert!(message.contains(model), "{message}");
        }
    }
}
