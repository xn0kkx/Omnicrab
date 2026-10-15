//! The `.omni/` project directory inside the TUI: loading it at unlock, `/omni`, custom
//! commands and agents, and keeping plans and sessions in it.

use std::path::PathBuf;

use super::*;
use crate::project::{Doc, PlanRecord, ProjectDir, resolve_agents};

/// A document being edited in `$EDITOR` (a decrypted copy that is deleted afterwards).
pub struct PendingEdit {
    doc: Doc,
    dir: PathBuf,
    file: PathBuf,
    original: String,
}

impl PendingEdit {
    fn run_editor(&self) -> std::io::Result<std::process::ExitStatus> {
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "nano".into());
        let mut parts = editor.split_whitespace();
        let mut c = std::process::Command::new(parts.next().unwrap_or("nano"));
        c.args(parts).arg(&self.file);
        c.status()
    }
}

/// What is shown in `/omni` and appended to saved plans.
fn plan_record(task: &str, steps: &[modes::Step]) -> PlanRecord {
    PlanRecord {
        at: Utc::now(),
        task: task.to_string(),
        steps: steps
            .iter()
            .map(|s| (s.agent.clone(), s.task.clone()))
            .collect(),
        result: None,
        error: None,
    }
}

/// Stores an approved plan with its outcome. Failures only get logged.
pub fn save_plan(
    project: &Option<ProjectDir>,
    vault: &Vault,
    task: &str,
    steps: &[modes::Step],
    outcome: &Result<String, String>,
) {
    let Some(p) = project else { return };
    let mut rec = plan_record(task, steps);
    match outcome {
        Ok(t) => rec.result = Some(t.clone()),
        Err(e) => rec.error = Some(e.clone()),
    }
    if let Err(e) = p.save_plan(vault, &rec) {
        tracing::warn!("saving plan to .omni: {e}");
    }
}

impl App {
    /// Opens (creating on first use) `<workspace>/.omni` and reads it.
    pub(super) fn open_project(&mut self) {
        let Some(root) = self.workspace.clone() else {
            return;
        };
        match ProjectDir::open(&root) {
            Ok(p) => {
                self.project = Some(p);
                self.reload_project();
            }
            Err(e) => self.notice = Some(e),
        }
    }

    /// Re-reads instructions, memory, settings, agents and commands.
    pub(super) fn reload_project(&mut self) {
        let (Some(p), Some(core)) = (self.project.clone(), self.core.clone()) else {
            return;
        };
        let v = &core.vault;
        let mut problems = vec![];
        self.project_context = p.context(v).unwrap_or_else(|e| {
            problems.push(e);
            None
        });
        match p.settings(v) {
            Ok(s) => self.read_only = self.read_only_flag || s.read_only,
            Err(e) => problems.push(e),
        }
        self.project_agent_defs = p.agents(v).unwrap_or_else(|e| {
            problems.push(e);
            vec![]
        });
        self.project_commands = p.commands(v).unwrap_or_else(|e| {
            problems.push(e);
            vec![]
        });
        self.merge_project_agents();
        if let Some(e) = problems.first() {
            self.notice = Some(format!(".omni: {e}"));
        }
    }

    /// Project agents are added after the ones from the database (same name: database wins).
    pub(super) fn merge_project_agents(&mut self) {
        let (extra, problems) = resolve_agents(&self.project_agent_defs, &self.accounts);
        let taken: HashSet<String> = self.agents.iter().map(|a| a.name.to_lowercase()).collect();
        let old = std::mem::take(&mut self.project_agent_ids);
        self.agents.retain(|a| !old.contains(&a.id));
        for a in extra {
            if !taken.contains(&a.name.to_lowercase()) {
                self.project_agent_ids.insert(a.id);
                self.agents.push(a);
            }
        }
        // Accounts are still loading on the first call; only report once they are known.
        if !self.accounts.is_empty()
            && let Some(p) = problems.first()
        {
            self.notice = Some(format!(".omni: {p}"));
        }
    }

    /// Runs `/<name> args` when a project command has that name.
    pub(super) fn project_command(&mut self, name: &str, args: &str) -> bool {
        let Some(c) = self
            .project_commands
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
            .cloned()
        else {
            return false;
        };
        let text = c.render(args);
        self.submit(text);
        true
    }

