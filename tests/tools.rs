//! The read-only workspace tools must behave like Python's `read_file` and `list_dir`.

mod support;

use agent86::tools::{builtin_registry, display_path, ToolContext};
use agent86::types::{ToolCall, ToolResult};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use support::fixture;

/// A throwaway workspace directory, removed on drop.
struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("agent86-tools-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn write(&self, relative: &str, contents: &[u8]) {
        let path = self.root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn mkdir(&self, relative: &str) {
        std::fs::create_dir_all(self.root.join(relative)).unwrap();
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

#[test]
fn specs_match_the_schemas_pydantic_generates() {
    let read_only: Value = serde_json::from_str(&fixture("tools/schemas.json")).unwrap();
    let writers: Value = serde_json::from_str(&fixture("tools/write_tools.json")).unwrap();
    let exec: Value = serde_json::from_str(&fixture("tools/exec_tools.json")).unwrap();
    let specs = builtin_registry().specs();

    let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "read_file",
            "write_file",
            "edit_file",
            "list_dir",
            "run_command",
            "python_exec"
        ]
    );
    for spec in &specs {
        let expected = match spec.name.as_str() {
            "write_file" | "edit_file" => &writers["schemas"][&spec.name],
            "run_command" | "python_exec" => &exec[&spec.name],
            _ => &read_only[&spec.name],
        };
        // Compared as text, so key order (pydantic's) is checked too.
        assert_eq!(
            serde_json::to_string(&spec.parameters).unwrap(),
            serde_json::to_string(expected).unwrap(),
            "{}",
            spec.name
        );
        let writes = !matches!(spec.name.as_str(), "read_file" | "list_dir");
        assert_eq!(spec.side_effecting, writes, "{}", spec.name);
    }
    assert_eq!(
        specs[0].description,
        "Read a UTF-8 text file within the workspace and return its contents."
    );
    assert_eq!(
        specs[3].description,
        "List the entries of a directory within the workspace."
    );
}

#[test]
fn read_file_returns_text_with_universal_newlines() {
    let workspace = Workspace::new("read");
    workspace.write("notes/a.txt", b"one\r\ntwo\rthree\nfour");
    let ctx = workspace.context();

    let result = dispatch(&ctx, "read_file", json!({"path": "notes/a.txt"}));

    assert!(result.ok, "{result:?}");
    assert_eq!(result.content, "one\ntwo\nthree\nfour");
    assert_eq!(result.call_id, "call-1");
    assert_eq!(result.name, "read_file");
}

#[test]
fn read_file_replaces_invalid_utf8() {
    let workspace = Workspace::new("read-binary");
    workspace.write("blob.bin", b"ok\xff!");

    let result = dispatch(
        &workspace.context(),
        "read_file",
        json!({"path": "blob.bin"}),
    );

    assert_eq!(result.content, "ok\u{fffd}!");
}

#[test]
fn read_file_reports_missing_files_and_directories() {
    let workspace = Workspace::new("read-missing");
    workspace.mkdir("sub");
    let ctx = workspace.context();

    let missing = dispatch(&ctx, "read_file", json!({"path": "nope.txt"}));
    assert_eq!(error_of(&missing), "No such file: nope.txt");

    let directory = dispatch(&ctx, "read_file", json!({"path": "sub"}));
    assert_eq!(error_of(&directory), "Not a file: sub");
}

#[test]
fn paths_outside_the_workspace_are_refused() {
    let workspace = Workspace::new("jail");
    workspace.write("inside.txt", b"x");
    let ctx = workspace.context();
    let root = display_path(ctx.policy.workspace());

    let parent = dispatch(&ctx, "read_file", json!({"path": "../outside.txt"}));
    assert_eq!(
        error_of(&parent),
        format!("PolicyError: Path '../outside.txt' resolves outside the workspace jail ({root}).")
    );

    let absolute = std::env::temp_dir().join("agent86-definitely-outside.txt");
    let absolute = absolute.to_string_lossy().into_owned();
    let result = dispatch(&ctx, "list_dir", json!({"path": absolute}));
    assert!(error_of(&result).starts_with("PolicyError: "), "{result:?}");

    // `..` that stays inside the workspace is fine.
    let roundabout = dispatch(&ctx, "read_file", json!({"path": "sub/../inside.txt"}));
    assert!(roundabout.ok, "{roundabout:?}");
}

