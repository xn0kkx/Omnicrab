//! Picks which account serves a role: the one with the most room left.
//!
//! Pure functions over the usage snapshot the UI already holds, so they are
//! cheap to call and easy to test. `Core::run` still handles failover on the
//! chain this produces.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::model::{Account, BalancePolicy, ProviderKind, Slot};
use crate::usage::{AccountUsage, LimitState};

/// Share of capacity assumed to be used by each step already assigned in a parallel batch.
const STEP_LOAD: f64 = 0.15;

/// Fraction (0..=1) of the account's capacity that is still free.
/// Unknown for providers that expose neither plan limits nor a budget.
pub fn headroom(a: &Account, usage: Option<&AccountUsage>, now: DateTime<Utc>) -> Option<f64> {
    if a.kind.is_local() {
        return Some(1.0);
    }
    if a.kind == ProviderKind::ClaudeCli
        && let Some((limits, _)) = usage.and_then(|u| u.limits.as_ref())
    {
        let used = [limits.five_hour, limits.seven_day]
            .into_iter()
            .flatten()
            .map(|w| match LimitState::of(&w, now) {
                LimitState::Active { used, .. } => used,
                LimitState::Reset => 0.0,
            })
            .fold(0.0, f64::max);
        return Some((1.0 - used).clamp(0.0, 1.0));
    }
    if let (Some(budget), Some(u)) = (a.budget_usd_month.filter(|b| *b > 0.0), usage) {
        return Some((1.0 - u.month_cost / budget).clamp(0.0, 1.0));
    }
    None
}

fn budget_exhausted(a: &Account, usage: Option<&AccountUsage>) -> bool {
    match (a.budget_usd_month, usage) {
        (Some(b), Some(u)) => u.month_cost >= b,
        _ => false,
    }
}

