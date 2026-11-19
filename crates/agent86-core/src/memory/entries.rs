//! Single facts and episodes, listed, pinned, and forgotten one at a time
//! (beyond Python, which can only search and prune them in bulk).
//!
//! Pins live in a table of agent86's own, `agent86_pins`, inside the memory
//! database: the `episodes` and `memories` rows, their `metadata_json`, and
//! `state_json` are never touched, so Python reads the database exactly as
//! before and simply does not see the pins (its own `prune` and retention still
//! delete pinned rows). Both tables are `AUTOINCREMENT`, so an id is never
//! reused and a pin left behind by a row Python deleted can never land on
//! another row; such pins are swept away whenever this side prunes.

use super::error;
use crate::session::{SessionStore, StoreError};
use rusqlite::{params, OptionalExtension, Row};
use serde_json::{Map, Value};
use std::time::{SystemTime, UNIX_EPOCH};

/// agent86's own table, created beside Python's schema when the store opens.
/// Python never reads it.
pub(crate) const PINS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS agent86_pins (
    kind TEXT NOT NULL,
    row_id INTEGER NOT NULL,
    pinned_at REAL NOT NULL,
    PRIMARY KEY (kind, row_id)
);
";

/// Which half of long-term memory an id belongs to; facts and episodes number
/// their rows separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A row of `memories`: what `remember` stored.
    Fact,
    /// A row of `episodes`: a finished turn, or an archived compaction.
    Episode,
}

impl EntryKind {
    /// `"fact"` or `"episode"`, as the pins table and ACP name it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Episode => "episode",
        }
    }

    /// The table holding this kind's rows.
    fn table(self) -> &'static str {
        match self {
            Self::Fact => "memories",
            Self::Episode => "episodes",
        }
    }

    /// `AND`-able SQL that keeps only this kind's unpinned rows of its table.
    pub(crate) fn unpinned(self) -> String {
        format!(
            "id NOT IN (SELECT row_id FROM agent86_pins WHERE kind = '{}')",
            self.as_str()
        )
    }
}

/// One fact or episode, as `memory list` and ACP show it.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub kind: EntryKind,
    pub id: i64,
    /// Seconds since the Unix epoch when it was stored.
    pub ts: f64,
    /// The fact's text, or the episode's task.
    pub text: String,
    /// The episode's outcome; `None` for a fact.
    pub outcome: Option<String>,
    /// The session the episode came from; `None` for a fact.
    pub session_id: Option<String>,
    /// The fact's `kind` column (`"fact"` for everything `remember` stores), or
    /// an episode's `metadata["kind"]` (`"compaction"` for an archive).
    pub category: Option<String>,
    /// When it was pinned, if it is.
    pub pinned_at: Option<f64>,
}

impl Entry {
    pub fn pinned(&self) -> bool {
        self.pinned_at.is_some()
    }
}

