//! Minimal modal form: text / secret / choice fields.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldId {
    Provider,
    Name,
    ApiKey,
    BaseUrl,
    Model,
    CliPath,
    ProfileDir,
    Budget,
    Role,
    Prompt,
    Account,
    Fallbacks,
    NewPw,
    ConfirmPw,
    StepText,
    Feedback,
}

#[derive(Clone, Debug)]
pub struct Field {
    pub id: FieldId,
    pub label: &'static str,
    pub value: String,
    pub secret: bool,
    pub choices: Vec<String>,
    pub choice: usize,
    pub hint: String,
}

impl Field {
    pub fn text(id: FieldId, label: &'static str, value: impl Into<String>) -> Self {
        Self {
            id,
            label,
            value: value.into(),
            secret: false,
            choices: vec![],
            choice: 0,
            hint: String::new(),
        }
    }
    pub fn secret(id: FieldId, label: &'static str, value: impl Into<String>) -> Self {
        Self {
            secret: true,
            ..Self::text(id, label, value)
        }
    }
    pub fn choice(id: FieldId, label: &'static str, choices: Vec<String>, selected: usize) -> Self {
        Self {
            choices,
            choice: selected,
            ..Self::text(id, label, "")
        }
    }
    pub fn hint(mut self, h: impl Into<String>) -> Self {
        self.hint = h.into();
        self
    }
    pub fn is_choice(&self) -> bool {
        !self.choices.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormKind {
    Account {
        edit: Option<uuid::Uuid>,
    },
    Agent {
        edit: Option<uuid::Uuid>,
    },
    Passwd,
    /// Edit the task text of step `n` in the plan under review.
    PlanStep(usize),
    /// Ask the planner to revise the plan under review.
    PlanFeedback,
}

#[derive(Clone, Debug)]
pub struct Form {
    pub kind: FormKind,
    pub title: String,
    pub fields: Vec<Field>,
    pub focus: usize,
    pub status: Option<String>,
}

impl Form {
    pub fn new(kind: FormKind, title: impl Into<String>, fields: Vec<Field>) -> Self {
        Self {
            kind,
            title: title.into(),
            fields,
            focus: 0,
            status: None,
        }
    }

    pub fn get(&self, id: FieldId) -> Option<&Field> {
        self.fields.iter().find(|f| f.id == id)
    }

    pub fn value(&self, id: FieldId) -> String {
        self.get(id)
            .map(|f| {
                if f.is_choice() {
                    f.choices[f.choice].clone()
                } else {
                    f.value.trim().to_string()
                }
            })
            .unwrap_or_default()
    }

    pub fn choice_idx(&self, id: FieldId) -> usize {
        self.get(id).map(|f| f.choice).unwrap_or(0)
    }

    /// Which fields are shown: the account form hides irrelevant ones per provider.
    pub fn visible(&self, f: &Field) -> bool {
        use crate::model::ProviderKind as K;
        if !matches!(self.kind, FormKind::Account { .. }) {
            return true;
        }
        let kind = K::ALL[self.choice_idx(FieldId::Provider)];
        match f.id {
            FieldId::ApiKey => kind.needs_api_key(),
            FieldId::BaseUrl => kind.is_local(),
            FieldId::CliPath => kind.is_cli(),
            FieldId::ProfileDir => kind.profile_env().is_some(),
            FieldId::Model => true,
            _ => true,
        }
    }

    pub fn visible_indices(&self) -> Vec<usize> {
        (0..self.fields.len())
            .filter(|&i| self.visible(&self.fields[i]))
            .collect()
    }

    pub fn move_focus(&mut self, delta: isize) {
        let vis = self.visible_indices();
        if vis.is_empty() {
            return;
        }
        let pos = vis.iter().position(|&i| i == self.focus).unwrap_or(0) as isize;
        let n = vis.len() as isize;
        self.focus = vis[((pos + delta).rem_euclid(n)) as usize];
    }

    pub fn type_char(&mut self, c: char) {
        let f = &mut self.fields[self.focus];
        if !f.is_choice() {
            f.value.push(c);
        }
    }

    pub fn backspace(&mut self) {
        let f = &mut self.fields[self.focus];
        if !f.is_choice() {
            f.value.pop();
        }
    }

    /// Returns true if a choice changed.
    pub fn cycle_choice(&mut self, delta: isize) -> bool {
        let f = &mut self.fields[self.focus];
        if !f.is_choice() {
            return false;
        }
        let n = f.choices.len() as isize;
        f.choice = ((f.choice as isize + delta).rem_euclid(n)) as usize;
        true
    }
}
