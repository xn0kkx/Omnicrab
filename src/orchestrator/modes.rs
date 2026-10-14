//! Multi-account modes: fan-out and plan → workers → synthesize.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{Core, LaneId, LaneTx, RunEvent, RunOutput, balance};
use crate::model::{Account, Agent, BalancePolicy, Role, Slot};
use crate::providers::{ChatRequest, ChatTurn};
use crate::usage::AccountUsage;

/// Longest result text sent to the synthesizer, per step.
const SYNTH_RESULT_CAP: usize = 8_000;

/// System prompt for pool executors: they answer one self-contained task.
const EXECUTOR_SYSTEM: &str =
    "Answer the task directly and concisely. No preamble, no restating the task.";

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

#[derive(Debug, Deserialize, PartialEq, Clone)]
pub struct Step {
    /// Empty means "any executor from the pool".
    #[serde(default)]
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
    if n.is_empty() {
        return None;
    }
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

/// Everything the multi-step modes need to pick accounts.
#[derive(Clone)]
pub struct Team {
    pub agents: Vec<Agent>,
    pub accounts: Vec<Account>,
    /// Ordered chain for planning (and for synthesis when `synth` is empty).
    pub planner: Vec<Account>,
    pub synth: Vec<Account>,
    /// Executor slots; steps without a named agent are spread over these.
    pub pool: Vec<Slot>,
    pub usage: HashMap<Uuid, AccountUsage>,
    pub now: DateTime<Utc>,
    pub policy: BalancePolicy,
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}\n[... truncated]")
}

/// Ask the planner for steps. `context` is an optional summary of the conversation.
///
/// Fixed instructions and the sorted roster come first and the task last, so the
/// shared prefix stays identical between requests and can be cached.
pub async fn plan(
    core: &Core,
    team: &Team,
    task: &str,
    context: Option<&str>,
    lane: LaneId,
    cancel: CancellationToken,
    ui: LaneTx,
) -> Result<Vec<Step>, String> {
    let mut roster: Vec<String> = team
        .agents
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
        .collect();
    roster.sort();
    let roster = if roster.is_empty() {
        "(no named agents)".to_string()
    } else {
        roster.join("\n")
    };
    let pool_note = if team.pool.iter().any(|s| s.enabled) {
        "Leave \"agent\" empty to let the executor pool pick the account."
    } else {
        "Every step must name an agent from the roster."
    };
    let ctx = context
        .map(|c| format!("\n\nConversation so far (summary):\n{c}"))
        .unwrap_or_default();
    let plan_prompt = format!(
        "Split the task below into 1-4 independent subtasks. Each subtask must be fully \
         self-contained: the executor sees only its own text, not this conversation. \
         Use a single step when the task is simple.\n{pool_note}\n\
         Reply with ONLY a JSON array like [{{\"agent\":\"name\",\"task\":\"what to do\"}}].\n\n\
         Roster:\n{roster}{ctx}\n\nTask:\n{task}"
    );
    let _ = ui.send((lane, RunEvent::Label("planner".into())));
    let out = core
        .run(
            lane,
            team.planner.clone(),
            user_req(None, plan_prompt),
            cancel,
            ui,
        )
        .await
        .map_err(|_| "planning failed".to_string())?;
    // An unparseable plan degrades to one step with the original task.
    Ok(parse_plan(&out.text).unwrap_or_else(|| {
        vec![Step {
            agent: String::new(),
            task: task.to_string(),
        }]
    }))
}

