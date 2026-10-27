//! Comment-preserving config write-back, as Python's `config_writer.py`.
//!
//! [`crate::config`] reads TOML and is left untouched; this adds the write half
//! with `toml_edit`, whose document model round-trips comments, blank lines, and
//! formatting, so a user's hand-written comments survive every save.
//!
//! Two steps by design: [`plan_edit`] computes the new text and a unified diff
//! without touching the disk, so the interface can show exactly what will change;
//! [`apply_edit`] then writes atomically and reloads the configuration.
//!
//! Secrets are never written: [`plan_edit`] refuses secret-looking leaf keys
//! unless the value is a pure `${VAR}` reference, the one accepted form under such
//! a key (for example an MCP `headers.Authorization`).

use crate::config::{load_config, Config, ConfigPaths, Overrides};
use crate::difflib::{splitlines, unified_diff};
use crate::secrets::redact_key_shapes;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use toml_edit::{DocumentMut, Item, Table, Value};

/// Leaf key names that would put a plaintext secret in config. Config only ever
/// names the environment variable that holds a key (`api_key_env`); the key itself
/// lives in the OS keyring.
const FORBIDDEN_LEAF_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "key",
    "token",
    "secret",
    "password",
    "authorization",
];

/// A `${VAR}` reference; the name must be an identifier, so a literal that merely
/// contains `${` cannot pass for one.
static VAR_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$\{[A-Za-z_][A-Za-z0-9_]*\}").expect("valid"));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `~/.agent86/config.toml`.
    User,
    /// `./.agent86/config.toml`.
    Project,
}

/// The target config file could not be parsed or written, or the change was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ConfigWriteError(pub String);

/// One change: set the value at a key path, or with `None` remove it (Python's
/// `DELETE`), so one edit can remove one server and set a key on another.
pub type Change = (Vec<String>, Option<Value>);

/// A prepared, not yet written change to one config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEdit {
    pub scope: Scope,
    pub path: PathBuf,
    pub before_text: String,
    pub after_text: String,
    pub diff: String,
}

impl ConfigEdit {
    pub fn is_noop(&self) -> bool {
        self.before_text == self.after_text
    }
}

pub fn scope_path(scope: Scope, paths: &ConfigPaths) -> Result<PathBuf, ConfigWriteError> {
    match scope {
        Scope::Project => Ok(paths.project.clone()),
        Scope::User => paths.user.clone().ok_or_else(|| {
            ConfigWriteError("no home directory, so there is no user config to write".to_owned())
        }),
    }
}

/// True when `value` is a pure `${VAR}` reference string: it holds at least one
/// reference, and with every reference blanked out, nothing key-shaped is left. A
/// decoy reference appended to a real key must not smuggle it past the guard.
fn is_var_ref(value: &Value) -> bool {
    let Some(text) = value.as_str() else {
        return false;
    };
    if !VAR_REF.is_match(text) {
        return false;
    }
    let rest = VAR_REF.replace_all(text, "");
    redact_key_shapes(&rest, "") == rest
}

/// Compute the text after `changes` and a unified diff. Touches nothing on disk.
pub fn plan_edit(
    scope: Scope,
    paths: &ConfigPaths,
    changes: &[Change],
) -> Result<ConfigEdit, ConfigWriteError> {
    let path = scope_path(scope, paths)?;
    let before_text = read_text(&path)?;
    let mut doc: DocumentMut = before_text.parse().map_err(|error| {
        ConfigWriteError(format!("Malformed config at {}: {error}", path.display()))
    })?;

    for (key_path, value) in changes {
        let Some((leaf, parents)) = key_path.split_last() else {
            return Err(ConfigWriteError("empty key path in changes".to_owned()));
        };
        let Some(value) = value else {
            delete(&mut doc, parents, leaf);
            continue;
        };
        if FORBIDDEN_LEAF_KEYS.contains(&leaf.to_lowercase().as_str()) && !is_var_ref(value) {
            return Err(ConfigWriteError(format!(
                "refusing to write '{}' to config: secrets belong in the OS keyring or a \
                 ${{VAR}} reference, never a literal value",
                key_path.join(".")
            )));
        }
        set(doc.as_table_mut(), parents, leaf, value.clone()).map_err(|part| {
            ConfigWriteError(format!(
                "cannot write '{}': '{part}' is not a table",
                key_path.join(".")
            ))
        })?;
    }

    let after_text = doc.to_string();
    let label = path.display().to_string();
    let diff = unified_diff(
        &splitlines(&before_text),
        &splitlines(&after_text),
        &label,
        &label,
        3,
    );
    Ok(ConfigEdit {
        scope,
        path,
        before_text,
        after_text,
        diff,
    })
}

