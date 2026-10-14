//! Runs requests across accounts: failover, budgets, usage accounting.

pub mod balance;
pub mod compact;
pub mod modes;

use std::sync::Arc;
use std::time::Instant;

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
}

pub type LaneId = u64;
pub type LaneTx = mpsc::UnboundedSender<(LaneId, RunEvent)>;

#[derive(Clone)]
pub struct Core {
    pub store: Store,
    pub vault: Arc<Vault>,
    pub prices: Arc<PriceTable>,
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
            let (tx, mut rx) = mpsc::channel(128);
            let (a2, r2, c2) = (acct.clone(), req.clone(), cancel.clone());
            let task = tokio::spawn(async move { providers::stream_chat(&a2, r2, tx, c2).await });

            let mut text = String::new();
            let mut outcome: Option<
                Result<(providers::TokenUsage, String), providers::ProviderError>,
            > = None;
            while let Some(ev) = rx.recv().await {
                match ev {
                    ProviderEvent::Delta(d) => {
                        text.push_str(&d);
                        send(RunEvent::Delta(d));
                    }
                    ProviderEvent::Note(n) => send(RunEvent::Note(n)),
                    ProviderEvent::Done { usage, model } => outcome = Some(Ok((usage, model))),
                    ProviderEvent::Error(e) => outcome = Some(Err(e)),
                }
            }
            let _ = task.await;
            let latency_ms = started.elapsed().as_millis() as u64;

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
                    if e.failover && text.is_empty() {
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
