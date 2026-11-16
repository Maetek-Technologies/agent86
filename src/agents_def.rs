//! Specialist sub-agents (beyond Python): named, reusable sub-agent definitions,
//! one Markdown file each, with frontmatter like a `SKILL.md`
//! ([docs/subagents.md](../docs/subagents.md)).
//!
//! A *specialty* has its own instructions (the file's body), and optionally a tool
//! allowlist, a model, a step budget, a stricter approval mode, tools that are
//! always asked about, and skills to preload. Python's `delegate` takes only a
//! free-form role; a specialty is named with `delegate`'s `agent` argument, with
//! `/agent`, or with `run --agent`.
//!
//! Locations, lowest precedence first (a later one replaces an earlier one of the
//! same name, so a project file overrides a user file, which overrides a built-in):
//!
//! ```text
//! built-in                               researcher, reviewer, planner
//! ~/.agent86/agents/<name>.md             user
//! <workspace>/.agent86/agents/<name>.md   project (untrusted until approved)
//! ```
//!
//! Parsing fails closed: a file with a malformed key is skipped with a notice
//! rather than run with that key ignored, since ignoring `tools` would widen
//! what it may do. The frontmatter parser is the skills' one.

use crate::skills::{parse_allowed_tools, parse_skill_md, split_frontmatter, Value};
use crate::types::ApprovalMode;
use indexmap::IndexMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The built-in specialties, embedded in the binary. Each is an ordinary
/// definition file, parsed like any other.
pub const BUILT_INS: [(&str, &str); 3] = [
    ("researcher", include_str!("agents/researcher.md")),
    ("reviewer", include_str!("agents/reviewer.md")),
    ("planner", include_str!("agents/planner.md")),
];

/// Where a definition came from, which decides whether it needs approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    BuiltIn,
    /// `~/.agent86/agents/<name>.md`.
    User(PathBuf),
    /// `<workspace>/.agent86/agents/<name>.md`: repo text, approved per file.
    Project(PathBuf),
}

impl Source {
    pub fn label(&self) -> &'static str {
        match self {
            Self::BuiltIn => "built-in",
            Self::User(_) => "user",
            Self::Project(_) => "project",
        }
    }

    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::BuiltIn => None,
            Self::User(path) | Self::Project(path) => Some(path),
        }
    }
}

/// One specialty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Specialty {
    pub name: String,
    pub description: String,
    /// The instructions: the file's body, which replaces the generic persona.
    pub instructions: String,
    /// The tool allowlist; `None` (no `tools` line) inherits the caller's tools
    /// except `delegate`. Present but empty means no tools.
    pub tools: Option<Vec<String>>,
    /// Its own `provider:model`; `None` runs on the turn's model.
    pub model: Option<String>,
    /// Its step budget, at least 1, never above the turn's.
    pub max_steps: Option<u32>,
    /// Applies only when stricter than the session's mode.
    pub approval: Option<ApprovalMode>,
    /// Tools always asked about, whatever the session's mode (but `deny`).
    pub confirm: Vec<String>,
    /// Skills whose instructions are preloaded into its system message.
    pub skills: Vec<String>,
    /// `isolation: worktree`.
    pub worktree: bool,
    pub source: Source,
    /// The file exactly as read: what is hashed for approval and shown to the user.
    pub text: String,
}

impl Specialty {
    /// Whether its file is repo text that must be approved before it runs.
    pub fn is_project(&self) -> bool {
        matches!(self.source, Source::Project(_))
    }

    /// The SHA-256 of the file as read, what an approval is remembered by.
    pub fn hash(&self) -> String {
        crate::trust::sha256_hex(self.text.as_bytes())
    }
}

/// Whether `name` can name a specialty: an ASCII letter, then ASCII letters,
/// digits, or hyphens, 64 characters at most. So `#12` is never one, and a name
/// (the one part of an unapproved project file the model sees) carries nothing
/// but a word.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-')
        && name.len() <= 64
}

