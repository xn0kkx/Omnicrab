//! The project directory omnicrab was opened in: which folder agents may work in
//! and where the encrypted `.omni/` state lives.
//!
//! `.omni/` plays the role of `.claude/` or `.gemini/`, but every file is sealed with the
//! vault key (AAD binds project, kind and file id, so files cannot be swapped or moved).
//! Only `project.toml` (a random project id) is plain text. Agents cannot read the files:
//! omnicrab decrypts instructions and memory and passes them along in prompts.
//!
//! ```text
//! .omni/
//!   project.toml            project id (plain)
//!   .gitignore              "*"
//!   instructions.enc        like CLAUDE.md / GEMINI.md
//!   memory.enc              notes that persist between runs
//!   settings.enc            JSON: {"read_only": true}
//!   agents.enc              JSON array of project agents
//!   commands.enc            JSON array of custom slash commands
//!   plans/<uuid>.enc        approved plans and their results
//!   sessions/<uuid>.enc     conversation transcripts
//! ```

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use uuid::Uuid;

use crate::crypto::Vault;
use crate::model::{Account, Agent, Message};

const DIR: &str = ".omni";
const FORMAT: u32 = 1;

/// Picks the workspace root. `explicit` comes from `--workspace`; without it the current
/// directory is used, except for `/` and the home directory, which are too broad to give
/// agents write access to by accident.
pub fn resolve_root(
    explicit: Option<PathBuf>,
    cwd: &Path,
    home: Option<&Path>,
) -> Result<PathBuf, String> {
    let (root, was_explicit) = match explicit {
        Some(p) => (p, true),
        None => (cwd.to_path_buf(), false),
    };
    let root = root
        .canonicalize()
        .map_err(|e| format!("workspace {}: {e}", root.display()))?;
    if !root.is_dir() {
        return Err(format!("workspace {} is not a directory", root.display()));
    }
    let too_broad = root.parent().is_none() || home.is_some_and(|h| h == root);
    if too_broad && !was_explicit {
        return Err(format!(
            "not using {} as the workspace (too broad); open omnicrab in a project folder or pass --workspace",
            root.display()
        ));
    }
    Ok(root)
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "core.quotepath=off"])
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Current commit of the workspace (`None` outside a git repository or with no commits).
pub fn git_head(root: &Path) -> Option<String> {
    git(root, &["rev-parse", "HEAD"]).map(|s| s.trim().to_string())
}

/// What changed in the working tree: `git status --short` plus `git diff --stat`, capped.
/// `None` when `root` is not a git repository; an empty string when nothing changed.
/// `.omni/` is ignored (it is not part of the project's changes).
pub fn git_changes(root: &Path) -> Option<String> {
    const MAX_LINES: usize = 40;
    let status = git(root, &["status", "--short", "--", ".", ":(exclude).omni"])?;
    if status.trim().is_empty() {
        return Some(String::new());
    }
    let stat = git(
        root,
        &["diff", "--stat", "HEAD", "--", ".", ":(exclude).omni"],
    )
    .or_else(|| git(root, &["diff", "--stat", "--", ".", ":(exclude).omni"]))
    .unwrap_or_default();
    let cap = |s: &str| {
        let lines: Vec<&str> = s.lines().collect();
        let mut out = lines
            .iter()
            .take(MAX_LINES)
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        if lines.len() > MAX_LINES {
            out.push_str(&format!("\n... and {} more", lines.len() - MAX_LINES));
        }
        out
    };
    let mut out = format!("changed files:\n{}", cap(&status));
    if !stat.trim().is_empty() {
        out.push_str(&format!("\n\n{}", cap(&stat)));
    }
    Some(out)
}

/// Documents inside `.omni/` (one file each).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Doc {
    Instructions,
    Memory,
    Settings,
    Agents,
    Commands,
}

