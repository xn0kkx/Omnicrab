//! Runs requests across accounts: failover, budgets, usage accounting.

pub mod balance;
pub mod compact;
pub mod lead;
pub mod modes;
pub mod questions;

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::crypto::Vault;
use crate::model::{Account, RateLimits, UsageEvent, UsageStatus};
use crate::providers::{self, ChatRequest, ProviderEvent};
use crate::store::Store;
use crate::usage::{PriceTable, start_of_month};

/// Progress of one logical request ("lane") as seen by the UI.
#[derive(Clone, Debug)]
pub enum RunEvent {
    /// Human label for the lane (e.g. "planner"), sent by multi-step modes.
    Label(String),
    /// The lane exists but has not started (shown with its task while it waits).
    Queued {
        label: String,
        task: String,
    },
    Started {
        account: String,
    },
    Delta(String),
    Note(String),
    Failover {
        from: String,
        reason: String,
    },
    Finished {
        account: String,
        model: String,
        input: u64,
        output: u64,
        cost: f64,
    },
    Failed(String),
    Cancelled,
    /// The lane starts over: clear the text shown so far (`reason` is for the note).
    Reset {
        reason: String,
    },
    /// The final answer of a multi-step run, shown as its own message.
    Final {
        account: String,
        text: String,
    },
    /// A line from the orchestrator itself (lead problems, what changed on disk).
    Info {
        who: String,
        text: String,
    },
}

pub type LaneId = u64;
pub type LaneTx = mpsc::UnboundedSender<(LaneId, RunEvent)>;

/// How long a run may stay silent before omnicrab says so, and before it gives up.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub warn_after: Duration,
    pub kill_after: Duration,
}

impl Limits {
    /// Defaults, overridable with `OMNICRAB_IDLE_WARN_SECS` / `OMNICRAB_IDLE_KILL_SECS`.
    pub fn from_env() -> Self {
        let secs = |k: &str, d: Duration| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(d)
        };
        let d = Self::default();
        Self {
            warn_after: secs("OMNICRAB_IDLE_WARN_SECS", d.warn_after),
            kill_after: secs("OMNICRAB_IDLE_KILL_SECS", d.kill_after),
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            warn_after: Duration::from_secs(90),
            kill_after: Duration::from_secs(600),
        }
    }
}

#[derive(Clone)]
pub struct Core {
    pub store: Store,
    pub vault: Arc<Vault>,
    pub prices: Arc<PriceTable>,
    pub limits: Limits,
}

#[derive(Clone, Debug)]
pub struct RunOutput {
    pub text: String,
    pub account: String,
}

