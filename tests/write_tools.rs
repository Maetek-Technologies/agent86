//! `write_file`, `edit_file`, their approval previews, and the approval gate.

mod support;

use agent86::approval::{build_preview, ApprovalGate, ApprovalPreview, ApprovalPrompt};
use agent86::tools::{builtin_registry, ToolContext};
use agent86::types::{ApprovalMode, ToolCall, ToolResult};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use support::fixture;

struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("agent86-write-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn write(&self, relative: &str, contents: &[u8]) {
        std::fs::write(self.root.join(relative), contents).unwrap();
    }

    fn read(&self, relative: &str) -> Vec<u8> {
        std::fs::read(self.root.join(relative)).unwrap()
    }

    fn context(&self) -> ToolContext {
        ToolContext::new(&self.root).unwrap()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn call(name: &str, arguments: Value) -> ToolCall {
    let Value::Object(arguments) = arguments else {
        panic!("arguments must be an object");
    };
    ToolCall {
        id: "call-1".to_owned(),
        name: name.to_owned(),
        arguments,
    }
}

fn dispatch(ctx: &ToolContext, name: &str, arguments: Value) -> ToolResult {
    builtin_registry().dispatch(&call(name, arguments), ctx)
}

fn error_of(result: &ToolResult) -> &str {
    assert!(!result.ok, "expected a failure, got {result:?}");
    result.error.as_deref().unwrap()
}

fn preview(ctx: &ToolContext, name: &str, arguments: Value) -> Option<String> {
    let registry = builtin_registry();
    let tool = registry.get(name).unwrap();
    build_preview(tool.as_ref(), &call(name, arguments), ctx).detail
}

/// `text` as Python's `write_text` puts it on disk on this platform.
fn on_disk(text: &str) -> Vec<u8> {
    if cfg!(windows) {
        text.replace('\n', "\r\n").into_bytes()
    } else {
        text.as_bytes().to_vec()
    }
}

fn golden() -> Value {
    serde_json::from_str(&fixture("tools/write_tools.json")).unwrap()
}

#[test]
fn write_file_creates_files_and_parent_directories() {
    let workspace = Workspace::new("create");
    let ctx = workspace.context();

    let result = dispatch(
        &ctx,
        "write_file",
        json!({"path": "notes/new.txt", "content": "a\nb\n"}),
    );

    assert!(result.ok, "{result:?}");
    assert_eq!(
        result.content,
        "Wrote 4 bytes to notes/new.txt (new file).\nnew file, 2 lines"
    );
    assert_eq!(result.metadata["created"], true);
    assert_eq!(workspace.read("notes/new.txt"), on_disk("a\nb\n"));
}

#[test]
fn write_file_reports_overwrites_as_a_diff() {
    let workspace = Workspace::new("overwrite");
    workspace.write("f.txt", b"a\nb\nc\n");
    let ctx = workspace.context();

    let changed = dispatch(
        &ctx,
        "write_file",
        json!({"path": "f.txt", "content": "a\nB\nc\n"}),
    );
    assert_eq!(
        changed.content,
        "Wrote 6 bytes to f.txt (overwrite).\n--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c"
    );
    assert_eq!(changed.metadata["created"], false);

    let same = dispatch(
        &ctx,
        "write_file",
        json!({"path": "f.txt", "content": "a\nB\nc\n"}),
    );
    assert_eq!(
        same.content,
        "Wrote 6 bytes to f.txt (overwrite).\nno change"
    );
}

#[test]
fn writes_outside_the_workspace_are_refused() {
    let workspace = Workspace::new("write-jail");
    let ctx = workspace.context();

    let result = dispatch(
        &ctx,
        "write_file",
        json!({"path": "../escape.txt", "content": "x"}),
    );

    assert!(error_of(&result).starts_with("PolicyError: Path '../escape.txt' resolves outside"));
    assert!(!workspace.root.parent().unwrap().join("escape.txt").exists());
}

#[test]
fn edit_file_replaces_one_exact_match() {
    let workspace = Workspace::new("edit");
    workspace.write("f.txt", b"a\nb\nc\n");

    let result = dispatch(
        &workspace.context(),
        "edit_file",
        json!({"path": "f.txt", "old_string": "b", "new_string": "B"}),
    );

    assert_eq!(
        result.content,
        "Edited f.txt (1 replacement).\n--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c"
    );
    assert_eq!(result.metadata["replacements"], 1);
    assert_eq!(workspace.read("f.txt"), b"a\nB\nc\n");
}

#[test]
fn edit_file_preserves_bom_and_crlf() {
    let workspace = Workspace::new("crlf");
    workspace.write("win.txt", b"\xef\xbb\xbfone\r\ntwo\r\n");
    workspace.write("mixed.txt", b"a\r\nb\nc");
    let ctx = workspace.context();

    dispatch(
        &ctx,
        "edit_file",
        json!({"path": "win.txt", "old": "two", "new": "2"}),
    );
    assert_eq!(workspace.read("win.txt"), b"\xef\xbb\xbfone\r\n2\r\n");

    // A mixed file is matched and written verbatim.
    dispatch(
        &ctx,
        "edit_file",
        json!({"path": "mixed.txt", "old": "c", "new": "C"}),
    );
    assert_eq!(workspace.read("mixed.txt"), b"a\r\nb\nC");
}

#[test]
fn edit_file_explains_every_refusal() {
    let workspace = Workspace::new("edit-errors");
    workspace.write("f.txt", b"x y x\n");
    workspace.write("bin.dat", b"ok\xff");
    let ctx = workspace.context();
    let edit = |arguments: Value| dispatch(&ctx, "edit_file", arguments);

    assert_eq!(
        error_of(&edit(json!({"path": "nope.txt", "old": "a", "new": "b"}))),
        "No such file: nope.txt"
    );
    assert_eq!(
        error_of(&edit(json!({"path": "f.txt", "old": "", "new": "b"}))),
        "old_string must not be empty; use write_file to create a file."
    );
    assert_eq!(
        error_of(&edit(json!({"path": "bin.dat", "old": "ok", "new": "b"}))),
        "Not a UTF-8 text file: bin.dat"
    );
    assert_eq!(
        error_of(&edit(json!({"path": "f.txt", "old": "zzz", "new": "b"}))),
        "old_string not found in f.txt (0 matches). It must match the file exactly, including \
         indentation and line breaks — read the file and copy the text verbatim."
    );
    assert_eq!(
        error_of(&edit(json!({"path": "f.txt", "old": "x", "new": "b"}))),
        "old_string is ambiguous in f.txt: 2 matches. Include more surrounding context so it \
         matches exactly once, or pass replace_all=true to replace all 2."
    );
    assert_eq!(
        error_of(&edit(json!({"path": "f.txt", "old": "y", "new": "y"}))),
        "old_string and new_string are identical; nothing to do."
    );
    assert_eq!(workspace.read("f.txt"), b"x y x\n");
}

#[test]
fn replace_all_replaces_every_occurrence() {
    let workspace = Workspace::new("replace-all");
    workspace.write("f.txt", b"x x x\n");

    let result = dispatch(
        &workspace.context(),
        "edit_file",
        json!({"path": "f.txt", "old": "x", "new": "y", "replace_all": "yes"}),
    );

    assert!(
        result
            .content
            .starts_with("Edited f.txt (3 replacements).\n"),
        "{result:?}"
    );
    assert_eq!(workspace.read("f.txt"), b"y y y\n");
}

#[test]
fn validation_messages_match_pydantic() {
    let workspace = Workspace::new("validation");
    workspace.write("p", b"a");
    let ctx = workspace.context();
    let golden = golden();
    let expect = |name: &str, key: &str, arguments: Value| {
        let message = format!(
            "Invalid arguments for '{name}': {}",
            golden["errors"][key].as_str().unwrap()
        );
        assert_eq!(error_of(&dispatch(&ctx, name, arguments)), message, "{key}");
    };

    expect("write_file", "write_empty", json!({}));
    expect("edit_file", "edit_empty", json!({}));
    expect(
        "edit_file",
        "edit_bad_types",
        json!({"path": 1, "old_string": null, "new_string": [], "replace_all": "maybe"}),
    );
    expect(
        "edit_file",
        "edit_bool_list",
        json!({"path": "p", "old": "a", "new": "b", "replace_all": [1]}),
    );
    expect(
        "edit_file",
        "edit_bool_two",
        json!({"path": "p", "old": "a", "new": "b", "replace_all": 2}),
    );
    expect(
        "edit_file",
        "edit_bool_float",
        json!({"path": "p", "old": "a", "new": "b", "replace_all": 1.5}),
    );
}

#[test]
fn booleans_and_aliases_are_accepted_like_pydantic() {
    let workspace = Workspace::new("bools");
    let ctx = workspace.context();
    let golden = golden();
    let inputs = [
        json!(true),
        json!(false),
        json!(1),
        json!(0),
        json!(1.0),
        json!("true"),
        json!("False"),
        json!("yes"),
        json!("no"),
        json!("on"),
        json!("off"),
        json!("1"),
        json!("0"),
        json!("t"),
        json!("f"),
        json!("y"),
        json!("n"),
        json!("YES"),
    ];
    let expected = golden["accepted"]["bools"].as_array().unwrap();
    assert_eq!(inputs.len(), expected.len());

    for (input, expected) in inputs.iter().zip(expected) {
        // "a a" has two matches: replace_all decides between success and ambiguity.
        workspace.write("f.txt", b"a a");
        let result = dispatch(
            &ctx,
            "edit_file",
            json!({"path": "f.txt", "old": "a", "new": "b", "replace_all": input}),
        );
        let replace_all = expected["replace_all"].as_bool().unwrap();
        assert_eq!(result.ok, replace_all, "{input}: {result:?}");
    }

    // With both spellings present, `old_string` wins, as with AliasChoices.
    workspace.write("f.txt", b"A a");
    let both = dispatch(
        &ctx,
        "edit_file",
        json!({"path": "f.txt", "old_string": "A", "old": "a", "new_string": "B"}),
    );
    assert!(both.ok, "{both:?}");
    assert_eq!(workspace.read("f.txt"), b"B a");
}

#[test]
fn previews_show_what_a_write_would_do() {
    let workspace = Workspace::new("previews");
    workspace.write("f.txt", b"a\nb\nc\n");
    workspace.write("twice.txt", b"x x\n");
    let ctx = workspace.context();

    assert_eq!(
        preview(
            &ctx,
            "write_file",
            json!({"path": "n.txt", "content": "a\nb\n"})
        )
        .unwrap(),
        "new file: n.txt (2 lines)\na\nb\n"
    );
    let long: String = (0..45).map(|i| format!("{i}\n")).collect();
    let head = preview(
        &ctx,
        "write_file",
        json!({"path": "n.txt", "content": long}),
    )
    .unwrap();
    assert!(head.starts_with("new file: n.txt (45 lines)\n0\n"));
    assert!(head.ends_with("\n39\n... [5 more lines]"), "{head}");

    assert_eq!(
        preview(
            &ctx,
            "write_file",
            json!({"path": "f.txt", "content": "a\nB\nc\n"})
        )
        .unwrap(),
        "--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c"
    );
    assert_eq!(
        preview(
            &ctx,
            "write_file",
            json!({"path": "f.txt", "content": "a\nb\nc\n"})
        )
        .unwrap(),
        "f.txt is already exactly this content (no change)."
    );

    let edit = |arguments: Value| preview(&ctx, "edit_file", arguments).unwrap();
    assert_eq!(
        edit(json!({"path": "nope", "old": "a", "new": "b"})),
        "nope: cannot be read (missing, or not a UTF-8 text file)."
    );
    assert_eq!(
        edit(json!({"path": "f.txt", "old": "zzz", "new": "b"})),
        "f.txt: old_string was NOT found — this edit would fail."
    );
    assert_eq!(
        edit(json!({"path": "twice.txt", "old": "x", "new": "y"})),
        "twice.txt: old_string is ambiguous — 2 matches, and replace_all is off."
    );
    assert_eq!(
        edit(json!({"path": "twice.txt", "old": "x", "new": "y", "replace_all": true})),
        "2 occurrences replaced\n--- a/twice.txt\n+++ b/twice.txt\n@@ -1 +1 @@\n-x x\n+y y"
    );
    assert_eq!(
        edit(json!({"path": "f.txt", "old": "b", "new": "b"})),
        "f.txt: old_string and new_string are identical (no change)."
    );

    // Unusable arguments give no detail rather than failing.
    assert_eq!(preview(&ctx, "write_file", json!({"path": 3})), None);
    assert_eq!(preview(&ctx, "read_file", json!({"path": "f.txt"})), None);
}

#[test]
fn the_gate_follows_the_approval_mode() {
    let workspace = Workspace::new("gate");
    let ctx = workspace.context();
    let registry = builtin_registry();
    let write = registry.get("write_file").unwrap();
    let read = registry.get("read_file").unwrap();
    let write_call = call("write_file", json!({"path": "x.txt", "content": "hi"}));
    let read_call = call("read_file", json!({"path": "x.txt"}));

    let reason =
        |gate: &ApprovalGate, tool: &std::sync::Arc<dyn agent86::tools::Tool>, call: &ToolCall| {
            let decision = gate.decide(tool.as_ref(), call, &ctx);
            (decision.approved, decision.reason)
        };

    let ask = ApprovalGate::new(ApprovalMode::Ask, None);
    assert_eq!(reason(&ask, &read, &read_call), (true, "read-only"));
    assert_eq!(
        reason(&ask, &write, &write_call),
        (false, "approval required but no prompt available")
    );
    let auto = ApprovalGate::new(ApprovalMode::Auto, None);
    assert_eq!(reason(&auto, &write, &write_call), (true, "auto-approved"));
    let deny = ApprovalGate::new(ApprovalMode::Deny, None);
    assert_eq!(
        reason(&deny, &write, &write_call),
        (false, "blocked by approval policy (deny)")
    );

    let seen: Arc<Mutex<Vec<(String, ApprovalPreview)>>> = Arc::default();
    let record = Arc::clone(&seen);
    let prompt: ApprovalPrompt = Box::new(move |name, preview| {
        record
            .lock()
            .unwrap()
            .push((name.to_owned(), preview.clone()));
        false
    });
    let asking = ApprovalGate::new(ApprovalMode::Ask, Some(prompt));
    assert_eq!(
        reason(&asking, &write, &write_call),
        (false, "declined by user")
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].0, "write_file");
    assert_eq!(seen[0].1.summary, r#"{"path": "x.txt", "content": "hi"}"#);
    assert_eq!(
        seen[0].1.detail.as_deref(),
        Some("new file: x.txt (1 line)\nhi")
    );
}

#[test]
fn writes_into_agent86s_own_files_are_asked_about_even_in_auto_mode() {
    let workspace = Workspace::new("guarded");
    let ctx = workspace.context();
    let registry = builtin_registry();
    let write = registry.get("write_file").unwrap();
    let edit = registry.get("edit_file").unwrap();
    let decide = |gate: &ApprovalGate, call: &ToolCall| {
        let tool = if call.name == "edit_file" {
            &edit
        } else {
            &write
        };
        let decision = gate.decide(tool.as_ref(), call, &ctx);
        (decision.approved, decision.reason)
    };
    let config = call(
        "write_file",
        json!({"path": ".agent86/config.toml", "content": "[mcp_servers.x]\ncommand = \"sh\"\n"}),
    );

    // Nobody to ask: declined, though the mode is auto.
    let auto = ApprovalGate::new(ApprovalMode::Auto, None);
    assert_eq!(
        decide(&auto, &config),
        (
            false,
            "writing agent86's own configuration or skills needs approval, and no prompt is available"
        )
    );
    let deny = ApprovalGate::new(ApprovalMode::Deny, None);
    assert_eq!(
        decide(&deny, &config),
        (false, "blocked by approval policy (deny)")
    );

    // Someone to ask: asked, with the warning, in auto mode too.
    let seen: Arc<Mutex<Vec<ApprovalPreview>>> = Arc::default();
    let record = Arc::clone(&seen);
    let prompt: ApprovalPrompt = Box::new(move |_, preview| {
        record.lock().unwrap().push(preview.clone());
        true
    });
    let asking = ApprovalGate::new(ApprovalMode::Auto, Some(prompt));
    assert_eq!(decide(&asking, &config), (true, "approved by user"));
    // Spelled as Windows and macOS would still find it, and project skills.
    for path in [
        ".AGENT86/skills/x/SKILL.md",
        "src/../.agent86./config.toml",
        ".claude/skills/x/SKILL.md",
    ] {
        let sneaky = call("write_file", json!({"path": path, "content": "x"}));
        assert_eq!(
            decide(&asking, &sneaky),
            (true, "approved by user"),
            "{path}"
        );
    }
    workspace.write("notes.txt", b"a\n");
    let edit_call = call(
        "edit_file",
        json!({"path": "notes.txt", "old_string": "a", "new_string": "b"}),
    );
    assert_eq!(decide(&asking, &edit_call), (true, "auto-approved"));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 4, "the ordinary write was not asked about");
    assert_eq!(
        seen[0].warning.as_deref(),
        Some(
            "This writes agent86's own configuration or skills (.agent86/config.toml), which \
             changes what agent86 runs or is told from now on. Asked even in auto mode."
        )
    );
    assert!(seen[0]
        .detail
        .as_deref()
        .unwrap()
        .starts_with("new file: .agent86/config.toml"));

    // An ordinary write in auto mode is still not asked about.
    let plain = call("write_file", json!({"path": "x.txt", "content": "hi"}));
    let quiet = ApprovalGate::new(ApprovalMode::Auto, None);
    assert_eq!(decide(&quiet, &plain), (true, "auto-approved"));
}

