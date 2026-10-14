//! Price table and usage aggregation.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use uuid::Uuid;

use crate::config::Price;
use crate::model::{Account, RateLimits, UsageEvent, UsageStatus};

#[derive(Clone, Debug)]
pub struct PriceTable(Vec<(String, Price)>);

/// Approximate public list prices (USD / 1M tokens). Override in config.toml.
const DEFAULTS: &[(&str, f64, f64)] = &[
    ("claude-opus", 15.0, 75.0),
    ("claude-sonnet", 3.0, 15.0),
    ("claude-haiku", 1.0, 5.0),
    ("gpt-5", 1.25, 10.0),
    ("gpt-4o", 2.5, 10.0),
    ("gpt-4.1", 2.0, 8.0),
    ("o3", 2.0, 8.0),
    ("gemini-2.5-pro", 1.25, 10.0),
    ("gemini-2.5-flash", 0.30, 2.5),
    ("deepseek", 0.28, 0.42),
    ("grok", 3.0, 15.0),
    ("mistral-large", 2.0, 6.0),
    ("llama-3.3-70b", 0.59, 0.79),
];

impl PriceTable {
    pub fn new(overrides: &BTreeMap<String, Price>) -> Self {
        let mut v: Vec<(String, Price)> = overrides.iter().map(|(k, p)| (k.clone(), *p)).collect();
        v.extend(DEFAULTS.iter().map(|(k, i, o)| {
            (
                k.to_string(),
                Price {
                    input: *i,
                    output: *o,
                },
            )
        }));
        // Longest prefix wins; overrides come first so they win ties.
        v.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));
        Self(v)
    }

    pub fn cost(&self, account: &Account, model: &str, input: u64, output: u64) -> f64 {
        if account.kind.is_local() {
            return 0.0;
        }
        let m = model.to_lowercase();
        let m = m.rsplit('/').next().unwrap_or(&m).to_string();
        self.0
            .iter()
            .find(|(k, _)| m.starts_with(k.as_str()) || model.to_lowercase().contains(k.as_str()))
            .map(|(_, p)| (input as f64 * p.input + output as f64 * p.output) / 1_000_000.0)
            .unwrap_or(0.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Window {
    Today,
    Days7,
    Days30,
    All,
}

impl Window {
    pub const ALL: [Window; 4] = [Window::Today, Window::Days7, Window::Days30, Window::All];

    pub fn label(self) -> &'static str {
        match self {
            Self::Today => "today",
            Self::Days7 => "7d",
            Self::Days30 => "30d",
            Self::All => "all",
        }
    }

    pub fn since(self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Self::Today => Some(start_of_day(now)),
            Self::Days7 => Some(start_of_day(now) - Duration::days(6)),
            Self::Days30 => Some(start_of_day(now) - Duration::days(29)),
            Self::All => None,
        }
    }
}

pub fn start_of_day(t: DateTime<Utc>) -> DateTime<Utc> {
    t.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc()
}