/// Run steps in parallel on lanes `base + 1 + i`. Returns `(label, result)` in step order.
pub async fn execute(
    core: &Core,
    team: &Team,
    steps: Vec<Step>,
    base: LaneId,
    cancel: CancellationToken,
    ui: LaneTx,
) -> Vec<(String, Result<RunOutput, String>)> {
    let pool_steps = steps
        .iter()
        .filter(|s| resolve_agent(&team.agents, &s.agent).is_none())
        .count();
    let mut pool_chains = balance::assign(
        pool_steps,
        &team.pool,
        &team.accounts,
        &team.usage,
        team.now,
        &team.policy,
    )
    .into_iter();

    let mut set = tokio::task::JoinSet::new();
    for (i, s) in steps.into_iter().enumerate() {
        let (core, ui, cancel) = (core.clone(), ui.clone(), cancel.clone());
        let (chain, req, label) = match resolve_agent(&team.agents, &s.agent) {
            Some(a) => (
                chain_for(a, &team.accounts),
                agent_req(a, s.task.clone()),
                format!("@{}", a.name),
            ),
            None => {
                let chain = pool_chains
                    .next()
                    .filter(|c| !c.is_empty())
                    .unwrap_or_else(|| team.planner.clone());
                let who = chain.first().map(|a| a.name.clone()).unwrap_or_default();
                (
                    chain,
                    user_req(Some(EXECUTOR_SYSTEM.into()), s.task.clone()),
                    format!("exec:{who}"),
                )
            }
        };
        let lane = base + 1 + i as u64;
        let _ = ui.send((lane, RunEvent::Label(label.clone())));
        set.spawn(async move { (i, label, core.run(lane, chain, req, cancel, ui).await) });
    }
    let mut results = Vec::new();
    while let Some(r) = set.join_next().await {
        if let Ok(r) = r {
            results.push(r);
        }
    }
    results.sort_by_key(|(i, ..)| *i);
    results.into_iter().map(|(_, l, r)| (l, r)).collect()
}

/// Merge step results into one answer. A single result is returned as is (no extra request).
pub async fn synthesize(
    core: &Core,
    team: &Team,
    task: &str,
    mut results: Vec<(String, Result<RunOutput, String>)>,
    lane: LaneId,
    cancel: CancellationToken,
    ui: LaneTx,
) -> Result<RunOutput, String> {
    if results.len() == 1 {
        return results.remove(0).1;
    }
    let mut digest = String::new();
    for (who, r) in &results {
        match r {
            Ok(o) => digest.push_str(&format!(
                "## {who} (via {})\n{}\n\n",
                o.account,
                truncate_chars(o.text.trim(), SYNTH_RESULT_CAP)
            )),
            Err(e) => digest.push_str(&format!("## {who}\n[failed: {e}]\n\n")),
        }
    }
    let synth_prompt = format!(
        "Original task:\n{task}\n\nResults from the team:\n\n{digest}\nWrite one coherent final answer that merges these results."
    );
    let chain = if team.synth.is_empty() {
        team.planner.clone()
    } else {
        team.synth.clone()
    };
    let _ = ui.send((lane, RunEvent::Label("synthesis".into())));
    core.run(lane, chain, user_req(None, synth_prompt), cancel, ui)
        .await
}

pub struct OrchestrationResult {
    pub final_text: Result<RunOutput, String>,
}

/// Plan → parallel workers → synthesis, without a review step (`/orchestrate`).
///
/// Lanes: `base` = planner, `base+1..=n` = workers, `base+n+1` = synthesizer
/// (worker count is only known after planning, so the UI creates lanes on first event).
pub async fn orchestrate(
    core: &Core,
    team: Team,
    task: String,
    context: Option<String>,
    base: LaneId,
    cancel: CancellationToken,
    ui: LaneTx,
) -> OrchestrationResult {
    match plan(
        core,
        &team,
        &task,
        context.as_deref(),
        base,
        cancel.clone(),
        ui.clone(),
    )
    .await
    {
        Ok(steps) => run_steps(core, &team, &task, steps, base, cancel, ui).await,
        Err(e) => OrchestrationResult { final_text: Err(e) },
    }
}

/// Execute (approved) steps and merge them.
pub async fn run_steps(
    core: &Core,
    team: &Team,
    task: &str,
    steps: Vec<Step>,
    base: LaneId,
    cancel: CancellationToken,
    ui: LaneTx,
) -> OrchestrationResult {
    let results = execute(core, team, steps, base, cancel.clone(), ui.clone()).await;
    let n = results.len() as u64;
    let final_text = synthesize(core, team, task, results, base + n + 1, cancel, ui).await;
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

    #[test]
    fn plan_without_agent_goes_to_pool() {
        let p = parse_plan(r#"[{"task":"summarize"}]"#).unwrap();
        assert_eq!(p[0].agent, "");
        assert!(resolve_agent(&[], &p[0].agent).is_none());
    }

    #[test]
    fn truncation_is_char_safe() {
        assert_eq!(truncate_chars("abc", 5), "abc");
        assert!(truncate_chars("ééééé", 2).starts_with("éé\n"));
    }
}