#[test]
fn the_users_agent86_directory_is_guarded_even_as_the_workspace() {
    // agent86 started in ~/.agent86 itself: its config.toml is at the workspace root.
    let home = Workspace::new("guarded-home");
    std::fs::create_dir_all(home.root.join(".agent86").join("skills")).unwrap();
    let agent_dir = home.root.join(".agent86");
    let registry = builtin_registry();
    let write = registry.get("write_file").unwrap();
    let asked: Arc<Mutex<usize>> = Arc::default();
    let count = Arc::clone(&asked);
    let prompt: ApprovalPrompt = Box::new(move |_, preview| {
        assert!(preview.warning.is_some());
        *count.lock().unwrap() += 1;
        false
    });
    let gate = ApprovalGate::new(ApprovalMode::Auto, Some(prompt));

    for (workspace, path) in [
        (agent_dir.clone(), "config.toml"),
        (agent_dir.clone(), "trusted-projects.json"),
        (agent_dir.join("skills"), "x/SKILL.md"),
        (home.root.clone(), ".agent86/config.toml"),
    ] {
        let mut ctx = ToolContext::new(&workspace).unwrap();
        ctx.policy.guard([agent_dir.clone()]);
        let decision = gate.decide(
            write.as_ref(),
            &call("write_file", json!({"path": path, "content": "x"})),
            &ctx,
        );
        assert_eq!(
            (decision.approved, decision.reason),
            (false, "declined by user"),
            "{path} in {}",
            workspace.display()
        );
    }
    assert_eq!(*asked.lock().unwrap(), 4);

    // Elsewhere in that workspace, auto still means auto.
    let mut ctx = ToolContext::new(&home.root).unwrap();
    ctx.policy.guard([agent_dir]);
    let decision = gate.decide(
        write.as_ref(),
        &call("write_file", json!({"path": "notes.txt", "content": "x"})),
        &ctx,
    );
    assert_eq!(decision.reason, "auto-approved");
}

