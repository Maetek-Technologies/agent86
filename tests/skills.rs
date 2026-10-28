//! Skills against Python's `skills/` package and `use_skill`: discovery order and
//! precedence, `SKILL.md` parsing, the system prompt section, progressive
//! disclosure, and the `allowed-tools` restriction.

mod support;

use agent86::config::{load_config, Config, ConfigPaths, Overrides};
use agent86::prompt::build_system_prompt;
use agent86::skills::{discover_skills, skill_roots, Skills};
use agent86::tools::{default_registry, ToolContext};
use agent86::types::{ToolCall, ToolResult};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Dir(PathBuf);

impl Dir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("agent86-skills-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join("workspace")).unwrap();
        std::fs::create_dir_all(path.join("home")).unwrap();
        Self(path)
    }

    fn workspace(&self) -> PathBuf {
        self.0.join("workspace")
    }

    fn home(&self) -> PathBuf {
        self.0.join("home")
    }

    fn config(&self, toml: &str) -> Config {
        let project = self.0.join("config.toml");
        std::fs::write(&project, toml).unwrap();
        load_config(
            &ConfigPaths {
                user: None,
                project,
            },
            &Overrides::default(),
        )
        .unwrap()
    }

    fn discover(&self, config: &Config) -> Skills {
        discover_skills(config, &self.workspace(), Some(&self.home()))
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn skill(root: &Path, folder: &str, text: &str) -> PathBuf {
    let dir = root.join(folder);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), text).unwrap();
    dir
}

#[test]
fn project_skills_shadow_user_skills_and_roots_are_searched_in_order() {
    let dir = Dir::new("discovery");
    let workspace = dir.workspace();
    let home = dir.home();
    let extra = dir.0.join("extra");
    skill(
        &workspace.join(".agent86/skills"),
        "b-dir",
        "---\nname: shared\ndescription: project copy\n---\nproject body",
    );
    skill(
        &workspace.join(".agent86/skills"),
        "A-first",
        "---\ndescription: named after its folder\n---\n",
    );
    skill(
        &workspace.join(".claude/skills"),
        "claude",
        "---\nname: from-claude\ndescription: shared with Claude Code\n---\n",
    );
    skill(
        &home.join(".agent86/skills"),
        "user",
        "---\nname: shared\ndescription: user copy\n---\n",
    );
    skill(
        &home.join(".claude/skills"),
        "user-claude",
        "---\nname: user-claude\ndescription: >\n  Folded\n  description.\n---\n",
    );
    skill(
        &extra,
        "extra",
        "---\nname: extra\ndescription: from config\n---\n",
    );
    // A folder without SKILL.md, and a loose file, are not skills.
    std::fs::create_dir_all(workspace.join(".agent86/skills/empty")).unwrap();
    std::fs::write(workspace.join(".agent86/skills/README.md"), "x").unwrap();

    let config = dir.config(&format!("[skills]\npaths = ['{}']\n", extra.display()));
    let skills = dir.discover(&config);
    let names: Vec<&str> = skills.keys().map(String::as_str).collect();
    // Within a root, folders are taken in Python's sorted order (case-insensitive
    // on Windows); across roots, the first root wins.
    assert_eq!(
        names,
        ["A-first", "shared", "from-claude", "user-claude", "extra"]
    );
    assert_eq!(skills["shared"].description, "project copy");
    assert_eq!(skills["A-first"].description, "named after its folder");
    assert_eq!(skills["user-claude"].description, "Folded description.");

    // Every existing root is readable by tools; a disabled `[skills]` finds nothing.
    assert_eq!(skill_roots(&config, &workspace, Some(&home)).len(), 5);
    let off = dir.config("[skills]\nenabled = false\n");
    assert!(dir.discover(&off).is_empty());
    assert!(skill_roots(&off, &workspace, Some(&home)).is_empty());
}

#[test]
fn the_prompt_lists_skills_on_one_line_each_with_their_tool_limits() {
    let dir = Dir::new("prompt");
    let root = dir.workspace().join(".agent86").join("skills");
    skill(
        &root,
        "pirate",
        "---\nname: pirate-speak\ndescription: >\n  Rewrite text in pirate dialect.\n  Use for nautical rewrites.\nallowed-tools: read_file write_file\n---\nArr.",
    );
    skill(
        &root,
        "plain",
        "---\nname: plain\ndescription: No limits.\n---\n",
    );
    let config = dir.config("");
    let skills = dir.discover(&config);
    let prompt = build_system_prompt(&config, &skills).content;
    let section = prompt.split_once("Sandbox: subprocess.").unwrap().1;
    assert_eq!(
        section,
        "\n\nAvailable skills (call use_skill with the name to load full instructions):\n\
         - pirate-speak: Rewrite text in pirate dialect. Use for nautical rewrites. [tools while active: read_file, write_file]\n\
         - plain: No limits."
    );
    assert!(!build_system_prompt(&config, &Skills::new())
        .content
        .contains("Available skills"));
}

fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: format!("call-{name}"),
        name: name.to_owned(),
        arguments: arguments.as_object().unwrap().clone(),
    }
}

fn context(dir: &Dir, skills: Skills) -> ToolContext {
    let mut context = ToolContext::new(&dir.workspace()).unwrap();
    context.skills = Arc::new(skills);
    context
}

#[test]
fn use_skill_loads_the_instructions_and_restricts_tools_for_the_turn() {
    let dir = Dir::new("use");
    let root = dir.workspace().join(".agent86").join("skills");
    let folder = skill(
        &root,
        "pirate",
        "---\nname: pirate-speak\ndescription: Pirate.\nallowed-tools: [read_file, list_dir]\n---\n\nSpeak like a pirate.\r\nSee reference.md.\n",
    );
    std::fs::write(folder.join("reference.md"), "ahoy").unwrap();
    std::fs::create_dir_all(folder.join("scripts")).unwrap();
    std::fs::write(dir.workspace().join("notes.txt"), "hello").unwrap();
    let config = dir.config("");
    let skills = dir.discover(&config);
    let registry = default_registry(false, true);
    assert!(registry.names().contains(&"use_skill".to_owned()));
    let ctx = context(&dir, skills);

    let result: ToolResult =
        registry.dispatch(&call("use_skill", json!({"name": "pirate-speak"})), &ctx);
    assert!(result.ok, "{result:?}");
    assert_eq!(
        result.content,
        format!(
            "Speak like a pirate.\nSee reference.md.\n\nBundled resources in the skill directory ({}), readable with read_file: reference.md, scripts\n\nWhile this skill is active you may only call these tools: read_file, list_dir (plus use_skill). Other tools will be refused.",
            folder.display()
        )
    );
    assert_eq!(
        Value::Object(result.metadata),
        json!({"skill": "pirate-speak", "allowed_tools": ["read_file", "list_dir"]})
    );

    // Listed tools run; others are refused with the skill's list; use_skill always runs.
    let read = registry.dispatch(&call("read_file", json!({"path": "notes.txt"})), &ctx);
    assert!(read.ok, "{read:?}");
    let refused = registry.dispatch(
        &call("write_file", json!({"path": "x.txt", "content": "x"})),
        &ctx,
    );
    assert!(!refused.ok);
    assert_eq!(refused.call_id, "call-write_file");
    assert_eq!(
        refused.error.as_deref(),
        Some(
            "Tool 'write_file' is not permitted while the skill 'pirate-speak' is active. \
             That skill's allowed-tools are: read_file, list_dir. Use one of those, or finish \
             with the skill first."
        )
    );
    assert!(!dir.workspace().join("x.txt").exists());
    let unknown = registry.dispatch(&call("use_skill", json!({"name": "nope"})), &ctx);
    assert_eq!(
        unknown.error.as_deref(),
        Some("No skill named 'nope'. Available: pirate-speak.")
    );

    // The turn ending lifts the restriction.
    ctx.clear_skill();
    assert!(ctx.allowed_tool("write_file"));
    let missing = registry.dispatch(&call("use_skill", json!({})), &context(&dir, Skills::new()));
    assert_eq!(
        missing.error.as_deref(),
        Some("Invalid arguments for 'use_skill': name: Field required")
    );
    let none = registry.dispatch(
        &call("use_skill", json!({"name": "x"})),
        &context(&dir, Skills::new()),
    );
    assert_eq!(
        none.error.as_deref(),
        Some("No skill named 'x'. Available: (none).")
    );
}

#[test]
fn use_skill_is_registered_only_when_a_skill_exists() {
    assert!(!default_registry(false, false)
        .names()
        .contains(&"use_skill".to_owned()));
    let schema: Value = serde_json::from_str(&support::fixture("skills/schemas.json")).unwrap();
    let spec = default_registry(false, true)
        .specs()
        .into_iter()
        .find(|spec| spec.name == "use_skill")
        .unwrap();
    assert_eq!(spec.parameters, schema["use_skill"]);
    assert!(!spec.side_effecting);
}
