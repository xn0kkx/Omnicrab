//! History compaction: old turns are replaced by a running summary so the CLIs
//! (which are single-shot and get the whole history flattened into one prompt)
//! don't pay for the full conversation on every message.
//!
//! Token counts are estimated as `chars / 4`. Selection and prompt building are
//! pure; the model call itself goes through `Core::run`.

use crate::model::Role;
use crate::providers::ChatTurn;

/// Rough token estimate (good enough for a threshold).
pub fn estimate_tokens(s: &str) -> usize {
    s.chars().count().div_ceil(4)
}

pub fn estimate_turns(turns: &[ChatTurn]) -> usize {
    turns.iter().map(|t| estimate_tokens(&t.content) + 4).sum()
}

/// Running summary of the turns before `covered`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    pub text: String,
    /// Number of leading turns (counting from index 1) the text stands for.
    pub covered: usize,
}

/// How many turns after the first one should be folded into the summary.
/// `None` when the history fits the budget or there is nothing old enough.
pub fn cut_point(turns: &[ChatTurn], budget_tokens: usize, keep_last: usize) -> Option<usize> {
    if estimate_turns(turns) <= budget_tokens {
        return None;
    }
    let tail_start = turns.len().saturating_sub(keep_last.max(1));
    (tail_start > 1).then(|| tail_start - 1)
}

/// Turns that still need summarizing: those in `1..=cut` not yet covered.
pub fn to_summarize(turns: &[ChatTurn], cut: usize, prev: Option<&Summary>) -> Vec<ChatTurn> {
    let done = prev.map(|s| s.covered.min(cut)).unwrap_or(0);
    turns[1 + done..=cut].to_vec()
}

pub fn summary_prompt(prev: Option<&Summary>, fresh: &[ChatTurn], focus: Option<&str>) -> String {
    let mut p = String::from(
        "Update the running summary of a conversation so a model can continue it without the \
         original messages. Keep: the user's goal, decisions made, constraints, exact values, \
         file names, identifiers and error messages mentioned, and next steps. Be terse. \
         Reply with the summary only.\n\n",
    );
    if let Some(f) = focus.filter(|f| !f.trim().is_empty()) {
        p.push_str(&format!("Pay particular attention to: {}\n\n", f.trim()));
    }
    if let Some(s) = prev.filter(|s| !s.text.is_empty()) {
        p.push_str(&format!("Current summary:\n{}\n\n", s.text));
    }
    p.push_str("New messages to fold in:\n");
    for t in fresh {
        let who = match t.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
        };
        p.push_str(&format!("[{who}] {}\n", t.content.trim()));
    }
    p
}

/// First turn + summary + the turns after `cut`.
pub fn apply(turns: &[ChatTurn], summary: &str, cut: usize) -> Vec<ChatTurn> {
    let mut out = Vec::with_capacity(turns.len() - cut + 1);
    out.push(turns[0].clone());
    out.push(ChatTurn {
        role: Role::User,
        content: format!("[Summary of the earlier conversation]\n{summary}"),
    });
    out.extend_from_slice(&turns[cut + 1..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(role: Role, n: usize) -> ChatTurn {
        ChatTurn {
            role,
            content: "x".repeat(n),
        }
    }

    fn convo(n: usize, size: usize) -> Vec<ChatTurn> {
        (0..n)
            .map(|i| {
                turn(
                    if i % 2 == 0 {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    size,
                )
            })
            .collect()
    }

    #[test]
    fn short_history_is_untouched() {
        assert_eq!(cut_point(&convo(6, 40), 1000, 4), None);
    }

    #[test]
    fn long_history_keeps_first_and_tail() {
        let t = convo(12, 400); // ~1250 tokens
        let cut = cut_point(&t, 500, 4).unwrap();
        assert_eq!(cut, 7); // turns 1..=7 summarized, 8..=11 kept
        let out = apply(&t, "S", cut);
        assert_eq!(out.len(), 1 + 1 + 4);
        assert!(out[1].content.contains('S'));
    }

    #[test]
    fn nothing_to_cut_when_tail_covers_everything() {
        assert_eq!(cut_point(&convo(4, 4000), 100, 6), None);
    }

    #[test]
    fn incremental_update_only_folds_new_turns() {
        let t = convo(12, 400);
        let prev = Summary {
            text: "old".into(),
            covered: 4,
        };
        let fresh = to_summarize(&t, 7, Some(&prev));
        assert_eq!(fresh.len(), 3); // turns 5..=7
        let p = summary_prompt(Some(&prev), &fresh, Some("the API keys"));
        assert!(p.contains("particular attention to: the API keys"));
        assert!(p.contains("Current summary:\nold"));
        assert_eq!(to_summarize(&t, 7, None).len(), 7);
    }
}
