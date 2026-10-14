//! Multi-account modes: fan-out and plan → workers → synthesize.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::questions::{self, Question};
use super::{Core, LaneId, LaneTx, RunEvent, RunOutput, balance};
use crate::model::{Access, Account, Agent, BalancePolicy, Role, Slot, Workspace};
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
        restart_on_partial: true,
        workspace: None,
        json_schema: None,
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
    /// `"read"` keeps the worker from changing files; anything else lets it write
    /// (when the team has a writable workspace).
    #[serde(default)]
    pub access: String,
}

impl Step {
    pub fn access(&self) -> Access {
        if self.access.trim().eq_ignore_ascii_case("read") {
            Access::Read
        } else {
            Access::Write
        }
    }
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

/// What the planner came back with.
#[derive(Debug, PartialEq)]
pub enum PlannerReply {
    Steps(Vec<Step>),
    /// It needs the user to decide something before it can plan.
    Questions(Vec<Question>),
}

/// Questions win over steps; unparseable output degrades to a single step with `task`.
pub fn parse_planner_reply(text: &str, task: &str) -> PlannerReply {
    if let Some(q) = questions::find_questions(text) {
        return PlannerReply::Questions(q);
    }
    PlannerReply::Steps(parse_plan(text).unwrap_or_else(|| {
        vec![Step {
            agent: String::new(),
            task: task.to_string(),
            access: String::new(),
        }]
    }))
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
        restart_on_partial: true,
        workspace: None,
        json_schema: None,
    }
}

/// Everything the multi-step modes need to pick accounts.
#[derive(Clone)]
pub struct Team {
    pub agents: Vec<Agent>,
    pub accounts: Vec<Account>,
    /// Ordered chain for planning (and for synthesis when `synth` is empty).
    pub planner: Vec<Account>,
    /// The chain that leads a run: the model the user picked, else the planner slots.
    pub lead: Vec<Account>,
    pub synth: Vec<Account>,
    /// Executor slots; steps without a named agent are spread over these.
    pub pool: Vec<Slot>,
    /// Cheap slots that read the project and docs before the lead plans.
    pub scouts: Vec<Slot>,
    pub project: Option<crate::project::ProjectDir>,
    pub usage: HashMap<Uuid, AccountUsage>,
    pub now: DateTime<Utc>,
    pub policy: BalancePolicy,
    /// Project directory agents work in (`None` = temp dir, web tools only).
    pub workspace: Option<std::path::PathBuf>,
    pub read_only: bool,
    /// Decrypted `.omni` instructions and memory, passed to every agent.
    pub project_context: Option<String>,
}

/// `system` followed by the project notes, if there are any.
fn with_context(system: Option<String>, ctx: Option<&str>) -> Option<String> {
    match (system, ctx) {
        (s, None) => s,
        (None, Some(c)) => Some(c.to_string()),
        (Some(s), Some(c)) => Some(format!("{s}\n\n{c}")),
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}\n[... truncated]")
}