/// Make `link` point at `target`, or `false` where this platform will not
/// (Windows without Developer Mode or elevation).
fn symlink(target: &std::path::Path, link: &std::path::Path) -> bool {
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file(target, link);
    match made {
        Ok(()) => true,
        Err(error) => {
            eprintln!("skipping: cannot create a symlink here ({error})");
            false
        }
    }
}

#[test]
fn a_dangling_symlink_cannot_carry_a_write_out_of_the_jail() {
    let workspace = Workspace::new("dangling");
    let outside = Workspace::new("dangling-outside");
    let ctx = workspace.context();
    let escape = outside.root.join("authorized_keys");
    if !symlink(&escape, &workspace.root.join("notes")) {
        return;
    }
    // A chain of links ends outside too.
    assert!(symlink(
        &workspace.root.join("notes"),
        &workspace.root.join("alias")
    ));
    std::fs::create_dir_all(workspace.root.join("sub")).unwrap();
    assert!(symlink(
        &workspace.root.join("sub").join("later.txt"),
        &workspace.root.join("inside")
    ));

    for path in ["notes", "alias"] {
        let result = dispatch(
            &ctx,
            "write_file",
            json!({"path": path, "content": "ssh-rsa x\n"}),
        );
        let error = error_of(&result);
        assert!(
            error.starts_with(&format!(
                "PolicyError: Path '{path}' resolves outside the workspace jail"
            )),
            "{error}"
        );
        let edited = dispatch(
            &ctx,
            "edit_file",
            json!({"path": path, "old_string": "a", "new_string": "b"}),
        );
        assert!(error_of(&edited).starts_with("PolicyError: "), "{edited:?}");
    }
    assert!(!escape.exists(), "nothing was written outside");

    // A dangling link that stays inside still works, writing its target.
    let result = dispatch(
        &ctx,
        "write_file",
        json!({"path": "inside", "content": "ok\n"}),
    );
    assert!(result.ok, "{result:?}");
    assert_eq!(workspace.read("sub/later.txt"), on_disk("ok\n"));
}

#[test]
fn writing_over_a_binary_file_is_called_an_overwrite() {
    let workspace = Workspace::new("binary-overwrite");
    workspace.write("logo.png", &[0x89, b'P', b'N', b'G', 0xff, 0x00]);
    let ctx = workspace.context();
    let arguments = json!({"path": "logo.png", "content": "oops\n"});

    let shown = preview(&ctx, "write_file", arguments.clone()).unwrap();
    assert_eq!(
        shown,
        "overwrite: logo.png is not a UTF-8 text file (6 bytes); all of it is replaced with 1 line\noops\n"
    );

    let result = dispatch(&ctx, "write_file", arguments);
    assert!(result.ok, "{result:?}");
    assert_eq!(
        result.content,
        "Wrote 5 bytes to logo.png (overwrite).\nreplaced a file that was not UTF-8 text (6 bytes) with 1 line"
    );
    assert_eq!(result.metadata["created"], json!(false));
    assert_eq!(workspace.read("logo.png"), on_disk("oops\n"));
}
