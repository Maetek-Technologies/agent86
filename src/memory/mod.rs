//! Long-term memory, ported from Python's `memory/` package: the episodic and
//! semantic halves of `store.py`, plus `episodic.py` and `semantic.py`.
//!
//! - **Episodes**: one row per completed turn (task and outcome), the flight data
//!   recorder. When a new task begins, the most similar past episodes are recalled
//!   into the system prompt, so the agent benefits from (or is warned by) what
//!   happened last time.
//! - **Memories**: durable facts the agent stores and looks up through the
//!   `remember` and `recall` tools.
//!
//! Both live in the session database ([`SessionStore`]) with their embeddings as
//! `float32` BLOBs. Search is a brute-force cosine scan, as Python's store does
//! without `sqlite-vec`; retention caps bound the log tables, though curated
//! facts are never pruned automatically. Rows embedded with a different
//! dimension (another embedder) are skipped, never compared.

pub mod embeddings;
pub mod hub;
pub mod sentence_transformers;

use crate::session::{SessionStore, StoreError};
use embeddings::{cosine, pack, unpack};
use rusqlite::{params, Connection};
use serde_json::{Map, Value};
use std::time::{SystemTime, UNIX_EPOCH};

/// Recalled episodes must score at least this, so unrelated past tasks stay out.
pub const EPISODE_MIN_SCORE: f64 = 0.35;
/// Facts `recall` returns must score at least this.
pub const FACT_MIN_SCORE: f64 = 0.30;
/// `metadata["kind"]` of an archived compaction rather than a turn; never recalled.
pub const COMPACTION_KIND: &str = "compaction";
/// The note's heading, as Python's `recall_note`.
const RECALL_HEADING: &str = "Relevant past experience (from memory; may help or warn):";

/// One search result, as Python's `Hit`.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub id: i64,
    /// The episode's task, or the fact's text.
    pub text: String,
    /// Cosine similarity to the query.
    pub score: f64,
    /// The row's metadata; an episode's also carries its `outcome`.
    pub metadata: Map<String, Value>,
}

impl Hit {
    /// An episode's outcome, or `""`.
    pub fn outcome(&self) -> &str {
        self.metadata
            .get("outcome")
            .and_then(Value::as_str)
            .unwrap_or_default()
    }
}