impl Doc {
    pub const ALL: [Doc; 5] = [
        Doc::Instructions,
        Doc::Memory,
        Doc::Settings,
        Doc::Agents,
        Doc::Commands,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Doc::Instructions => "instructions",
            Doc::Memory => "memory",
            Doc::Settings => "settings",
            Doc::Agents => "agents",
            Doc::Commands => "commands",
        }
    }

    pub fn parse(s: &str) -> Option<Doc> {
        Doc::ALL.into_iter().find(|d| d.name() == s.trim())
    }

    pub fn is_json(self) -> bool {
        matches!(self, Doc::Settings | Doc::Agents | Doc::Commands)
    }

    /// Text a new document starts with in the editor.
    pub fn template(self) -> &'static str {
        match self {
            Doc::Instructions => "# Project instructions\n\n",
            Doc::Memory => "",
            Doc::Settings => "{\n  \"read_only\": false\n}\n",
            Doc::Agents => {
                "[\n  {\"name\": \"reviewer\", \"role\": \"code reviewer\", \"system_prompt\": \"\", \
                 \"account\": \"<account name>\", \"model\": null, \"fallbacks\": []}\n]\n"
            }
            Doc::Commands => {
                "[\n  {\"name\": \"review\", \"description\": \"Review a file\", \
                 \"prompt\": \"Review $ARGUMENTS for bugs.\"}\n]\n"
            }
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ProjectSettings {
    /// Agents may only read this project.
    #[serde(default)]
    pub read_only: bool,
}

/// An agent as written in `agents.enc`; accounts are referenced by name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectAgent {
    pub name: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub system_prompt: String,
    pub account: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub fallbacks: Vec<String>,
}

/// A custom slash command; `$ARGUMENTS` in `prompt` is replaced by what follows the name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectCommand {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub prompt: String,
}

impl ProjectCommand {
    pub fn render(&self, args: &str) -> String {
        if self.prompt.contains("$ARGUMENTS") {
            self.prompt.replace("$ARGUMENTS", args)
        } else if args.is_empty() {
            self.prompt.clone()
        } else {
            format!("{}\n\n{args}", self.prompt)
        }
    }
}

/// An approved plan and what came of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanRecord {
    pub at: chrono::DateTime<chrono::Utc>,
    pub task: String,
    /// `(agent, task)` per step.
    pub steps: Vec<(String, String)>,
    pub result: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Digest {
    head: String,
    text: String,
}

#[derive(Serialize, Deserialize)]
struct Meta {
    project_id: Uuid,
    format: u32,
}

/// Handle to `<root>/.omni`. Cheap to clone; the vault is passed to every call.
#[derive(Clone, Debug)]
pub struct ProjectDir {
    dir: PathBuf,
    id: Uuid,
}

