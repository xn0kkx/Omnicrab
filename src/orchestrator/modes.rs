//! Multi-account modes: fan-out and plan → workers → synthesize.

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::{Core, LaneId, LaneTx, RunEvent, RunOutput};
use crate::model::{Account, Agent, Role};
use crate::providers::{ChatRequest, ChatTurn};

pub fn user_req(system: Option<String>, prompt: String) -> ChatRequest {
    ChatRequest {
        system,
        turns: vec![ChatTurn {
            role: Role::User,
            content: prompt,
        }],
        model: None,
    }
}

/// Same prompt to N accounts in parallel. Lane ids start at `first_lane`.
pub async fn fan_out(
    core: &Core,
    accounts: Vec<Account>,
    prompt: String,
    first_lane: LaneId,
    cancel: CancellationToken,
    ui: LaneTx,
) -> Vec<Result<RunOutput, String>> {
    let mut set = tokio::task::JoinSet::new();
    for (i, a) in accounts.into_iter().enumerate() {
        let (core, ui, cancel, prompt) = (core.clone(), ui.clone(), cancel.clone(), prompt.clone());
        set.spawn(async move {
            (
                i,
                core.run(
                    first_lane + i as u64,
                    vec![a],
                    user_req(None, prompt),
                    cancel,
                    ui,
                )
                .await,
            )
        });
    }
    let mut out: Vec<(usize, Result<RunOutput, String>)> = Vec::new();
    while let Some(r) = set.join_next().await {
        if let Ok(r) = r {
            out.push(r);
        }
    }
    out.sort_by_key(|(i, _)| *i);
    out.into_iter().map(|(_, r)| r).collect()
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct Step {
    pub agent: String,
    pub task: String,
}

/// Extract the first JSON array of steps from model output (tolerates prose / code fences).
pub fn parse_plan(text: &str) -> Option<Vec<Step>> {
    let start = text.find('[')?;
    let end = text.rfind(']')?;
    if end <= start {
        return None;
    }
    let steps: Vec<Step> = serde_json::from_str(&text[start..=end]).ok()?;
    (!steps.is_empty()).then_some(steps)
}

pub fn resolve_agent<'a>(agents: &'a [Agent], name: &str) -> Option<&'a Agent> {
    let n = name.trim().trim_start_matches('@').to_lowercase();
    agents.iter().find(|a| a.name.to_lowercase() == n)
}

/// Account chain for an agent: primary account then its fallbacks.
pub fn chain_for(agent: &Agent, accounts: &[Account]) -> Vec<Account> {
    std::iter::once(&agent.account_id)
        .chain(agent.fallback_account_ids.iter())
        .filter_map(|id| accounts.iter().find(|a| a.id == *id).cloned())
        .collect()
}

pub fn agent_req(agent: &Agent, prompt: String) -> ChatRequest {
    let system = if agent.system_prompt.trim().is_empty() && agent.role.trim().is_empty() {
        None
    } else {
        Some(
            format!(
                "You are {}. {}\n{}",
                agent.name, agent.role, agent.system_prompt
            )
            .trim()
            .to_string(),
        )
    };
    ChatRequest {
        system,
        turns: vec![ChatTurn {
            role: Role::User,
            content: prompt,
        }],
        model: agent.model.clone(),
    }
}

pub struct OrchestrationResult {
    pub final_text: Result<RunOutput, String>,
}

/// Plan → parallel workers → synthesis.
///
/// Lanes: `base` = planner, `base+1..=n` = workers, `base+n+1` = synthesizer
/// (worker count is only known after planning, so the UI creates lanes on first event).
#[allow(clippy::too_many_arguments)]
pub async fn orchestrate(
    core: &Core,
    agents: Vec<Agent>,
    accounts: Vec<Account>,
    planner_chain: Vec<Account>,
    task: String,
    base: LaneId,
    cancel: CancellationToken,
    ui: LaneTx,
) -> OrchestrationResult {
    let roster = agents
        .iter()
        .map(|a| {
            format!(
                "- {}: {}",
                a.name,
                if a.role.is_empty() {
                    "general assistant"
                } else {
                    &a.role
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let plan_prompt = format!(
        "Split the task below into 1-4 independent subtasks and assign each to one agent from the roster.\n\
         Reply with ONLY a JSON array like [{{\"agent\":\"name\",\"task\":\"what to do\"}}].\n\n\
         Roster:\n{roster}\n\nTask:\n{task}"
    );
    let _ = ui.send((base, RunEvent::Label("planner".into())));
    let planned = core
        .run(
            base,
            planner_chain.clone(),
            user_req(None, plan_prompt),
            cancel.clone(),
            ui.clone(),
        )
        .await;
    let Ok(plan_out) = planned else {
        return OrchestrationResult {
            final_text: Err("planning failed".into()),
        };
    };

    // Unparseable plan or unknown agents degrade gracefully to one step on the first agent.
    let steps = parse_plan(&plan_out.text).unwrap_or_else(|| {
        vec![Step {
            agent: agents.first().map(|a| a.name.clone()).unwrap_or_default(),
            task: task.clone(),
        }]
    });

    let mut set = tokio::task::JoinSet::new();
    for (i, s) in steps.into_iter().enumerate() {
        let (core, ui, cancel) = (core.clone(), ui.clone(), cancel.clone());
        let (chain, req) = match resolve_agent(&agents, &s.agent) {
            Some(a) => (chain_for(a, &accounts), agent_req(a, s.task.clone())),
            None => (planner_chain.clone(), user_req(None, s.task.clone())),
        };
        let lane = base + 1 + i as u64;
        let label = s.agent.clone();
        let _ = ui.send((lane, RunEvent::Label(format!("@{label}"))));
        set.spawn(async move { (i, label, core.run(lane, chain, req, cancel, ui).await) });
    }
    let mut results = Vec::new();
    while let Some(r) = set.join_next().await {
        if let Ok(r) = r {
            results.push(r);
        }
    }
    results.sort_by_key(|(i, ..)| *i);
    let n = results.len() as u64;

    let mut digest = String::new();
    for (_, who, r) in &results {
        match r {
            Ok(o) => digest.push_str(&format!(
                "## {who} (via {})\n{}\n\n",
                o.account,
                o.text.trim()
            )),
            Err(e) => digest.push_str(&format!("## {who}\n[failed: {e}]\n\n")),
        }
    }
    let synth_prompt = format!(
        "Original task:\n{task}\n\nResults from the team:\n\n{digest}\nWrite one coherent final answer that merges these results."
    );
    let _ = ui.send((base + n + 1, RunEvent::Label("synthesis".into())));
    let final_text = core
        .run(
            base + n + 1,
            planner_chain,
            user_req(None, synth_prompt),
            cancel,
            ui,
        )
        .await;
    OrchestrationResult { final_text }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_parsing() {
        let p = parse_plan("Sure!\n```json\n[{\"agent\":\"coder\",\"task\":\"write\"},{\"agent\":\"qa\",\"task\":\"test\"}]\n```").unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(
            p[0],
            Step {
                agent: "coder".into(),
                task: "write".into()
            }
        );
        assert!(parse_plan("no json here").is_none());
        assert!(parse_plan("[]").is_none());
        assert!(parse_plan("[not json]").is_none());
    }
}