/// Rows a prune removed, per table, in Python's dict order.
pub type Removed = Vec<(&'static str, i64)>;

fn error(error: rusqlite::Error) -> StoreError {
    StoreError(error.to_string())
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

impl SessionStore {
    // ---- episodes -----------------------------------------------------------

    /// Record a completed turn, as Python's `add_episode`. Returns the new row id.
    pub fn add_episode(
        &self,
        session_id: &str,
        task: &str,
        outcome: &str,
        metadata: &Map<String, Value>,
    ) -> Result<i64, StoreError> {
        let embedding = pack(&self.embedder.encode_one(task));
        let connection = self.connection();
        connection
            .execute(
                "INSERT INTO episodes (session_id, ts, task, outcome, embedding, metadata_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    session_id,
                    now(),
                    task,
                    outcome,
                    embedding,
                    metadata_json(metadata)
                ],
            )
            .map_err(error)?;
        Ok(connection.last_insert_rowid())
    }

    /// The `k` episodes whose task is most similar to `query`.
    pub fn search_episodes(&self, query: &str, k: usize) -> Result<Vec<Hit>, StoreError> {
        let query = self.embedder.encode_one(query);
        let connection = self.connection();
        rank(
            &connection,
            "SELECT id, task, embedding, metadata_json, outcome FROM episodes",
            &query,
            k,
            true,
        )
    }

    // ---- semantic memories --------------------------------------------------

    /// Store a fact, as Python's `add_memory`. Returns the new row id.
    pub fn add_memory(
        &self,
        text: &str,
        kind: &str,
        metadata: &Map<String, Value>,
    ) -> Result<i64, StoreError> {
        let embedding = pack(&self.embedder.encode_one(text));
        let connection = self.connection();
        connection
            .execute(
                "INSERT INTO memories (ts, kind, text, embedding, metadata_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![now(), kind, text, embedding, metadata_json(metadata)],
            )
            .map_err(error)?;
        Ok(connection.last_insert_rowid())
    }

    /// The `k` facts most similar to `query`.
    pub fn search_memories(&self, query: &str, k: usize) -> Result<Vec<Hit>, StoreError> {
        let query = self.embedder.encode_one(query);
        let connection = self.connection();
        rank(
            &connection,
            "SELECT id, text, embedding, metadata_json FROM memories",
            &query,
            k,
            false,
        )
    }

    /// Delete one fact by id; `true` if a row was removed.
    pub fn delete_memory(&self, id: i64) -> Result<bool, StoreError> {
        self.connection()
            .execute("DELETE FROM memories WHERE id = ?1", params![id])
            .map(|removed| removed > 0)
            .map_err(error)
    }

    // ---- episodic and semantic facets ---------------------------------------

    /// Past episodes relevant to `task`, compactions excluded, as Python's
    /// `EpisodicMemory.recall`.
    pub fn recall_episodes(&self, task: &str, k: usize) -> Result<Vec<Hit>, StoreError> {
        Ok(self
            .search_episodes(task, k)?
            .into_iter()
            .filter(|hit| {
                hit.score >= EPISODE_MIN_SCORE
                    && hit.metadata.get("kind").and_then(Value::as_str) != Some(COMPACTION_KIND)
            })
            .collect())
    }

    /// A system-context note summarizing relevant past turns, or `None`, as
    /// Python's `EpisodicMemory.recall_note`.
    pub fn recall_note(&self, task: &str) -> Option<String> {
        let hits = self.recall_episodes(task, 3).ok()?;
        if hits.is_empty() {
            return None;
        }
        let mut lines = vec![RECALL_HEADING.to_owned()];
        for hit in &hits {
            let mut outcome = hit
                .outcome()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if outcome.chars().count() > 200 {
                outcome = outcome.chars().take(200).collect::<String>() + " ...";
            }
            lines.push(format!(
                "- task: {} -> {outcome}",
                crate::types::python_repr(&hit.text)
            ));
        }
        Some(lines.join("\n"))
    }

    /// Facts relevant to `query`, as Python's `SemanticMemory.search`.
    pub fn search_facts(&self, query: &str, k: usize) -> Result<Vec<Hit>, StoreError> {
        Ok(self
            .search_memories(query, k)?
            .into_iter()
            .filter(|hit| hit.score >= FACT_MIN_SCORE)
            .collect())
    }

    // ---- pruning ------------------------------------------------------------

    /// Apply retention limits, as Python's `prune`: episodes and sessions (the
    /// append-only log) always, curated facts only when `memories` is set.
    pub fn prune(
        &self,
        older_than_days: Option<f64>,
        keep_last: Option<i64>,
        memories: bool,
    ) -> Result<Removed, StoreError> {
        let connection = self.connection();
        let mut removed = vec![
            (
                "episodes",
                prune_table(&connection, Table::Episodes, older_than_days, keep_last)?,
            ),
            (
                "sessions",
                prune_table(&connection, Table::Sessions, older_than_days, keep_last)?,
            ),
        ];
        if memories {
            removed.push((
                "memories",
                prune_table(&connection, Table::Memories, older_than_days, keep_last)?,
            ));
        }
        Ok(removed)
    }

    /// Per-table retention caps for the log, as Python's `enforce_retention`. A cap
    /// of 0 disables it; facts are never touched. Only tables that lost rows are
    /// reported.
    pub fn enforce_retention(
        &self,
        max_episodes: i64,
        max_sessions: i64,
        max_age_days: f64,
    ) -> Result<Removed, StoreError> {
        let age = (max_age_days != 0.0).then_some(max_age_days);
        let connection = self.connection();
        let mut removed = Removed::new();
        for (name, table, cap) in [
            ("episodes", Table::Episodes, max_episodes),
            ("sessions", Table::Sessions, max_sessions),
        ] {
            if cap != 0 || age.is_some() {
                let count = prune_table(&connection, table, age, (cap != 0).then_some(cap))?;
                if count != 0 {
                    removed.push((name, count));
                }
            }
        }
        Ok(removed)
    }
}

/// `json.dumps(metadata or {})`.
fn metadata_json(metadata: &Map<String, Value>) -> String {
    crate::pyjson::python_json_dumps(&Value::Object(metadata.clone()), true)
}

/// Score every row against `query` and keep the best `k`, as Python's `_rank`.
///
/// The SELECT yields `id, text, embedding, metadata_json` and, for episodes, the
/// outcome, which joins the metadata. The sort is stable, so ties keep row order.
fn rank(
    connection: &Connection,
    sql: &str,
    query: &[f64],
    k: usize,
    with_outcome: bool,
) -> Result<Vec<Hit>, StoreError> {
    let mut statement = connection.prepare(sql).map_err(error)?;
    let mut rows = statement.query([]).map_err(error)?;
    let mut scored = Vec::new();
    while let Some(row) = rows.next().map_err(error)? {
        let blob: Option<Vec<u8>> = row.get(2).map_err(error)?;
        let Some(blob) = blob.filter(|blob| !blob.is_empty()) else {
            continue;
        };
        let vector = unpack(&blob);
        if vector.len() != query.len() {
            // Embedded by a different embedder: a cross-dimension score means nothing.
            continue;
        }
        let raw: Option<String> = row.get(3).map_err(error)?;
        let mut metadata = raw
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Map<String, Value>>(raw).ok())
            .unwrap_or_default();
        if with_outcome {
            let outcome: String = row.get(4).map_err(error)?;
            metadata.insert("outcome".into(), Value::String(outcome));
        }
        scored.push(Hit {
            id: row.get(0).map_err(error)?,
            text: row.get(1).map_err(error)?,
            score: cosine(query, &vector),
            metadata,
        });
    }
    scored.sort_by(|a, b| b.score.total_cmp(&a.score));
    scored.truncate(k);
    Ok(scored)
}

#[derive(Clone, Copy)]
enum Table {
    Episodes,
    Sessions,
    Memories,
}

impl Table {
    /// The table, its timestamp column, and its id column.
    fn columns(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Episodes => ("episodes", "ts", "id"),
            Self::Sessions => ("sessions", "updated_at", "session_id"),
            Self::Memories => ("memories", "ts", "id"),
        }
    }
}

/// Delete rows failing any given constraint and count them, as Python's
/// `_prune_table`: a row survives only if it is newer than the age cutoff and
/// among the `keep_last` most recent.
fn prune_table(
    connection: &Connection,
    table: Table,
    older_than_days: Option<f64>,
    keep_last: Option<i64>,
) -> Result<i64, StoreError> {
    let (table, ts, id) = table.columns();
    let mut deleted = 0;
    if let Some(days) = older_than_days {
        let cutoff = now() - days * 86400.0;
        deleted += connection
            .execute(
                &format!("DELETE FROM {table} WHERE {ts} < ?1"),
                params![cutoff],
            )
            .map_err(error)?;
    }
    if let Some(keep) = keep_last {
        deleted += connection
            .execute(
                &format!(
                    "DELETE FROM {table} WHERE {id} NOT IN \
                     (SELECT {id} FROM {table} ORDER BY {ts} DESC LIMIT ?1)"
                ),
                params![keep.max(0)],
            )
            .map_err(error)?;
    }
    Ok(deleted as i64)
}