/// Ask the planner for steps (or, when `allow_questions`, for clarifications).
/// `context` is an optional summary of the conversation.
///
/// Fixed instructions and the sorted roster come first and the task last, so the
/// shared prefix stays identical between requests and can be cached.
#[allow(clippy::too_many_arguments)]
pub async fn plan(
    core: &Core,
    team: &Team,
    task: &str,
    context: Option<&str>,
    allow_questions: bool,
    lane: LaneId,
    cancel: CancellationToken,
    ui: LaneTx,
) -> Result<PlannerReply, String> {
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
    let ws_note = match (&team.workspace, team.read_only) {
        (Some(_), false) => {
            "\nExecutors work inside the user's project and can read and edit files and run \
             commands there: name files and the exact change in each step."
        }
        (Some(_), true) => "\nExecutors can read the user's project but not change it.",
        _ => "",
    };
    let ctx = context
        .map(|c| format!("\n\nConversation so far (summary):\n{c}"))
        .unwrap_or_default();
    let ctx = match &team.project_context {
        Some(p) => format!("\n\n{p}{ctx}"),
        None => ctx,
    };
    let ask_note = if allow_questions {
        "\nIf a requirement is missing and guessing would likely waste work, reply instead with \
         ONLY {\"questions\":[{\"header\":\"short\",\"question\":\"...?\",\"multi\":false,\
         \"options\":[{\"label\":\"...\",\"description\":\"...\"}]}]} (at most 4 questions, \
         2-4 options each; the user can also type their own answer). Do not ask what you can \
         decide yourself or what the task already states."
    } else {
        ""
    };
    let plan_prompt = format!(
        "Split the task below into 1-4 independent subtasks. Each subtask must be fully \
         self-contained: the executor sees only its own text, not this conversation. \
         Use a single step when the task is simple.\n{pool_note}{ws_note}\n\
         Reply with ONLY a JSON array like [{{\"agent\":\"name\",\"task\":\"what to do\"}}].{ask_note}\n\n\
         Roster:\n{roster}{ctx}\n\nTask:\n{task}"
    );
    let _ = ui.send((lane, RunEvent::Label("planner".into())));
    let mut req = user_req(None, plan_prompt);
    req.workspace = workspace_for(team, Access::Read);
    let out = core
        .run(lane, team.planner.clone(), req, cancel, ui)
        .await
        .map_err(|_| "planning failed".to_string())?;
    let reply = parse_planner_reply(&out.text, task);
    Ok(match reply {
        // Questions without a way to ask them: plan with the task as given.
        PlannerReply::Questions(_) if !allow_questions => PlannerReply::Steps(vec![Step {
            agent: String::new(),
            task: task.to_string(),
            access: String::new(),
        }]),
        r => r,
    })
}

/// Longest result text handed to the next step when steps run one after another.
const HANDOFF_CAP: usize = 1_500;

/// The directory agents may work in, with the given access (read-only when asked to).
pub(super) fn workspace_for(team: &Team, access: Access) -> Option<Workspace> {
    team.workspace.as_ref().map(|root| Workspace {
        root: root.clone(),
        access: if team.read_only { Access::Read } else { access },
    })
}

/// Steps that can change files run one after another so they do not trample each other.
fn sequential(team: &Team, steps: &[Step]) -> bool {
    team.workspace.is_some() && !team.read_only && steps.iter().any(|s| s.access() == Access::Write)
}