    pub(super) fn cmd_omni(&mut self, rest: &str) {
        let (sub, arg) = rest
            .split_once(char::is_whitespace)
            .map(|(a, b)| (a, b.trim()))
            .unwrap_or((rest, ""));
        let (Some(p), Some(core)) = (self.project.clone(), self.core.clone()) else {
            self.notice = Some("no project folder: start omnicrab inside a project".into());
            return;
        };
        let v = core.vault.clone();
        match sub {
            "" | "status" => {
                let docs: Vec<String> = Doc::ALL
                    .iter()
                    .map(|d| {
                        let has = p
                            .read_doc(&v, *d)
                            .ok()
                            .flatten()
                            .is_some_and(|t| !t.trim().is_empty());
                        format!("{}{}", d.name(), if has { "*" } else { "" })
                    })
                    .collect();
                self.push_system(
                    "omni",
                    format!(
                        "{}\nencrypted with the vault key; * = has content\n  {}\n  plans: {}   sessions: {}\n  commands: {}\n\n/omni edit <{}>  /omni init  /omni remember <note>",
                        p.path().display(),
                        docs.join("  "),
                        p.count("plans"),
                        p.count("sessions"),
                        if self.project_commands.is_empty() {
                            "none".to_string()
                        } else {
                            self.project_commands
                                .iter()
                                .map(|c| format!("/{}", c.name))
                                .collect::<Vec<_>>()
                                .join(" ")
                        },
                        Doc::ALL.map(|d| d.name()).join("|"),
                    ),
                );
            }
            "init" => {
                let Some(root) = self.workspace.clone() else {
                    return;
                };
                self.notice = Some(match p.import_instructions(&v, &root) {
                    Ok(f) if f.is_empty() => {
                        "no CLAUDE.md, GEMINI.md or AGENTS.md to import".into()
                    }
                    Ok(f) => {
                        self.reload_project();
                        format!("imported {} into .omni instructions", f.join(", "))
                    }
                    Err(e) => e,
                });
            }
            "remember" => {
                if arg.is_empty() {
                    self.notice = Some("usage: /omni remember <note>".into());
                    return;
                }
                self.notice = Some(match p.remember(&v, arg) {
                    Ok(()) => {
                        self.reload_project();
                        "saved to project memory".into()
                    }
                    Err(e) => e,
                });
            }
            "edit" => {
                let doc = if arg.is_empty() {
                    Some(Doc::Instructions)
                } else {
                    Doc::parse(arg)
                };
                let Some(doc) = doc else {
                    self.notice = Some(format!(
                        "unknown document '{arg}' - one of {}",
                        Doc::ALL.map(|d| d.name()).join(", ")
                    ));
                    return;
                };
                self.begin_edit(&p, &v, doc);
            }
            other => {
                self.notice = Some(format!(
                    "unknown /omni {other} - try status, init, edit, remember"
                ))
            }
        }
    }

    fn push_system(&mut self, who: &str, text: String) {
        self.entries.push(Entry::new(Role::System, who, text));
        self.scroll_up = 0;
    }

    /// Decrypts `doc` into a private temp file and asks the run loop to open the editor.
    fn begin_edit(&mut self, p: &ProjectDir, v: &Vault, doc: Doc) {
        let original = match p.read_doc(v, doc) {
            Ok(t) => t.unwrap_or_else(|| doc.template().to_string()),
            Err(e) => {
                self.notice = Some(e);
                return;
            }
        };
        let dir = std::env::temp_dir().join(format!("omnicrab-edit-{}", Uuid::new_v4()));
        let ext = if doc.is_json() { "json" } else { "md" };
        let file = dir.join(format!("{}.{ext}", doc.name()));
        let made = (|| -> std::io::Result<()> {
            use std::io::Write;
            use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
            std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&file)?;
            f.write_all(original.as_bytes())
        })();
        if let Err(e) = made {
            let _ = std::fs::remove_dir_all(&dir);
            self.notice = Some(format!("cannot prepare the editor copy: {e}"));
            return;
        }
        self.pending_edit = Some(PendingEdit {
            doc,
            dir,
            file,
            original,
        });
    }

    /// Called by the run loop (terminal already restored): edit, re-encrypt, clean up.
    pub(super) fn run_pending_edit(&mut self) -> bool {
        let Some(edit) = self.pending_edit.take() else {
            return false;
        };
        let status = edit.run_editor();
        self.notice = Some(self.finish_edit(&edit, status));
        let _ = std::fs::remove_dir_all(&edit.dir);
        true
    }

    fn finish_edit(
        &mut self,
        e: &PendingEdit,
        status: std::io::Result<std::process::ExitStatus>,
    ) -> String {
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => return format!("editor exited with {s}; nothing saved"),
            Err(err) => return format!("could not start the editor ($EDITOR): {err}"),
        }
        let text = match std::fs::read_to_string(&e.file) {
            Ok(t) => t,
            Err(err) => return format!("cannot read the edited file: {err}"),
        };
        if text == e.original {
            return "no changes".into();
        }
        let (Some(p), Some(core)) = (self.project.clone(), self.core.clone()) else {
            return "no project".into();
        };
        match p.write_doc(&core.vault, e.doc, &text) {
            Ok(()) => {
                self.reload_project();
                format!("saved {} (encrypted)", e.doc.name())
            }
            // Keep the user's text: it is still in the editor copy until we clean up.
            Err(err) => {
                let keep = std::env::temp_dir().join(format!("omnicrab-{}.rejected", e.doc.name()));
                let _ = std::fs::write(&keep, &text);
                format!("not saved: {err} (your text is in {})", keep.display())
            }
        }
    }

    /// Writes the current conversation into `.omni/sessions/`.
    pub(super) fn save_session_copy(&self) {
        let (Some(p), Some(core), Some(id)) =
            (self.project.clone(), self.core.clone(), self.session_id)
        else {
            return;
        };
        let msgs: Vec<Message> = self
            .entries
            .iter()
            .filter(|e| !e.text.trim().is_empty())
            .map(|e| Message {
                role: e.role,
                content: e.text.clone(),
                agent_name: None,
                account_name: (e.role == Role::Assistant && e.who != "...").then(|| e.who.clone()),
            })
            .collect();
        tokio::spawn(async move {
            if let Err(err) = p.save_session(&core.vault, id, &msgs) {
                tracing::warn!("saving session to .omni: {err}");
            }
        });
    }
}