/// What to list.
#[derive(Debug, Clone, Copy)]
pub struct ListQuery {
    pub kind: EntryKind,
    /// Most entries to return; `None` for all.
    pub limit: Option<usize>,
    /// Entries to skip first, for paging.
    pub offset: usize,
    /// Only pinned entries.
    pub pinned_only: bool,
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

/// The SELECT every reader shares, so `entry` and `list_entries` agree.
fn select(kind: EntryKind) -> String {
    let columns = match kind {
        EntryKind::Fact => "t.id, t.ts, t.text, NULL, NULL, t.kind, t.metadata_json",
        EntryKind::Episode => "t.id, t.ts, t.task, t.outcome, t.session_id, NULL, t.metadata_json",
    };
    format!(
        "SELECT {columns}, p.pinned_at FROM {table} t \
         LEFT JOIN agent86_pins p ON p.kind = '{name}' AND p.row_id = t.id",
        table = kind.table(),
        name = kind.as_str(),
    )
}

fn read(kind: EntryKind, row: &Row<'_>) -> rusqlite::Result<Entry> {
    let category = match kind {
        EntryKind::Fact => row.get(5)?,
        EntryKind::Episode => row
            .get::<_, Option<String>>(6)?
            .and_then(|raw| serde_json::from_str::<Map<String, Value>>(&raw).ok())
            .and_then(|metadata| metadata.get("kind")?.as_str().map(str::to_owned)),
    };
    Ok(Entry {
        kind,
        id: row.get(0)?,
        ts: row.get::<_, Option<f64>>(1)?.unwrap_or(0.0),
        text: row.get(2)?,
        outcome: row.get(3)?,
        session_id: row.get(4)?,
        category,
        pinned_at: row.get(7)?,
    })
}

impl SessionStore {
    /// Facts or episodes, newest first.
    pub fn list_entries(&self, query: ListQuery) -> Result<Vec<Entry>, StoreError> {
        let filter = if query.pinned_only {
            " WHERE p.pinned_at IS NOT NULL"
        } else {
            ""
        };
        let sql = format!(
            "{}{filter} ORDER BY t.ts DESC, t.id DESC LIMIT ?1 OFFSET ?2",
            select(query.kind)
        );
        // SQLite reads a negative limit as none.
        let limit = query
            .limit
            .map_or(-1, |limit| i64::try_from(limit).unwrap_or(i64::MAX));
        let offset = i64::try_from(query.offset).unwrap_or(i64::MAX);
        let connection = self.connection();
        let mut statement = connection.prepare(&sql).map_err(error)?;
        let rows = statement
            .query_map(params![limit, offset], |row| read(query.kind, row))
            .map_err(error)?;
        rows.collect::<Result<_, _>>().map_err(error)
    }

    /// One fact or episode by id.
    pub fn entry(&self, kind: EntryKind, id: i64) -> Result<Option<Entry>, StoreError> {
        let sql = format!("{} WHERE t.id = ?1", select(kind));
        self.connection()
            .query_row(&sql, params![id], |row| read(kind, row))
            .optional()
            .map_err(error)
    }

    /// Pin or unpin a fact or episode: a pinned one is never removed by `prune`
    /// or retention. `Ok(None)` when there is no such row; otherwise the entry
    /// as it now stands. Pinning a pinned entry keeps its first `pinned_at`.
    pub fn set_pinned(
        &self,
        kind: EntryKind,
        id: i64,
        pinned: bool,
    ) -> Result<Option<Entry>, StoreError> {
        {
            let connection = self.connection();
            let exists = connection
                .query_row(
                    &format!("SELECT 1 FROM {} WHERE id = ?1", kind.table()),
                    params![id],
                    |_| Ok(()),
                )
                .optional()
                .map_err(error)?
                .is_some();
            if !exists {
                return Ok(None);
            }
            if pinned {
                connection
                    .execute(
                        "INSERT OR IGNORE INTO agent86_pins (kind, row_id, pinned_at) \
                         VALUES (?1, ?2, ?3)",
                        params![kind.as_str(), id, now()],
                    )
                    .map_err(error)?;
            } else {
                connection
                    .execute(
                        "DELETE FROM agent86_pins WHERE kind = ?1 AND row_id = ?2",
                        params![kind.as_str(), id],
                    )
                    .map_err(error)?;
            }
        }
        self.entry(kind, id)
    }

    /// Delete one fact or episode, pinned or not, with its pin; `true` if a row
    /// was removed.
    pub fn forget(&self, kind: EntryKind, id: i64) -> Result<bool, StoreError> {
        let connection = self.connection();
        let removed = connection
            .execute(
                &format!("DELETE FROM {} WHERE id = ?1", kind.table()),
                params![id],
            )
            .map_err(error)?;
        connection
            .execute(
                "DELETE FROM agent86_pins WHERE kind = ?1 AND row_id = ?2",
                params![kind.as_str(), id],
            )
            .map_err(error)?;
        Ok(removed > 0)
    }
}

/// Drop pins whose row is gone (Python's `prune` or an older agent86 removed
/// it), so the table holds only live pins.
pub(crate) fn sweep_pins(connection: &rusqlite::Connection) -> Result<(), StoreError> {
    connection
        .execute(
            "DELETE FROM agent86_pins WHERE \
             (kind = 'episode' AND row_id NOT IN (SELECT id FROM episodes)) OR \
             (kind = 'fact' AND row_id NOT IN (SELECT id FROM memories))",
            [],
        )
        .map(drop)
        .map_err(error)
}
