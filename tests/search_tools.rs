//! `search_files` and `find_files` (beyond Python): read-only, jailed, ignore-aware
//! search with stable, `/`-separated output on every OS, and bounds that say how
//! to narrow a search they cut.

#[path = "../crates/agent86-core/tests/support/mod.rs"]
mod support;

use agent86::tools::search::{LINE_MARKER, MAX_FILES, MAX_LINE_CHARS};
use agent86::tools::{default_registry, ToolContext};
use agent86::types::{ToolCall, ToolResult};
use serde_json::{json, Value};
use std::path::PathBuf;
use support::fixture;

/// A throwaway directory, removed on drop.
struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("agent86-search-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn write(&self, relative: &str, contents: &[u8]) {
        let path = self.root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
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

fn dispatch(ctx: &ToolContext, name: &str, arguments: Value) -> ToolResult {
    let Value::Object(arguments) = arguments else {
        panic!("arguments must be an object");
    };
    let call = ToolCall {
        id: "call-1".to_owned(),
        name: name.to_owned(),
        arguments,
    };
    default_registry(false, false, false, false).dispatch(&call, ctx)
}

fn content(result: &ToolResult) -> &str {
    assert!(result.ok, "expected success, got {result:?}");
    &result.content
}

fn error_of(result: &ToolResult) -> &str {
    assert!(!result.ok, "expected a failure, got {result:?}");
    result.error.as_deref().unwrap()
}

/// A small project: nested sources, an ignored build directory, hidden
/// configuration, and version-control metadata.
fn project(name: &str) -> Workspace {
    let workspace = Workspace::new(name);
    workspace.write("src/main.rs", b"fn main() {\n    needle();\n}\n");
    workspace.write("src/lib/util.rs", b"pub fn needle() {}\n");
    workspace.write("README.md", b"Find the needle here.\n");
    workspace.write(".gitignore", b"target/\n*.tmp\n");
    workspace.write("target/debug/out.rs", b"needle in a build product\n");
    workspace.write("scratch.tmp", b"needle in scratch\n");
    workspace.write(".github/workflows/ci.yml", b"run: needle\n");
    workspace.write(".git/config", b"needle in git metadata\n");
    workspace
}

#[test]
fn search_tools_are_registered_read_only_with_their_schemas() {
    let fixtures: Value = serde_json::from_str(&fixture("tools/schemas.json")).unwrap();
    let marked = fixtures["_beyond_python"].as_array().unwrap();
    let registry = default_registry(false, false, false, false);
    let names: Vec<String> = registry.specs().into_iter().map(|spec| spec.name).collect();
    let at = names.iter().position(|name| name == "web_fetch").unwrap();
    assert_eq!(names[at + 1..at + 3], ["search_files", "find_files"]);

    for name in ["search_files", "find_files"] {
        assert!(
            marked.contains(&json!(name)),
            "{name} is marked beyond Python"
        );
        let tool = registry.get(name).unwrap();
        assert!(!tool.side_effecting(), "{name}");
        assert!(tool.parallel_safe(), "{name}");
        // Compared as text, so key order is checked too.
        assert_eq!(
            serde_json::to_string(&tool.parameters()).unwrap(),
            serde_json::to_string(&fixtures[name]).unwrap(),
            "{name}"
        );
    }
}

#[test]
fn search_files_returns_matches_in_path_order_with_forward_slashes() {
    let workspace = project("order");
    let ctx = workspace.context();

    let result = dispatch(&ctx, "search_files", json!({"pattern": "needle"}));

    // Hidden configuration is searched; ignored files and `.git` are not.
    assert_eq!(
        content(&result),
        ".github/workflows/ci.yml:1: run: needle\n\
         README.md:1: Find the needle here.\n\
         src/lib/util.rs:1: pub fn needle() {}\n\
         src/main.rs:2:     needle();"
    );
    assert_eq!(result.metadata["matches"], json!(4));
    assert_eq!(result.metadata["files"], json!(4));
    assert_eq!(result.metadata["truncated"], json!(false));
}

#[test]
fn ignore_files_apply_without_a_git_repository_and_inside_one() {
    for with_git in [false, true] {
        let workspace = Workspace::new(&format!("ignore-{with_git}"));
        if with_git {
            workspace.write(".git/HEAD", b"ref: refs/heads/main\n");
        }
        workspace.write(".gitignore", b"/generated\n");
        workspace.write(".ignore", b"*.snap\n");
        workspace.write("kept.txt", b"marker\n");
        workspace.write("generated/out.txt", b"marker\n");
        workspace.write("tests/case.snap", b"marker\n");
        workspace.write("sub/.gitignore", b"local.txt\n");
        workspace.write("sub/local.txt", b"marker\n");
        let ctx = workspace.context();

        let result = dispatch(&ctx, "search_files", json!({"pattern": "marker"}));
        assert_eq!(
            content(&result),
            "kept.txt:1: marker",
            "with_git={with_git}"
        );

        let found = dispatch(&ctx, "find_files", json!({"pattern": "*.txt"}));
        assert_eq!(content(&found), "kept.txt", "with_git={with_git}");
    }
}

#[test]
fn version_control_metadata_is_never_searched() {
    let workspace = project("vcs");
    workspace.write(".hg/store", b"needle\n");
    workspace.write(".svn/entries", b"needle\n");
    workspace.write(".jj/repo", b"needle\n");
    let ctx = workspace.context();

    let found = dispatch(&ctx, "find_files", json!({"pattern": "**"}));
    for line in content(&found).lines() {
        assert!(
            ![".git/", ".hg/", ".svn/", ".jj/"]
                .iter()
                .any(|vcs| line.starts_with(vcs)),
            "{line}"
        );
    }

    // Naming the metadata directory as `path` does not open it either.
    let inside = dispatch(
        &ctx,
        "search_files",
        json!({"pattern": "needle", "path": ".git"}),
    );
    assert!(
        content(&inside).starts_with("No matches: '.git' is inside version-control metadata"),
        "{inside:?}"
    );
    let inside = dispatch(&ctx, "find_files", json!({"pattern": "*", "path": ".git"}));
    assert!(content(&inside).starts_with("No files matched: '.git'"));
}

#[test]
fn binary_files_are_skipped() {
    let workspace = Workspace::new("binary");
    let mut binary = b"needle".to_vec();
    binary.push(0);
    binary.extend_from_slice(b"needle");
    workspace.write("blob.bin", &binary);
    workspace.write("text.txt", b"needle\n");
    let ctx = workspace.context();

    let result = dispatch(&ctx, "search_files", json!({"pattern": "needle"}));
    assert_eq!(content(&result), "text.txt:1: needle");
}

#[test]
fn glob_case_and_context_narrow_and_widen_the_output() {
    let workspace = project("filters");
    workspace.write(
        "notes/todo.txt",
        b"one\ntwo\nNeedle three\nfour\nfive\nsix\nseven\nneedle eight\nnine\r\n",
    );
    let ctx = workspace.context();

    // A glob with `/` matches the whole path, one without matches file names.
    let rust = dispatch(
        &ctx,
        "search_files",
        json!({"pattern": "needle", "glob": "src/**/*.rs"}),
    );
    assert_eq!(
        content(&rust),
        "src/lib/util.rs:1: pub fn needle() {}\nsrc/main.rs:2:     needle();"
    );
    let named = dispatch(
        &ctx,
        "search_files",
        json!({"pattern": "needle", "glob": "util.rs"}),
    );
    assert_eq!(content(&named), "src/lib/util.rs:1: pub fn needle() {}");
    // A model on Windows may write backslashes.
    let windows = dispatch(
        &ctx,
        "search_files",
        json!({"pattern": "needle", "glob": "src\\lib\\*.rs"}),
    );
    assert_eq!(content(&windows), "src/lib/util.rs:1: pub fn needle() {}");

    let sensitive = dispatch(
        &ctx,
        "search_files",
        json!({"pattern": "needle", "path": "notes"}),
    );
    assert_eq!(content(&sensitive), "notes/todo.txt:8: needle eight");

    let context = dispatch(
        &ctx,
        "search_files",
        json!({"pattern": "needle", "path": "notes", "case_insensitive": true, "context": 1}),
    );
    assert_eq!(
        content(&context),
        "notes/todo.txt-2- two\n\
         notes/todo.txt:3: Needle three\n\
         notes/todo.txt-4- four\n\
         --\n\
         notes/todo.txt-7- seven\n\
         notes/todo.txt:8: needle eight\n\
         notes/todo.txt-9- nine"
    );

    // A single file can be named as `path`.
    let file = dispatch(
        &ctx,
        "search_files",
        json!({"pattern": "fn", "path": "src/main.rs"}),
    );
    assert_eq!(content(&file), "src/main.rs:1: fn main() {");
}

#[test]
fn results_are_capped_with_a_note_on_narrowing() {
    let workspace = Workspace::new("cap");
    let body: String = (0..30).map(|n| format!("hit {n}\n")).collect();
    workspace.write("a.txt", body.as_bytes());
    workspace.write("b.txt", body.as_bytes());
    let ctx = workspace.context();

    let result = dispatch(
        &ctx,
        "search_files",
        json!({"pattern": "hit", "max_results": 40}),
    );
    let text = content(&result);
    let (matches, note) = text.split_once("\n\n").unwrap();
    assert_eq!(matches.lines().count(), 40);
    assert!(matches.ends_with("b.txt:10: hit 9"), "{matches}");
    assert_eq!(
        note,
        "[Showing the first 40 matching lines. Narrow the search with a more specific \
         pattern, a smaller `path`, or a `glob`, or raise `max_results` (at most 2000).]"
    );
    assert_eq!(result.metadata["truncated"], json!(true));

    // Exactly at the cap is not truncated.
    let exact = dispatch(
        &ctx,
        "search_files",
        json!({"pattern": "hit", "max_results": 60}),
    );
    assert!(!content(&exact).contains("[Showing"));

    let found = dispatch(
        &ctx,
        "find_files",
        json!({"pattern": "*.txt", "max_results": 1}),
    );
    assert_eq!(
        content(&found),
        "a.txt\n\n[Showing the first 1 paths. Narrow the search with a more specific \
         pattern or a smaller `path`, or raise `max_results` (at most 2000).]"
    );

    // Above the hard cap is refused as pydantic's `le=` would.
    let over = dispatch(
        &ctx,
        "search_files",
        json!({"pattern": "hit", "max_results": 2001}),
    );
    assert_eq!(
        error_of(&over),
        "Invalid arguments for 'search_files': max_results: Input should be less than or equal to 2000"
    );
    let under = dispatch(
        &ctx,
        "find_files",
        json!({"pattern": "*", "max_results": 0}),
    );
    assert_eq!(
        error_of(&under),
        "Invalid arguments for 'find_files': max_results: Input should be greater than or equal to 1"
    );
}

#[test]
fn long_lines_are_cut_with_a_marker() {
    let workspace = Workspace::new("long");
    let line = format!("needle{}", "x".repeat(MAX_LINE_CHARS * 2));
    workspace.write("min.js", line.as_bytes());
    let ctx = workspace.context();

    let result = dispatch(&ctx, "search_files", json!({"pattern": "needle"}));
    let text = content(&result);
    assert!(text.ends_with(LINE_MARKER), "{text}");
    assert_eq!(
        text.len(),
        "min.js:1: ".len() + MAX_LINE_CHARS + LINE_MARKER.len()
    );
}

#[test]
fn a_tree_past_the_file_cap_stops_with_a_note() {
    let workspace = Workspace::new("many");
    for dir in 0..=MAX_FILES / 1000 {
        let dir = workspace.root.join(format!("d{dir:03}"));
        std::fs::create_dir_all(&dir).unwrap();
        for file in 0..1000 {
            std::fs::write(dir.join(format!("f{file:04}.txt")), b"").unwrap();
        }
    }
    let ctx = workspace.context();

    let found = dispatch(&ctx, "find_files", json!({"pattern": "nothing-matches"}));
    assert_eq!(
        content(&found),
        format!(
            "No files matched.\n\n[Stopped after looking at {MAX_FILES} files. Narrow the \
             search with a smaller `path`.]"
        )
    );
    assert_eq!(found.metadata["truncated"], json!(true));

    let searched = dispatch(&ctx, "search_files", json!({"pattern": "x"}));
    assert!(
        content(&searched).ends_with(&format!(
            "[Stopped after looking at {MAX_FILES} files. Narrow the search with a smaller \
             `path` or a `glob`.]"
        )),
        "{searched:?}"
    );

    // A narrower path stays under the cap.
    let narrow = dispatch(
        &ctx,
        "find_files",
        json!({"pattern": "*.txt", "path": "d000", "max_results": 2000}),
    );
    assert_eq!(content(&narrow).lines().count(), 1000);
    assert_eq!(narrow.metadata["truncated"], json!(false));
}

#[test]
fn paths_outside_the_workspace_are_refused() {
    let workspace = project("jail");
    let ctx = workspace.context();

    for path in ["..", "../..", "src/../.."] {
        let result = dispatch(&ctx, "search_files", json!({"pattern": "x", "path": path}));
        assert!(
            error_of(&result).starts_with("PolicyError: "),
            "{path}: {result:?}"
        );
        let result = dispatch(&ctx, "find_files", json!({"pattern": "*", "path": path}));
        assert!(
            error_of(&result).starts_with("PolicyError: "),
            "{path}: {result:?}"
        );
    }
    let outside = std::env::temp_dir();
    let absolute = dispatch(
        &ctx,
        "find_files",
        json!({"pattern": "*", "path": outside.to_string_lossy()}),
    );
    assert!(error_of(&absolute).starts_with("PolicyError: "));

    let missing = dispatch(
        &ctx,
        "search_files",
        json!({"pattern": "x", "path": "nope"}),
    );
    assert_eq!(error_of(&missing), "Path not found: nope");
}

/// A symlink, or `false` where the OS will not make one (Windows without
/// Developer Mode or the privilege).
fn symlink(target: &std::path::Path, link: &std::path::Path, dir: bool) -> bool {
    #[cfg(unix)]
    {
        let _ = dir;
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
        if dir {
            std::os::windows::fs::symlink_dir(target, link).is_ok()
        } else {
            std::os::windows::fs::symlink_file(target, link).is_ok()
        }
    }
}

#[test]
fn symlinks_are_not_followed_out_of_the_workspace() {
    let workspace = Workspace::new("links");
    let outside = Workspace::new("links-outside");
    outside.write("secret.txt", b"needle outside\n");
    outside.write("dir/inner.txt", b"needle outside dir\n");
    workspace.write("real.txt", b"needle inside\n");
    let made = symlink(
        &outside.root.join("secret.txt"),
        &workspace.root.join("leak.txt"),
        false,
    ) && symlink(
        &outside.root.join("dir"),
        &workspace.root.join("leakdir"),
        true,
    ) && symlink(
        &workspace.root.join("real.txt"),
        &workspace.root.join("alias.txt"),
        false,
    );
    if !made {
        eprintln!("skipping: symlinks cannot be created here");
        return;
    }
    let ctx = workspace.context();

    let result = dispatch(&ctx, "search_files", json!({"pattern": "needle"}));
    assert_eq!(
        content(&result),
        "alias.txt:1: needle inside\nreal.txt:1: needle inside"
    );
    let found = dispatch(&ctx, "find_files", json!({"pattern": "*"}));
    assert_eq!(content(&found), "alias.txt\nreal.txt");
}

#[test]
fn an_allowed_root_is_searched_only_when_named() {
    let workspace = Workspace::new("allowed");
    let skills = Workspace::new("allowed-skills");
    workspace.write("a.txt", b"needle in workspace\n");
    skills.write("demo/SKILL.md", b"needle in skill\n");
    let mut ctx = workspace.context();
    let root = std::fs::canonicalize(&skills.root).unwrap();
    ctx.policy.allow([root.clone()]);

    let default = dispatch(&ctx, "search_files", json!({"pattern": "needle"}));
    assert_eq!(content(&default), "a.txt:1: needle in workspace");

    let named = dispatch(
        &ctx,
        "search_files",
        json!({"pattern": "needle", "path": root.join("demo").to_string_lossy()}),
    );
    let text = content(&named);
    assert!(
        text.ends_with("/demo/SKILL.md:1: needle in skill"),
        "{text}"
    );
    assert!(!text.contains('\\'), "{text}");
}

#[test]
fn bad_patterns_are_argument_errors() {
    let workspace = project("bad");
    let ctx = workspace.context();

    let regex = dispatch(&ctx, "search_files", json!({"pattern": "(unclosed"}));
    assert_eq!(
        error_of(&regex),
        "Invalid arguments for 'search_files': pattern: Invalid regex: unclosed group"
    );
    let glob = dispatch(&ctx, "search_files", json!({"pattern": "x", "glob": "[z"}));
    assert!(
        error_of(&glob).starts_with("Invalid arguments for 'search_files': glob: Invalid glob: "),
        "{glob:?}"
    );
    let pattern = dispatch(&ctx, "find_files", json!({"pattern": "a{b"}));
    assert!(
        error_of(&pattern)
            .starts_with("Invalid arguments for 'find_files': pattern: Invalid glob: "),
        "{pattern:?}"
    );
    let missing = dispatch(&ctx, "find_files", json!({}));
    assert_eq!(
        error_of(&missing),
        "Invalid arguments for 'find_files': pattern: Field required"
    );
    let context = dispatch(&ctx, "search_files", json!({"pattern": "x", "context": 11}));
    assert_eq!(
        error_of(&context),
        "Invalid arguments for 'search_files': context: Input should be less than or equal to 10"
    );
}

#[test]
fn find_files_matches_globs_in_path_order() {
    let workspace = project("find");
    let ctx = workspace.context();

    let rust = dispatch(&ctx, "find_files", json!({"pattern": "src/**/*.rs"}));
    assert_eq!(content(&rust), "src/lib/util.rs\nsrc/main.rs");
    let named = dispatch(&ctx, "find_files", json!({"pattern": "*.rs"}));
    assert_eq!(content(&named), "src/lib/util.rs\nsrc/main.rs");
    // `*` does not cross a separator.
    let shallow = dispatch(&ctx, "find_files", json!({"pattern": "src/*.rs"}));
    assert_eq!(content(&shallow), "src/main.rs");
    let hidden = dispatch(&ctx, "find_files", json!({"pattern": "*.yml"}));
    assert_eq!(content(&hidden), ".github/workflows/ci.yml");
    let under = dispatch(
        &ctx,
        "find_files",
        json!({"pattern": "*", "path": "src/lib"}),
    );
    assert_eq!(content(&under), "src/lib/util.rs");
    let none = dispatch(&ctx, "find_files", json!({"pattern": "*.py"}));
    assert_eq!(content(&none), "No files matched.");
    assert_eq!(none.metadata["files"], json!(0));
}

#[test]
fn search_results_are_redacted_through_the_vault() {
    let workspace = Workspace::new("vault");
    workspace.write("config.txt", b"token = hunter2-hunter2-hunter2\n");
    let ctx = workspace.context();
    ctx.secrets
        .set("TOKEN", "hunter2-hunter2-hunter2".to_owned());

    let result = dispatch(&ctx, "search_files", json!({"pattern": "token"}));
    assert_eq!(content(&result), "config.txt:1: token = ${secret:TOKEN}");
}