/// Run steps on lanes `base + 1 + i`: in parallel, or in order when they can edit files.
/// Returns `(label, result)` in step order.
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

    // Show every lane (and the synthesis) right away, with the task each one will do.
    for (i, s) in steps.iter().enumerate() {
        let label = match resolve_agent(&team.agents, &s.agent) {
            Some(a) => format!("@{}", a.name),
            None => "executor".to_string(),
        };
        let _ = ui.send((
            base + 1 + i as u64,
            RunEvent::Queued {
                label,
                task: s.task.clone(),
            },
        ));
    }

    // (lane, label, chain, request) for every step.
    let mut jobs = Vec::new();
    for (i, s) in steps.iter().enumerate() {
        let (chain, mut req, label) = match resolve_agent(&team.agents, &s.agent) {
            Some(a) => (
                chain_for(a, &team.accounts),
                agent_req(a, s.task.clone()),
                format!("@{}", a.name),
            ),
            None => {
                let chain = pool_chains
                    .next()
                    .filter(|c| !c.is_empty())
                    .unwrap_or_else(|| team.lead.clone());
                let who = chain.first().map(|a| a.name.clone()).unwrap_or_default();
                (
                    chain,
                    user_req(Some(EXECUTOR_SYSTEM.into()), s.task.clone()),
                    format!("exec:{who}"),
                )
            }
        };
        req.system = with_context(req.system.take(), team.project_context.as_deref());
        req.workspace = workspace_for(team, s.access());
        jobs.push((base + 1 + i as u64, label, chain, req));
    }

    if sequential(team, &steps) {
        let mut results: Vec<(String, Result<RunOutput, String>)> = Vec::new();
        for (lane, label, chain, mut req) in jobs {
            if cancel.is_cancelled() {
                break;
            }
            let _ = ui.send((lane, RunEvent::Label(label.clone())));
            if let Some(prev) = handoff(&results)
                && let Some(t) = req.turns.last_mut()
            {
                t.content.push_str(&prev);
            }
            let r = core.run(lane, chain, req, cancel.clone(), ui.clone()).await;
            results.push((label, r));
        }
        return results;
    }

    let mut set = tokio::task::JoinSet::new();
    for (i, (lane, label, chain, req)) in jobs.into_iter().enumerate() {
        let (core, ui, cancel) = (core.clone(), ui.clone(), cancel.clone());
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

/// What earlier steps reported, appended to the next step's task.
fn handoff(done: &[(String, Result<RunOutput, String>)]) -> Option<String> {
    if done.is_empty() {
        return None;
    }
    let mut s =
        String::from("\n\nEarlier steps already done in this project (do not repeat them):\n");
    for (who, r) in done {
        match r {
            Ok(o) => s.push_str(&format!(
                "- {who}: {}\n",
                truncate_chars(o.text.trim(), HANDOFF_CAP)
            )),
            Err(e) => s.push_str(&format!(
                "- {who}: failed ({})\n",
                e.lines().next().unwrap_or("")
            )),
        }
    }
    Some(s)
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
    // The planner chain backs up the synthesizers, so a failing one does not lose the work.
    let mut chain = team.synth.clone();
    for a in &team.planner {
        if !chain.iter().any(|c| c.id == a.id) {
            chain.push(a.clone());
        }
    }
    let _ = ui.send((lane, RunEvent::Label("synthesis".into())));
    match core
        .run(
            lane,
            chain,
            user_req(None, synth_prompt),
            cancel,
            ui.clone(),
        )
        .await
    {
        Ok(o) => Ok(o),
        Err(e) if e == "cancelled" => Err(e),
        Err(e) => {
            // Nothing could merge the results: hand them over unmerged.
            let text = format!(
                "[synthesis failed: {}]\n\n{}",
                e.lines().next().unwrap_or(""),
                digest.trim_end()
            );
            let _ = ui.send((lane, RunEvent::Delta(format!("\n{text}"))));
            Ok(RunOutput {
                text,
                account: "unmerged".into(),
            })
        }
    }
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
        false,
        base,
        cancel.clone(),
        ui.clone(),
    )
    .await
    {
        Ok(PlannerReply::Steps(steps)) => {
            run_steps(core, &team, &task, steps, base, cancel, ui).await
        }
        Ok(PlannerReply::Questions(_)) => OrchestrationResult {
            final_text: Err("planner asked questions".into()),
        },
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
    if steps.len() > 1 {
        let _ = ui.send((
            base + steps.len() as u64 + 1,
            RunEvent::Queued {
                label: "synthesis".into(),
                task: format!("waiting for {} steps", steps.len()),
            },
        ));
    }
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
                task: "write".into(),
                access: String::new()
            }
        );
        assert!(parse_plan("no json here").is_none());
        assert!(parse_plan("[]").is_none());
        assert!(parse_plan("[not json]").is_none());
    }

    #[test]
    fn planner_reply_kinds() {
        let q = r#"{"questions":[{"question":"Which?","options":[{"label":"a"},{"label":"b"}]}]}"#;
        assert!(matches!(
            parse_planner_reply(q, "t"),
            PlannerReply::Questions(_)
        ));
        assert!(matches!(
            parse_planner_reply(r#"[{"task":"x"}]"#, "t"),
            PlannerReply::Steps(s) if s[0].task == "x"
        ));
        assert!(matches!(
            parse_planner_reply("whatever", "t"),
            PlannerReply::Steps(s) if s[0].task == "t"
        ));
    }

    #[test]
    fn project_context_is_appended() {
        assert_eq!(with_context(None, None), None);
        assert_eq!(with_context(Some("a".into()), None).as_deref(), Some("a"));
        assert_eq!(with_context(None, Some("c")).as_deref(), Some("c"));
        assert_eq!(
            with_context(Some("a".into()), Some("c")).as_deref(),
            Some("a\n\nc")
        );
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
