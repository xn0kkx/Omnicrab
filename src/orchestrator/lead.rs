//! The lead loop: a Hermes-style way of running a task.
//!
//! Cheap *scouts* read the project and its docs first. The *lead* (the model the user
//! picked) then plans, delegates work to cheap workers, reads their reports together with
//! what really changed on disk, and decides what happens next: ask the user, delegate again,
//! or finish. The lead has no tools: every turn is one JSON object (`say` + `actions`).

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::modes::{self, Step, Team};
use super::questions::{self, Question};
use super::{Core, LaneId, LaneTx, RunEvent, RunOutput, balance};
use crate::model::Access;
use crate::project;

/// The lead may take this many turns before it has to finish.
pub const MAX_TURNS: usize = 8;
/// Longest worker or scout report kept in the lead's log.
const REPORT_CAP: usize = 2_500;
const DIGEST_CAP: usize = 6_000;
/// The log handed to the lead is trimmed (oldest reports first) beyond this size.
const LOG_CAP: usize = 24_000;

const PROTOCOL: &str = "You lead a team of AI agents working in the user's project. You have NO tools: \
you plan, delegate, read the reports and decide. Reply with ONE JSON object every turn: \
{\"say\": \"short status for the user\", \"actions\": [...]}.\n\
\n\
Actions (at most one control action per turn: ask, plan, delegate, scout or finish; remember may be added):\n\
- ask: {\"type\":\"ask\",\"questions\":[{\"header\":\"short\",\"question\":\"...?\",\"multi\":false,\
\"options\":[{\"label\":\"...\",\"description\":\"...\"}]}]} - only when you cannot go on without a user decision \
(max 4 questions, 2-4 options each; the user can also type an answer).\n\
- plan: {\"type\":\"plan\",\"steps\":[{\"agent\":\"\",\"task\":\"...\",\"access\":\"write\"}]} - your first move for any task \
that changes files or needs several steps. 1-6 self-contained steps: a worker sees only its task text and the \
project notes. The user may review the plan; once approved the steps run and you get the reports.\n\
- delegate: same steps; runs without review. Use it for follow-ups, fixes and extra checks after reports.\n\
- scout: {\"type\":\"scout\",\"topics\":[\"question about the code or docs\"]} - cheap readers investigate and report.\n\
- remember: {\"type\":\"remember\",\"note\":\"...\"} - a durable fact about the project for future runs.\n\
- finish: {\"type\":\"finish\",\"answer\":\"message for the user: what was done, what changed, what is left\"}.\n\
\n\
Rules: workers edit files themselves, so never paste whole files into tasks; name files and the exact change. \
Steps with access \"write\" run one after another in order, \"read\" steps run in parallel. Use an agent name only \
from the roster; leave \"agent\" empty for the executor pool. Before finishing compare the reports with the \
working-tree changes; if a step said it edited files but nothing changed, delegate it again or say so. \
Use the fewest steps that do the job. Write \"say\" and \"answer\" in the language the user wrote in.";

const SCOUT_SYSTEM: &str = "You are a scout. Read the project and answer from what the files really say. \
Facts only, with file paths and names; no plans, no code changes. At most 600 words.";

const OVERVIEW_PROMPT: &str = "Survey this project: language and stack, directory layout, how to build, run and \
test it, conventions and anything in README, docs/, CLAUDE.md, AGENTS.md or similar that a developer must know.";

/// What the lead can ask the UI for while the loop runs.
pub enum LeadMsg {
    Ask {
        questions: Vec<Question>,
        reply: oneshot::Sender<Option<Vec<(String, String)>>>,
    },
    Review {
        steps: Vec<Step>,
        reply: oneshot::Sender<Decision>,
    },
}

pub enum Decision {
    Approve(Vec<Step>),
    /// Plan again with this feedback.
    Revise(String),
    Discard,
}

pub type Notify = Arc<dyn Fn(LeadMsg) + Send + Sync>;

#[derive(Debug, PartialEq)]
pub enum Action {
    Ask(Vec<Question>),
    Plan(Vec<Step>),
    Delegate(Vec<Step>),
    Scout(Vec<String>),
    Remember(String),
    Finish(String),
}