pub fn start_of_month(t: DateTime<Utc>) -> DateTime<Utc> {
    NaiveDate::from_ymd_opt(t.year(), t.month(), 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Totals {
    pub requests: u64,
    pub errors: u64,
    pub input: u64,
    pub output: u64,
    pub cost: f64,
}

impl Totals {
    fn add(&mut self, e: &UsageEvent) {
        self.requests += 1;
        if e.status != UsageStatus::Ok {
            self.errors += 1;
        }
        self.input += e.input_tokens;
        self.output += e.output_tokens;
        self.cost += e.cost_usd;
    }
}

#[derive(Clone, Debug, Default)]
pub struct AccountUsage {
    pub today: Totals,
    pub d7: Totals,
    pub d30: Totals,
    pub all: Totals,
    pub month_cost: f64,
    /// Tokens per day for the last 30 days, oldest first.
    pub daily_tokens: Vec<u64>,
    /// Latest plan limits seen for the account and when they were observed.
    pub limits: Option<(RateLimits, DateTime<Utc>)>,
}

/// How much of a plan-limit window is in use right now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LimitState {
    /// Still counting: fraction used and when it resets.
    Active { used: f64, resets_at: DateTime<Utc> },
    /// The window ended after the snapshot, so usage is back to ~0.
    Reset,
}

impl LimitState {
    pub fn of(w: &crate::model::LimitWindow, now: DateTime<Utc>) -> Self {
        if w.resets_at <= now {
            Self::Reset
        } else {
            Self::Active {
                used: w.utilization,
                resets_at: w.resets_at,
            }
        }
    }
}

impl AccountUsage {
    pub fn window(&self, w: Window) -> &Totals {
        match w {
            Window::Today => &self.today,
            Window::Days7 => &self.d7,
            Window::Days30 => &self.d30,
            Window::All => &self.all,
        }
    }
}

pub fn aggregate(events: &[UsageEvent], now: DateTime<Utc>) -> HashMap<Uuid, AccountUsage> {
    let mut map: HashMap<Uuid, AccountUsage> = HashMap::new();
    let (t0, t7, t30, m0) = (
        Window::Today.since(now).unwrap(),
        Window::Days7.since(now).unwrap(),
        Window::Days30.since(now).unwrap(),
        start_of_month(now),
    );
    let mut by_time: Vec<&UsageEvent> = events.iter().collect();
    by_time.sort_by_key(|e| e.at);
    for e in by_time {
        let u = map.entry(e.account_id).or_insert_with(|| AccountUsage {
            daily_tokens: vec![0; 30],
            ..Default::default()
        });
        u.all.add(e);
        if e.at >= t30 {
            u.d30.add(e);
            let idx = (start_of_day(e.at) - t30).num_days();
            if (0..30).contains(&idx) {
                u.daily_tokens[idx as usize] += e.input_tokens + e.output_tokens;
            }
        }
        if e.at >= t7 {
            u.d7.add(e);
        }
        if e.at >= t0 {
            u.today.add(e);
        }
        if e.at >= m0 {
            u.month_cost += e.cost_usd;
        }
        if let Some(l) = e.rate_limits.clone().filter(|l| !l.is_empty()) {
            match &mut u.limits {
                Some((cur, at)) => {
                    cur.merge(l);
                    *at = e.at;
                }
                None => u.limits = Some((l, e.at)),
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProviderKind;

    fn acct(kind: ProviderKind) -> Account {
        Account {
            id: Uuid::nil(),
            name: "a".into(),
            kind,
            api_key: None,
            base_url: None,
            model: String::new(),
            cli_path: None,
            profile_dir: None,
            budget_usd_month: None,
            enabled: true,
        }
    }

    #[test]
    fn pricing_prefix_and_local_free() {
        let t = PriceTable::new(&BTreeMap::new());
        let c = t.cost(
            &acct(ProviderKind::Anthropic),
            "claude-sonnet-5-5",
            1_000_000,
            1_000_000,
        );
        assert!((c - 18.0).abs() < 1e-9);
        assert_eq!(
            t.cost(&acct(ProviderKind::Ollama), "claude-sonnet", 1_000_000, 1),
            0.0
        );
        assert_eq!(
            t.cost(&acct(ProviderKind::Anthropic), "unknown-model", 100, 100),
            0.0
        );
    }

    #[test]
    fn override_wins() {
        let mut o = BTreeMap::new();
        o.insert(
            "claude-sonnet".to_string(),
            Price {
                input: 1.0,
                output: 1.0,
            },
        );
        let t = PriceTable::new(&o);
        assert!(
            (t.cost(
                &acct(ProviderKind::Anthropic),
                "claude-sonnet-5-5",
                1_000_000,
                0
            ) - 1.0)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn aggregation_windows() {
        let now = Utc::now();
        let id = Uuid::new_v4();
        let ev = |ago: Duration, cost: f64| UsageEvent {
            model: "m".into(),
            input_tokens: 10,
            output_tokens: 5,
            cost_usd: cost,
            latency_ms: 1,
            status: UsageStatus::Ok,
            rate_limits: None,
            at: now - ago,
            account_id: id,
        };
        let evs = vec![
            ev(Duration::zero(), 1.0),
            ev(Duration::days(3), 2.0),
            ev(Duration::days(20), 4.0),
            ev(Duration::days(100), 8.0),
        ];
        let a = &aggregate(&evs, now)[&id];
        assert_eq!(a.today.requests, 1);
        assert_eq!(a.d7.requests, 2);
        assert_eq!(a.d30.requests, 3);
        assert_eq!(a.all.requests, 4);
        assert_eq!(a.daily_tokens.len(), 30);
        assert_eq!(a.daily_tokens.iter().sum::<u64>(), 45);
    }

    #[test]
    fn limits_use_latest_snapshot_and_expire() {
        use crate::model::LimitWindow;
        let now = Utc::now();
        let id = Uuid::new_v4();
        let win = |u: f64, in_h: i64| LimitWindow {
            utilization: u,
            resets_at: now + Duration::hours(in_h),
        };
        let ev = |ago: Duration, l: RateLimits| UsageEvent {
            model: "m".into(),
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: 0.0,
            latency_ms: 1,
            status: UsageStatus::Ok,
            rate_limits: Some(l),
            at: now - ago,
            account_id: id,
        };
        let old = RateLimits {
            five_hour: Some(win(0.1, 2)),
            seven_day: Some(win(0.2, 50)),
            status: "allowed".into(),
        };
        let new = RateLimits {
            five_hour: Some(win(0.5, 3)),
            seven_day: None,
            status: "allowed".into(),
        };
        // Deliberately out of order: the newer event must win per window.
        let evs = vec![ev(Duration::minutes(1), new), ev(Duration::hours(1), old)];
        let (l, at) = aggregate(&evs, now)[&id].limits.clone().unwrap();
        assert_eq!(l.five_hour.unwrap().utilization, 0.5);
        assert_eq!(l.seven_day.unwrap().utilization, 0.2);
        assert_eq!(at, now - Duration::minutes(1));

        assert!(matches!(
            LimitState::of(&win(0.9, 1), now),
            LimitState::Active { .. }
        ));
        assert_eq!(LimitState::of(&win(0.9, -1), now), LimitState::Reset);
    }
}
