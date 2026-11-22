//! A surface's data model: a JSON value addressed by JSON Pointers (RFC 6901),
//! with the rules every A2UI SDK shares (the specification's conformance suite,
//! `core/data_model.yaml`):
//!
//! - Empty segments collapse, so `//a///b//`, `/a/b/`, and `/a/b` are one path;
//!   `""` and `/` are the root. `~1` unescapes to `/` before `~0` to `~`.
//! - `null` reads as absent, and writing through it creates the containers.
//! - A write creates missing containers: an array when the next segment is an
//!   index, else an object. Array indices are canonical (no leading zeros), at
//!   most [`MAX_ARRAY_INDEX`]; writing past the end pads with `null`.
//! - Writing through a string, number, or boolean is an error, as is a
//!   non-numeric segment on an array.
//! - `__proto__`, `constructor`, and `prototype` are refused as segments in
//!   every SDK, whatever its language, so models move between them unchanged.
//! - A watcher is notified when the value at its own path changed: a write to
//!   an ancestor, the path, or a descendant that left it the same notifies no one.

use serde_json::{Map, Value};

/// The largest array index a write may use.
pub const MAX_ARRAY_INDEX: usize = 10_000;

const FORBIDDEN: &[&str] = &["__proto__", "constructor", "prototype"];

/// A data model operation the model or the user cannot do.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct DataError {
    /// What went wrong, naming the path.
    pub message: String,
    /// The path as given.
    pub path: String,
}

fn error(path: &str, message: String) -> DataError {
    DataError {
        message,
        path: path.to_string(),
    }
}

/// The segments of a JSON Pointer, unescaped, with empty segments dropped.
/// Refuses the forbidden segments.
pub fn segments(path: &str) -> Result<Vec<String>, DataError> {
    let mut out = Vec::new();
    for raw in path.split('/').filter(|s| !s.is_empty()) {
        let segment = raw.replace("~1", "/").replace("~0", "~");
        if FORBIDDEN.contains(&segment.as_str()) {
            return Err(error(
                path,
                format!("Forbidden path segment '{segment}' in path '{path}'."),
            ));
        }
        out.push(segment);
    }
    Ok(out)
}

/// The JSON Pointer for segments (`/` for none).
pub fn pointer(segments: &[String]) -> String {
    if segments.is_empty() {
        return "/".into();
    }
    segments
        .iter()
        .map(|s| format!("/{}", s.replace('~', "~0").replace('/', "~1")))
        .collect()
}

/// A path in its normal form: segments collapsed and re-escaped, `/` for the
/// root.
pub fn normalize(path: &str) -> Result<String, DataError> {
    Ok(pointer(&segments(path)?))
}

/// Resolve `path` against a context path: an absolute path (starting with `/`)
/// stands alone; a relative one, inside a template, extends the context; `""`
/// and `.` are the context itself.
pub fn resolve(context: &str, path: &str) -> String {
    if path.starts_with('/') {
        return path.to_string();
    }
    let base = context.trim_end_matches('/');
    if path.is_empty() || path == "." {
        return if base.is_empty() {
            "/".into()
        } else {
            base.to_string()
        };
    }
    format!("{base}/{path}")
}

/// A canonical array index: decimal digits, no leading zero unless `0`.
fn index(segment: &str) -> Option<usize> {
    let canonical = !segment.is_empty()
        && segment.bytes().all(|b| b.is_ascii_digit())
        && (segment == "0" || !segment.starts_with('0'));
    if canonical {
        segment.parse().ok()
    } else {
        None
    }
}

/// An id for one watch, so the same path can be watched twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WatchId(usize);

/// A surface's data model.
#[derive(Debug, Clone, PartialEq)]
pub struct DataModel {
    root: Value,
    watches: Vec<(WatchId, Vec<String>)>,
    next: usize,
}

impl Default for DataModel {
    fn default() -> Self {
        Self::new(Value::Object(Map::new()))
    }
}

impl DataModel {
    /// A data model holding `root`.
    pub fn new(root: Value) -> Self {
        Self {
            root,
            watches: Vec::new(),
            next: 0,
        }
    }

    /// The whole model.
    pub fn root(&self) -> &Value {
        &self.root
    }

    /// The value at `path`, or `None` when it is absent or `null`.
    pub fn get(&self, path: &str) -> Result<Option<&Value>, DataError> {
        Ok(lookup(&self.root, &segments(path)?))
    }

    /// Watch `path`: [`DataModel::set`] and [`DataModel::delete`] report this
    /// id when the value there changes.
    pub fn watch(&mut self, path: &str) -> Result<WatchId, DataError> {
        let id = WatchId(self.next);
        self.next += 1;
        self.watches.push((id, segments(path)?));
        Ok(id)
    }

    /// Stop every watch.
    pub fn dispose(&mut self) {
        self.watches.clear();
    }