#[derive(Debug, Default, PartialEq)]
pub struct Turn {
    pub say: String,
    pub actions: Vec<Action>,
}

/// One action as the model writes it: every field optional, unknown ones ignored.
#[derive(Deserialize, Default)]
struct RawAction {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    questions: Value,
    #[serde(default)]
    steps: Vec<Step>,
    #[serde(default)]
    tasks: Vec<Step>,
    #[serde(default)]
    topics: Vec<String>,
    #[serde(default)]
    note: String,
    #[serde(default)]
    answer: String,
}

#[derive(Deserialize, Default)]
struct RawTurn {
    #[serde(default)]
    say: String,
    #[serde(default)]
    actions: Vec<RawAction>,
}

fn clean_steps(steps: Vec<Step>) -> Vec<Step> {
    steps
        .into_iter()
        .filter(|s| !s.task.trim().is_empty())
        .collect()
}

/// Parses the lead's reply (JSON, possibly inside prose or a code fence).
pub fn parse_turn(text: &str) -> Result<Turn, String> {
    let t = text.trim();
    let raw: RawTurn = serde_json::from_str(t)
        .or_else(|_| {
            let (s, e) = (t.find('{'), t.rfind('}'));
            match (s, e) {
                (Some(s), Some(e)) if e > s => serde_json::from_str(&t[s..=e]),
                _ => serde_json::from_str(t),
            }
        })
        .map_err(|e| format!("not a JSON object ({e})"))?;
    let mut actions = vec![];
    for a in raw.actions {
        match a.kind.as_str() {
            "ask" => match questions::parse_questions(&a.questions.to_string()) {
                Some(q) => actions.push(Action::Ask(q)),
                None => return Err("ask needs questions with 2-4 options each".into()),
            },
            "plan" | "delegate" => {
                let steps = clean_steps(if a.steps.is_empty() { a.tasks } else { a.steps });
                if steps.is_empty() {
                    return Err(format!("{} needs at least one step with a task", a.kind));
                }
                actions.push(if a.kind == "plan" {
                    Action::Plan(steps)
                } else {
                    Action::Delegate(steps)
                });
            }
            "scout" => {
                let topics: Vec<String> = a
                    .topics
                    .into_iter()
                    .filter(|t| !t.trim().is_empty())
                    .collect();
                if topics.is_empty() {
                    return Err("scout needs topics".into());
                }
                actions.push(Action::Scout(topics));
            }
            "remember" if !a.note.trim().is_empty() => actions.push(Action::Remember(a.note)),
            "finish" => actions.push(Action::Finish(a.answer)),
            other => return Err(format!("unknown action type '{other}'")),
        }
    }
    if actions.is_empty() && raw.say.trim().is_empty() {
        return Err("empty reply".into());
    }
    Ok(Turn {
        say: raw.say,
        actions,
    })
}

/// JSON schema handed to CLIs that can enforce it (Claude, agy).
pub fn schema() -> String {
    let step = json!({"type":"object","properties":{
        "agent":{"type":"string"},"task":{"type":"string"},
        "access":{"type":"string","enum":["write","read"]}},"required":["task"]});
    let question = json!({"type":"object","properties":{
        "header":{"type":"string"},"question":{"type":"string"},"multi":{"type":"boolean"},
        "options":{"type":"array","items":{"type":"object","properties":{
            "label":{"type":"string"},"description":{"type":"string"}},"required":["label"]}}},
        "required":["question","options"]});
    json!({"type":"object","properties":{
        "say":{"type":"string"},
        "actions":{"type":"array","items":{"type":"object","properties":{
            "type":{"type":"string","enum":["ask","plan","delegate","scout","remember","finish"]},
            "questions":{"type":"array","items":question},
            "steps":{"type":"array","items":step.clone()},
            "tasks":{"type":"array","items":step},
            "topics":{"type":"array","items":{"type":"string"}},
            "note":{"type":"string"},"answer":{"type":"string"}},
            "required":["type"]}}},
        "required":["actions"]})
    .to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    format!(
        "{}\n[... truncated]",
        s.chars().take(max).collect::<String>()
    )
}