/// Remove `parents.leaf`; a path that is not there is left alone, so removing a
/// server already deleted by hand shows "No changes." rather than failing.
///
/// Comments written above a removed table stay in the file, as with Python's
/// `tomlkit`, which keeps them with the table before it: they move onto the next
/// table, or to the end of the file.
fn delete(doc: &mut DocumentMut, parents: &[String], leaf: &str) {
    let mut node: &mut dyn toml_edit::TableLike = doc.as_table_mut();
    for part in parents {
        match node.get_mut(part).and_then(Item::as_table_like_mut) {
            Some(next) => node = next,
            None => return,
        }
    }
    let Some(Item::Table(table)) = node.remove(leaf) else {
        return;
    };
    let comments = prefix_of(&table);
    if comments.trim().is_empty() {
        return;
    }
    let position = table.position().unwrap_or(isize::MAX);
    let mut nearest = None;
    next_table(doc.as_table(), position, &mut Vec::new(), &mut nearest);
    match nearest.and_then(|(_, path)| table_at(doc.as_table_mut(), &path)) {
        Some(next) => {
            let existing = prefix_of(next);
            next.decor_mut().set_prefix(format!("{comments}{existing}"));
        }
        None => {
            let trailing = doc.trailing().as_str().unwrap_or_default().to_owned();
            doc.set_trailing(format!("{trailing}{comments}"));
        }
    }
}

fn prefix_of(table: &Table) -> String {
    table
        .decor()
        .prefix()
        .and_then(|prefix| prefix.as_str())
        .unwrap_or_default()
        .to_owned()
}

/// The key path of the table drawn first after `position`, at any depth.
fn next_table(
    table: &Table,
    position: isize,
    path: &mut Vec<String>,
    nearest: &mut Option<(isize, Vec<String>)>,
) {
    for (key, item) in table.iter() {
        let Item::Table(child) = item else {
            continue;
        };
        path.push(key.to_owned());
        if let Some(own) = child.position().filter(|&own| own > position) {
            if !child.is_implicit() && nearest.as_ref().is_none_or(|(best, _)| own < *best) {
                *nearest = Some((own, path.clone()));
            }
        }
        next_table(child, position, path, nearest);
        path.pop();
    }
}

fn table_at<'a>(table: &'a mut Table, path: &[String]) -> Option<&'a mut Table> {
    match path.split_first() {
        None => Some(table),
        Some((first, rest)) => table_at(table.get_mut(first)?.as_table_mut()?, rest),
    }
}

/// Set `parents.leaf = value`, creating missing tables. A value replacing another
/// keeps the old one's surrounding whitespace and trailing comment.
fn set(table: &mut Table, parents: &[String], leaf: &str, mut value: Value) -> Result<(), String> {
    let mut node: &mut dyn toml_edit::TableLike = table;
    for part in parents {
        if !node.contains_key(part) {
            let mut table = Table::new();
            // `[providers.groq]` alone, without an empty `[providers]` above it.
            table.set_implicit(true);
            node.insert(part, Item::Table(table));
        }
        node = node
            .get_mut(part)
            .and_then(Item::as_table_like_mut)
            .ok_or_else(|| part.clone())?;
    }
    if let Some(Item::Value(old)) = node.get(leaf) {
        *value.decor_mut() = old.decor().clone();
    }
    node.insert(leaf, Item::Value(value));
    Ok(())
}