fn private_dir(p: &Path) -> std::io::Result<()> {
    fs::create_dir_all(p)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(p, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Writes through a temp file and a rename, so a crash never leaves a half-written file.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(tmp, path)
}

impl ProjectDir {
    /// Opens `<root>/.omni`, creating it (and a project id) on first use.
    pub fn open(root: &Path) -> Result<Self, String> {
        let dir = root.join(DIR);
        let meta_path = dir.join("project.toml");
        let id = match fs::read_to_string(&meta_path) {
            Ok(t) => {
                let m: Meta = toml::from_str(&t).map_err(|e| format!(".omni/project.toml: {e}"))?;
                if m.format > FORMAT {
                    return Err(format!(
                        ".omni uses format {}, this omnicrab understands {FORMAT}",
                        m.format
                    ));
                }
                m.project_id
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                private_dir(&dir).map_err(|e| format!("creating .omni: {e}"))?;
                let m = Meta {
                    project_id: Uuid::new_v4(),
                    format: FORMAT,
                };
                let text = toml::to_string(&m).map_err(|e| e.to_string())?;
                write_atomic(&meta_path, text.as_bytes()).map_err(|e| format!(".omni: {e}"))?;
                // Ciphertext has no use in version control.
                let _ = write_atomic(&dir.join(".gitignore"), b"*\n");
                m.project_id
            }
            Err(e) => return Err(format!(".omni/project.toml: {e}")),
        };
        Ok(Self { dir, id })
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// AAD table for this project: files of another project fail to open.
    fn table(&self) -> String {
        format!("omni:{}", self.id)
    }

    fn put_bytes(
        &self,
        vault: &Vault,
        sub: Option<&str>,
        kind: &str,
        row: Uuid,
        pt: &[u8],
    ) -> Result<(), String> {
        let blob = vault
            .seal(&self.table(), kind, &row, pt)
            .map_err(|e| e.to_string())?;
        let dir = match sub {
            Some(s) => self.dir.join(s),
            None => self.dir.clone(),
        };
        private_dir(&dir).map_err(|e| e.to_string())?;
        let file = match sub {
            Some(_) => format!("{row}.enc"),
            None => format!("{kind}.enc"),
        };
        write_atomic(&dir.join(file), &blob).map_err(|e| e.to_string())
    }

    fn get_bytes(
        &self,
        vault: &Vault,
        sub: Option<&str>,
        kind: &str,
        row: Uuid,
    ) -> Result<Option<Vec<u8>>, String> {
        let file = match sub {
            Some(s) => self.dir.join(s).join(format!("{row}.enc")),
            None => self.dir.join(format!("{kind}.enc")),
        };
        let blob = match fs::read(&file) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", file.display())),
        };
        let pt = vault
            .open(&self.table(), kind, &row, &blob)
            .map_err(|e| format!("{}: {e}", file.display()))?;
        Ok(Some(pt.to_vec()))
    }

    /// Reads a document as text (`None` when it does not exist yet).
    pub fn read_doc(&self, vault: &Vault, doc: Doc) -> Result<Option<String>, String> {
        self.get_bytes(vault, None, doc.name(), Uuid::nil())?
            .map(|b| String::from_utf8(b).map_err(|e| e.to_string()))
            .transpose()
    }

    pub fn write_doc(&self, vault: &Vault, doc: Doc, text: &str) -> Result<(), String> {
        if doc.is_json() {
            check_json(doc, text)?;
        }
        self.put_bytes(vault, None, doc.name(), Uuid::nil(), text.as_bytes())
    }

    fn read_json<T: DeserializeOwned + Default>(
        &self,
        vault: &Vault,
        doc: Doc,
    ) -> Result<T, String> {
        match self.read_doc(vault, doc)? {
            Some(t) if !t.trim().is_empty() => {
                serde_json::from_str(&t).map_err(|e| format!("{}: {e}", doc.name()))
            }
            _ => Ok(T::default()),
        }
    }

    pub fn settings(&self, vault: &Vault) -> Result<ProjectSettings, String> {
        self.read_json(vault, Doc::Settings)
    }

    pub fn agents(&self, vault: &Vault) -> Result<Vec<ProjectAgent>, String> {
        self.read_json(vault, Doc::Agents)
    }

    pub fn commands(&self, vault: &Vault) -> Result<Vec<ProjectCommand>, String> {
        self.read_json(vault, Doc::Commands)
    }

    /// Instructions and memory, ready to put in a system prompt.
    pub fn context(&self, vault: &Vault) -> Result<Option<String>, String> {
        let mut parts = vec![];
        for (doc, title) in [
            (Doc::Instructions, "Project instructions"),
            (Doc::Memory, "Project memory"),
        ] {
            if let Some(t) = self.read_doc(vault, doc)?.filter(|t| !t.trim().is_empty()) {
                parts.push(format!("## {title}\n{}", t.trim()));
            }
        }
        Ok((!parts.is_empty()).then(|| parts.join("\n\n")))
    }

    /// Adds a line to the memory document.
    pub fn remember(&self, vault: &Vault, note: &str) -> Result<(), String> {
        let mut text = self.read_doc(vault, Doc::Memory)?.unwrap_or_default();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("- {}\n", note.trim()));
        self.write_doc(vault, Doc::Memory, &text)
    }

    /// Project overview the scouts wrote at commit `head` (`None` once the commit moved on).
    pub fn load_digest(&self, vault: &Vault, head: &str) -> Result<Option<String>, String> {
        let Some(b) = self.get_bytes(vault, None, "digest", Uuid::nil())? else {
            return Ok(None);
        };
        let d: Digest = serde_json::from_slice(&b).map_err(|e| e.to_string())?;
        Ok((d.head == head).then_some(d.text))
    }

    pub fn save_digest(&self, vault: &Vault, head: &str, text: &str) -> Result<(), String> {
        let d = Digest {
            head: head.to_string(),
            text: text.to_string(),
        };
        let json = serde_json::to_vec(&d).map_err(|e| e.to_string())?;
        self.put_bytes(vault, None, "digest", Uuid::nil(), &json)
    }

    pub fn save_plan(&self, vault: &Vault, plan: &PlanRecord) -> Result<(), String> {
        let json = serde_json::to_vec(plan).map_err(|e| e.to_string())?;
        self.put_bytes(vault, Some("plans"), "plans", Uuid::new_v4(), &json)
    }

    /// Rewrites the transcript of a session.
    pub fn save_session(&self, vault: &Vault, id: Uuid, msgs: &[Message]) -> Result<(), String> {
        let json = serde_json::to_vec(msgs).map_err(|e| e.to_string())?;
        self.put_bytes(vault, Some("sessions"), "sessions", id, &json)
    }

    #[cfg(test)]
    pub fn load_session(&self, vault: &Vault, id: Uuid) -> Result<Option<Vec<Message>>, String> {
        self.get_bytes(vault, Some("sessions"), "sessions", id)?
            .map(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
            .transpose()
    }

    /// Number of files in `plans/` or `sessions/`.
    pub fn count(&self, sub: &str) -> usize {
        fs::read_dir(self.dir.join(sub))
            .map(|d| d.filter_map(Result::ok).count())
            .unwrap_or(0)
    }

    /// Imports `CLAUDE.md` / `GEMINI.md` / `AGENTS.md` from the project root as instructions.
    pub fn import_instructions(&self, vault: &Vault, root: &Path) -> Result<Vec<String>, String> {
        let mut found = vec![];
        let mut text = self.read_doc(vault, Doc::Instructions)?.unwrap_or_default();
        for name in ["CLAUDE.md", "GEMINI.md", "AGENTS.md"] {
            if let Ok(t) = fs::read_to_string(root.join(name))
                && !t.trim().is_empty()
            {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&format!("<!-- imported from {name} -->\n{}\n", t.trim()));
                found.push(name.to_string());
            }
        }
        if !found.is_empty() {
            self.write_doc(vault, Doc::Instructions, &text)?;
        }
        Ok(found)
    }
}