fn roster(team: &Team) -> String {
    let mut agents: Vec<String> = team
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
    agents.sort();
    let agents = if agents.is_empty() {
        "(no named agents: leave \"agent\" empty)".to_string()
    } else {
        agents.join("\n")
    };
    let pool = if team.pool.iter().any(|s| s.enabled) {
        "Executor pool: cheap workers; an empty \"agent\" picks one."
    } else {
        "Executor pool: empty (steps without an agent run on the lead's own account)."
    };
    let ws = match (&team.workspace, team.read_only) {
        (Some(r), false) => format!(
            "Workspace: {} - workers can read and edit files and run commands there.",
            r.display()
        ),
        (Some(r), true) => format!(
            "Workspace: {} - read-only, nothing can be changed.",
            r.display()
        ),
        _ => "Workspace: none (workers only have web tools).".to_string(),
    };
    format!("Agents:\n{agents}\n{pool}\n{ws}")
}

/// The lead's whole input for one turn. The fixed parts come first, so a provider's
/// prompt cache can reuse them from turn to turn.
fn build_prompt(
    team: &Team,
    task: &str,
    summary: Option<&str>,
    digest: Option<&str>,
    log: &[String],
    turn: usize,
) -> String {
    let mut p = format!("# Task\n{task}\n\n# Team\n{}\n", roster(team));
    if let Some(s) = summary {
        p.push_str(&format!("\n# Conversation so far (summary)\n{s}\n"));
    }
    if let Some(c) = &team.project_context {
        p.push_str(&format!("\n# Project notes\n{c}\n"));
    }
    if let Some(d) = digest {
        p.push_str(&format!("\n# What the scouts found\n{d}\n"));
    }
    // Keep the newest entries when the log gets long.
    let mut kept: Vec<&String> = vec![];
    let mut size = 0;
    for e in log.iter().rev() {
        size += e.len();
        if size > LOG_CAP && !kept.is_empty() {
            break;
        }
        kept.push(e);
    }
    kept.reverse();
    if !log.is_empty() {
        p.push_str("\n# Log of this run\n");
        if kept.len() < log.len() {
            p.push_str("(older entries trimmed)\n");
        }
        for e in kept {
            p.push_str(&format!("{e}\n\n"));
        }
    }
    let left = MAX_TURNS - turn;
    p.push_str(&format!(
        "\n# Turn {turn} of {MAX_TURNS}\n{}",
        if left == 0 {
            "This is your LAST turn: finish now with what you have."
        } else {
            "Reply with the JSON object for this turn."
        }
    ));
    p
}

