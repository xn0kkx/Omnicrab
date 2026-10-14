//! Multiple-choice questions a model can ask the user (like Claude Code's AskUserQuestion).
//!
//! The planner answers with `{"questions":[...]}` instead of steps; in chat the model ends
//! its reply with a fenced `omni-questions` block. Pure parsing, no I/O.

use serde::Deserialize;

pub const MAX_QUESTIONS: usize = 4;
pub const MAX_OPTIONS: usize = 4;
const FENCE: &str = "```omni-questions";

/// Appended to the system prompt of normal chats.
pub const CHAT_HINT: &str = "If you cannot continue without the user choosing between \
alternatives, end your reply with ONE fenced block instead of asking in prose:\n\
```omni-questions\n\
{\"questions\":[{\"header\":\"short label\",\"question\":\"full question?\",\"multi\":false,\
\"options\":[{\"label\":\"choice\",\"description\":\"what it means\"}]}]}\n\
```\n\
Use at most 4 questions with 2-4 options each; the user may also type their own answer. \
Only ask when the answer changes what you would do.";

#[derive(Debug, Deserialize, Clone, PartialEq)]
pub struct QOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
pub struct Question {
    #[serde(default)]
    pub header: String,
    pub question: String,
    #[serde(default)]
    pub multi: bool,
    pub options: Vec<QOption>,
}

#[derive(Deserialize)]
struct Wrapper {
    questions: Vec<Question>,
}

/// Drops unusable questions (fewer than 2 options) and applies the size limits.
fn sanitize(qs: Vec<Question>) -> Option<Vec<Question>> {
    let out: Vec<Question> = qs
        .into_iter()
        .filter(|q| !q.question.trim().is_empty() && q.options.len() >= 2)
        .take(MAX_QUESTIONS)
        .enumerate()
        .map(|(i, mut q)| {
            q.options.truncate(MAX_OPTIONS);
            let h = q.header.trim();
            q.header = if h.is_empty() {
                format!("Q{}", i + 1)
            } else {
                h.chars().take(14).collect()
            };
            q
        })
        .collect();
    (!out.is_empty()).then_some(out)
}

/// Accepts `{"questions":[...]}` or a bare array of questions.
pub fn parse_questions(json: &str) -> Option<Vec<Question>> {
    let json = json.trim();
    let qs = serde_json::from_str::<Wrapper>(json)
        .map(|w| w.questions)
        .or_else(|_| serde_json::from_str::<Vec<Question>>(json))
        .ok()?;
    sanitize(qs)
}

/// Looks for a `{"questions":...}` object anywhere in `text` (prose and code fences tolerated).
pub fn find_questions(text: &str) -> Option<Vec<Question>> {
    let (s, e) = (text.find('{')?, text.rfind('}')?);
    if e <= s {
        return None;
    }
    let w: Wrapper = serde_json::from_str(&text[s..=e]).ok()?;
    sanitize(w.questions)
}

/// Splits an `omni-questions` block off a chat reply. Returns the text without the block
/// and the parsed questions; text is returned unchanged when there is no valid block.
pub fn extract_questions(text: &str) -> (String, Option<Vec<Question>>) {
    let Some(start) = text.find(FENCE) else {
        return (text.to_string(), None);
    };
    let body = &text[start + FENCE.len()..];
    let (json, after) = match body.find("```") {
        Some(end) => (&body[..end], &body[end + 3..]),
        None => (body, ""),
    };
    match parse_questions(json) {
        Some(qs) => {
            let mut clean = text[..start].trim_end().to_string();
            let after = after.trim();
            if !after.is_empty() {
                clean.push_str("\n\n");
                clean.push_str(after);
            }
            (clean, Some(qs))
        }
        None => (text.to_string(), None),
    }
}

/// `- header: answer` lines for the follow-up message.
pub fn format_answers(answers: &[(String, String)]) -> String {
    answers
        .iter()
        .map(|(h, a)| format!("- {h}: {a}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q: &str = r#"{"questions":[{"header":"Gateway","question":"Which gateway?","options":[{"label":"Stripe","description":"cards"},{"label":"Mercado Pago"}]}]}"#;

    #[test]
    fn parses_wrapper_and_bare_array() {
        let qs = parse_questions(Q).unwrap();
        assert_eq!(qs[0].header, "Gateway");
        assert_eq!(qs[0].options[1].description, "");
        let bare = r#"[{"question":"Pick","options":[{"label":"a"},{"label":"b"}]}]"#;
        assert_eq!(parse_questions(bare).unwrap()[0].header, "Q1");
    }

    #[test]
    fn rejects_unusable_questions() {
        assert!(parse_questions("not json").is_none());
        assert!(parse_questions(r#"{"questions":[]}"#).is_none());
        // One option is not a choice.
        let one = r#"{"questions":[{"question":"Pick","options":[{"label":"a"}]}]}"#;
        assert!(parse_questions(one).is_none());
    }

    #[test]
    fn limits_sizes() {
        let opts = r#"{"label":"a"},{"label":"b"},{"label":"c"},{"label":"d"},{"label":"e"}"#;
        let q = format!(r#"{{"question":"x","options":[{opts}]}}"#);
        let json = format!(r#"{{"questions":[{q},{q},{q},{q},{q}]}}"#);
        let qs = parse_questions(&json).unwrap();
        assert_eq!(qs.len(), MAX_QUESTIONS);
        assert_eq!(qs[0].options.len(), MAX_OPTIONS);
    }

    #[test]
    fn planner_reply_with_prose() {
        let text = format!("Before I plan:\n```json\n{Q}\n```");
        assert_eq!(find_questions(&text).unwrap()[0].question, "Which gateway?");
        // A plan is not a questions object.
        assert!(find_questions(r#"[{"agent":"","task":"do"}]"#).is_none());
    }

    #[test]
    fn extracts_block_from_chat_reply() {
        let text = format!("I need one thing.\n\n{FENCE}\n{Q}\n```\n");
        let (clean, qs) = extract_questions(&text);
        assert_eq!(clean, "I need one thing.");
        assert_eq!(qs.unwrap().len(), 1);
        // Invalid block: text is untouched.
        let bad = format!("hi\n{FENCE}\n{{oops}}\n```");
        assert_eq!(extract_questions(&bad), (bad.clone(), None));
        assert_eq!(extract_questions("plain"), ("plain".into(), None));
    }

    #[test]
    fn answers_format() {
        let a = vec![("Gateway".to_string(), "Stripe".to_string())];
        assert_eq!(format_answers(&a), "- Gateway: Stripe");
    }
}