/// Accounts for `slots` (slot model/effort applied), best first.
///
/// * disabled slots/accounts and accounts over their monthly budget are dropped;
/// * accounts above `policy.skip_above` use go last but stay available for failover;
/// * `prefer` (the account used before) wins while it is not over the threshold,
///   which keeps prompt caches warm;
/// * `load` is virtual usage added by steps already assigned in this batch.
pub fn rank_with_load(
    slots: &[Slot],
    accounts: &[Account],
    usage: &HashMap<Uuid, AccountUsage>,
    now: DateTime<Utc>,
    policy: &BalancePolicy,
    prefer: Option<Uuid>,
    load: &HashMap<Uuid, f64>,
) -> Vec<Account> {
    struct Cand {
        acct: Account,
        score: f64,
        over: bool,
        local: bool,
        sticky: bool,
        today: u64,
    }
    let mut c: Vec<Cand> = slots
        .iter()
        .filter(|s| s.enabled)
        .filter_map(|s| {
            let mut a = accounts.iter().find(|a| a.id == s.account_id)?.clone();
            let u = usage.get(&a.id);
            if !a.enabled || budget_exhausted(&a, u) {
                return None;
            }
            if let Some(m) = s.model.as_ref().filter(|m| !m.is_empty()) {
                a.model = m.clone();
            }
            a.effort = s.effort.clone().filter(|e| !e.is_empty());
            let room = headroom(&a, u, now);
            let used = room.map(|r| 1.0 - r).unwrap_or(0.5);
            Some(Cand {
                over: used > policy.skip_above,
                score: room.unwrap_or(0.5) - load.get(&a.id).copied().unwrap_or(0.0),
                local: a.kind.is_local(),
                sticky: prefer == Some(a.id),
                today: u.map(|u| u.today.requests).unwrap_or(0),
                acct: a,
            })
        })
        .collect();
    c.sort_by(|x, y| {
        x.over
            .cmp(&y.over)
            .then_with(|| (!x.sticky).cmp(&!y.sticky))
            .then_with(|| {
                if policy.prefer_local {
                    (!x.local).cmp(&!y.local)
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .then_with(|| y.score.total_cmp(&x.score))
            .then_with(|| x.today.cmp(&y.today))
    });
    c.into_iter().map(|c| c.acct).collect()
}

pub fn rank(
    slots: &[Slot],
    accounts: &[Account],
    usage: &HashMap<Uuid, AccountUsage>,
    now: DateTime<Utc>,
    policy: &BalancePolicy,
    prefer: Option<Uuid>,
) -> Vec<Account> {
    rank_with_load(slots, accounts, usage, now, policy, prefer, &HashMap::new())
}

/// One ordered chain per parallel step, spreading first choices over the pool.
pub fn assign(
    steps: usize,
    slots: &[Slot],
    accounts: &[Account],
    usage: &HashMap<Uuid, AccountUsage>,
    now: DateTime<Utc>,
    policy: &BalancePolicy,
) -> Vec<Vec<Account>> {
    let mut load: HashMap<Uuid, f64> = HashMap::new();
    (0..steps)
        .map(|_| {
            let chain = rank_with_load(slots, accounts, usage, now, policy, None, &load);
            if let Some(first) = chain.first() {
                *load.entry(first.id).or_insert(0.0) += STEP_LOAD;
            }
            chain
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LimitWindow, RateLimits};
    use chrono::Duration;

    fn acct(kind: ProviderKind, name: &str) -> Account {
        Account {
            id: Uuid::new_v4(),
            name: name.into(),
            kind,
            api_key: None,
            base_url: None,
            model: "default".into(),
            cli_path: None,
            profile_dir: None,
            budget_usd_month: None,
            effort: None,
            enabled: true,
        }
    }

    fn claude_usage(util5: f64, util7: f64, now: DateTime<Utc>) -> AccountUsage {
        let w = |u| LimitWindow {
            utilization: u,
            resets_at: now + Duration::hours(2),
        };
        AccountUsage {
            limits: Some((
                RateLimits {
                    five_hour: Some(w(util5)),
                    seven_day: Some(w(util7)),
                    status: String::new(),
                },
                now,
            )),
            ..Default::default()
        }
    }

    fn names(v: &[Account]) -> Vec<&str> {
        v.iter().map(|a| a.name.as_str()).collect()
    }

    #[test]
    fn most_headroom_first_and_overloaded_last() {
        let now = Utc::now();
        let (a, b, c) = (
            acct(ProviderKind::ClaudeCli, "a"),
            acct(ProviderKind::ClaudeCli, "b"),
            acct(ProviderKind::ClaudeCli, "c"),
        );
        let usage = HashMap::from([
            (a.id, claude_usage(0.30, 0.10, now)),
            (b.id, claude_usage(0.10, 0.92, now)), // weekly nearly gone
            (c.id, claude_usage(0.05, 0.05, now)),
        ]);
        let slots: Vec<Slot> = [&a, &b, &c].iter().map(|x| Slot::new(x.id)).collect();
        let accounts = vec![a, b, c];
        let r = rank(
            &slots,
            &accounts,
            &usage,
            now,
            &BalancePolicy::default(),
            None,
        );
        assert_eq!(names(&r), ["c", "a", "b"]);
    }

    #[test]
    fn reset_window_counts_as_free() {
        let now = Utc::now();
        let a = acct(ProviderKind::ClaudeCli, "a");
        let mut u = claude_usage(0.99, 0.0, now);
        if let Some((l, _)) = u.limits.as_mut()
            && let Some(w) = l.five_hour.as_mut()
        {
            w.resets_at = now - Duration::minutes(1);
        }
        assert_eq!(headroom(&a, Some(&u), now), Some(1.0));
    }

    #[test]
    fn local_first_when_preferred_and_budget_removes_account() {
        let now = Utc::now();
        let local = acct(ProviderKind::Ollama, "local");
        let cli = acct(ProviderKind::ClaudeCli, "cli");
        let mut api = acct(ProviderKind::Anthropic, "api");
        api.budget_usd_month = Some(10.0);
        let usage = HashMap::from([
            (cli.id, claude_usage(0.0, 0.0, now)),
            (
                api.id,
                AccountUsage {
                    month_cost: 10.0,
                    ..Default::default()
                },
            ),
        ]);
        let slots: Vec<Slot> = [&cli, &api, &local]
            .iter()
            .map(|x| Slot::new(x.id))
            .collect();
        let accounts = vec![local, cli, api];
        let policy = BalancePolicy {
            prefer_local: true,
            ..Default::default()
        };
        let r = rank(&slots, &accounts, &usage, now, &policy, None);
        assert_eq!(names(&r), ["local", "cli"]); // exhausted API account dropped
    }

    #[test]
    fn sticky_account_stays_first_until_over_threshold() {
        let now = Utc::now();
        let (a, b) = (
            acct(ProviderKind::ClaudeCli, "a"),
            acct(ProviderKind::ClaudeCli, "b"),
        );
        let mut usage = HashMap::from([
            (a.id, claude_usage(0.5, 0.5, now)),
            (b.id, claude_usage(0.0, 0.0, now)),
        ]);
        let slots = vec![Slot::new(a.id), Slot::new(b.id)];
        let accounts = vec![a.clone(), b];
        let p = BalancePolicy::default();
        let r = rank(&slots, &accounts, &usage, now, &p, Some(a.id));
        assert_eq!(names(&r), ["a", "b"]);
        usage.insert(a.id, claude_usage(0.95, 0.5, now));
        let r = rank(&slots, &accounts, &usage, now, &p, Some(a.id));
        assert_eq!(names(&r), ["b", "a"]);
    }

    #[test]
    fn parallel_steps_spread_over_pool_and_apply_slot_model() {
        let now = Utc::now();
        let (a, b) = (
            acct(ProviderKind::ClaudeCli, "a"),
            acct(ProviderKind::ClaudeCli, "b"),
        );
        let mut slot_a = Slot::new(a.id);
        slot_a.model = Some("sonnet".into());
        slot_a.effort = Some("low".into());
        let slots = vec![slot_a, Slot::new(b.id)];
        let accounts = vec![a, b];
        let chains = assign(
            4,
            &slots,
            &accounts,
            &HashMap::new(),
            now,
            &BalancePolicy::default(),
        );
        let firsts: Vec<&str> = chains.iter().map(|c| c[0].name.as_str()).collect();
        assert_eq!(firsts.iter().filter(|n| **n == "a").count(), 2);
        let a0 = chains[0].iter().find(|x| x.name == "a").unwrap();
        assert_eq!(a0.model, "sonnet");
        assert_eq!(a0.effort.as_deref(), Some("low"));
    }
}