fn describe_steps(steps: &[Step]) -> String {
    steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            format!(
                "{}. [{}] {}{}",
                i + 1,
                if s.access() == Access::Read {
                    "read"
                } else {
                    "write"
                },
                if s.agent.is_empty() {
                    String::new()
                } else {
                    format!("@{}: ", s.agent)
                },
                s.task.lines().next().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Unknown agent names fall back to the pool; say so instead of doing it silently.
fn check_agents(team: &Team, steps: &mut [Step]) -> Vec<String> {
    let mut warnings = vec![];
    for s in steps {
        if !s.agent.trim().is_empty() && modes::resolve_agent(&team.agents, &s.agent).is_none() {
            warnings.push(format!(
                "agent '{}' does not exist, the executor pool takes that step",
                s.agent.trim()
            ));
            s.agent.clear();
        }
    }
    warnings
}

pub struct LeadResult {
    pub result: Result<RunOutput, String>,
    /// Every set of steps that ran, in order (saved with the plan).
    pub waves: Vec<Vec<Step>>,
}

/// Allocates lane ids for one run.
struct Lanes(LaneId);

impl Lanes {
    /// Reserves `n` lanes and returns the first.
    fn take(&mut self, n: u64) -> LaneId {
        let first = self.0;
        self.0 += n.max(1);
        first
    }
}

fn info(ui: &LaneTx, lane: LaneId, who: &str, text: impl Into<String>) {
    let _ = ui.send((
        lane,
        RunEvent::Info {
            who: who.into(),
            text: text.into(),
        },
    ));
}

fn finish_event(ui: &LaneTx, lane: LaneId, account: &str, text: &str) {
    let _ = ui.send((
        lane,
        RunEvent::Final {
            account: account.into(),
            text: text.into(),
        },
    ));
}

/// Asks the lead for its next turn; one repair attempt when the reply is unusable.
async fn ask_lead(
    core: &Core,
    team: &Team,
    prompt: &str,
    lane: LaneId,
    cancel: &CancellationToken,
    ui: &LaneTx,
) -> Result<(Turn, String), String> {
    let mut req = modes::user_req(Some(PROTOCOL.into()), prompt.to_string());
    req.json_schema = Some(schema());
    let _ = ui.send((lane, RunEvent::Label("lead".into())));
    let first = core
        .run(
            lane,
            team.lead.clone(),
            req.clone(),
            cancel.clone(),
            ui.clone(),
        )
        .await?;
    let err = match parse_turn(&first.text) {
        Ok(t) => return Ok((t, first.account)),
        Err(e) => e,
    };
    info(
        ui,
        lane,
        "lead",
        format!("the reply could not be used ({err}); asking again"),
    );
    req.turns[0].content = format!(
        "{prompt}\n\n---\nYour previous reply could not be used: {err}.\nPrevious reply:\n{}\n\n\
         Reply again with ONLY the JSON object.",
        truncate(&first.text, 3_000)
    );
    let second = core
        .run(lane, team.lead.clone(), req, cancel.clone(), ui.clone())
        .await?;
    match parse_turn(&second.text) {
        Ok(t) => Ok((t, second.account)),
        Err(e) => Err(format!(
            "the lead ({}) did not return a usable reply: {e}\n\n{}",
            second.account,
            truncate(&second.text, 1_500)
        )),
    }
}

/// Runs read-only scouts in parallel on the cheap slots. `(label, prompt)` per scout.
async fn run_scouts(
    core: &Core,
    team: &Team,
    jobs: Vec<(String, String)>,
    lanes: &mut Lanes,
    cancel: &CancellationToken,
    ui: &LaneTx,
) -> Vec<(String, Result<RunOutput, String>)> {
    let slots = if team.scouts.iter().any(|s| s.enabled) {
        &team.scouts
    } else {
        &team.pool
    };
    let mut chains = balance::assign(
        jobs.len(),
        slots,
        &team.accounts,
        &team.usage,
        team.now,
        &team.policy,
    )
    .into_iter();
    let first = lanes.take(jobs.len() as u64);
    let mut set = tokio::task::JoinSet::new();
    for (i, (label, prompt)) in jobs.into_iter().enumerate() {
        let lane = first + i as u64;
        let chain = chains
            .next()
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| team.lead.clone());
        let _ = ui.send((
            lane,
            RunEvent::Queued {
                label: format!("scout:{label}"),
                task: prompt.clone(),
            },
        ));
        let mut req = modes::user_req(Some(SCOUT_SYSTEM.into()), prompt);
        req.workspace = modes::workspace_for(team, Access::Read);
        let (core, ui, cancel) = (core.clone(), ui.clone(), cancel.clone());
        set.spawn(async move {
            let _ = ui.send((lane, RunEvent::Label(format!("scout:{label}"))));
            (i, label, core.run(lane, chain, req, cancel, ui).await)
        });
    }
    let mut out = vec![];
    while let Some(r) = set.join_next().await {
        if let Ok(r) = r {
            out.push(r);
        }
    }
    out.sort_by_key(|(i, ..)| *i);
    out.into_iter().map(|(_, l, r)| (l, r)).collect()
}

fn reports(results: &[(String, Result<RunOutput, String>)]) -> String {
    let mut s = String::new();
    for (who, r) in results {
        match r {
            Ok(o) => s.push_str(&format!(
                "## {who} (via {})\n{}\n\n",
                o.account,
                truncate(o.text.trim(), REPORT_CAP)
            )),
            Err(e) => s.push_str(&format!(
                "## {who}\n[failed: {}]\n\n",
                e.lines().next().unwrap_or("")
            )),
        }
    }
    s
}

/// Reads the docs and code before the lead plans. The project overview is cached in
/// `.omni` per commit; what matters for this task is looked up every time.
async fn scout_phase(
    core: &Core,
    team: &Team,
    task: &str,
    lanes: &mut Lanes,
    cancel: &CancellationToken,
    ui: &LaneTx,
) -> Option<String> {
    let root = team.workspace.clone()?;
    let head = project::git_head(&root);
    let cached = match (&team.project, &head) {
        (Some(p), Some(h)) => p.load_digest(&core.vault, h).ok().flatten(),
        _ => None,
    };
    let mut jobs = vec![];
    if cached.is_none() {
        jobs.push(("overview".to_string(), OVERVIEW_PROMPT.to_string()));
    }
    jobs.push((
        "task".to_string(),
        format!(
            "Find what matters for this task: the files, functions, docs and tests involved, how \
             they work today and what would have to change. Task: {task}"
        ),
    ));
    let results = run_scouts(core, team, jobs, lanes, cancel, ui).await;
    let mut parts = vec![];
    if let Some(c) = cached {
        parts.push(format!("## overview (cached)\n{c}"));
    }
    for (label, r) in results {
        if let Ok(o) = r {
            let text = truncate(o.text.trim(), DIGEST_CAP);
            if label == "overview"
                && let (Some(p), Some(h)) = (&team.project, &head)
            {
                let _ = p.save_digest(&core.vault, h, &text);
            }
            parts.push(format!("## {label} (via {})\n{text}", o.account));
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// Runs a set of steps and returns the report for the lead's log, with what changed on disk.
#[allow(clippy::too_many_arguments)]
async fn run_wave(
    core: &Core,
    team: &Team,
    steps: Vec<Step>,
    lanes: &mut Lanes,
    cancel: &CancellationToken,
    ui: &LaneTx,
    info_lane: LaneId,
) -> String {
    let base = lanes.take(steps.len() as u64 + 1);
    let wrote = steps.iter().any(|s| s.access() == Access::Write);
    let results = modes::execute(core, team, steps, base, cancel.clone(), ui.clone()).await;
    let mut report = reports(&results);
    if let Some(root) = team.workspace.clone() {
        let changes = tokio::task::spawn_blocking(move || project::git_changes(&root))
            .await
            .ok()
            .flatten();
        match changes {
            Some(c) if !c.is_empty() => {
                info(ui, info_lane, "changes", c.clone());
                report.push_str(&format!("Working tree now:\n{c}\n"));
            }
            Some(_) if wrote => {
                info(ui, info_lane, "changes", "no files changed on disk");
                report.push_str("Working tree now: NO files changed.\n");
            }
            _ => {}
        }
    }
    report
}

async fn await_reply<T>(rx: oneshot::Receiver<T>, cancel: &CancellationToken) -> Result<T, String> {
    tokio::select! {
        _ = cancel.cancelled() => Err("cancelled".to_string()),
        r = rx => r.map_err(|_| "cancelled".to_string()),
    }
}

/// Runs the lead loop for `task`. `review` asks the user to approve the first plan.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    core: &Core,
    team: &Team,
    task: &str,
    summary: Option<&str>,
    base: LaneId,
    review: bool,
    cancel: CancellationToken,
    ui: LaneTx,
    notify: Notify,
) -> LeadResult {
    let mut lanes = Lanes(base);
    let mut waves: Vec<Vec<Step>> = vec![];
    let info_lane = lanes.take(1);
    let fail = |waves: Vec<Vec<Step>>, e: String| LeadResult {
        result: Err(e),
        waves,
    };
    if team.lead.is_empty() {
        return fail(
            waves,
            "no model available to lead: pick one with /use or set a Planner".into(),
        );
    }

    let digest = if team.workspace.is_some() {
        scout_phase(core, team, task, &mut lanes, &cancel, &ui).await
    } else {
        None
    };

    let mut log: Vec<String> = vec![];
    let mut reviewed = !review;
    let mut last_say = String::new();
    for turn in 1..=MAX_TURNS {
        let prompt = build_prompt(team, task, summary, digest.as_deref(), &log, turn);
        let lane = lanes.take(1);
        let (reply, lead_account) = match ask_lead(core, team, &prompt, lane, &cancel, &ui).await {
            Ok(r) => r,
            Err(e) => {
                if e != "cancelled" {
                    info(&ui, info_lane, "lead", format!("stopped: {e}"));
                }
                return fail(waves, e);
            }
        };
        if !reply.say.trim().is_empty() {
            last_say = reply.say.clone();
        }
        let mut shown = reply.say.trim().to_string();
        // A reply without actions is a plain answer.
        if reply.actions.is_empty() {
            finish_event(&ui, info_lane, &lead_account, &reply.say);
            return LeadResult {
                result: Ok(RunOutput {
                    text: reply.say,
                    account: lead_account,
                }),
                waves,
            };
        }
        // `remember` runs right away; of the rest only the first (the control action) counts.
        let mut control: Option<Action> = None;
        for a in reply.actions {
            match a {
                Action::Remember(n) => {
                    if let Some(p) = &team.project
                        && p.remember(&core.vault, &n).is_ok()
                    {
                        shown.push_str(&format!("\n↳ remembered: {n}"));
                    }
                }
                other if control.is_none() => control = Some(other),
                _ => {}
            }
        }
        let Some(action) = control else {
            let _ = ui.send((lane, RunEvent::Delta(shown)));
            log.push(format!(
                "Turn {turn} (you): {last_say}\nNothing else was asked."
            ));
            continue;
        };
        match &action {
            Action::Ask(q) => shown.push_str(&format!("\n→ asks {} question(s)", q.len())),
            Action::Plan(s) => shown.push_str(&format!("\n→ plan\n{}", describe_steps(s))),
            Action::Delegate(s) => shown.push_str(&format!("\n→ delegates\n{}", describe_steps(s))),
            Action::Scout(t) => shown.push_str(&format!("\n→ scouts: {}", t.join("; "))),
            Action::Finish(_) => shown.push_str("\n→ finishes"),
            Action::Remember(_) => {}
        }
        let _ = ui.send((lane, RunEvent::Delta(shown)));
        log.push(format!(
            "Turn {turn} (you): {}",
            if reply.say.is_empty() {
                "(no comment)"
            } else {
                reply.say.trim()
            }
        ));

        match action {
            Action::Finish(answer) => {
                let text = if answer.trim().is_empty() {
                    last_say.clone()
                } else {
                    answer
                };
                finish_event(&ui, info_lane, &lead_account, &text);
                return LeadResult {
                    result: Ok(RunOutput {
                        text,
                        account: lead_account,
                    }),
                    waves,
                };
            }
            Action::Ask(questions) => {
                let (tx, rx) = oneshot::channel();
                notify(LeadMsg::Ask {
                    questions,
                    reply: tx,
                });
                match await_reply(rx, &cancel).await {
                    Err(e) => return fail(waves, e),
                    Ok(Some(answers)) => log.push(format!(
                        "The user answered:\n{}",
                        questions::format_answers(&answers)
                    )),
                    Ok(None) => log.push(
                        "The user skipped the questions: decide yourself and state your assumptions."
                            .into(),
                    ),
                }
            }
            Action::Plan(mut steps) | Action::Delegate(mut steps) => {
                for w in check_agents(team, &mut steps) {
                    info(&ui, info_lane, "lead", w);
                }
                // The first plan is the one the user reviews; later waves follow it.
                if !reviewed {
                    reviewed = true;
                    let (tx, rx) = oneshot::channel();
                    notify(LeadMsg::Review { steps, reply: tx });
                    match await_reply(rx, &cancel).await {
                        Err(e) => return fail(waves, e),
                        Ok(Decision::Discard) => return fail(waves, "plan discarded".into()),
                        Ok(Decision::Revise(fb)) => {
                            reviewed = false;
                            log.push(format!("The user asked you to revise the plan: {fb}"));
                            continue;
                        }
                        Ok(Decision::Approve(approved)) => steps = clean_steps(approved),
                    }
                    if steps.is_empty() {
                        return fail(waves, "plan discarded".into());
                    }
                }
                waves.push(steps.clone());
                let report = run_wave(core, team, steps, &mut lanes, &cancel, &ui, info_lane).await;
                if cancel.is_cancelled() {
                    return fail(waves, "cancelled".into());
                }
                log.push(format!("Reports from the workers:\n{report}"));
            }
            Action::Scout(topics) => {
                let jobs = topics
                    .into_iter()
                    .enumerate()
                    .map(|(i, t)| (format!("{}", i + 1), t))
                    .collect();
                let results = run_scouts(core, team, jobs, &mut lanes, &cancel, &ui).await;
                log.push(format!("Reports from the scouts:\n{}", reports(&results)));
            }
            Action::Remember(_) => {}
        }
    }
    // The lead never finished: hand over what it said last and what ran.
    let text = format!(
        "The lead used all {MAX_TURNS} turns without finishing.\n\n{}",
        if last_say.is_empty() {
            "(no final message)".to_string()
        } else {
            last_say
        }
    );
    finish_event(&ui, info_lane, "lead", &text);
    LeadResult {
        result: Ok(RunOutput {
            text,
            account: "lead".into(),
        }),
        waves,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_action() {
        let t = parse_turn(
            r#"```json
{"say":"ok","actions":[
 {"type":"remember","note":"uses sqlx"},
 {"type":"plan","steps":[{"agent":"","task":"edit a.rs","access":"write"},{"task":"  "}]},
 {"type":"delegate","tasks":[{"task":"check"}]},
 {"type":"scout","topics":["where is auth?"]},
 {"type":"ask","questions":[{"question":"Which?","options":[{"label":"a"},{"label":"b"}]}]},
 {"type":"finish","answer":"done"}]}
```"#,
        )
        .unwrap();
        assert_eq!(t.say, "ok");
        assert_eq!(t.actions.len(), 6);
        assert!(
            matches!(&t.actions[1], Action::Plan(s) if s.len() == 1 && s[0].task == "edit a.rs")
        );
        assert!(matches!(&t.actions[2], Action::Delegate(s) if s[0].access() == Access::Write));
        assert!(matches!(&t.actions[5], Action::Finish(a) if a == "done"));
    }

    #[test]
    fn rejects_unusable_replies() {
        assert!(parse_turn("I will now plan the work.").is_err());
        assert!(parse_turn(r#"{"actions":[{"type":"plan","steps":[]}]}"#).is_err());
        assert!(parse_turn(r#"{"actions":[{"type":"dance"}]}"#).is_err());
        assert!(parse_turn(r#"{"actions":[{"type":"ask","questions":[{"question":"x","options":[{"label":"a"}]}]}]}"#).is_err());
        assert!(parse_turn(r#"{"actions":[]}"#).is_err());
        // Only a message: it is the answer.
        let t = parse_turn(r#"{"say":"hello","actions":[]}"#).unwrap();
        assert!(t.actions.is_empty() && t.say == "hello");
    }

    #[test]
    fn schema_is_valid_json_with_every_action() {
        let v: Value = serde_json::from_str(&schema()).unwrap();
        let kinds = &v["properties"]["actions"]["items"]["properties"]["type"]["enum"];
        assert_eq!(kinds.as_array().unwrap().len(), 6);
    }

    #[test]
    fn log_is_trimmed_from_the_oldest_end() {
        let team = test_team();
        let log: Vec<String> = (0..5)
            .map(|i| format!("entry-{i} {}", "x".repeat(10_000)))
            .collect();
        let p = build_prompt(&team, "task", None, Some("digest"), &log, 3);
        assert!(p.contains("entry-4") && !p.contains("entry-0"));
        assert!(p.contains("older entries trimmed") && p.contains("# Turn 3 of 8"));
        let last = build_prompt(&team, "task", None, None, &[], MAX_TURNS);
        assert!(last.contains("LAST turn"));
    }

    #[test]
    fn unknown_agents_are_reported() {
        let team = test_team();
        let mut steps = vec![Step {
            agent: "verifier".into(),
            task: "t".into(),
            access: String::new(),
        }];
        let w = check_agents(&team, &mut steps);
        assert_eq!(w.len(), 1);
        assert!(steps[0].agent.is_empty());
    }

    pub(super) fn test_team() -> Team {
        Team {
            agents: vec![],
            accounts: vec![],
            planner: vec![],
            lead: vec![],
            synth: vec![],
            pool: vec![],
            scouts: vec![],
            project: None,
            usage: Default::default(),
            now: chrono::Utc::now(),
            policy: Default::default(),
            workspace: None,
            read_only: false,
            project_context: None,
        }
    }
}
