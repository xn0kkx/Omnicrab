//! State of the question picker (like Claude Code's AskUserQuestion). Pure, no rendering.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::input::InputBuf;
use crate::orchestrator::questions::Question;

/// Where the answers go once submitted.
#[derive(Debug)]
pub enum Origin {
    /// The lead loop is waiting for them (`None` = the user skipped).
    Lead(tokio::sync::oneshot::Sender<Option<Vec<(String, String)>>>),
    /// The chat model asked; the answers are sent as the next message.
    Chat,
}

pub enum Outcome {
    Pending,
    Cancel,
    Submit,
}

pub struct QuestionsState {
    pub questions: Vec<Question>,
    pub origin: Origin,
    /// Current question (tab).
    pub tab: usize,
    /// Highlighted row per question; the last row is "Other".
    pub cursor: Vec<usize>,
    /// Selection per question, one flag per option plus one for "Other".
    pub picked: Vec<Vec<bool>>,
    /// Free-text answer per question.
    pub other: Vec<InputBuf>,
    /// Typing into "Other" of the current question.
    pub typing: bool,
}

impl QuestionsState {
    pub fn new(questions: Vec<Question>, origin: Origin) -> Self {
        let n = questions.len();
        Self {
            picked: questions
                .iter()
                .map(|q| vec![false; q.options.len() + 1])
                .collect(),
            cursor: vec![0; n],
            other: vec![InputBuf::default(); n],
            questions,
            origin,
            tab: 0,
            typing: false,
        }
    }

    /// Rows of question `q`: its options plus "Other".
    pub fn rows(&self, q: usize) -> usize {
        self.questions[q].options.len() + 1
    }

    pub fn is_other(&self, q: usize, row: usize) -> bool {
        row == self.questions[q].options.len()
    }

    pub fn answered(&self, q: usize) -> bool {
        let n = self.questions[q].options.len();
        self.picked[q][..n].iter().any(|p| *p)
            || (self.picked[q][n] && !self.other[q].as_str().trim().is_empty())
    }

    fn first_unanswered(&self) -> Option<usize> {
        (0..self.questions.len()).find(|q| !self.answered(*q))
    }

    /// Marks `row` of question `q`. Single-choice questions keep one selection.
    fn select(&mut self, q: usize, row: usize) {
        if self.questions[q].multi {
            self.picked[q][row] = !self.picked[q][row];
        } else {
            self.picked[q].iter_mut().for_each(|p| *p = false);
            self.picked[q][row] = true;
        }
    }