/// Write the edit atomically, then reload the configuration as Python's
/// `load_config()` does: the files and the environment, without the command line's
/// `--model` or `--approval`.
pub fn apply_edit(edit: &ConfigEdit, paths: &ConfigPaths) -> Result<Config, ConfigWriteError> {
    write_atomic(&edit.path, &edit.after_text)?;
    load_config(paths, &Overrides::from_env(None, None))
        .map_err(|error| ConfigWriteError(error.to_string()))
}

/// The file's text with Python's universal newlines, or empty when it does not exist.
fn read_text(path: &Path) -> Result<String, ConfigWriteError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text.replace("\r\n", "\n").replace('\r', "\n")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(ConfigWriteError(format!(
            "Could not read {}: {error}",
            path.display()
        ))),
    }
}

/// Write then rename, so a crash mid-write never truncates an existing config. The
/// temporary file sits in the same directory, which keeps the rename atomic.
fn write_atomic(path: &Path, text: &str) -> Result<(), ConfigWriteError> {
    let fail = |error: std::io::Error| {
        ConfigWriteError(format!("Could not write {}: {error}", path.display()))
    };
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(fail)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let tmp = parent.join(format!(".config-{}-{nanos}.toml.tmp", std::process::id()));
    let written = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.map_err(fail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(name: &str) -> (PathBuf, ConfigPaths) {
        let root =
            std::env::temp_dir().join(format!("agent86-writer-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let paths = ConfigPaths {
            user: Some(root.join("home").join(".agent86").join("config.toml")),
            project: root.join("project").join(".agent86").join("config.toml"),
        };
        (root, paths)
    }

    fn change(path: &str, value: &str) -> Change {
        (
            path.split('.').map(str::to_owned).collect(),
            Some(Value::from(value)),
        )
    }

    #[test]
    fn edits_keep_comments_and_show_a_diff() {
        let (root, paths) = workspace("comments");
        let user = paths.user.clone().unwrap();
        std::fs::create_dir_all(user.parent().unwrap()).unwrap();
        std::fs::write(
            &user,
            "# my settings\n[model]\ndefault = \"openai:gpt-4o\"  # the usual\n\n\
             # local box\n[providers.local]\nbase_url = \"http://localhost:8080/v1\"\n",
        )
        .unwrap();
        let edit = plan_edit(
            Scope::User,
            &paths,
            &[
                change("model.default", "groq:llama-3.3-70b-versatile"),
                change("providers.groq.api_key_env", "GROQ_API_KEY"),
            ],
        )
        .unwrap();
        assert_eq!(
            edit.after_text,
            "# my settings\n[model]\ndefault = \"groq:llama-3.3-70b-versatile\"  # the usual\n\n\
             # local box\n[providers.local]\nbase_url = \"http://localhost:8080/v1\"\n\n\
             [providers.groq]\napi_key_env = \"GROQ_API_KEY\"\n"
        );
        let label = user.display().to_string();
        assert!(edit
            .diff
            .starts_with(&format!("--- {label}\n+++ {label}\n@@ ")));
        assert!(edit
            .diff
            .contains("\n-default = \"openai:gpt-4o\"  # the usual\n+default = \"groq:"));
        assert!(!edit.is_noop());
        // Nothing is written until the edit is applied.
        assert!(std::fs::read_to_string(&user)
            .unwrap()
            .contains("openai:gpt-4o"));

        let config = apply_edit(&edit, &paths).unwrap();
        assert_eq!(config.model.default, "groq:llama-3.3-70b-versatile");
        assert_eq!(std::fs::read_to_string(&user).unwrap(), edit.after_text);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_missing_file_is_created_with_new_tables() {
        let (root, paths) = workspace("new");
        let edit = plan_edit(
            Scope::Project,
            &paths,
            &[
                change("model.default", "openrouter:openai/gpt-4o-mini"),
                change(
                    "providers.openrouter.base_url",
                    "https://openrouter.ai/api/v1",
                ),
            ],
        )
        .unwrap();
        assert_eq!(edit.before_text, "");
        assert_eq!(
            edit.after_text,
            "[model]\ndefault = \"openrouter:openai/gpt-4o-mini\"\n\n\
             [providers.openrouter]\nbase_url = \"https://openrouter.ai/api/v1\"\n"
        );
        apply_edit(&edit, &paths).unwrap();
        assert_eq!(
            std::fs::read_to_string(&paths.project).unwrap(),
            edit.after_text
        );
        // Setting what is already there changes nothing.
        let again = plan_edit(
            Scope::Project,
            &paths,
            &[change("model.default", "openrouter:openai/gpt-4o-mini")],
        )
        .unwrap();
        assert!(again.is_noop());
        assert_eq!(again.diff, "");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn secrets_are_refused_unless_they_are_references() {
        let (root, paths) = workspace("secrets");
        let refused = plan_edit(
            Scope::Project,
            &paths,
            &[change(
                "providers.groq.api_key",
                "gsk_abcdefghijklmnopqrstuvwxyz",
            )],
        )
        .unwrap_err();
        assert_eq!(
            refused.0,
            "refusing to write 'providers.groq.api_key' to config: secrets belong in the OS \
             keyring or a ${VAR} reference, never a literal value"
        );
        assert!(plan_edit(
            Scope::Project,
            &paths,
            &[change(
                "mcp.servers.gh.headers.Authorization",
                "Bearer ${GH_TOKEN}"
            )],
        )
        .is_ok());
        assert!(
            plan_edit(
                Scope::Project,
                &paths,
                &[change("env.TOKEN", "sk-liveXXXXXXXXXXXXXXXXXXXX${notreal}")],
            )
            .is_err(),
            "a decoy reference does not excuse a key"
        );
        assert!(!paths.project.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn servers_are_removed_with_their_comments_kept_and_lists_written() {
        let (root, paths) = workspace("delete");
        std::fs::create_dir_all(paths.project.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.project,
            "# servers\n[mcp.servers.fs]\ncommand = \"npx\"\n\n\
             # the backup one\n[mcp.servers.old]\nurl = \"http://x/mcp\"\n\n\
             [mcp.servers.old.headers]\nX-Trace = \"1\"\n\n\
             # sandbox\n[sandbox]\nmode = \"subprocess\"\n",
        )
        .unwrap();
        let path = |text: &str| text.split('.').map(str::to_owned).collect::<Vec<_>>();
        let args = Value::Array(["-y", "server"].into_iter().collect());
        let edit = plan_edit(
            Scope::Project,
            &paths,
            &[
                (path("mcp.servers.gh.command"), Some(Value::from("npx"))),
                (path("mcp.servers.gh.args"), Some(args)),
                (path("mcp.servers.gh.enabled"), Some(Value::from(true))),
                (path("mcp.servers.old"), None),
                (path("mcp.servers.never"), None),
            ],
        )
        .unwrap();
        assert_eq!(
            edit.after_text,
            "# servers\n[mcp.servers.fs]\ncommand = \"npx\"\n\n\
             [mcp.servers.gh]\ncommand = \"npx\"\nargs = [\"-y\", \"server\"]\nenabled = true\n\n\
             # the backup one\n\n# sandbox\n[sandbox]\nmode = \"subprocess\"\n"
        );
        let config = apply_edit(&edit, &paths).unwrap();
        assert!(!config.mcp_servers.contains_key("old"));
        assert_eq!(config.mcp_servers["gh"].args, ["-y", "server"]);

        // The last table's comments move to the end of the file.
        let edit = plan_edit(Scope::Project, &paths, &[(path("sandbox"), None)]).unwrap();
        assert!(
            edit.after_text
                .ends_with("enabled = true\n\n# the backup one\n\n# sandbox\n"),
            "{}",
            edit.after_text
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_files_are_reported_not_overwritten() {
        let (root, paths) = workspace("malformed");
        std::fs::create_dir_all(paths.project.parent().unwrap()).unwrap();
        std::fs::write(&paths.project, "[model\n").unwrap();
        let error =
            plan_edit(Scope::Project, &paths, &[change("model.default", "a:b")]).unwrap_err();
        assert!(error.0.starts_with("Malformed config at "), "{error}");
        let _ = std::fs::remove_dir_all(root);
    }
}