    /// Set the value at `path`, creating containers on the way; `/` replaces
    /// the whole model. Returns the watches whose value changed.
    pub fn set(&mut self, path: &str, value: Value) -> Result<Vec<WatchId>, DataError> {
        let parts = segments(path)?;
        let before = self.snapshot();
        write(&mut self.root, &parts, path, Some(value))?;
        Ok(self.changed(before))
    }

    /// Remove the value at `path` (an array element becomes `null`; `/` resets
    /// the model to `{}`). Nothing is created; an absent path is a no-op.
    pub fn delete(&mut self, path: &str) -> Result<Vec<WatchId>, DataError> {
        let parts = segments(path)?;
        let before = self.snapshot();
        if parts.is_empty() {
            self.root = Value::Object(Map::new());
        } else if lookup_parent(&self.root, &parts).is_some() {
            write(&mut self.root, &parts, path, None)?;
        }
        Ok(self.changed(before))
    }

    fn snapshot(&self) -> Vec<Option<Value>> {
        self.watches
            .iter()
            .map(|(_, path)| lookup(&self.root, path).cloned())
            .collect()
    }

    fn changed(&self, before: Vec<Option<Value>>) -> Vec<WatchId> {
        self.watches
            .iter()
            .zip(before)
            .filter(|((_, path), old)| lookup(&self.root, path) != old.as_ref())
            .map(|((id, _), _)| *id)
            .collect()
    }
}

fn lookup<'a>(root: &'a Value, parts: &[String]) -> Option<&'a Value> {
    let mut current = root;
    for part in parts {
        current = match current {
            Value::Object(map) => map.get(part)?,
            Value::Array(items) => items.get(index(part)?)?,
            _ => return None,
        };
    }
    (!current.is_null()).then_some(current)
}

/// Whether the container that would hold the last segment exists.
fn lookup_parent<'a>(root: &'a Value, parts: &[String]) -> Option<&'a Value> {
    let (_, parents) = parts.split_last()?;
    let mut current = root;
    for part in parents {
        current = match current {
            Value::Object(map) => map.get(part)?,
            Value::Array(items) => items.get(index(part)?)?,
            _ => return None,
        };
    }
    matches!(current, Value::Object(_) | Value::Array(_)).then_some(current)
}

fn container_for(next: &str) -> Value {
    if index(next).is_some() {
        Value::Array(Vec::new())
    } else {
        Value::Object(Map::new())
    }
}

/// Write `value` (or remove, for `None`) at `parts` under `root`.
fn write(
    root: &mut Value,
    parts: &[String],
    path: &str,
    value: Option<Value>,
) -> Result<(), DataError> {
    let Some((last, parents)) = parts.split_last() else {
        root.clone_from(&value.unwrap_or_else(|| Value::Object(Map::new())));
        return Ok(());
    };
    let primitive = |segment: &str| {
        error(
            path,
            format!("Cannot set path '{path}': segment '{segment}' is a primitive value."),
        )
    };
    if !matches!(root, Value::Object(_) | Value::Array(_) | Value::Null) {
        return Err(error(
            path,
            format!("Cannot set path '{path}': the data model root is a primitive value."),
        ));
    }
    if root.is_null() {
        *root = container_for(&parts[0]);
    }
    let mut current = root;
    for (position, part) in parents.iter().enumerate() {
        let next = &parts[position + 1];
        let slot = match current {
            Value::Object(map) => map.entry(part.clone()).or_insert(Value::Null),
            Value::Array(items) => {
                let i = array_index(part, path)?;
                if i >= items.len() {
                    items.resize(i + 1, Value::Null);
                }
                &mut items[i]
            }
            _ => return Err(primitive(part)),
        };
        if slot.is_null() {
            if let Some(i) = index(next) {
                if i > MAX_ARRAY_INDEX {
                    return Err(too_large(path, next));
                }
            }
            *slot = container_for(next);
        } else if !matches!(slot, Value::Object(_) | Value::Array(_)) {
            return Err(primitive(next));
        }
        current = slot;
    }
    match current {
        Value::Object(map) => match value {
            Some(value) => {
                map.insert(last.clone(), value);
            }
            None => {
                map.remove(last);
            }
        },
        Value::Array(items) => {
            let i = array_index(last, path)?;
            match value {
                Some(value) => {
                    if i >= items.len() {
                        items.resize(i + 1, Value::Null);
                    }
                    items[i] = value;
                }
                None => {
                    if let Some(slot) = items.get_mut(i) {
                        *slot = Value::Null;
                    }
                }
            }
        }
        _ => return Err(primitive(last)),
    }
    Ok(())
}

fn array_index(segment: &str, path: &str) -> Result<usize, DataError> {
    let Some(i) = index(segment) else {
        return Err(error(
            path,
            format!("Cannot use non-numeric segment '{segment}' on an array in path '{path}'."),
        ));
    };
    if i > MAX_ARRAY_INDEX {
        return Err(too_large(path, segment));
    }
    Ok(i)
}

fn too_large(path: &str, segment: &str) -> DataError {
    error(
        path,
        format!(
            "Cannot set path '{path}': array index '{segment}' exceeds maximum supported index ({MAX_ARRAY_INDEX})."
        ),
    )
}