    /// Moves on after answering: next question, or submit when everything is answered.
    fn advance(&mut self) -> Outcome {
        match self.first_unanswered() {
            None => Outcome::Submit,
            Some(next) => {
                self.tab = if self.tab + 1 < self.questions.len() && !self.answered(self.tab + 1) {
                    self.tab + 1
                } else {
                    next
                };
                Outcome::Pending
            }
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) -> Outcome {
        let q = self.tab;
        if self.typing {
            match k.code {
                KeyCode::Esc => self.typing = false,
                KeyCode::Enter => {
                    self.typing = false;
                    let row = self.questions[q].options.len();
                    let has_text = !self.other[q].as_str().trim().is_empty();
                    if has_text {
                        // Re-selecting would toggle a multi-choice "Other" off.
                        if !(self.questions[q].multi && self.picked[q][row]) {
                            self.select(q, row);
                        }
                        return self.advance();
                    }
                    self.picked[q][row] = false;
                }
                KeyCode::Up | KeyCode::Down => {
                    self.typing = false;
                    return self.on_key(k);
                }
                KeyCode::Backspace => self.other[q].backspace(),
                KeyCode::Left => self.other[q].left(),
                KeyCode::Right => self.other[q].right(),
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.other[q].insert(c)
                }
                _ => {}
            }
            return Outcome::Pending;
        }
        let n = self.questions.len();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => return Outcome::Cancel,
            KeyCode::Left | KeyCode::BackTab => self.tab = (q + n - 1) % n,
            KeyCode::Right | KeyCode::Tab => self.tab = (q + 1) % n,
            KeyCode::Up | KeyCode::Char('k') => self.cursor[q] = self.cursor[q].saturating_sub(1),
            KeyCode::Char('p') if ctrl => self.cursor[q] = self.cursor[q].saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.cursor[q] = (self.cursor[q] + 1).min(self.rows(q) - 1)
            }
            KeyCode::Char('n') if ctrl => {
                self.cursor[q] = (self.cursor[q] + 1).min(self.rows(q) - 1)
            }
            KeyCode::Home | KeyCode::PageUp => self.cursor[q] = 0,
            KeyCode::End | KeyCode::PageDown => self.cursor[q] = self.rows(q) - 1,
            // Space and the number keys only mark; they never move on or close the window.
            KeyCode::Char(c @ '1'..='9') => {
                let row = c as usize - '1' as usize;
                if row < self.rows(q) {
                    self.cursor[q] = row;
                    self.mark(q, row);
                }
            }
            KeyCode::Char(' ') => self.mark(q, self.cursor[q]),
            KeyCode::Enter => return self.confirm(q),
            _ => {}
        }
        Outcome::Pending
    }

    /// Marks `row`: toggles it (multiple choice) or makes it the only one (single choice).
    /// On "Other" it starts typing instead.
    fn mark(&mut self, q: usize, row: usize) {
        if self.is_other(q, row) {
            self.typing = true;
        } else {
            self.select(q, row);
        }
    }

    /// Enter: the highlighted row counts if nothing is marked yet, then go to the next
    /// question, or submit once every question has an answer.
    fn confirm(&mut self, q: usize) -> Outcome {
        let row = self.cursor[q];
        if self.is_other(q, row) {
            self.typing = true;
            return Outcome::Pending;
        }
        if !self.answered(q) {
            self.select(q, row);
        }
        self.advance()
    }

    /// `(header, answer)` per question, in order.
    pub fn answers(&self) -> Vec<(String, String)> {
        self.questions
            .iter()
            .enumerate()
            .map(|(i, q)| {
                let mut parts: Vec<String> = q
                    .options
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| self.picked[i][*j])
                    .map(|(_, o)| o.label.clone())
                    .collect();
                let other = self.other[i].as_str().trim();
                if self.picked[i][q.options.len()] && !other.is_empty() {
                    parts.push(other.to_string());
                }
                (q.header.clone(), parts.join(", "))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::questions::{QOption, Question};

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn q(header: &str, multi: bool) -> Question {
        Question {
            header: header.into(),
            question: format!("{header}?"),
            multi,
            options: ["a", "b", "c"]
                .map(|l| QOption {
                    label: l.into(),
                    description: String::new(),
                })
                .to_vec(),
        }
    }

    #[test]
    fn arrows_move_the_highlight_and_clamp() {
        let mut s = QuestionsState::new(vec![q("One", false)], Origin::Chat);
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Down));
        assert_eq!(s.cursor[0], 2);
        for _ in 0..9 {
            s.on_key(key(KeyCode::Down));
        }
        assert_eq!(s.cursor[0], 3); // "Other" is the last row
        s.on_key(key(KeyCode::Up));
        assert_eq!(s.cursor[0], 2);
    }

    #[test]
    fn single_choice_enter_confirms_and_submits() {
        let mut s = QuestionsState::new(vec![q("One", false), q("Two", false)], Origin::Chat);
        s.on_key(key(KeyCode::Down));
        assert!(matches!(s.on_key(key(KeyCode::Enter)), Outcome::Pending));
        assert_eq!(s.tab, 1);
        s.on_key(key(KeyCode::Char('3')));
        assert_eq!(s.tab, 1, "number keys only mark");
        assert!(matches!(s.on_key(key(KeyCode::Enter)), Outcome::Submit));
        assert_eq!(
            s.answers(),
            [("One".into(), "b".into()), ("Two".into(), "c".into())]
        );
    }

    #[test]
    fn space_marks_without_closing() {
        // One single-choice question: Space used to answer and submit at once.
        let mut s = QuestionsState::new(vec![q("One", false)], Origin::Chat);
        assert!(matches!(
            s.on_key(key(KeyCode::Char(' '))),
            Outcome::Pending
        ));
        assert!(s.answered(0));
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Char(' ')));
        assert_eq!(s.answers()[0].1, "b", "single choice keeps one selection");
        assert!(matches!(s.on_key(key(KeyCode::Enter)), Outcome::Submit));
    }

    #[test]
    fn multi_choice_toggles_then_confirms() {
        let mut s = QuestionsState::new(vec![q("M", true)], Origin::Chat);
        s.on_key(key(KeyCode::Char(' ')));
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Char(' ')));
        s.on_key(key(KeyCode::Up));
        s.on_key(key(KeyCode::Char(' ')));
        s.on_key(key(KeyCode::Char(' ')));
        assert!(matches!(s.on_key(key(KeyCode::Enter)), Outcome::Submit));
        assert_eq!(s.answers()[0].1, "a, c");
    }

    #[test]
    fn arrows_leave_the_other_field() {
        let mut s = QuestionsState::new(vec![q("O", false)], Origin::Chat);
        s.on_key(key(KeyCode::End));
        s.on_key(key(KeyCode::Char(' ')));
        assert!(s.typing);
        s.on_key(key(KeyCode::Up));
        assert!(!s.typing);
        assert_eq!(s.cursor[0], 2);
    }

    #[test]
    fn other_takes_free_text() {
        let mut s = QuestionsState::new(vec![q("O", false)], Origin::Chat);
        s.on_key(key(KeyCode::Char('4')));
        assert!(s.typing);
        for c in "pix".chars() {
            s.on_key(key(KeyCode::Char(c)));
        }
        assert!(matches!(s.on_key(key(KeyCode::Enter)), Outcome::Submit));
        assert_eq!(s.answers()[0].1, "pix");
    }

    #[test]
    fn empty_other_does_not_count_and_esc_cancels() {
        let mut s = QuestionsState::new(vec![q("O", false)], Origin::Chat);
        s.on_key(key(KeyCode::Char('4')));
        s.on_key(key(KeyCode::Enter));
        assert!(!s.answered(0));
        assert!(matches!(s.on_key(key(KeyCode::Esc)), Outcome::Cancel));
    }

    #[test]
    fn enter_with_unanswered_questions_jumps_to_them() {
        let mut s = QuestionsState::new(vec![q("A", false), q("B", false)], Origin::Chat);
        s.on_key(key(KeyCode::Right));
        s.on_key(key(KeyCode::Enter));
        // "B" answered, "A" still open: go back to it instead of submitting.
        assert_eq!(s.tab, 0);
    }
}
