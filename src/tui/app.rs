//! Application state and event handling. Rendering lives in `ui.rs`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use futures::StreamExt;
use secrecy::{ExposeSecret, SecretString};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::form::{Field, FieldId, Form, FormKind};
use crate::crypto::{KdfParams, Vault, VaultHeader};
use crate::model::*;
use crate::orchestrator::{Core, LaneId, RunEvent, balance, compact, modes};
use crate::providers::{self, ChatRequest, ChatTurn};
use crate::store::Store;
use crate::usage::{AccountUsage, PriceTable, Window, aggregate};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Chat,
    Accounts,
    Agents,
    Usage,
    Sessions,
    Routing,
}

impl Tab {
    pub const ALL: [Tab; 6] = [
        Tab::Chat,
        Tab::Accounts,
        Tab::Agents,
        Tab::Usage,
        Tab::Sessions,
        Tab::Routing,
    ];
    pub fn title(self) -> &'static str {
        match self {
            Tab::Chat => "Chat",
            Tab::Accounts => "Accounts",
            Tab::Agents => "Agents",
            Tab::Usage => "Usage",
            Tab::Sessions => "Sessions",
            Tab::Routing => "Routing",
        }
    }
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap()
    }
    /// Text of the tab bar entry, e.g. "4 Usage".
    pub fn label(self) -> String {
        format!("{} {}", self.index() + 1, self.title())
    }
    /// Single-letter shortcut used by the quick switcher (`Ctrl+P`).
    pub fn mnemonic(self) -> char {
        match self {
            Tab::Chat => 'c',
            Tab::Accounts => 'a',
            Tab::Agents => 'g',
            Tab::Usage => 'u',
            Tab::Sessions => 's',
            Tab::Routing => 'r',
        }
    }
    /// Which tab sits under screen column `col` of the tab bar. Mirrors how
    /// `ratatui::widgets::Tabs` lays titles out: 1 column of border, then
    /// each title padded by one space on both sides, separated by one divider.
    pub fn at_column(col: u16) -> Option<Tab> {
        let mut x = 1u16;
        for t in Self::ALL {
            let w = t.label().chars().count() as u16 + 2;
            if (x..x + w).contains(&col) {
                return Some(t);
            }
            x += w + 1;
        }
        None
    }
}

pub struct UnlockState {
    pub first_run: bool,
    pub header: Option<VaultHeader>,
    pub pw: String,
    pub confirm: String,
    pub on_confirm: bool,
    pub busy: bool,
    pub error: Option<String>,
}

pub enum Screen {
    Unlock(UnlockState),
    Main,
}

pub enum Modal {
    Form(Form),
    Confirm {
        text: String,
        action: ConfirmAction,
    },
    Help,
    /// Quick tab switcher (`Ctrl+P`); holds the highlighted tab index.
    Switcher(usize),
    /// Filterable list (models, accounts).
    Picker(Picker),
    /// Review of `App::pending_plan`.
    Plan,
}

/// What happens with the value chosen in a [`Picker`].
pub enum PickPurpose {
    /// Add an account to routing section `.0`.
    AddSlot(usize),
    /// Set the model of slot `.1` in section `.0`.
    SlotModel(usize, usize),
    /// Override the model of the chat (`/model`).
    ChatModel,
    /// Fill the Model field of the form that opened the picker.
    FormModel(Box<Form>),
}

pub struct Picker {
    pub title: String,
    /// `(label, value)`
    pub items: Vec<(String, String)>,
    pub filter: String,
    pub sel: usize,
    pub purpose: PickPurpose,
}

impl Picker {
    pub fn visible(&self) -> Vec<&(String, String)> {
        let f = self.filter.to_lowercase();
        self.items
            .iter()
            .filter(|(l, _)| f.is_empty() || l.to_lowercase().contains(&f))
            .collect()
    }

    /// Models accept typed text when the list has no match.
    fn free_text(&self) -> bool {
        !matches!(self.purpose, PickPurpose::AddSlot(_))
    }
}

/// A plan waiting for approval.
pub struct PlanReview {
    /// The user's original task.
    pub task: String,
    pub steps: Vec<modes::Step>,
    pub sel: usize,
    /// First lane of the block reserved for this run.
    pub base: LaneId,
}

/// One selectable line of the Routing tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoutingRow {
    Header(usize),
    Slot(usize, usize),
    Policy(usize),
}

pub const POLICY_ROWS: usize = 4;

#[allow(clippy::enum_variant_names)]
pub enum ConfirmAction {
    DeleteAccount(Uuid),
    DeleteAgent(Uuid),
    DeleteSession(Uuid),
}

/// What a plain chat message does (cycled with `Shift+Tab`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Straight to the active target.
    Normal,
    /// Planner proposes steps, you review, then they run.
    Plan,
    /// Planner proposes steps and they run immediately.
    Auto,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Account(Uuid),
    Agent(Uuid),
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub role: Role,
    pub who: String,
    pub text: String,
    pub streaming: bool,
    pub meta: String,
    pub notes: Vec<String>,
    pub failed: bool,
}

impl Entry {
    fn new(role: Role, who: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role,
            who: who.into(),
            text: text.into(),
            streaming: false,
            meta: String::new(),
            notes: vec![],
            failed: false,
        }
    }
}

pub enum Msg {
    RunDone,
    Unlocked(Result<(Arc<Vault>, Option<VaultHeader>), String>),
    Loaded(Box<Loaded>),
    Notice(String),
    FormStatus(String),
    SessionLoaded(Uuid, Vec<Message>),
    HeaderReady(Option<VaultHeader>),
    Models(Uuid, Result<Vec<String>, String>),
    PlanReady {
        task: String,
        steps: Vec<modes::Step>,
        base: LaneId,
    },
    Summary(compact::Summary),
}

pub struct Loaded {
    pub accounts: Vec<Account>,
    pub agents: Vec<Agent>,
    pub sessions: Vec<SessionMeta>,
    pub usage: HashMap<Uuid, AccountUsage>,
    pub routing: Option<Routing>,
}

pub struct App {
    pub store: Store,
    pub prices: Arc<PriceTable>,
    pub core: Option<Core>,
    pub screen: Screen,
    pub tab: Tab,
    pub accounts: Vec<Account>,
    pub agents: Vec<Agent>,
    pub sessions: Vec<SessionMeta>,
    pub usage: HashMap<Uuid, AccountUsage>,
    pub window: Window,
    pub entries: Vec<Entry>,
    pub input: String,
    pub scroll_up: u16,
    pub sel: [usize; 6],
    pub modal: Option<Modal>,
    pub notice: Option<String>,
    pub target: Option<Target>,
    pub model_override: Option<String>,
    pub routing: Routing,
    /// The routing config is read from the database once; later edits are local.
    routing_ready: bool,
    pub models: HashMap<Uuid, Vec<String>>,
    pub models_err: HashMap<Uuid, String>,
    models_asked: HashSet<Uuid>,
    pub mode: Mode,
    pub pending_plan: Option<PlanReview>,
    summary: Option<compact::Summary>,
    /// Account that planned last; kept first while it has room (prompt cache).
    planner_last: Option<Uuid>,
    pub session_id: Option<Uuid>,
    pub session_cost: f64,
    pub session_tokens: u64,
    pub lanes: HashMap<LaneId, usize>,
    pub next_lane: LaneId,
    pub active_runs: usize,
    pub cancel: Option<CancellationToken>,
    pub tx: mpsc::UnboundedSender<Msg>,
    pub lane_tx: mpsc::UnboundedSender<(LaneId, RunEvent)>,
    pub quit: bool,
    pub tick: u64,
    /// Wall clock sampled on every tick, so rendering stays a pure function of `App`.
    pub now: chrono::DateTime<Utc>,
    /// Set by `L`; the run loop suspends the TUI and runs it on the real terminal.
    pending_login: Option<(std::process::Command, Account)>,
}

pub const COMMANDS: &[(&str, &str)] = &[
    ("/help", "show keys and commands"),
    ("/new", "start a new session"),
    ("/sessions", "open the sessions tab"),
    ("/usage", "open the usage tab"),
    ("/accounts", "open the accounts tab"),
    ("/agents", "open the agents tab"),
    ("/use", "/use <account|agent> - choose who answers"),
    (
        "/model",
        "/model [name|reset] - pick or set the model override",
    ),
    (
        "/fanout",
        "/fanout a,b,c <prompt> - same prompt to several accounts",
    ),
    (
        "/orchestrate",
        "/orchestrate <task> - plan, run agents in parallel, synthesize",
    ),
    (
        "/plan",
        "toggle plan mode (Shift+Tab cycles chat/plan/auto)",
    ),
    ("/rename", "/rename <title> - rename this session"),
    ("/passwd", "change the master password"),
    ("/lock", "lock the vault"),
    ("/clear", "clear the screen (keeps the session)"),
    ("/quit", "exit"),
];

fn init_terminal() -> ratatui::DefaultTerminal {
    let t = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    t
}

fn restore_terminal() {
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
}

impl App {
    pub async fn new(
        store: Store,
        prices: PriceTable,
    ) -> Result<(
        Self,
        mpsc::UnboundedReceiver<Msg>,
        mpsc::UnboundedReceiver<(LaneId, RunEvent)>,
    )> {
        let header = store.vault_header().await?;
        let (tx, rx) = mpsc::unbounded_channel();
        let (lane_tx, lane_rx) = mpsc::unbounded_channel();
        let app = Self {
            store,
            prices: Arc::new(prices),
            core: None,
            screen: Screen::Unlock(UnlockState {
                first_run: header.is_none(),
                header,
                pw: String::new(),
                confirm: String::new(),
                on_confirm: false,
                busy: false,
                error: None,
            }),
            tab: Tab::Chat,
            accounts: vec![],
            agents: vec![],
            sessions: vec![],
            usage: HashMap::new(),
            window: Window::Days30,
            entries: vec![],
            input: String::new(),
            scroll_up: 0,
            sel: [0; 6],
            modal: None,
            notice: None,
            target: None,
            model_override: None,
            routing: Routing::default(),
            routing_ready: false,
            models: HashMap::new(),
            models_err: HashMap::new(),
            models_asked: HashSet::new(),
            mode: Mode::Normal,
            pending_plan: None,
            summary: None,
            planner_last: None,
            session_id: None,
            session_cost: 0.0,
            session_tokens: 0,
            lanes: HashMap::new(),
            next_lane: 1,
            active_runs: 0,
            cancel: None,
            tx,
            lane_tx,
            quit: false,
            tick: 0,
            now: Utc::now(),
            pending_login: None,
        };
        Ok((app, rx, lane_rx))
    }