impl Core {
    /// Try `chain` in order until one account answers. Records usage for every attempt.
    pub async fn run(
        &self,
        lane: LaneId,
        chain: Vec<Account>,
        req: ChatRequest,
        cancel: CancellationToken,
        ui: LaneTx,
    ) -> Result<RunOutput, String> {
        let send = |e: RunEvent| {
            let _ = ui.send((lane, e));
        };
        let month_spend = self.month_spend().await;
        let mut last_err = String::from("no usable account");

        for acct in chain {
            if !acct.enabled {
                continue;
            }
            if let Some(b) = acct.budget_usd_month
                && month_spend.get(&acct.id).copied().unwrap_or(0.0) >= b
            {
                last_err = format!("{}: monthly budget ${b:.2} reached", acct.name);
                send(RunEvent::Failover {
                    from: acct.name.clone(),
                    reason: "budget reached".into(),
                });
                continue;
            }
            send(RunEvent::Started {
                account: acct.name.clone(),
            });
            let started = Instant::now();
            tracing::info!(lane, account = %acct.name, model = %acct.model, "run started");
            let (tx, mut rx) = mpsc::channel(128);
            // Own token, so the watchdog can stop this attempt without cancelling the whole run.
            let local = cancel.child_token();
            let (a2, r2, c2) = (acct.clone(), req.clone(), local.clone());
            let task = tokio::spawn(async move { providers::stream_chat(&a2, r2, tx, c2).await });

            let mut text = String::new();
            let mut outcome: Option<
                Result<(providers::TokenUsage, String), providers::ProviderError>,
            > = None;
            // Structured answers (JSON for the orchestrator) are not shown as they stream.
            let show_text = req.json_schema.is_none();
            let (mut idle, mut warned) = (Duration::ZERO, false);
            let tick = (self.limits.warn_after / 4)
                .clamp(Duration::from_millis(10), Duration::from_secs(5));
            loop {
                let ev = match tokio::time::timeout(tick, rx.recv()).await {
                    Ok(Some(ev)) => ev,
                    Ok(None) => break,
                    Err(_) => {
                        idle += tick;
                        if idle >= self.limits.kill_after {
                            tracing::warn!(lane, account = %acct.name, "no output, stopping");
                            local.cancel();
                            outcome = Some(Err(providers::ProviderError::new(
                                format!("no output for {}s (timed out)", idle.as_secs()),
                                true,
                            )));
                            break;
                        }
                        if idle >= self.limits.warn_after && !warned {
                            warned = true;
                            send(RunEvent::Note(format!(
                                "no output for {}s (still running)",
                                idle.as_secs()
                            )));
                        }
                        continue;
                    }
                };
                idle = Duration::ZERO;
                warned = false;
                match ev {
                    ProviderEvent::Delta(d) => {
                        text.push_str(&d);
                        if show_text {
                            send(RunEvent::Delta(d));
                        }
                    }
                    ProviderEvent::Note(n) => send(RunEvent::Note(n)),
                    ProviderEvent::Reset => {
                        text.clear();
                        send(RunEvent::Reset {
                            reason: "provider restarted".into(),
                        });
                    }
                    ProviderEvent::Done { usage, model } => outcome = Some(Ok((usage, model))),
                    ProviderEvent::Error(e) => outcome = Some(Err(e)),
                }
            }
            let _ = task.await;
            let latency_ms = started.elapsed().as_millis() as u64;
            tracing::info!(
                lane,
                account = %acct.name,
                ms = latency_ms,
                chars = text.len(),
                result = match &outcome {
                    None => "cancelled".to_string(),
                    Some(Ok(_)) => "ok".to_string(),
                    Some(Err(e)) => format!("error: {}", e.message.lines().next().unwrap_or("")),
                },
                "run finished"
            );

            match outcome {
                None => {
                    self.record(&acct, &req, 0, 0, 0.0, latency_ms, UsageStatus::Cancelled)
                        .await;
                    send(RunEvent::Cancelled);
                    return Err("cancelled".into());
                }
                Some(Ok((u, model))) => {
                    let limits = u.rate_limits.clone();
                    let cost = u
                        .reported_cost_usd
                        .unwrap_or_else(|| self.prices.cost(&acct, &model, u.input, u.output));
                    self.record_model(
                        &acct,
                        &model,
                        u.input,
                        u.output,
                        cost,
                        latency_ms,
                        UsageStatus::Ok,
                        limits,
                    )
                    .await;
                    send(RunEvent::Finished {
                        account: acct.name.clone(),
                        model,
                        input: u.input,
                        output: u.output,
                        cost,
                    });
                    return Ok(RunOutput {
                        text,
                        account: acct.name,
                    });
                }
                Some(Err(e)) => {
                    let status = if e.failover {
                        UsageStatus::FailedOver
                    } else {
                        UsageStatus::Error
                    };
                    self.record(&acct, &req, 0, 0, 0.0, latency_ms, status)
                        .await;
                    last_err = format!("{}: {}", acct.name, e.message);
                    // Only fail over if nothing was shown to the user yet.
                    if e.failover && (text.is_empty() || req.restart_on_partial) {
                        if !text.is_empty() {
                            send(RunEvent::Reset {
                                reason: e.message.clone(),
                            });
                        }
                        send(RunEvent::Failover {
                            from: acct.name.clone(),
                            reason: e.message,
                        });
                        continue;
                    }
                    send(RunEvent::Failed(last_err.clone()));
                    return Err(last_err);
                }
            }
        }
        send(RunEvent::Failed(last_err.clone()));
        Err(last_err)
    }

    /// Sends a minimal prompt to one account so its usage (and, for Claude
    /// CLI accounts, its plan limits) gets recorded.
    pub async fn probe(&self, acct: Account) -> Result<(), String> {
        let req = ChatRequest {
            system: None,
            turns: vec![providers::ChatTurn {
                role: crate::model::Role::User,
                content: "Reply with the single word: ok".into(),
            }],
            model: None,
            restart_on_partial: false,
            workspace: None,
            json_schema: None,
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        self.run(0, vec![acct], req, CancellationToken::new(), tx)
            .await
            .map(|_| ())
    }

    async fn month_spend(&self) -> std::collections::HashMap<uuid::Uuid, f64> {
        let events = self
            .store
            .load_usage(&self.vault, Some(start_of_month(Utc::now())))
            .await
            .unwrap_or_default();
        let mut m = std::collections::HashMap::new();
        for e in events {
            *m.entry(e.account_id).or_insert(0.0) += e.cost_usd;
        }
        m
    }

    #[allow(clippy::too_many_arguments)]
    async fn record(
        &self,
        a: &Account,
        req: &ChatRequest,
        i: u64,
        o: u64,
        cost: f64,
        ms: u64,
        st: UsageStatus,
    ) {
        let model = req
            .model
            .clone()
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| a.model.clone());
        self.record_model(a, &model, i, o, cost, ms, st, None).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn record_model(
        &self,
        a: &Account,
        model: &str,
        i: u64,
        o: u64,
        cost: f64,
        ms: u64,
        st: UsageStatus,
        rate_limits: Option<RateLimits>,
    ) {
        let ev = UsageEvent {
            model: model.to_string(),
            input_tokens: i,
            output_tokens: o,
            cost_usd: cost,
            latency_ms: ms,
            status: st,
            rate_limits,
            at: Utc::now(),
            account_id: a.id,
        };
        if let Err(e) = self.store.record_usage(&self.vault, a.id, &ev).await {
            tracing::warn!("failed to record usage: {e:#}");
        }
    }
}