/// Parse a definition whose file stem is `stem`. `Err` says why it is skipped.
pub fn parse(stem: &str, text: &str, source: Source) -> Result<Specialty, String> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    if !valid_name(stem) {
        return Err(format!(
            "'{stem}' is not a valid specialty name (an ASCII letter, then letters, digits, or \
             hyphens, at most 64 characters)"
        ));
    }
    let (raw, _) = split_frontmatter(&text);
    if raw.trim().is_empty() {
        return Err("it has no frontmatter".into());
    }
    let (meta, body) = parse_skill_md(&text);
    let name = text_value(meta.get("name"));
    if name != stem {
        return Err(format!(
            "its name ({}) must equal the file's name ({stem})",
            if name.is_empty() { "missing" } else { &name }
        ));
    }
    let description = text_value(meta.get("description"));
    if description.is_empty() {
        return Err("it has no description".into());
    }
    let list = |key: &str| -> Result<Option<Vec<String>>, String> {
        if malformed_list(&raw, key) {
            return Err(format!("its '{key}' list is malformed"));
        }
        Ok(meta.get(key).map(parse_allowed_tools))
    };
    let tools = list("tools")?;
    let confirm = list("confirm")?.unwrap_or_default();
    let skills = list("skills")?.unwrap_or_default();
    let model = Some(text_value(meta.get("model"))).filter(|model| !model.is_empty());
    let max_steps = match meta.get("max_steps") {
        None => None,
        Some(value) => match text_value(Some(value)).parse::<u32>() {
            Ok(steps) if steps >= 1 => Some(steps),
            _ => return Err("its 'max_steps' must be a whole number of at least 1".into()),
        },
    };
    let approval = match meta.get("approval") {
        None => None,
        Some(value) => Some(match text_value(Some(value)).as_str() {
            "auto" => ApprovalMode::Auto,
            "ask" => ApprovalMode::Ask,
            "deny" => ApprovalMode::Deny,
            _ => return Err("its 'approval' must be auto, ask, or deny".into()),
        }),
    };
    let worktree = match meta.get("isolation") {
        None => false,
        Some(value) => match text_value(Some(value)).as_str() {
            "worktree" => true,
            _ => return Err("its 'isolation' must be worktree".into()),
        },
    };
    Ok(Specialty {
        name,
        description: crate::skills::one_line(&description),
        instructions: body.trim().to_owned(),
        tools,
        model,
        max_steps,
        approval,
        confirm,
        skills,
        worktree,
        source,
        text,
    })
}

fn text_value(value: Option<&Value>) -> String {
    match value {
        Some(Value::Str(text)) => text.trim().to_owned(),
        Some(Value::List(_)) => String::new(),
        None => String::new(),
    }
}

/// Whether the raw frontmatter's `key` is a list the parser would misread: an
/// empty value followed by indented lines that are not list items (the parser
/// would read no tools, or every tool), or an unbalanced `[`.
fn malformed_list(raw: &str, key: &str) -> bool {
    let lines: Vec<&str> = raw.lines().collect();
    for (index, line) in lines.iter().enumerate() {
        if line.starts_with([' ', '\t']) {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().to_lowercase() != key {
            continue;
        }
        let value = value.trim();
        if value.starts_with('[') != value.ends_with(']') {
            return true;
        }
        if value.is_empty() {
            for next in &lines[index + 1..] {
                let item = next.trim();
                if item.is_empty() {
                    continue;
                }
                if !next.starts_with([' ', '\t']) {
                    break;
                }
                if item != "-" && !item.starts_with("- ") {
                    return true;
                }
            }
        }
    }
    false
}

/// The discovered specialties, by name, and why any file was skipped.
#[derive(Debug, Clone, Default)]
pub struct Specialties {
    pub found: IndexMap<String, Arc<Specialty>>,
    /// One line per skipped file, for the user (never the model).
    pub notices: Vec<String>,
}

impl Specialties {
    pub fn get(&self, name: &str) -> Option<&Arc<Specialty>> {
        self.found.get(name)
    }

    pub fn is_empty(&self) -> bool {
        self.found.is_empty()
    }

    pub fn names(&self) -> Vec<&str> {
        self.found.keys().map(String::as_str).collect()
    }

    /// Only the built-ins, as a run with nothing else to read has.
    pub fn built_in() -> Self {
        let mut specialties = Self::default();
        specialties.add_built_ins();
        specialties
    }

    fn add_built_ins(&mut self) {
        for (name, text) in BUILT_INS {
            match parse(name, text, Source::BuiltIn) {
                Ok(specialty) => {
                    self.found.insert(name.to_owned(), Arc::new(specialty));
                }
                Err(error) => self
                    .notices
                    .push(format!("built-in specialty '{name}' skipped: {error}")),
            }
        }
    }

    /// Read every `<name>.md` in `dir`, in name order, replacing what came before.
    fn add_dir(&mut self, dir: &Path, source: fn(PathBuf) -> Source) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "md"))
            .collect();
        files.sort();
        for path in files {
            let stem = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            let shown = crate::escape::show_controls(&crate::tools::display_path(&path));
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) => {
                    self.notices
                        .push(format!("specialty file {shown} skipped: {error}"));
                    continue;
                }
            };
            let resolved = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            match parse(&stem, &text, source(resolved)) {
                Ok(specialty) => {
                    self.found.insert(stem, Arc::new(specialty));
                }
                Err(error) => self.notices.push(format!(
                    "specialty file {shown} skipped: {}",
                    crate::escape::show_controls(&error)
                )),
            }
        }
    }
}