    pub async fn run(
        mut self,
        mut rx: mpsc::UnboundedReceiver<Msg>,
        mut lane_rx: mpsc::UnboundedReceiver<(LaneId, RunEvent)>,
    ) -> Result<()> {
        let mut terminal = init_terminal();
        let mut events = EventStream::new();
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(150));
        let result: Result<()> = async {
            while !self.quit {
                terminal.draw(|f| super::ui::draw(f, &self))?;
                tokio::select! {
                    ev = events.next() => match ev {
                        Some(Ok(Event::Key(k))) if k.kind == KeyEventKind::Press => {
                            self.on_key(k);
                            if let Some((mut cmd, acct)) = self.pending_login.take() {
                                drop(events);
                                restore_terminal();
                                let status = cmd.status();
                                terminal = init_terminal();
                                events = EventStream::new();
                                self.notice = Some(match status {
                                    Ok(s) if s.success() => {
                                        self.test_account(acct, false);
                                        "signed in, testing connection...".to_string()
                                    }
                                    Ok(s) => format!("sign-in exited with {s}"),
                                    Err(e) => format!("could not start sign-in: {e}"),
                                });
                            }
                        }
                        Some(Ok(Event::Mouse(m))) => match m.kind {
                            MouseEventKind::ScrollUp => {
                                self.scroll_up = self.scroll_up.saturating_add(3)
                            }
                            MouseEventKind::ScrollDown => {
                                self.scroll_up = self.scroll_up.saturating_sub(3)
                            }
                            MouseEventKind::Down(MouseButton::Left) => self.on_click(m.column, m.row),
                            _ => {}
                        },
                        Some(Err(e)) => return Err(e.into()),
                        None => break,
                        _ => {}
                    },
                    Some(m) = rx.recv() => self.on_msg(m),
                    Some((lane, ev)) = lane_rx.recv() => self.on_lane(lane, ev),
                    _ = ticker.tick() => {
                        self.tick = self.tick.wrapping_add(1);
                        if self.tick.is_multiple_of(7) {
                            self.now = Utc::now();
                        }
                    }
                }
            }
            Ok(())
        }
        .await;
        restore_terminal();
        result
    }

    // ------------------------------------------------------------------ messages

    fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::RunDone => {
                self.active_runs = self.active_runs.saturating_sub(1);
                if self.active_runs == 0 {
                    self.cancel = None;
                }
                self.reload();
            }
            Msg::Unlocked(Ok((vault, new_header))) => {
                let _ = new_header;
                self.core = Some(Core {
                    store: self.store.clone(),
                    vault,
                    prices: self.prices.clone(),
                });
                self.screen = Screen::Main;
                self.reload();
                self.notice = Some("vault unlocked - press ? for help".into());
            }
            Msg::Unlocked(Err(e)) => {
                if let Screen::Unlock(u) = &mut self.screen {
                    u.busy = false;
                    u.error = Some(e);
                    u.pw.clear();
                    u.confirm.clear();
                    u.on_confirm = false;
                }
            }
            Msg::Loaded(l) => {
                self.accounts = l.accounts;
                self.agents = l.agents;
                self.sessions = l.sessions;
                self.usage = l.usage;
                if !self.routing_ready {
                    self.routing_ready = true;
                    if let Some(r) = l.routing {
                        self.routing = r;
                    }
                }
                self.routing.prune(&self.accounts);
                let missing: Vec<Account> = self
                    .accounts
                    .iter()
                    .filter(|a| a.enabled && !self.models_asked.contains(&a.id))
                    .cloned()
                    .collect();
                for a in missing {
                    self.fetch_models(a);
                }
                if self.target.is_none() {
                    self.target = self
                        .accounts
                        .iter()
                        .find(|a| a.enabled)
                        .map(|a| Target::Account(a.id));
                }
                let rows = self.routing_rows().len();
                for (i, n) in [
                    (1, self.accounts.len()),
                    (2, self.agents.len()),
                    (4, self.sessions.len()),
                    (5, rows),
                ] {
                    self.sel[i] = self.sel[i].min(n.saturating_sub(1));
                }
            }
            Msg::HeaderReady(h) => {
                if let Screen::Unlock(u) = &mut self.screen {
                    u.busy = false;
                    u.first_run = h.is_none();
                    u.header = h;
                }
            }
            Msg::Notice(s) => self.notice = Some(s),
            Msg::Models(id, r) => match r {
                Ok(list) => {
                    self.models_err.remove(&id);
                    self.models.insert(id, list);
                }
                Err(e) => {
                    self.models_err
                        .insert(id, e.lines().next().unwrap_or("").to_string());
                }
            },
            Msg::PlanReady { task, steps, base } => {
                self.pending_plan = Some(PlanReview {
                    task,
                    steps,
                    sel: 0,
                    base,
                });
                self.modal = Some(Modal::Plan);
                self.notice = Some("review the plan: Enter approves, Esc discards".into());
            }
            Msg::Summary(s) => self.summary = Some(s),
            Msg::FormStatus(s) => {
                if let Some(Modal::Form(f)) = &mut self.modal {
                    f.status = Some(s);
                }
            }
            Msg::SessionLoaded(id, msgs) => {
                self.session_id = Some(id);
                self.entries = msgs
                    .into_iter()
                    .map(|m| {
                        let who = m.account_name.or(m.agent_name).unwrap_or_else(|| {
                            if m.role == Role::User {
                                "you".into()
                            } else {
                                "assistant".into()
                            }
                        });
                        Entry::new(m.role, who, m.content)
                    })
                    .collect();
                self.session_cost = 0.0;
                self.session_tokens = 0;
                self.summary = None;
                self.scroll_up = 0;
                self.tab = Tab::Chat;
            }
        }
    }

    fn on_lane(&mut self, lane: LaneId, ev: RunEvent) {
        if let RunEvent::Label(l) = &ev {
            let idx = self.lane_entry(lane);
            self.entries[idx].notes.insert(0, format!("[{l}]"));
            return;
        }
        let idx = self.lane_entry(lane);
        let e = &mut self.entries[idx];
        match ev {
            RunEvent::Label(_) => {}
            RunEvent::Started { account } => {
                e.who = account;
                e.streaming = true;
            }
            RunEvent::Delta(d) => e.text.push_str(&d),
            RunEvent::Note(n) => e.notes.push(n),
            RunEvent::Failover { from, reason } => e.notes.push(format!(
                "failover: {from} - {}",
                reason.lines().next().unwrap_or("")
            )),
            RunEvent::Finished {
                account,
                model,
                input,
                output,
                cost,
            } => {
                e.who = account;
                e.streaming = false;
                e.meta = format!("{model} - {input} in / {output} out - ${cost:.4}");
                self.session_cost += cost;
                self.session_tokens += input + output;
            }
            RunEvent::Failed(err) => {
                e.streaming = false;
                e.failed = true;
                e.text.push_str(&format!("\n[error] {err}"));
            }
            RunEvent::Cancelled => {
                e.streaming = false;
                e.notes.push("cancelled".into());
            }
        }
        self.scroll_up = 0;
    }

    fn lane_entry(&mut self, lane: LaneId) -> usize {
        if let Some(i) = self.lanes.get(&lane) {
            return *i;
        }
        let mut e = Entry::new(Role::Assistant, "...", "");
        e.streaming = true;
        self.entries.push(e);
        let i = self.entries.len() - 1;
        self.lanes.insert(lane, i);
        i
    }

    pub fn reload(&self) {
        let Some(core) = self.core.clone() else {
            return;
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let r: Result<Loaded> = async {
                let accounts = core.store.list_accounts(&core.vault).await?;
                let agents = core.store.list_agents(&core.vault).await?;
                let sessions = core.store.list_sessions(&core.vault).await?;
                let events = core.store.load_usage(&core.vault, None).await?;
                let routing = core.store.load_setting(&core.vault, "routing").await?;
                Ok(Loaded {
                    accounts,
                    agents,
                    sessions,
                    usage: aggregate(&events, Utc::now()),
                    routing,
                })
            }
            .await;
            match r {
                Ok(l) => {
                    let _ = tx.send(Msg::Loaded(Box::new(l)));
                }
                Err(e) => {
                    let _ = tx.send(Msg::Notice(format!("load failed: {e:#}")));
                }
            }
        });
    }

    // ------------------------------------------------------------------ keys

    fn on_key(&mut self, mut k: KeyEvent) {
        // Many terminals send ^H (0x08) or ^? for Backspace; crossterm reports those as Ctrl+char.
        if k.modifiers == KeyModifiers::CONTROL
            && matches!(k.code, KeyCode::Char('h') | KeyCode::Char('?'))
        {
            k.code = KeyCode::Backspace;
            k.modifiers = KeyModifiers::NONE;
        }
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            if self.active_runs > 0 {
                self.cancel_runs();
            } else {
                self.quit = true;
            }
            return;
        }
        match self.screen {
            Screen::Unlock(_) => self.on_key_unlock(k),
            Screen::Main => {
                if self.modal.is_some() {
                    self.on_key_modal(k);
                    // Closing a form opened from the plan review returns to the review.
                    if self.modal.is_none() && self.pending_plan.is_some() {
                        self.modal = Some(Modal::Plan);
                    }
                } else {
                    self.on_key_main(k)
                }
            }
        }
    }

    fn cancel_runs(&mut self) {
        if let Some(c) = &self.cancel {
            c.cancel();
        }
        self.notice = Some("cancelling...".into());
    }

    fn on_key_unlock(&mut self, k: KeyEvent) {
        let Screen::Unlock(u) = &mut self.screen else {
            return;
        };
        if u.busy {
            return;
        }
        match k.code {
            KeyCode::Esc => self.quit = true,
            KeyCode::Char(c) => {
                if u.on_confirm {
                    u.confirm.push(c)
                } else {
                    u.pw.push(c)
                }
            }
            KeyCode::Backspace => {
                if u.on_confirm {
                    u.confirm.pop();
                } else {
                    u.pw.pop();
                }
            }
            KeyCode::Tab | KeyCode::Down | KeyCode::Up if u.first_run => {
                u.on_confirm = !u.on_confirm
            }
            KeyCode::Enter => {
                if u.first_run && !u.on_confirm {
                    u.on_confirm = true;
                    return;
                }
                if u.pw.is_empty() {
                    return;
                }
                if u.first_run {
                    if u.pw.chars().count() < 8 {
                        u.error = Some("use at least 8 characters".into());
                        u.on_confirm = false;
                        return;
                    }
                    if u.pw != u.confirm {
                        u.error = Some("passwords do not match".into());
                        u.confirm.clear();
                        return;
                    }
                }
                u.busy = true;
                u.error = None;
                let (pw, first, header, store, tx) = (
                    u.pw.clone(),
                    u.first_run,
                    u.header.clone(),
                    self.store.clone(),
                    self.tx.clone(),
                );
                tokio::spawn(async move {
                    let res = if first {
                        store
                            .init_vault(&pw)
                            .await
                            .map(|v| (v, None))
                            .map_err(|e| format!("{e:#}"))
                    } else {
                        let h = header.expect("header present when not first run");
                        let r = tokio::task::spawn_blocking(move || {
                            Vault::unlock(&pw, &h).map(|v| (Arc::new(v), h))
                        })
                        .await;
                        match r {
                            Ok(Ok((v, h))) => Ok((v, Some(h))),
                            Ok(Err(e)) => Err(e.to_string()),
                            Err(e) => Err(e.to_string()),
                        }
                    };
                    let _ = tx.send(Msg::Unlocked(res));
                });
            }
            _ => {}
        }
    }

    fn on_key_main(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        // Global tab switching.
        match k.code {
            KeyCode::F(n @ 1..=6) => {
                self.tab = Tab::ALL[(n - 1) as usize];
                self.on_tab_enter();
                return;
            }
            KeyCode::Right if ctrl => {
                self.step_tab(1);
                return;
            }
            KeyCode::Left if ctrl => {
                self.step_tab(-1);
                return;
            }
            KeyCode::Char('p') if ctrl => {
                self.modal = Some(Modal::Switcher(self.tab.index()));
                return;
            }
            KeyCode::Char('n') if ctrl => {
                self.new_session();
                return;
            }
            KeyCode::Char('l') if ctrl => {
                self.lock();
                return;
            }
            _ => {}
        }
        if self.tab == Tab::Chat {
            self.on_key_chat(k);
            return;
        }
        // List tabs
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.modal = Some(Modal::Help),
            KeyCode::Char(c @ '1'..='6') => {
                self.tab = Tab::ALL[c as usize - '1' as usize];
                self.on_tab_enter();
            }
            KeyCode::Tab => self.step_tab(1),
            KeyCode::BackTab => self.step_tab(-1),
            KeyCode::Esc => self.tab = Tab::Chat,
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            _ => match self.tab {
                Tab::Accounts => self.on_key_accounts(k),
                Tab::Agents => self.on_key_agents(k),
                Tab::Usage => {
                    if k.code == KeyCode::Char('t') {
                        let i = Window::ALL
                            .iter()
                            .position(|w| *w == self.window)
                            .unwrap_or(0);
                        self.window = Window::ALL[(i + 1) % 4];
                    } else if k.code == KeyCode::Char('r') {
                        self.reload();
                    } else if k.code == KeyCode::Char('p') {
                        self.probe_selected();
                    }
                }
                Tab::Sessions => self.on_key_sessions(k),
                Tab::Routing => self.on_key_routing(k),
                Tab::Chat => {}
            },
        }
    }

    fn goto_tab(&mut self, t: Tab) {
        self.tab = t;
        self.on_tab_enter();
    }

    fn step_tab(&mut self, d: isize) {
        let n = Tab::ALL.len() as isize;
        let i = (self.tab.index() as isize + d).rem_euclid(n);
        self.goto_tab(Tab::ALL[i as usize]);
    }

    fn on_click(&mut self, col: u16, row: u16) {
        // The tab bar is a 3-row bordered block at the top of the main screen.
        if !matches!(self.screen, Screen::Main) || self.modal.is_some() || row > 2 {
            return;
        }
        if let Some(t) = Tab::at_column(col) {
            self.goto_tab(t);
        }
    }

    /// Sends a tiny prompt to the account selected in Usage so its plan
    /// limits are refreshed (only Claude CLI accounts report them).
    fn probe_selected(&mut self) {
        let (Some(core), Some(a)) = (
            self.core.clone(),
            self.accounts.get(self.sel[Tab::Usage.index()]).cloned(),
        ) else {
            return;
        };
        if a.kind != ProviderKind::ClaudeCli {
            self.notice = Some(format!("'{}' does not report plan limits", a.name));
            return;
        }
        self.notice = Some(format!("refreshing limits for '{}'...", a.name));
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let msg = match core.probe(a.clone()).await {
                Ok(()) => format!("limits refreshed for '{}'", a.name),
                Err(e) => format!("probe failed: {}", e.lines().next().unwrap_or("")),
            };
            let _ = tx.send(Msg::Notice(msg));
            let _ = tx.send(Msg::RunDone);
        });
        self.active_runs += 1;
    }

    fn on_tab_enter(&mut self) {
        if matches!(
            self.tab,
            Tab::Usage | Tab::Sessions | Tab::Accounts | Tab::Routing
        ) {
            self.reload();
        }
    }

    fn list_len(&self) -> usize {
        match self.tab {
            Tab::Accounts => self.accounts.len(),
            Tab::Agents => self.agents.len(),
            Tab::Sessions => self.sessions.len(),
            Tab::Usage => self.accounts.len(),
            Tab::Routing => self.routing_rows().len(),
            Tab::Chat => 0,
        }
    }

    fn move_sel(&mut self, d: isize) {
        let n = self.list_len();
        if n == 0 {
            return;
        }
        let s = &mut self.sel[self.tab.index()];
        *s = ((*s as isize + d).rem_euclid(n as isize)) as usize;
    }

    fn on_key_chat(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => {
                if self.active_runs > 0 {
                    self.cancel_runs();
                } else if !self.input.is_empty() {
                    self.input.clear();
                }
            }
            KeyCode::Enter
                if k.modifiers.contains(KeyModifiers::ALT)
                    || k.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.input.push('\n')
            }
            KeyCode::Char('j') if ctrl => self.input.push('\n'),
            KeyCode::Char('u') if ctrl => self.input.clear(),
            KeyCode::Char('o') if ctrl => self.toggle_plan_mode(),
            KeyCode::BackTab => self.cycle_mode(),
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.input);
                if !text.trim().is_empty() {
                    self.submit(text.trim().to_string());
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::PageUp => self.scroll_up = self.scroll_up.saturating_add(8),
            KeyCode::PageDown => self.scroll_up = self.scroll_up.saturating_sub(8),
            KeyCode::Tab => {
                if self.input.starts_with('/') || self.input.starts_with('@') {
                    if let Some(c) = self.completions().first() {
                        self.input = format!("{} ", c.0);
                    }
                } else {
                    self.tab = Tab::Accounts;
                    self.on_tab_enter();
                }
            }
            KeyCode::Char('?') if self.input.is_empty() => self.modal = Some(Modal::Help),
            KeyCode::Char(c) if !ctrl => self.input.push(c),
            _ => {}
        }
    }

    pub fn completions(&self) -> Vec<(String, String)> {
        let first = self.input.split_whitespace().next().unwrap_or("");
        if self.input.contains(' ') {
            return vec![];
        }
        if first.starts_with('/') {
            return COMMANDS
                .iter()
                .filter(|(c, _)| c.starts_with(first))
                .map(|(c, d)| (c.to_string(), d.to_string()))
                .collect();
        }
        if let Some(p) = first.strip_prefix('@') {
            let p = p.to_lowercase();
            let mut v: Vec<(String, String)> = self
                .agents
                .iter()
                .filter(|a| a.name.to_lowercase().starts_with(&p))
                .map(|a| (format!("@{}", a.name), "agent".into()))
                .collect();
            v.extend(
                self.accounts
                    .iter()
                    .filter(|a| a.name.to_lowercase().starts_with(&p))
                    .map(|a| (format!("@{}", a.name), a.kind.short().to_string())),
            );
            return v;
        }
        vec![]
    }

    // ------------------------------------------------------------------ accounts tab

    fn on_key_accounts(&mut self, k: KeyEvent) {
        let sel = self.accounts.get(self.sel[1]).cloned();
        match k.code {
            KeyCode::Char('m') => {
                if let Some(a) = sel {
                    self.notice = Some(format!("refreshing models of '{}'...", a.name));
                    self.fetch_models(a);
                }
            }
            KeyCode::Char('a') => self.modal = Some(Modal::Form(self.account_form(None))),
            KeyCode::Char('e') => {
                if let Some(a) = sel {
                    self.modal = Some(Modal::Form(self.account_form(Some(&a))))
                }
            }
            KeyCode::Char('r') => {
                if let Some(a) = sel {
                    self.modal = Some(Modal::Form(self.account_form(Some(&a))))
                }
            }
            KeyCode::Char('d') => {
                if let Some(a) = sel {
                    self.modal = Some(Modal::Confirm {
                        text: format!("Delete account '{}' and its usage history?", a.name),
                        action: ConfirmAction::DeleteAccount(a.id),
                    })
                }
            }
            KeyCode::Char(' ') => {
                if let Some(mut a) = sel {
                    a.enabled = !a.enabled;
                    self.save_account(a);
                }
            }
            KeyCode::Enter | KeyCode::Char('u') => {
                if let Some(a) = sel {
                    self.target = Some(Target::Account(a.id));
                    self.notice = Some(format!("now chatting with account '{}'", a.name));
                    self.tab = Tab::Chat;
                }
            }
            KeyCode::Char('t') => {
                if let Some(a) = sel {
                    self.test_account(a, false)
                }
            }
            KeyCode::Char('D') => self.detect_local(),
            KeyCode::Char('L') => {
                if let Some(a) = sel {
                    match providers::cli_backend::login_command(&a) {
                        Ok(c) => self.pending_login = Some((c, a)),
                        Err(e) => self.notice = Some(e.to_string()),
                    }
                }
            }
            _ => {}
        }
    }

    fn detect_local(&mut self) {
        let tx = self.tx.clone();
        let missing_clis: Vec<ProviderKind> = [
            ProviderKind::ClaudeCli,
            ProviderKind::CodexCli,
            ProviderKind::GeminiCli,
            ProviderKind::AgyCli,
        ]
        .into_iter()
        .filter(|k| {
            !self.accounts.iter().any(|a| a.kind == *k)
                && k.cli_binary().and_then(providers::which).is_some()
        })
        .collect();
        let core = self.core.clone();
        self.notice = Some("detecting installed CLIs...".into());
        tokio::spawn(async move {
            let Some(core) = core else { return };
            let mut added = vec![];
            for k in missing_clis {
                let a = Account {
                    id: Uuid::nil(),
                    name: k.label().split(" (").next().unwrap_or("CLI").to_string(),
                    kind: k,
                    api_key: None,
                    base_url: None,
                    model: String::new(),
                    cli_path: None,
                    profile_dir: None,
                    budget_usd_month: None,
                    enabled: true,
                    effort: None,
                };
                if core.store.upsert_account(&core.vault, &a).await.is_ok() {
                    added.push(k.cli_binary().unwrap_or("cli").to_string());
                }
            }
            let _ = tx.send(Msg::Notice(if added.is_empty() {
                "nothing new detected".into()
            } else {
                format!("added: {}", added.join(", "))
            }));
            let _ = tx.send(Msg::RunDone);
        });
        self.active_runs += 1;
    }

    pub fn account_form(&self, edit: Option<&Account>) -> Form {
        let kinds: Vec<String> = ProviderKind::ALL
            .iter()
            .map(|k| k.label().to_string())
            .collect();
        let kind = edit.map(|a| a.kind).unwrap_or(ProviderKind::Anthropic);
        let kind_idx = ProviderKind::ALL
            .iter()
            .position(|k| *k == kind)
            .unwrap_or(0);
        let key_hint = match edit.and_then(|a| a.api_key.as_ref()) {
            Some(k) => format!(
                "current: {} (leave empty to keep)",
                mask_secret(k.expose_secret())
            ),
            None => String::new(),
        };
        let fields = vec![
            Field::choice(FieldId::Provider, "Provider", kinds, kind_idx).hint("<-/-> to change"),
            Field::text(
                FieldId::Name,
                "Name",
                edit.map(|a| a.name.clone()).unwrap_or_default(),
            )
            .hint("e.g. Claude Work, OpenAI Personal"),
            Field::secret(FieldId::ApiKey, "API key", "").hint(key_hint),
            Field::text(
                FieldId::BaseUrl,
                "Base URL",
                edit.and_then(|a| a.base_url.clone())
                    .or_else(|| kind.default_base_url().map(Into::into))
                    .unwrap_or_default(),
            ),
            Field::text(
                FieldId::Model,
                "Model",
                edit.map(|a| a.model.clone())
                    .unwrap_or_else(|| kind.default_model().to_string()),
            )
            .hint("empty = CLI default"),
            Field::text(
                FieldId::CliPath,
                "CLI path",
                edit.and_then(|a| a.cli_path.clone()).unwrap_or_default(),
            )
            .hint("empty = look up in PATH"),
            Field::text(
                FieldId::ProfileDir,
                "Profile dir",
                edit.and_then(|a| a.profile_dir.clone()).unwrap_or_default(),
            )
            .hint("empty = default login; e.g. ~/.omnicrab/claude-work (press L in Accounts to sign in)"),
            Field::text(
                FieldId::Budget,
                "Budget USD/mo",
                edit.and_then(|a| a.budget_usd_month)
                    .map(|b| b.to_string())
                    .unwrap_or_default(),
            )
            .hint("optional"),
        ];
        let title = if edit.is_some() {
            "Edit account"
        } else {
            "Add account"
        };
        let mut f = Form::new(
            FormKind::Account {
                edit: edit.map(|a| a.id),
            },
            title,
            fields,
        );
        f.focus = 1;
        f
    }

    fn account_from_form(&self, f: &Form, edit: Option<Uuid>) -> Result<Account, String> {
        let kind = ProviderKind::ALL[f.choice_idx(FieldId::Provider)];
        let name = f.value(FieldId::Name);
        if name.is_empty() {
            return Err("name is required".into());
        }
        let existing = edit.and_then(|id| self.accounts.iter().find(|a| a.id == id));
        let key = f.value(FieldId::ApiKey);
        let api_key = if !kind.needs_api_key() {
            None
        } else if key.is_empty() {
            existing.and_then(|a| a.api_key.clone())
        } else {
            Some(SecretString::from(key))
        };
        if kind.needs_api_key() && api_key.is_none() {
            return Err("API key is required".into());
        }
        let budget = f.value(FieldId::Budget);
        let budget_usd_month = if budget.is_empty() {
            None
        } else {
            Some(
                budget
                    .parse::<f64>()
                    .map_err(|_| "budget must be a number")?,
            )
        };
        let opt = |id| Some(f.value(id)).filter(|s| !s.is_empty());
        Ok(Account {
            id: edit.unwrap_or_else(Uuid::nil),
            name,
            kind,
            api_key,
            base_url: if kind.is_local() {
                opt(FieldId::BaseUrl)
            } else {
                None
            },
            model: f.value(FieldId::Model),
            cli_path: if kind.is_cli() {
                opt(FieldId::CliPath)
            } else {
                None
            },
            profile_dir: if kind.profile_env().is_some() {
                opt(FieldId::ProfileDir)
            } else {
                None
            },
            budget_usd_month,
            enabled: existing.map(|a| a.enabled).unwrap_or(true),
            effort: None,
        })
    }

    fn save_account(&mut self, a: Account) {
        let Some(core) = self.core.clone() else {
            return;
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match core.store.upsert_account(&core.vault, &a).await {
                Ok(_) => {
                    let _ = tx.send(Msg::Notice(format!("saved account '{}'", a.name)));
                }
                Err(e) => {
                    let _ = tx.send(Msg::Notice(format!("{e:#}")));
                }
            }
            let _ = tx.send(Msg::RunDone);
        });
        self.active_runs += 1;
    }

    fn test_account(&mut self, a: Account, in_form: bool) {
        let tx = self.tx.clone();
        let msg = move |s: String| {
            if in_form {
                Msg::FormStatus(s)
            } else {
                Msg::Notice(s)
            }
        };
        let _ = tx.send(msg(format!("testing '{}'...", a.name)));
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            let res = providers::test_connection(&a).await;
            let out = match res {
                Ok(m) => format!(
                    "OK '{}' answered via {m} in {} ms",
                    a.name,
                    started.elapsed().as_millis()
                ),
                Err(e) => format!(
                    "FAILED '{}': {}",
                    a.name,
                    e.message.lines().next().unwrap_or("")
                ),
            };
            let _ = tx.send(msg(out));
        });
    }

    // ------------------------------------------------------------------ agents tab

    fn on_key_agents(&mut self, k: KeyEvent) {
        let sel = self.agents.get(self.sel[2]).cloned();
        match k.code {
            KeyCode::Char('a') => {
                if self.accounts.is_empty() {
                    self.notice = Some("add an account first".into());
                    return;
                }
                self.modal = Some(Modal::Form(self.agent_form(None)));
            }
            KeyCode::Char('e') => {
                if let Some(a) = sel {
                    self.modal = Some(Modal::Form(self.agent_form(Some(&a))))
                }
            }
            KeyCode::Char('d') => {
                if let Some(a) = sel {
                    self.modal = Some(Modal::Confirm {
                        text: format!("Delete agent '{}'?", a.name),
                        action: ConfirmAction::DeleteAgent(a.id),
                    })
                }
            }
            KeyCode::Enter | KeyCode::Char('u') => {
                if let Some(a) = sel {
                    self.target = Some(Target::Agent(a.id));
                    self.notice = Some(format!("now chatting with agent '{}'", a.name));
                    self.tab = Tab::Chat;
                }
            }
            _ => {}
        }
    }

    fn agent_form(&self, edit: Option<&Agent>) -> Form {
        let names: Vec<String> = self.accounts.iter().map(|a| a.name.clone()).collect();
        let acc_idx = edit
            .and_then(|e| self.accounts.iter().position(|a| a.id == e.account_id))
            .unwrap_or(0);
        let fb = edit
            .map(|e| {
                e.fallback_account_ids
                    .iter()
                    .filter_map(|id| self.accounts.iter().find(|a| a.id == *id))
                    .map(|a| a.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let fields = vec![
            Field::text(
                FieldId::Name,
                "Name",
                edit.map(|a| a.name.clone()).unwrap_or_default(),
            )
            .hint("used as @name"),
            Field::text(
                FieldId::Role,
                "Role",
                edit.map(|a| a.role.clone()).unwrap_or_default(),
            )
            .hint("e.g. senior Rust reviewer"),
            Field::text(
                FieldId::Prompt,
                "System prompt",
                edit.map(|a| a.system_prompt.clone()).unwrap_or_default(),
            ),
            Field::choice(FieldId::Account, "Account", names, acc_idx).hint("<-/-> to change"),
            Field::text(
                FieldId::Model,
                "Model",
                edit.and_then(|a| a.model.clone()).unwrap_or_default(),
            )
            .hint("empty = account default"),
            Field::text(FieldId::Fallbacks, "Fallbacks", fb).hint("account names, comma separated"),
        ];
        Form::new(
            FormKind::Agent {
                edit: edit.map(|a| a.id),
            },
            if edit.is_some() {
                "Edit agent"
            } else {
                "Add agent"
            },
            fields,
        )
    }

    fn agent_from_form(&self, f: &Form, edit: Option<Uuid>) -> Result<Agent, String> {
        let name = f.value(FieldId::Name);
        if name.is_empty() {
            return Err("name is required".into());
        }
        if self
            .agents
            .iter()
            .any(|a| a.name.eq_ignore_ascii_case(&name) && Some(a.id) != edit)
        {
            return Err("an agent with this name exists".into());
        }
        let account = self
            .accounts
            .get(f.choice_idx(FieldId::Account))
            .ok_or("no account")?;
        let mut fallback = vec![];
        for n in f
            .value(FieldId::Fallbacks)
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let a = self
                .accounts
                .iter()
                .find(|a| a.name.eq_ignore_ascii_case(n))
                .ok_or_else(|| format!("unknown account '{n}'"))?;
            fallback.push(a.id);
        }
        Ok(Agent {
            id: edit.unwrap_or_else(Uuid::nil),
            name,
            role: f.value(FieldId::Role),
            system_prompt: f.value(FieldId::Prompt),
            account_id: account.id,
            model: Some(f.value(FieldId::Model)).filter(|s| !s.is_empty()),
            fallback_account_ids: fallback,
        })
    }

    // ------------------------------------------------------------------ sessions tab

    fn on_key_sessions(&mut self, k: KeyEvent) {
        let sel = self.sessions.get(self.sel[4]).cloned();
        match k.code {
            KeyCode::Enter => {
                if let (Some(s), Some(core)) = (sel, self.core.clone()) {
                    let tx = self.tx.clone();
                    tokio::spawn(async move {
                        match core.store.load_messages(&core.vault, s.id).await {
                            Ok(m) => {
                                let _ = tx.send(Msg::SessionLoaded(s.id, m));
                            }
                            Err(e) => {
                                let _ = tx.send(Msg::Notice(format!("{e:#}")));
                            }
                        }
                    });
                }
            }
            KeyCode::Char('n') => self.new_session(),
            KeyCode::Char('d') => {
                if let Some(s) = sel {
                    self.modal = Some(Modal::Confirm {
                        text: format!("Delete session '{}'?", s.title),
                        action: ConfirmAction::DeleteSession(s.id),
                    })
                }
            }
            _ => {}
        }
    }

    fn new_session(&mut self) {
        self.entries.clear();
        self.lanes.clear();
        self.session_id = None;
        self.session_cost = 0.0;
        self.session_tokens = 0;
        self.summary = None;
        self.tab = Tab::Chat;
        self.notice = Some("new session".into());
    }

    fn lock(&mut self) {
        if self.active_runs > 0 {
            self.cancel_runs();
        }
        self.core = None;
        self.entries.clear();
        self.lanes.clear();
        self.accounts.clear();
        self.agents.clear();
        self.sessions.clear();
        self.usage.clear();
        self.session_id = None;
        self.summary = None;
        self.pending_plan = None;
        self.routing = Routing::default();
        self.routing_ready = false;
        self.models.clear();
        self.models_err.clear();
        self.models_asked.clear();
        self.target = None;
        self.modal = None;
        self.screen = Screen::Unlock(UnlockState {
            first_run: false,
            header: None,
            pw: String::new(),
            confirm: String::new(),
            on_confirm: false,
            busy: true,
            error: None,
        });
        let (store, tx) = (self.store.clone(), self.tx.clone());
        tokio::spawn(async move {
            let h = store.vault_header().await.ok().flatten();
            let _ = tx.send(Msg::HeaderReady(h));
        });
    }

    // ------------------------------------------------------------------ modal keys

    fn on_key_modal(&mut self, k: KeyEvent) {
        match self.modal.take() {
            None => {}
            Some(Modal::Help) => { /* any key closes */ }
            Some(Modal::Switcher(sel)) => {
                let n = Tab::ALL.len();
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                let target = match k.code {
                    KeyCode::Esc => return,
                    KeyCode::Char('p') if ctrl => return,
                    KeyCode::Enter => Some(Tab::ALL[sel]),
                    KeyCode::Char(c @ '1'..='6') => Some(Tab::ALL[c as usize - '1' as usize]),
                    KeyCode::Char(c) if !ctrl => Tab::ALL.into_iter().find(|t| t.mnemonic() == c),
                    _ => None,
                };
                match (target, k.code) {
                    (Some(t), _) => self.goto_tab(t),
                    (None, KeyCode::Down | KeyCode::Right | KeyCode::Char('j')) => {
                        self.modal = Some(Modal::Switcher((sel + 1) % n))
                    }
                    (None, KeyCode::Up | KeyCode::Left | KeyCode::Char('k')) => {
                        self.modal = Some(Modal::Switcher((sel + n - 1) % n))
                    }
                    _ => self.modal = Some(Modal::Switcher(sel)),
                }
            }
            Some(Modal::Picker(mut p)) => {
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                match k.code {
                    KeyCode::Esc => {
                        if let PickPurpose::FormModel(f) = p.purpose {
                            self.modal = Some(Modal::Form(*f));
                        }
                        return;
                    }
                    KeyCode::Up => p.sel = p.sel.saturating_sub(1),
                    KeyCode::Down => {
                        let n = p.visible().len();
                        p.sel = (p.sel + 1).min(n.saturating_sub(1));
                    }
                    KeyCode::Backspace => {
                        p.filter.pop();
                        p.sel = 0;
                    }
                    KeyCode::Enter => {
                        let value = p.visible().get(p.sel).map(|i| i.1.clone()).or_else(|| {
                            (p.free_text() && !p.filter.is_empty()).then(|| p.filter.clone())
                        });
                        if let Some(v) = value {
                            self.apply_pick(p.purpose, v);
                            return;
                        }
                    }
                    KeyCode::Char(c) if !ctrl => {
                        p.filter.push(c);
                        p.sel = 0;
                    }
                    _ => {}
                }
                self.modal = Some(Modal::Picker(p));
            }
            Some(Modal::Plan) => {
                let Some(mut pr) = self.pending_plan.take() else {
                    return;
                };
                let n = pr.steps.len();
                match k.code {
                    KeyCode::Esc => {
                        self.notice = Some("plan discarded".into());
                        return;
                    }
                    KeyCode::Up | KeyCode::Char('k') => pr.sel = pr.sel.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => {
                        pr.sel = (pr.sel + 1).min(n.saturating_sub(1))
                    }
                    KeyCode::Char('d') => {
                        if n > 0 {
                            pr.steps.remove(pr.sel);
                        }
                        if pr.steps.is_empty() {
                            self.notice = Some("plan discarded".into());
                            return;
                        }
                        pr.sel = pr.sel.min(pr.steps.len() - 1);
                    }
                    KeyCode::Char('e') => {
                        if let Some(step) = pr.steps.get(pr.sel) {
                            let f = Form::new(
                                FormKind::PlanStep(pr.sel),
                                format!("Edit step {}", pr.sel + 1),
                                vec![Field::text(FieldId::StepText, "Task", step.task.clone())],
                            );
                            self.pending_plan = Some(pr);
                            self.modal = Some(Modal::Form(f));
                            return;
                        }
                    }
                    KeyCode::Char('r') => {
                        let f = Form::new(
                            FormKind::PlanFeedback,
                            "Ask the planner to revise",
                            vec![Field::text(FieldId::Feedback, "Feedback", "")],
                        );
                        self.pending_plan = Some(pr);
                        self.modal = Some(Modal::Form(f));
                        return;
                    }
                    KeyCode::Enter => {
                        self.approve_plan(pr);
                        return;
                    }
                    _ => {}
                }
                self.pending_plan = Some(pr);
                self.modal = Some(Modal::Plan);
            }
            Some(Modal::Confirm { text, action }) => match k.code {
                KeyCode::Char('y') | KeyCode::Enter => self.confirm(action),
                KeyCode::Char('n') | KeyCode::Esc => {}
                _ => self.modal = Some(Modal::Confirm { text, action }),
            },
            Some(Modal::Form(mut f)) => {
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                match k.code {
                    KeyCode::Esc => return,
                    KeyCode::Tab | KeyCode::Down => f.move_focus(1),
                    KeyCode::BackTab | KeyCode::Up => f.move_focus(-1),
                    KeyCode::Left | KeyCode::Right => {
                        let d = if k.code == KeyCode::Left { -1 } else { 1 };
                        if f.cycle_choice(d) && f.fields[f.focus].id == FieldId::Provider {
                            self.apply_provider_defaults(&mut f);
                        }
                    }
                    KeyCode::Backspace => f.backspace(),
                    KeyCode::Char('o') if ctrl && f.fields[f.focus].id == FieldId::Model => {
                        let items = self.form_model_items(&f);
                        if items.is_empty() {
                            f.status = Some(
                                "no model list yet - press m on the Accounts tab to fetch it"
                                    .into(),
                            );
                        } else {
                            self.modal = Some(Modal::Picker(Picker {
                                title: "Model".into(),
                                items,
                                filter: String::new(),
                                sel: 0,
                                purpose: PickPurpose::FormModel(Box::new(f)),
                            }));
                            return;
                        }
                    }
                    KeyCode::Char('t') if ctrl => {
                        if let FormKind::Account { edit } = f.kind.clone() {
                            match self.account_from_form(&f, edit) {
                                Ok(a) => self.test_account(a, true),
                                Err(e) => f.status = Some(e),
                            }
                        }
                    }
                    KeyCode::Char(c) if !ctrl => f.type_char(c),
                    KeyCode::Enter => match self.submit_form(&f) {
                        Ok(()) => return,
                        Err(e) => f.status = Some(e),
                    },
                    _ => {}
                }
                self.modal = Some(Modal::Form(f));
            }
        }
    }

    fn apply_provider_defaults(&self, f: &mut Form) {
        let kind = ProviderKind::ALL[f.choice_idx(FieldId::Provider)];
        for fld in &mut f.fields {
            match fld.id {
                FieldId::Model => fld.value = kind.default_model().to_string(),
                FieldId::BaseUrl => fld.value = kind.default_base_url().unwrap_or("").to_string(),
                _ => {}
            }
        }
    }

    fn submit_form(&mut self, f: &Form) -> Result<(), String> {
        match f.kind.clone() {
            FormKind::Account { edit } => {
                let a = self.account_from_form(f, edit)?;
                self.save_account(a);
            }
            FormKind::Agent { edit } => {
                let a = self.agent_from_form(f, edit)?;
                let Some(core) = self.core.clone() else {
                    return Ok(());
                };
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let m = match core.store.upsert_agent(&core.vault, &a).await {
                        Ok(_) => format!("saved agent '{}'", a.name),
                        Err(e) => format!("{e:#}"),
                    };
                    let _ = tx.send(Msg::Notice(m));
                    let _ = tx.send(Msg::RunDone);
                });
                self.active_runs += 1;
            }
            FormKind::PlanStep(i) => {
                let text = f.value(FieldId::StepText);
                if text.is_empty() {
                    return Err("the task cannot be empty".into());
                }
                if let Some(step) = self
                    .pending_plan
                    .as_mut()
                    .and_then(|pr| pr.steps.get_mut(i))
                {
                    step.task = text;
                }
            }
            FormKind::PlanFeedback => {
                let fb = f.value(FieldId::Feedback);
                if fb.is_empty() {
                    return Err("write what should change".into());
                }
                if let Some(pr) = self.pending_plan.take() {
                    let prompt = format!(
                        "{}\n\nFeedback on the previous plan (apply it): {fb}",
                        pr.task
                    );
                    self.start_plan(false, prompt, pr.task);
                }
            }
            FormKind::Passwd => {
                let (pw, confirm) = (f.value(FieldId::NewPw), f.value(FieldId::ConfirmPw));
                // `value()` trims; use raw values so passwords with edge spaces work.
                let raw = |id| f.get(id).map(|x| x.value.clone()).unwrap_or_default();
                let (pw, confirm) = (
                    if raw(FieldId::NewPw).is_empty() {
                        pw
                    } else {
                        raw(FieldId::NewPw)
                    },
                    if raw(FieldId::ConfirmPw).is_empty() {
                        confirm
                    } else {
                        raw(FieldId::ConfirmPw)
                    },
                );
                if pw.chars().count() < 8 {
                    return Err("use at least 8 characters".into());
                }
                if pw != confirm {
                    return Err("passwords do not match".into());
                }
                let Some(core) = self.core.clone() else {
                    return Ok(());
                };
                let tx = self.tx.clone();
                self.notice = Some("re-wrapping key...".into());
                tokio::spawn(async move {
                    let r = tokio::task::spawn_blocking({
                        let v = core.vault.clone();
                        move || v.wrap(&pw, KdfParams::default())
                    })
                    .await;
                    let m = match r {
                        Ok(Ok(h)) => core
                            .store
                            .save_header(&h)
                            .await
                            .map(|_| "master password changed".to_string())
                            .unwrap_or_else(|e| format!("{e:#}")),
                        _ => "password change failed".into(),
                    };
                    let _ = tx.send(Msg::Notice(m));
                });
            }
        }
        Ok(())
    }

    fn confirm(&mut self, action: ConfirmAction) {
        let Some(core) = self.core.clone() else {
            return;
        };
        let tx = self.tx.clone();
        if let ConfirmAction::DeleteSession(id) = &action
            && self.session_id == Some(*id)
        {
            self.new_session();
        }
        tokio::spawn(async move {
            let r = match action {
                ConfirmAction::DeleteAccount(id) => core.store.delete_account(id).await,
                ConfirmAction::DeleteAgent(id) => core.store.delete_agent(id).await,
                ConfirmAction::DeleteSession(id) => core.store.delete_session(id).await,
            };
            let _ = tx.send(Msg::Notice(match r {
                Ok(()) => "deleted".into(),
                Err(e) => format!("{e:#}"),
            }));
            let _ = tx.send(Msg::RunDone);
        });
        self.active_runs += 1;
    }

    // ------------------------------------------------------------------ chat submit & commands

    /// Longest agent/account name at the start of `s` (case-insensitive) that ends at
    /// whitespace, a comma or the end of the text. Agents win ties.
    fn take_name<'a>(&self, s: &'a str, accounts_only: bool) -> Option<(Target, &'a str)> {
        let agents = self
            .agents
            .iter()
            .map(|a| (a.name.as_str(), Target::Agent(a.id)));
        let accounts = self
            .accounts
            .iter()
            .map(|a| (a.name.as_str(), Target::Account(a.id)));
        let mut best: Option<(usize, Target)> = None;
        let cands: Vec<(&str, Target)> = if accounts_only {
            accounts.collect()
        } else {
            agents.chain(accounts).collect()
        };
        for (name, t) in cands {
            let n = name.len();
            let fits = s.is_char_boundary(n)
                && s.get(..n).is_some_and(|p| p.eq_ignore_ascii_case(name))
                && s[n..]
                    .chars()
                    .next()
                    .is_none_or(|c| c.is_whitespace() || c == ',');
            if fits && best.is_none_or(|(b, _)| n > b) {
                best = Some((n, t));
            }
        }
        best.map(|(n, t)| (t, &s[n..]))
    }

    fn find_account(&self, name: &str) -> Option<&Account> {
        let n = name.trim().trim_start_matches('@');
        self.accounts
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(n))
    }

    fn chain_for_target(&self, t: Target) -> Option<(Vec<Account>, Option<Agent>)> {
        match t {
            Target::Account(id) => self
                .accounts
                .iter()
                .find(|a| a.id == id)
                .map(|a| (vec![a.clone()], None)),
            Target::Agent(id) => {
                let ag = self.agents.iter().find(|a| a.id == id)?.clone();
                let chain = modes::chain_for(&ag, &self.accounts);
                Some((chain, Some(ag)))
            }
        }
    }

    fn submit(&mut self, text: String) {
        if self.core.is_none() {
            return;
        }
        if let Some(cmd) = text.strip_prefix('/') {
            self.command(cmd.trim());
            return;
        }
        // @name routing
        let routed = text.starts_with('@');
        let (target, prompt) = match text.strip_prefix('@') {
            Some(rest) => match self.take_name(rest, false) {
                Some((t, tail)) if !tail.trim().is_empty() => (Some(t), tail.trim().to_string()),
                Some(_) => {
                    self.notice = Some("write a message after @name".into());
                    return;
                }
                None => {
                    self.notice = Some("no agent/account with that name".into());
                    return;
                }
            },
            None => (self.target, text),
        };
        if !routed {
            match self.mode {
                Mode::Normal => {}
                Mode::Plan => {
                    self.start_plan(true, prompt.clone(), prompt);
                    return;
                }
                Mode::Auto => {
                    let shown = prompt.clone();
                    self.orchestrate_task(prompt, shown);
                    return;
                }
            }
        }
        let Some(target) = target else {
            self.notice = Some(
                "no account yet - press Tab, then 'a' to add one (or 'D' to detect local ones)"
                    .into(),
            );
            return;
        };
        let Some((chain, agent)) = self.chain_for_target(target) else {
            self.notice = Some("target no longer exists".into());
            return;
        };
        if chain.is_empty() {
            self.notice = Some("agent has no valid account".into());
            return;
        }

        // History for the model (before adding this turn).
        let mut turns: Vec<ChatTurn> = self
            .entries
            .iter()
            .filter(|e| {
                !e.failed
                    && matches!(e.role, Role::User | Role::Assistant)
                    && !e.text.trim().is_empty()
            })
            .map(|e| ChatTurn {
                role: e.role,
                content: e.text.clone(),
            })
            .collect();
        turns.push(ChatTurn {
            role: Role::User,
            content: prompt.clone(),
        });
        let mut req = match &agent {
            Some(a) => {
                let mut r = modes::agent_req(a, prompt.clone());
                r.turns = turns;
                r
            }
            None => ChatRequest {
                system: None,
                turns,
                model: None,
            },
        };
        if let Some(m) = &self.model_override {
            req.model = Some(m.clone());
        }

        // Old turns are folded into a running summary by the auxiliary account(s).
        let policy = self.routing.policy.clone();
        let aux = if self.routing.auxiliary.iter().any(|s| s.enabled) {
            balance::rank(
                &self.routing.auxiliary,
                &self.accounts,
                &self.usage,
                self.now,
                &policy,
                None,
            )
        } else {
            vec![]
        };
        let cut = (!aux.is_empty())
            .then(|| {
                compact::cut_point(
                    &req.turns,
                    policy.history_budget_tokens as usize,
                    policy.keep_last_turns as usize,
                )
            })
            .flatten();
        let prev = self.summary.clone();
        let msg_tx = self.tx.clone();

        self.push_user(&prompt);
        let lane = self.alloc_lanes(1);
        let who_agent = agent.as_ref().map(|a| a.name.clone());
        self.spawn_run(move |core, cancel, ui, session| async move {
            let mut req = req;
            if let Some(cut) = cut {
                let summary = compact_history(&core, &req, cut, prev, aux, &cancel, &msg_tx).await;
                if let Some(s) = summary {
                    req.turns = compact::apply(&req.turns, &s.text, s.covered);
                    let _ = msg_tx.send(Msg::Summary(s));
                }
            }
            if let Ok(out) = core.run(lane, chain, req, cancel, ui).await {
                let m = Message {
                    role: Role::Assistant,
                    content: out.text,
                    agent_name: who_agent,
                    account_name: Some(out.account),
                };
                let _ = core.store.append_message(&core.vault, session, &m).await;
            }
        });
    }

    fn push_user(&mut self, prompt: &str) {
        self.entries.push(Entry::new(Role::User, "you", prompt));
        self.scroll_up = 0;
    }

    fn alloc_lanes(&mut self, n: u64) -> LaneId {
        let base = self.next_lane;
        self.next_lane += n.max(1);
        base
    }

    /// Ensures a session exists, persists the latest user entry, then runs `f`.
    fn spawn_run<F, Fut>(&mut self, f: F)
    where
        F: FnOnce(Core, CancellationToken, mpsc::UnboundedSender<(LaneId, RunEvent)>, Uuid) -> Fut
            + Send
            + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        self.spawn_task(true, f)
    }

    /// Like `spawn_run`; `persist_user` is false when the user entry was already stored
    /// (approving or revising a plan).
    fn spawn_task<F, Fut>(&mut self, persist_user: bool, f: F)
    where
        F: FnOnce(Core, CancellationToken, mpsc::UnboundedSender<(LaneId, RunEvent)>, Uuid) -> Fut
            + Send
            + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let Some(core) = self.core.clone() else {
            return;
        };
        let new_session = self.session_id.is_none();
        let session = *self.session_id.get_or_insert_with(Uuid::new_v4);
        let user_text = self
            .entries
            .iter()
            .rev()
            .find(|e| e.role == Role::User)
            .map(|e| e.text.clone())
            .unwrap_or_default();
        let title: String = user_text.chars().take(48).collect();
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        self.active_runs += 1;
        let (tx, ui) = (self.tx.clone(), self.lane_tx.clone());
        tokio::spawn(async move {
            if new_session
                && let Err(e) = core
                    .store
                    .create_session(&core.vault, session, &title)
                    .await
            {
                let _ = tx.send(Msg::Notice(format!("session: {e:#}")));
            }
            if persist_user {
                let m = Message {
                    role: Role::User,
                    content: user_text,
                    agent_name: None,
                    account_name: None,
                };
                let _ = core.store.append_message(&core.vault, session, &m).await;
            }
            f(core, cancel, ui, session).await;
            let _ = tx.send(Msg::RunDone);
        });
    }

    fn command(&mut self, line: &str) {
        let (cmd, rest) = line
            .split_once(char::is_whitespace)
            .map(|(a, b)| (a, b.trim()))
            .unwrap_or((line, ""));
        match cmd {
            "help" | "h" | "?" => self.modal = Some(Modal::Help),
            "new" => self.new_session(),
            "sessions" => {
                self.tab = Tab::Sessions;
                self.on_tab_enter();
            }
            "usage" => {
                self.tab = Tab::Usage;
                self.on_tab_enter();
            }
            "accounts" | "account" => {
                self.tab = Tab::Accounts;
                self.on_tab_enter();
            }
            "agents" | "agent" => self.tab = Tab::Agents,
            "clear" => {
                self.entries.clear();
                self.lanes.clear();
                self.summary = None;
            }
            "plan" => self.toggle_plan_mode(),
            "quit" | "exit" | "q" => self.quit = true,
            "lock" => self.lock(),
            "passwd" => {
                let mut f = Form::new(
                    FormKind::Passwd,
                    "Change master password",
                    vec![
                        Field::secret(FieldId::NewPw, "New password", ""),
                        Field::secret(FieldId::ConfirmPw, "Confirm", ""),
                    ],
                );
                f.status =
                    Some("only the key wrapper changes; stored data is not re-encrypted".into());
                self.modal = Some(Modal::Form(f));
            }
            "model" => {
                if rest.is_empty() {
                    let items = self.all_model_items();
                    if items.is_empty() {
                        self.notice = Some(
                            "no model lists yet - press m on the Accounts tab, or /model <name>"
                                .into(),
                        );
                    } else {
                        self.modal = Some(Modal::Picker(Picker {
                            title: "Model for the chat".into(),
                            items,
                            filter: String::new(),
                            sel: 0,
                            purpose: PickPurpose::ChatModel,
                        }));
                    }
                    return;
                }
                self.model_override = Some(rest.to_string()).filter(|s| s != "reset");
                self.notice = Some(match &self.model_override {
                    Some(m) => format!("model override: {m}"),
                    None => "model override cleared".into(),
                });
            }
            "rename" => {
                if rest.is_empty() {
                    self.notice = Some("usage: /rename <title>".into());
                    return;
                }
                if let (Some(id), Some(core)) = (self.session_id, self.core.clone()) {
                    let (t, tx) = (rest.to_string(), self.tx.clone());
                    tokio::spawn(async move {
                        let _ = core.store.rename_session(&core.vault, id, &t).await;
                        let _ = tx.send(Msg::Notice("session renamed".into()));
                    });
                } else {
                    self.notice = Some("send a message first".into());
                }
            }
            "use" => {
                if let Some((id, name)) =
                    modes::resolve_agent(&self.agents, rest).map(|a| (a.id, a.name.clone()))
                {
                    self.target = Some(Target::Agent(id));
                    self.notice = Some(format!("using agent '{name}'"));
                } else if let Some((id, name)) =
                    self.find_account(rest).map(|a| (a.id, a.name.clone()))
                {
                    self.target = Some(Target::Account(id));
                    self.notice = Some(format!("using account '{name}'"));
                } else {
                    self.notice = Some(format!("no agent/account named '{rest}'"));
                }
            }
            "fanout" => self.cmd_fanout(rest),
            "orchestrate" | "orch" => self.cmd_orchestrate(rest),
            other => self.notice = Some(format!("unknown command /{other} - try /help")),
        }
    }

    fn cmd_fanout(&mut self, rest: &str) {
        let mut accts: Vec<Account> = vec![];
        let mut tail = rest;
        // Names may contain spaces, so match known account names instead of splitting.
        while let Some((Target::Account(id), after)) = self.take_name(tail, true) {
            if let Some(a) = self.accounts.iter().find(|a| a.id == id) {
                accts.push(a.clone());
            }
            tail = after;
            match tail.strip_prefix(',') {
                Some(t) => tail = t.trim_start(),
                None => break,
            }
        }
        if accts.len() < 2 || tail.trim().is_empty() {
            self.notice =
                Some("usage: /fanout <account>,<account>[,...] <prompt>  (2+ accounts)".into());
            return;
        }
        let prompt = tail;
        let prompt = prompt.trim().to_string();
        self.push_user(&format!(
            "/fanout {} - {}",
            accts
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            prompt
        ));
        let base = self.alloc_lanes(accts.len() as u64);
        self.spawn_run(move |core, cancel, ui, session| async move {
            let res = modes::fan_out(&core, accts, prompt, base, cancel, ui).await;
            for r in res.into_iter().flatten() {
                let m = Message {
                    role: Role::Assistant,
                    content: r.text,
                    agent_name: None,
                    account_name: Some(r.account),
                };
                let _ = core.store.append_message(&core.vault, session, &m).await;
            }
        });
    }

    fn cmd_orchestrate(&mut self, task: &str) {
        if task.is_empty() {
            self.notice = Some("usage: /orchestrate <task>".into());
            return;
        }
        self.orchestrate_task(task.to_string(), format!("/orchestrate {task}"));
    }

    /// Plan → execute → synthesize without review. `shown` is the user line in the chat.
    fn orchestrate_task(&mut self, task: String, shown: String) {
        let team = match self.team() {
            Ok(t) => t,
            Err(e) => {
                self.notice = Some(e);
                return;
            }
        };
        let context = self.summary.as_ref().map(|s| s.text.clone());
        self.push_user(&shown);
        let base = self.alloc_lanes(64);
        self.spawn_run(move |core, cancel, ui, session| async move {
            let r = modes::orchestrate(&core, team, task, context, base, cancel, ui).await;
            if let Ok(o) = r.final_text {
                let m = Message {
                    role: Role::Assistant,
                    content: o.text,
                    agent_name: None,
                    account_name: Some(o.account),
                };
                let _ = core.store.append_message(&core.vault, session, &m).await;
            }
        });
    }

    // ------------------------------------------------------------------ routing, plan mode

    /// Planner/executor/synthesizer accounts for a multi-step run. The planner comes from the
    /// Routing tab, or from the active target when none is configured.
    fn team(&mut self) -> Result<modes::Team, String> {
        let policy = self.routing.policy.clone();
        let rank = |slots: &[Slot], prefer, app: &Self| {
            balance::rank(slots, &app.accounts, &app.usage, app.now, &policy, prefer)
        };
        let planner = if self.routing.planner.iter().any(|s| s.enabled) {
            rank(&self.routing.planner, self.planner_last, self)
        } else {
            self.target
                .and_then(|t| self.chain_for_target(t))
                .map(|(c, _)| c)
                .unwrap_or_default()
        };
        if planner.is_empty() {
            return Err(
                "no planner available: set one on the Routing tab or choose a target with /use"
                    .into(),
            );
        }
        self.planner_last = planner.first().map(|a| a.id);
        Ok(modes::Team {
            agents: self.agents.clone(),
            accounts: self.accounts.clone(),
            synth: rank(&self.routing.synthesizer, None, self),
            planner,
            pool: self.routing.executors.clone(),
            usage: self.usage.clone(),
            now: self.now,
            policy,
        })
    }

    /// `Shift+Tab` in Chat, like Claude Code's mode cycling: normal → plan → auto.
    fn cycle_mode(&mut self) {
        self.set_mode(match self.mode {
            Mode::Normal => Mode::Plan,
            Mode::Plan => Mode::Auto,
            Mode::Auto => Mode::Normal,
        });
    }

    fn toggle_plan_mode(&mut self) {
        self.set_mode(if self.mode == Mode::Plan {
            Mode::Normal
        } else {
            Mode::Plan
        });
    }

    fn set_mode(&mut self, m: Mode) {
        self.mode = m;
        self.notice = Some(
            match m {
                Mode::Normal => "mode: chat - messages go straight to the target",
                Mode::Plan => "mode: plan - messages are planned, reviewed by you, then executed",
                Mode::Auto => "mode: auto - messages are planned and executed without review",
            }
            .into(),
        );
    }

    /// Asks the planner for steps; the review modal opens when they arrive.
    /// `task` is what the planner is told, `original` what the user typed.
    fn start_plan(&mut self, persist_user: bool, task: String, original: String) {
        let team = match self.team() {
            Ok(t) => t,
            Err(e) => {
                self.notice = Some(e);
                return;
            }
        };
        if persist_user {
            self.push_user(&original);
        }
        let context = self.summary.as_ref().map(|s| s.text.clone());
        let base = self.alloc_lanes(64);
        let tx = self.tx.clone();
        self.spawn_task(persist_user, move |core, cancel, ui, _session| async move {
            match modes::plan(&core, &team, &task, context.as_deref(), base, cancel, ui).await {
                Ok(steps) => {
                    let _ = tx.send(Msg::PlanReady {
                        task: original,
                        steps,
                        base,
                    });
                }
                Err(e) => {
                    let _ = tx.send(Msg::Notice(e));
                }
            }
        });
    }

    fn approve_plan(&mut self, pr: PlanReview) {
        let team = match self.team() {
            Ok(t) => t,
            Err(e) => {
                self.notice = Some(e);
                return;
            }
        };
        let PlanReview {
            task, steps, base, ..
        } = pr;
        self.spawn_task(false, move |core, cancel, ui, session| async move {
            let listing = steps
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let who = if s.agent.is_empty() {
                        String::new()
                    } else {
                        format!("@{}: ", s.agent)
                    };
                    format!("{}. {who}{}", i + 1, s.task)
                })
                .collect::<Vec<_>>()
                .join("\n");
            let m = Message {
                role: Role::Assistant,
                content: format!("Approved plan:\n{listing}"),
                agent_name: Some("planner".into()),
                account_name: None,
            };
            let _ = core.store.append_message(&core.vault, session, &m).await;
            let r = modes::run_steps(&core, &team, &task, steps, base, cancel, ui).await;
            if let Ok(o) = r.final_text {
                let m = Message {
                    role: Role::Assistant,
                    content: o.text,
                    agent_name: None,
                    account_name: Some(o.account),
                };
                let _ = core.store.append_message(&core.vault, session, &m).await;
            }
        });
    }

    pub fn routing_rows(&self) -> Vec<RoutingRow> {
        let mut rows = vec![];
        for sec in 0..4 {
            rows.push(RoutingRow::Header(sec));
            rows.extend((0..self.routing.section(sec).len()).map(|i| RoutingRow::Slot(sec, i)));
        }
        rows.extend((0..POLICY_ROWS).map(RoutingRow::Policy));
        rows
    }

    fn save_routing(&mut self) {
        let Some(core) = self.core.clone() else {
            return;
        };
        let (r, tx) = (self.routing.clone(), self.tx.clone());
        tokio::spawn(async move {
            if let Err(e) = core.store.save_setting(&core.vault, "routing", &r).await {
                let _ = tx.send(Msg::Notice(format!("saving routing: {e:#}")));
            }
        });
    }

    fn fetch_models(&mut self, a: Account) {
        self.models_asked.insert(a.id);
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let r = providers::catalog::list_models(&a)
                .await
                .map_err(|e| e.message);
            let _ = tx.send(Msg::Models(a.id, r));
        });
    }

    /// `account › model` for every known model, used by `/model`.
    fn all_model_items(&self) -> Vec<(String, String)> {
        self.accounts
            .iter()
            .flat_map(|a| {
                self.models
                    .get(&a.id)
                    .into_iter()
                    .flatten()
                    .map(move |m| (format!("{} › {m}", a.name), m.clone()))
            })
            .collect()
    }

    fn form_model_items(&self, f: &Form) -> Vec<(String, String)> {
        if let FormKind::Account { edit: Some(id) } = &f.kind
            && let Some(list) = self.models.get(id)
        {
            return list.iter().map(|m| (m.clone(), m.clone())).collect();
        }
        self.all_model_items()
    }

    fn open_slot_model_picker(&mut self, sec: usize, i: usize) {
        let Some(slot) = self.routing.section(sec).get(i) else {
            return;
        };
        let id = slot.account_id;
        let mut items = vec![("(account default)".to_string(), String::new())];
        items.extend(
            self.models
                .get(&id)
                .into_iter()
                .flatten()
                .map(|m| (m.clone(), m.clone())),
        );
        if items.len() == 1 {
            self.notice = Some(match self.models_err.get(&id) {
                Some(e) => format!("no model list: {e} (type a name to set it)"),
                None => "no model list yet - type a name to set it".into(),
            });
        }
        self.modal = Some(Modal::Picker(Picker {
            title: "Model for this slot".into(),
            items,
            filter: String::new(),
            sel: 0,
            purpose: PickPurpose::SlotModel(sec, i),
        }));
    }

    fn apply_pick(&mut self, purpose: PickPurpose, value: String) {
        match purpose {
            PickPurpose::AddSlot(sec) => {
                if let Ok(id) = value.parse::<Uuid>() {
                    self.routing.section_mut(sec).push(Slot::new(id));
                    self.save_routing();
                    self.notice = Some("added - press m to choose its model".into());
                }
            }
            PickPurpose::SlotModel(sec, i) => {
                if let Some(slot) = self.routing.section_mut(sec).get_mut(i) {
                    slot.model = Some(value).filter(|m| !m.is_empty());
                    self.save_routing();
                }
            }
            PickPurpose::ChatModel => {
                self.notice = Some(format!("model override: {value}"));
                self.model_override = Some(value);
            }
            PickPurpose::FormModel(mut f) => {
                if let Some(fld) = f.fields.iter_mut().find(|x| x.id == FieldId::Model) {
                    fld.value = value;
                }
                self.modal = Some(Modal::Form(*f));
            }
        }
    }

    fn on_key_routing(&mut self, k: KeyEvent) {
        let rows = self.routing_rows();
        let Some(row) = rows.get(self.sel[Tab::Routing.index()]).copied() else {
            return;
        };
        let (sec, idx) = match row {
            RoutingRow::Header(s) => (Some(s), None),
            RoutingRow::Slot(s, i) => (Some(s), Some(i)),
            RoutingRow::Policy(_) => (None, None),
        };
        match (k.code, row) {
            (KeyCode::Char('a'), _) if sec.is_some() => {
                let items: Vec<(String, String)> = self
                    .accounts
                    .iter()
                    .filter(|a| a.enabled)
                    .map(|a| (format!("{} ({})", a.name, a.kind.short()), a.id.to_string()))
                    .collect();
                if items.is_empty() {
                    self.notice = Some("add an account first".into());
                } else {
                    self.modal = Some(Modal::Picker(Picker {
                        title: format!(
                            "Add to {}",
                            ROUTING_SECTIONS[sec.unwrap_or(0)].to_lowercase()
                        ),
                        items,
                        filter: String::new(),
                        sel: 0,
                        purpose: PickPurpose::AddSlot(sec.unwrap_or(0)),
                    }));
                }
            }
            (KeyCode::Char('m'), RoutingRow::Slot(s, i)) => self.open_slot_model_picker(s, i),
            (KeyCode::Char('f'), RoutingRow::Slot(s, i)) => {
                const LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
                let kind = self
                    .routing
                    .section(s)
                    .get(i)
                    .and_then(|sl| self.accounts.iter().find(|a| a.id == sl.account_id))
                    .map(|a| a.kind);
                if kind != Some(ProviderKind::ClaudeCli) {
                    self.notice = Some("effort applies to Claude Code accounts only".into());
                } else if let Some(slot) = self.routing.section_mut(s).get_mut(i) {
                    slot.effort = match slot.effort.as_deref() {
                        None | Some("") => Some(LEVELS[0].to_string()),
                        Some(cur) => LEVELS
                            .iter()
                            .position(|l| *l == cur)
                            .and_then(|p| LEVELS.get(p + 1))
                            .map(|l| l.to_string()),
                    };
                    self.save_routing();
                }
            }
            (KeyCode::Char(' '), RoutingRow::Slot(s, i)) => {
                if let Some(slot) = self.routing.section_mut(s).get_mut(i) {
                    slot.enabled = !slot.enabled;
                    self.save_routing();
                }
            }
            (KeyCode::Char('J'), RoutingRow::Slot(s, i)) => {
                let v = self.routing.section_mut(s);
                if i + 1 < v.len() {
                    v.swap(i, i + 1);
                    self.sel[Tab::Routing.index()] += 1;
                    self.save_routing();
                }
            }
            (KeyCode::Char('K'), RoutingRow::Slot(s, i)) if i > 0 => {
                self.routing.section_mut(s).swap(i, i - 1);
                self.sel[Tab::Routing.index()] -= 1;
                self.save_routing();
            }
            (KeyCode::Char('d') | KeyCode::Delete, RoutingRow::Slot(s, i)) => {
                self.routing.section_mut(s).remove(i);
                self.save_routing();
            }
            (KeyCode::Char('+' | '=' | ' '), RoutingRow::Policy(n)) => self.adjust_policy(n, 1),
            (KeyCode::Char('-'), RoutingRow::Policy(n)) => self.adjust_policy(n, -1),
            _ => {}
        }
        let _ = idx;
    }

    fn adjust_policy(&mut self, row: usize, d: i32) {
        let p = &mut self.routing.policy;
        match row {
            0 => p.skip_above = ((p.skip_above + 0.05 * d as f64) * 100.0).round() / 100.0,
            1 => p.prefer_local = !p.prefer_local,
            2 => {
                p.history_budget_tokens =
                    (p.history_budget_tokens as i64 + 4_000 * d as i64).clamp(2_000, 200_000) as u32
            }
            _ => p.keep_last_turns = (p.keep_last_turns as i32 + d).clamp(2, 30) as u8,
        }
        p.skip_above = p.skip_above.clamp(0.30, 1.0);
        self.save_routing();
    }
}