/// JSON documents must parse as what the app will read them as.
fn check_json(doc: Doc, text: &str) -> Result<(), String> {
    let err = |e: serde_json::Error| format!("{} is not valid: {e}", doc.name());
    if text.trim().is_empty() {
        return Ok(());
    }
    match doc {
        Doc::Settings => serde_json::from_str::<ProjectSettings>(text)
            .map(drop)
            .map_err(err),
        Doc::Agents => serde_json::from_str::<Vec<ProjectAgent>>(text)
            .map(drop)
            .map_err(err),
        Doc::Commands => serde_json::from_str::<Vec<ProjectCommand>>(text)
            .map(drop)
            .map_err(err),
        _ => Ok(()),
    }
}

/// Turns project agents into runtime agents, matching accounts by name (case-insensitive).
/// Returns the agents plus a note for each one that could not be used.
pub fn resolve_agents(list: &[ProjectAgent], accounts: &[Account]) -> (Vec<Agent>, Vec<String>) {
    let find = |n: &str| {
        accounts
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(n.trim()))
            .map(|a| a.id)
    };
    let (mut out, mut problems) = (vec![], vec![]);
    for pa in list {
        let Some(account_id) = find(&pa.account) else {
            problems.push(format!(
                "agent {}: no account named '{}'",
                pa.name, pa.account
            ));
            continue;
        };
        out.push(Agent {
            // Stable per name, so a selection survives reloads.
            id: Uuid::new_v5(&Uuid::NAMESPACE_OID, pa.name.to_lowercase().as_bytes()),
            name: pa.name.clone(),
            role: pa.role.clone(),
            system_prompt: pa.system_prompt.clone(),
            account_id,
            model: pa.model.clone().filter(|m| !m.is_empty()),
            fallback_account_ids: pa.fallbacks.iter().filter_map(|f| find(f)).collect(),
        });
    }
    (out, problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("omnicrab-{name}-{}", Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    fn vault(pw: &str) -> Vault {
        Vault::create(pw).unwrap().0
    }

    #[test]
    fn refuses_root_and_home_unless_explicit() {
        let t = tmp("root");
        assert!(resolve_root(None, Path::new("/"), None).is_err());
        assert!(resolve_root(None, &t, Some(&t)).is_err());
        assert_eq!(resolve_root(Some(t.clone()), &t, Some(&t)).unwrap(), t);
        assert_eq!(resolve_root(None, &t, None).unwrap(), t);
        assert!(resolve_root(Some("/definitely/not/here".into()), &t, None).is_err());
        let _ = fs::remove_dir_all(t);
    }

    #[test]
    fn roundtrip_and_encrypted_on_disk() {
        let root = tmp("proj");
        let v = vault("correct horse battery");
        let p = ProjectDir::open(&root).unwrap();
        p.write_doc(&v, Doc::Instructions, "use tabs, never spaces")
            .unwrap();
        assert_eq!(
            p.read_doc(&v, Doc::Instructions).unwrap().as_deref(),
            Some("use tabs, never spaces")
        );
        let raw = fs::read(root.join(".omni/instructions.enc")).unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("tabs"));
        assert_eq!(
            fs::read_to_string(root.join(".omni/.gitignore")).unwrap(),
            "*\n"
        );
        // Reopening finds the same project.
        assert_eq!(ProjectDir::open(&root).unwrap().id, p.id);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn files_cannot_be_swapped_or_read_with_another_key() {
        let root = tmp("swap");
        let (v, other) = (vault("correct horse battery"), vault("another passphrase!"));
        let p = ProjectDir::open(&root).unwrap();
        p.write_doc(&v, Doc::Instructions, "secret").unwrap();
        p.write_doc(&v, Doc::Memory, "other").unwrap();
        assert!(p.read_doc(&other, Doc::Instructions).is_err());
        // A document moved over another one fails authentication (AAD has the kind).
        fs::copy(
            root.join(".omni/instructions.enc"),
            root.join(".omni/memory.enc"),
        )
        .unwrap();
        assert!(p.read_doc(&v, Doc::Memory).is_err());
        // So does a file from another project.
        let root2 = tmp("swap2");
        let p2 = ProjectDir::open(&root2).unwrap();
        p2.write_doc(&v, Doc::Instructions, "x").unwrap();
        fs::copy(
            root2.join(".omni/instructions.enc"),
            root.join(".omni/instructions.enc"),
        )
        .unwrap();
        assert!(p.read_doc(&v, Doc::Instructions).is_err());
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(root2);
    }

    #[test]
    fn json_docs_are_validated_and_parsed() {
        let root = tmp("json");
        let v = vault("correct horse battery");
        let p = ProjectDir::open(&root).unwrap();
        assert!(p.write_doc(&v, Doc::Settings, "{oops").is_err());
        assert!(!p.settings(&v).unwrap().read_only);
        p.write_doc(&v, Doc::Settings, r#"{"read_only":true}"#)
            .unwrap();
        assert!(p.settings(&v).unwrap().read_only);
        // The editor templates must be valid for what they describe.
        for d in [Doc::Settings, Doc::Agents, Doc::Commands] {
            check_json(d, d.template()).unwrap();
        }
        // An agent without an account is rejected before it is saved.
        assert!(p.write_doc(&v, Doc::Agents, r#"[{"name":"x"}]"#).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn context_memory_plans_and_sessions() {
        let root = tmp("ctx");
        let v = vault("correct horse battery");
        let p = ProjectDir::open(&root).unwrap();
        assert!(p.context(&v).unwrap().is_none());
        p.write_doc(&v, Doc::Instructions, "Rust 2024").unwrap();
        p.save_digest(&v, "abc", "overview").unwrap();
        assert_eq!(
            p.load_digest(&v, "abc").unwrap().as_deref(),
            Some("overview")
        );
        assert!(p.load_digest(&v, "def").unwrap().is_none(), "new commit");
        p.remember(&v, "db is postgres").unwrap();
        let c = p.context(&v).unwrap().unwrap();
        assert!(c.contains("Rust 2024") && c.contains("- db is postgres"));
        p.save_plan(
            &v,
            &PlanRecord {
                at: chrono::Utc::now(),
                task: "t".into(),
                steps: vec![("".into(), "do".into())],
                result: Some("ok".into()),
                error: None,
            },
        )
        .unwrap();
        assert_eq!(p.count("plans"), 1);
        let sid = Uuid::new_v4();
        let msgs = vec![Message {
            role: crate::model::Role::User,
            content: "hi".into(),
            agent_name: None,
            account_name: None,
        }];
        p.save_session(&v, sid, &msgs).unwrap();
        assert_eq!(p.load_session(&v, sid).unwrap().unwrap()[0].content, "hi");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn imports_claude_md() {
        let root = tmp("import");
        let v = vault("correct horse battery");
        fs::write(root.join("CLAUDE.md"), "be terse").unwrap();
        let p = ProjectDir::open(&root).unwrap();
        assert_eq!(p.import_instructions(&v, &root).unwrap(), ["CLAUDE.md"]);
        let t = p.read_doc(&v, Doc::Instructions).unwrap().unwrap();
        assert!(t.contains("be terse"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn git_changes_reports_edits() {
        let root = tmp("git");
        let run = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            )
        };
        assert!(git_changes(&root).is_none(), "not a repository yet");
        run(&["init", "-q"]);
        fs::write(root.join("a.txt"), "one\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "init"]);
        assert!(git_head(&root).is_some());
        assert_eq!(git_changes(&root).as_deref(), Some(""));
        fs::write(root.join("a.txt"), "two\n").unwrap();
        fs::write(root.join("b.txt"), "new\n").unwrap();
        let c = git_changes(&root).unwrap();
        assert!(c.contains("a.txt") && c.contains("b.txt"), "{c}");
        // The encrypted project state never counts as a change.
        ProjectDir::open(&root).unwrap();
        assert!(!git_changes(&root).unwrap().contains(".omni"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn command_arguments() {
        let c = ProjectCommand {
            name: "r".into(),
            description: String::new(),
            prompt: "Review $ARGUMENTS now".into(),
        };
        assert_eq!(c.render("a.rs"), "Review a.rs now");
        let c = ProjectCommand {
            prompt: "Summarize".into(),
            ..c
        };
        assert_eq!(c.render("the repo"), "Summarize\n\nthe repo");
    }
}