/// Every specialty for `workspace`: the built-ins, then the user's, then the
/// project's, a later one replacing an earlier one of the same name. `home` is
/// explicit so tests never read the real one.
pub fn discover(workspace: &Path, home: Option<&Path>) -> Specialties {
    let mut specialties = Specialties::built_in();
    if let Some(home) = home {
        specialties.add_dir(&home.join(".agent86").join("agents"), Source::User);
    }
    specialties.add_dir(&workspace.join(".agent86").join("agents"), Source::Project);
    specialties
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Result<Specialty, String> {
        parse("reviewer", text, Source::BuiltIn)
    }

    #[test]
    fn a_full_definition_reads_every_key() {
        let specialty = parsed(
            "---\nname: reviewer\ndescription: >\n  Reviews\n  diffs.\ntools: read_file, list_dir\n\
             model: anthropic:claude-sonnet-5-5\nmax_steps: 12\napproval: ask\nconfirm: [run_command]\n\
             skills:\n  - lint\nisolation: worktree\n---\n\nYou review.\n",
        )
        .unwrap();
        assert_eq!(specialty.description, "Reviews diffs.");
        assert_eq!(specialty.instructions, "You review.");
        assert_eq!(
            specialty.tools,
            Some(vec!["read_file".to_owned(), "list_dir".to_owned()])
        );
        assert_eq!(
            specialty.model.as_deref(),
            Some("anthropic:claude-sonnet-5-5")
        );
        assert_eq!(specialty.max_steps, Some(12));
        assert_eq!(specialty.approval, Some(ApprovalMode::Ask));
        assert_eq!(specialty.confirm, ["run_command"]);
        assert_eq!(specialty.skills, ["lint"]);
        assert!(specialty.worktree);
    }

    #[test]
    fn a_missing_tools_line_inherits_and_an_empty_one_means_none() {
        let base = "---\nname: reviewer\ndescription: d\n";
        assert_eq!(parsed(&format!("{base}---\nx")).unwrap().tools, None);
        assert_eq!(
            parsed(&format!("{base}tools:\n---\nx")).unwrap().tools,
            Some(Vec::new())
        );
        assert_eq!(
            parsed(&format!("{base}tools: []\n---\nx")).unwrap().tools,
            Some(Vec::new())
        );
        assert_eq!(
            parsed(&format!("{base}tools:\n  - read_file\n---\nx"))
                .unwrap()
                .tools,
            Some(vec!["read_file".to_owned()])
        );
    }

    #[test]
    fn parsing_fails_closed() {
        let base = "---\nname: reviewer\ndescription: d\n";
        for (extra, why) in [
            ("tools:\n  read_file\n", "tools"),
            ("tools: [read_file\n", "tools"),
            ("confirm: run_command]\n", "confirm"),
            ("skills:\n  lint\n", "skills"),
            ("max_steps: 0\n", "max_steps"),
            ("max_steps: many\n", "max_steps"),
            ("approval: sometimes\n", "approval"),
            ("isolation: container\n", "isolation"),
        ] {
            let error = parsed(&format!("{base}{extra}---\nx")).unwrap_err();
            assert!(error.contains(why), "{extra:?}: {error}");
        }
        assert!(parsed("---\nname: reviewer\n---\nx")
            .unwrap_err()
            .contains("description"));
        assert!(parsed("---\nname: other\ndescription: d\n---\nx")
            .unwrap_err()
            .contains("must equal"));
        assert!(parsed("no frontmatter")
            .unwrap_err()
            .contains("frontmatter"));
        assert!(parse(
            "12",
            "---\nname: 12\ndescription: d\n---\n",
            Source::BuiltIn
        )
        .is_err());
    }

    #[test]
    fn names_are_plain_ascii_words() {
        let long = format!("a{}", "b".repeat(63));
        for name in ["reviewer", "test-writer", "a1", long.as_str()] {
            assert!(valid_name(name), "{name}");
        }
        let too_long = format!("{long}c");
        for name in [
            "",
            "1st",
            "-x",
            "a_b",
            "a b",
            "caf\u{e9}",
            "a.b",
            too_long.as_str(),
        ] {
            assert!(!valid_name(name), "{name:?}");
        }
    }

    #[test]
    fn the_built_ins_parse_with_their_settled_tools() {
        let built_in = Specialties::built_in();
        assert!(built_in.notices.is_empty(), "{:?}", built_in.notices);
        assert_eq!(built_in.names(), ["researcher", "reviewer", "planner"]);
        let tools = |name: &str| built_in.get(name).unwrap().tools.clone().unwrap();
        assert_eq!(
            tools("researcher"),
            [
                "read_file",
                "list_dir",
                "search_files",
                "find_files",
                "web_fetch",
                "recall"
            ]
        );
        assert_eq!(
            tools("reviewer"),
            [
                "read_file",
                "list_dir",
                "search_files",
                "find_files",
                "run_command"
            ]
        );
        assert_eq!(built_in.get("reviewer").unwrap().confirm, ["run_command"]);
        assert_eq!(
            tools("planner"),
            ["read_file", "list_dir", "search_files", "find_files"]
        );
        for specialty in built_in.found.values() {
            assert!(!specialty.instructions.is_empty());
            assert!(specialty.model.is_none() && specialty.approval.is_none());
        }
    }

    #[test]
    fn later_roots_override_earlier_ones_and_bad_files_are_noted() {
        let root = std::env::temp_dir().join(format!("agent86-agents-def-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        let workspace = root.join("work");
        let user = home.join(".agent86").join("agents");
        let project = workspace.join(".agent86").join("agents");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            user.join("reviewer.md"),
            "---\nname: reviewer\ndescription: mine\n---\nUser body.",
        )
        .unwrap();
        std::fs::write(
            user.join("writer.md"),
            "---\nname: writer\ndescription: writes\n---\nW.",
        )
        .unwrap();
        std::fs::write(
            project.join("writer.md"),
            "---\nname: writer\ndescription: repo writes\n---\nP.",
        )
        .unwrap();
        std::fs::write(project.join("broken.md"), "---\nname: other\n---\n").unwrap();
        std::fs::write(project.join("notes.txt"), "ignored").unwrap();

        let found = discover(&workspace, Some(&home));
        assert_eq!(found.get("reviewer").unwrap().description, "mine");
        assert!(matches!(
            found.get("reviewer").unwrap().source,
            Source::User(_)
        ));
        let writer = found.get("writer").unwrap();
        assert_eq!(writer.description, "repo writes");
        assert!(writer.is_project());
        assert!(found.get("broken").is_none());
        assert_eq!(found.notices.len(), 1);
        assert!(
            found.notices[0].contains("broken.md"),
            "{:?}",
            found.notices
        );
        // The built-ins nobody replaced are still there.
        assert!(found.get("planner").is_some());
        let _ = std::fs::remove_dir_all(&root);
    }
}