/// Folds the old part of `req.turns` into a summary with the auxiliary chain.
/// Reuses `prev` when fewer than 4 turns have piled up since it was written.
async fn compact_history(
    core: &Core,
    req: &ChatRequest,
    cut: usize,
    prev: Option<compact::Summary>,
    aux: Vec<Account>,
    cancel: &CancellationToken,
    tx: &mpsc::UnboundedSender<Msg>,
) -> Option<compact::Summary> {
    if let Some(p) = prev
        .as_ref()
        .filter(|p| cut.saturating_sub(p.covered) < 4 && p.covered < req.turns.len())
    {
        return Some(p.clone());
    }
    let fresh = compact::to_summarize(&req.turns, cut, prev.as_ref());
    let prompt = compact::summary_prompt(prev.as_ref(), &fresh);
    let _ = tx.send(Msg::Notice("summarizing older messages...".into()));
    let (quiet_tx, _quiet_rx) = mpsc::unbounded_channel();
    let out = core
        .run(
            0,
            aux,
            modes::user_req(None, prompt),
            cancel.clone(),
            quiet_tx,
        )
        .await
        .ok()?;
    let text = out.text.trim().to_string();
    (!text.is_empty()).then_some(compact::Summary { text, covered: cut })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_bar_hit_testing() {
        // "│ 1 Chat │ 2 Accounts │..." : border, then " 1 Chat " (8 cols), divider...
        assert_eq!(Tab::at_column(0), None);
        assert_eq!(Tab::at_column(1), Some(Tab::Chat));
        assert_eq!(Tab::at_column(8), Some(Tab::Chat));
        assert_eq!(Tab::at_column(9), None); // divider
        assert_eq!(Tab::at_column(10), Some(Tab::Accounts));
        assert_eq!(Tab::at_column(200), None);
        for t in Tab::ALL {
            assert_eq!(
                Tab::ALL
                    .iter()
                    .filter(|x| x.mnemonic() == t.mnemonic())
                    .count(),
                1
            );
        }
    }
}