#[cfg(unix)]
#[test]
fn symlinks_pointing_outside_the_workspace_are_refused() {
    let workspace = Workspace::new("symlink");
    let outside = Workspace::new("symlink-target");
    outside.write("secret.txt", b"secret");
    std::os::unix::fs::symlink(
        outside.root.join("secret.txt"),
        workspace.root.join("link.txt"),
    )
    .unwrap();

    let result = dispatch(
        &workspace.context(),
        "read_file",
        json!({"path": "link.txt"}),
    );

    assert!(error_of(&result).starts_with("PolicyError: "), "{result:?}");
}

#[test]
fn invalid_arguments_use_pydantic_messages() {
    let workspace = Workspace::new("args");
    let ctx = workspace.context();

    let missing = dispatch(&ctx, "read_file", json!({}));
    assert_eq!(
        error_of(&missing),
        "Invalid arguments for 'read_file': path: Field required"
    );

    for bad in [json!(3), json!(null), json!(["a"]), json!(true)] {
        let result = dispatch(&ctx, "read_file", json!({"path": bad}));
        assert_eq!(
            error_of(&result),
            "Invalid arguments for 'read_file': path: Input should be a valid string"
        );
    }

    let null_dir = dispatch(&ctx, "list_dir", json!({"path": null}));
    assert_eq!(
        error_of(&null_dir),
        "Invalid arguments for 'list_dir': path: Input should be a valid string"
    );
}

#[test]
fn unknown_argument_keys_are_ignored() {
    let workspace = Workspace::new("extra");
    workspace.write("a.txt", b"hi");

    let result = dispatch(
        &workspace.context(),
        "read_file",
        json!({"path": "a.txt", "encoding": "latin-1"}),
    );

    assert!(result.ok, "{result:?}");
}

#[test]
fn list_dir_puts_directories_first_and_sorts_case_insensitively() {
    let workspace = Workspace::new("list");
    workspace.write("b.txt", b"");
    workspace.write("A.md", b"");
    workspace.write("c.rs", b"");
    workspace.mkdir("zeta");
    workspace.mkdir("Alpha");
    let ctx = workspace.context();

    let result = dispatch(&ctx, "list_dir", json!({}));

    assert!(result.ok, "{result:?}");
    assert_eq!(result.content, "Alpha/\nzeta/\nA.md\nb.txt\nc.rs");

    let explicit = dispatch(&ctx, "list_dir", json!({"path": "."}));
    assert_eq!(explicit.content, result.content);
}

#[test]
fn list_dir_reports_empty_and_non_directories() {
    let workspace = Workspace::new("list-edge");
    workspace.mkdir("empty");
    workspace.write("file.txt", b"x");
    let ctx = workspace.context();

    assert_eq!(
        dispatch(&ctx, "list_dir", json!({"path": "empty"})).content,
        "(empty)"
    );

    let file = dispatch(&ctx, "list_dir", json!({"path": "file.txt"}));
    assert_eq!(error_of(&file), "Not a directory: file.txt");
    let missing = dispatch(&ctx, "list_dir", json!({"path": "missing"}));
    assert_eq!(error_of(&missing), "Not a directory: missing");
}

#[test]
fn long_output_is_truncated_with_a_count() {
    let workspace = Workspace::new("truncate");
    workspace.write("big.txt", "é".repeat(12).as_bytes());
    let mut ctx = workspace.context();
    ctx.policy.max_output_chars = 10;

    let result = dispatch(&ctx, "read_file", json!({"path": "big.txt"}));

    // Python counts characters, despite calling them bytes.
    assert_eq!(
        result.content,
        format!("{}\n... [truncated 2 bytes]", "é".repeat(10))
    );
}

#[test]
fn unknown_tools_name_the_available_ones() {
    let workspace = Workspace::new("unknown");

    let result = dispatch(&workspace.context(), "web_fetch", json!({"url": "x"}));

    assert_eq!(result.call_id, "call-1");
    assert_eq!(result.name, "web_fetch");
    assert_eq!(
        error_of(&result),
        "Unknown tool 'web_fetch'. Available: edit_file, list_dir, python_exec, read_file, run_command, write_file."
    );
}

#[test]
fn duplicate_registration_is_refused() {
    let mut registry = builtin_registry();
    let error = registry
        .register(Box::new(agent86::tools::files::ReadFileTool))
        .unwrap_err();
    assert_eq!(error, "Tool 'read_file' is already registered.");
}

#[test]
fn workspaces_must_exist() {
    assert!(ToolContext::new(Path::new("definitely/not/a/real/workspace")).is_err());
}
