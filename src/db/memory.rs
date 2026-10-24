use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};

use super::{cosine_of_bytes, embedding_to_bytes, fts, Database};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: i64,
    pub key: String,
    pub value: String,
    pub memory_type: String,
    pub tags: Option<Vec<String>>,
    pub created_at: String,
    pub updated_at: String,
    /// active | stale | superseded
    pub status: String,
    /// manual | claude-auto | session
    pub source: String,
    /// row id of the memory that replaced this one. Only non-active rows carry
    /// a pointer: keys repeat across history, so ids are the only stable link.
    pub superseded_by: Option<i64>,
    pub reviewed_at: Option<String>,
}

/// A near-duplicate cluster flagged at distillation time instead of being
/// auto-superseded (multiple matches, or the match was manually written).
/// Rows are hints: a pair whose sides are no longer both active is pruned on
/// listing, so merging or staling either side resolves the conflict.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryConflict {
    pub id: i64,
    pub score: Option<f64>,
    pub created_at: String,
    pub memory: MemoryEntry,
    pub matched: MemoryEntry,
}

const MEMORY_COLS: &str =
    "id, key, value, memory_type, tags, created_at, updated_at, status, source, superseded_by, reviewed_at";

/// An active memory ranked by cosine similarity to a query embedding.
#[derive(Debug, Clone)]
pub struct SimilarMemory {
    pub entry: MemoryEntry,
    pub score: f32,
}

impl Database {
    pub fn memory_set(
        &self,
        key: &str,
        value: &str,
        memory_type: &str,
        tags: Option<&[String]>,
    ) -> Result<i64> {
        self.memory_set_full(key, value, memory_type, tags, "manual")
    }

    /// Insert or update a memory entry with explicit source tracking.
    ///
    /// The upsert targets the active-only partial unique index: an existing
    /// ACTIVE row with this key is updated in place, while retired (superseded
    /// or stale) rows never conflict - reusing their key creates a fresh row
    /// instead of resurrecting history. A value the update replaces is kept
    /// as a retired row first (see [`snapshot_active`]). Returns the active
    /// row's id.
    pub fn memory_set_full(
        &self,
        key: &str,
        value: &str,
        memory_type: &str,
        tags: Option<&[String]>,
        source: &str,
    ) -> Result<i64> {
        self.memory_upsert(key, value, memory_type, tags, source, false, None)
            .map(|id| id.expect("an unguarded upsert always writes"))
    }

    /// [`memory_set_full`](Self::memory_set_full) for background writers: an
    /// active hand-written row under `key` is left untouched and `None` is
    /// returned. The check is part of the upsert itself, so a `memory set`
    /// landing after the caller looked can't be overwritten either.
    pub fn memory_set_unless_manual(
        &self,
        key: &str,
        value: &str,
        memory_type: &str,
        tags: Option<&[String]>,
        source: &str,
    ) -> Result<Option<i64>> {
        self.memory_upsert(key, value, memory_type, tags, source, true, None)
    }

    /// [`memory_set_unless_manual`](Self::memory_set_unless_manual) that also
    /// leaves `key` alone when its active row has been marked distinct from
    /// row `original_id`. Used for a distilled rewrite's sibling key: a sibling
    /// a human called a separate fact must not be overwritten, even by a mark
    /// committed after the caller picked the key.
    pub fn memory_set_unless_manual_or_distinct(
        &self,
        key: &str,
        value: &str,
        memory_type: &str,
        source: &str,
        original_id: i64,
    ) -> Result<Option<i64>> {
        self.memory_upsert(
            key,
            value,
            memory_type,
            None,
            source,
            true,
            Some(original_id),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn memory_upsert(
        &self,
        key: &str,
        value: &str,
        memory_type: &str,
        tags: Option<&[String]>,
        source: &str,
        protect_manual: bool,
        distinct_from: Option<i64>,
    ) -> Result<Option<i64>> {
        let tags_json = tags.map(|t| serde_json::to_string(t).unwrap());
        let mut guards = Vec::new();
        if protect_manual {
            guards.push("memory.source != 'manual'");
        }
        if distinct_from.is_some() {
            guards.push(
                "NOT EXISTS (SELECT 1 FROM memory_distinct d
                             WHERE d.lo_id = MIN(memory.id, ?6) AND d.hi_id = MAX(memory.id, ?6))",
            );
        }
        let guard = guards.join(" AND ");
        let sql = format!(
            "INSERT INTO memory (key, value, memory_type, tags, source)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(key) WHERE status = 'active' DO UPDATE SET
               value      = excluded.value,
               -- A vector of the old value is wrong for the new one; clear it
               -- so the backfill re-embeds it.
               embedding  = CASE WHEN memory.value IS excluded.value
                                 THEN memory.embedding END,
               memory_type = excluded.memory_type,
               tags       = excluded.tags,
               updated_at = datetime('now')
             {}",
            if guard.is_empty() {
                String::new()
            } else {
                format!("WHERE {guard}")
            }
        );
        let write = |conn: &Connection| -> Result<Option<i64>> {
            // Same guard as the update, so a refused write keeps no copy.
            snapshot_active(
                conn,
                key,
                value,
                memory_type,
                Some(tags_json.as_deref()),
                &guard,
                distinct_from,
            )?;
            let written = match distinct_from {
                Some(original) => conn.execute(
                    &sql,
                    params![key, value, memory_type, tags_json, source, original],
                )?,
                None => conn.execute(&sql, params![key, value, memory_type, tags_json, source])?,
            };
            if written == 0 {
                return Ok(None);
            }
            // last_insert_rowid is stale on the DO UPDATE path; look the row up.
            Ok(Some(conn.query_row(
                "SELECT id FROM memory WHERE key = ?1 AND status = 'active'",
                params![key],
                |r| r.get(0),
            )?))
        };
        // The snapshot and the update land together or not at all.
        if !self.conn.is_autocommit() {
            return write(&self.conn);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let id = write(&tx)?;
        tx.commit()?;
        Ok(id)
    }

    /// Fetch by key: the active row if one exists, else the newest historical
    /// row bearing the key (so retired keys stay inspectable).
    pub fn memory_get(&self, key: &str) -> Result<Option<MemoryEntry>> {
        let sql = format!(
            "SELECT {MEMORY_COLS} FROM memory WHERE key = ?1
             ORDER BY (status = 'active') DESC, updated_at DESC, id DESC
             LIMIT 1"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        match stmt.query_row(params![key], row_to_memory) {
            Ok(e) => Ok(Some(e)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn memory_get_by_id(&self, id: i64) -> Result<Option<MemoryEntry>> {
        let sql = format!("SELECT {MEMORY_COLS} FROM memory WHERE id = ?1");
        let mut stmt = self.conn.prepare(&sql)?;
        match stmt.query_row(params![id], row_to_memory) {
            Ok(e) => Ok(Some(e)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Full history for a key: every row that ever bore it (any status),
    /// including values replaced in place, plus the forward supersession
    /// chain those rows point into. The active row comes first, then the rest
    /// newest first: a replaced value is dated when it was written, which can
    /// be after the active row's own `created_at`.
    pub fn memory_history(&self, key: &str) -> Result<Vec<MemoryEntry>> {
        let sql = format!(
            "SELECT {MEMORY_COLS} FROM memory WHERE key = ?1
             ORDER BY (status = 'active') DESC, created_at DESC, id DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut entries: Vec<MemoryEntry> = stmt
            .query_map(params![key], row_to_memory)?
            .collect::<Result<Vec<_>, _>>()?;

        let mut seen: std::collections::HashSet<i64> = entries.iter().map(|e| e.id).collect();
        let mut queue: Vec<i64> = entries.iter().filter_map(|e| e.superseded_by).collect();
        while let Some(id) = queue.pop() {
            if !seen.insert(id) {
                continue;
            }
            if let Some(e) = self.memory_get_by_id(id)? {
                if let Some(next) = e.superseded_by {
                    queue.push(next);
                }
                entries.push(e);
            }
        }
        entries.sort_by(|a, b| {
            (b.status == "active", &b.created_at, b.id).cmp(&(
                a.status == "active",
                &a.created_at,
                a.id,
            ))
        });
        Ok(entries)
    }

    /// List memories. Defaults to active only; pass status="all" to include stale/superseded.
    /// List memories. Defaults to active only; pass status="all" to include all.
    pub fn memory_list(
        &self,
        memory_type: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<MemoryEntry>> {
        // All four combinations: (type filter, status filter) each parameterized.
        let filter_status = !matches!(status, Some("all") | None);
        let active_only = status.is_none();

        let sql = format!(
            "SELECT {MEMORY_COLS} FROM memory WHERE {} ORDER BY updated_at DESC",
            match (memory_type.is_some(), filter_status, active_only) {
                (true, true, _) => "memory_type = ?1 AND status = ?2",
                (true, false, false) => "memory_type = ?1",
                (true, false, true) => "memory_type = ?1 AND status = 'active'",
                (false, true, _) => "status = ?1",
                (false, false, false) => "1=1",
                (false, false, true) => "status = 'active'",
            }
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = match (memory_type, filter_status, active_only) {
            (Some(t), true, _) => stmt.query_map(params![t, status.unwrap()], row_to_memory)?,
            (Some(t), false, _) => stmt.query_map(params![t], row_to_memory)?,
            (None, true, _) => stmt.query_map(params![status.unwrap()], row_to_memory)?,
            (None, false, _) => stmt.query_map([], row_to_memory)?,
        };
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Search active memories by default. Pass status="all" to search everything.
    pub fn memory_search(&self, query: &str) -> Result<Vec<MemoryEntry>> {
        self.memory_search_filtered(query, "active")
    }

    pub fn memory_search_filtered(&self, query: &str, status: &str) -> Result<Vec<MemoryEntry>> {
        let fts_query = fts::sanitize(query);
        let like_pat = fts::like_contains(query);
        let filter_status = status != "all";

        // Tier 1: FTS prefix matches, most relevant (bm25) first. Recency only
        // breaks ties: ordering by it alone put any later passing mention ahead
        // of the memory that is actually about the term.
        let fts_sql = if filter_status {
            format!(
                "WITH hits AS (SELECT rowid AS hit_id, rank FROM memory_fts WHERE memory_fts MATCH ?1)
                 SELECT {MEMORY_COLS} FROM memory JOIN hits ON hits.hit_id = memory.id
                     WHERE status = ?2
                     ORDER BY hits.rank, updated_at DESC"
            )
        } else {
            format!(
                "WITH hits AS (SELECT rowid AS hit_id, rank FROM memory_fts WHERE memory_fts MATCH ?1)
                 SELECT {MEMORY_COLS} FROM memory JOIN hits ON hits.hit_id = memory.id
                     ORDER BY hits.rank, updated_at DESC"
            )
        };
        let mut stmt = self.conn.prepare(&fts_sql)?;
        let fts_hits: Vec<MemoryEntry> = if filter_status {
            stmt.query_map(params![fts_query, status], row_to_memory)?
        } else {
            stmt.query_map(params![fts_query], row_to_memory)?
        }
        .collect::<Result<Vec<_>, _>>()?;
        let fts_ids: std::collections::HashSet<i64> = fts_hits.iter().map(|m| m.id).collect();

        // Tier 2: LIKE substring fallback
        let like_sql = if filter_status {
            format!(
                "SELECT {MEMORY_COLS} FROM memory
                     WHERE (key LIKE ?1 ESCAPE '\\' OR value LIKE ?1 ESCAPE '\\')
                       AND status = ?2
                     ORDER BY updated_at DESC"
            )
        } else {
            format!(
                "SELECT {MEMORY_COLS} FROM memory
                     WHERE (key LIKE ?1 ESCAPE '\\' OR value LIKE ?1 ESCAPE '\\')
                     ORDER BY updated_at DESC"
            )
        };
        let mut stmt2 = self.conn.prepare(&like_sql)?;
        let like_extras: Vec<MemoryEntry> = if filter_status {
            stmt2.query_map(params![like_pat, status], row_to_memory)?
        } else {
            stmt2.query_map(params![like_pat], row_to_memory)?
        }
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|m| !fts_ids.contains(&m.id))
        .collect();

        let mut results = fts_hits;
        results.extend(like_extras);
        Ok(results)
    }

    /// Active memories whose values are near-duplicates of `value`, best first.
    ///
    /// Gathers candidates via an FTS OR-query over the value's most distinctive
    /// tokens, then scores each by token overlap and keeps those at or above
    /// `threshold` (0.0-1.0). Used at distillation time: a single match can be
    /// superseded in place, while several matches mean the cluster is too
    /// ambiguous to merge automatically and must be flagged for review.
    pub fn memory_find_near_duplicates(
        &self,
        value: &str,
        threshold: f64,
    ) -> Result<Vec<SimilarMemory>> {
        let tokens = significant_tokens(value);
        if tokens.is_empty() {
            return Ok(Vec::new());
        }

        // Build an FTS OR-query from the most distinctive (longest) tokens.
        // sanitize() joins with implicit AND, which is too strict here, so we
        // construct the OR-query directly with quoted prefix terms.
        let mut ranked: Vec<&String> = tokens.iter().collect();
        ranked.sort_by_key(|t| std::cmp::Reverse(t.len()));
        let fts_query = ranked
            .iter()
            .take(8)
            .map(|t| format!("\"{}\"*", t.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" OR ");

        let sql = format!(
            "SELECT {MEMORY_COLS} FROM memory
                 WHERE id IN (SELECT rowid FROM memory_fts WHERE memory_fts MATCH ?1)
                   AND status = 'active'
                 ORDER BY updated_at DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let candidates: Vec<MemoryEntry> = stmt
            .query_map(params![fts_query], row_to_memory)?
            .collect::<Result<Vec<_>, _>>()?;

        let mut hits: Vec<SimilarMemory> = candidates
            .into_iter()
            .filter_map(|c| {
                let score = token_overlap(value, &c.value);
                (score >= threshold).then_some(SimilarMemory {
                    entry: c,
                    score: score as f32,
                })
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(hits)
    }

    /// Store (or replace) the embedding vector for a memory's ACTIVE row.
    /// Returns false if the key has no active row.
    /// Returns false, storing nothing, if the database holds another model's
    /// vectors.
    pub fn memory_set_embedding(&self, key: &str, embedding: &[f32]) -> Result<bool> {
        self.write_with_vectors(true, |keep| {
            if !keep {
                return Ok(false);
            }
            let n = self.conn.execute(
                "UPDATE memory SET embedding = ?1 WHERE key = ?2 AND status = 'active'",
                params![embedding_to_bytes(embedding), key],
            )?;
            Ok(n > 0)
        })
    }

    /// Active memories with no embedding yet - the backfill work list.
    pub fn memory_needing_embedding(&self) -> Result<Vec<MemoryEntry>> {
        let sql = format!(
            "SELECT {MEMORY_COLS} FROM memory
             WHERE embedding IS NULL AND status = 'active'
             ORDER BY updated_at DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_memory)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Active memories ranked by cosine similarity to `query_embedding`.
    /// Only rows with a stored embedding participate.
    /// Active memories nearest to `query_embedding`, via the sqlite-vec KNN index
    /// (`memory_vec`, cosine). The index is maintained active-only by triggers,
    /// so this is a plain KNN join. `score` = 1 - cosine distance.
    pub fn memory_similar(
        &self,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<SimilarMemory>> {
        self.semantic_read(|| {
            let query = embedding_to_bytes(query_embedding);
            let sql = format!(
                "SELECT {MEMORY_COLS}, v.distance
             FROM memory_vec v
             JOIN memory m ON m.id = v.rowid
             WHERE v.embedding MATCH ?1 AND k = ?2
             ORDER BY v.distance"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params![query, limit as i64], |row| {
                let entry = row_to_memory(row)?;
                let distance: f64 = row.get(11)?; // after the 11 MEMORY_COLS (0-10)
                Ok(SimilarMemory {
                    entry,
                    score: 1.0 - distance as f32,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
    }

    /// Semantic counterpart to `memory_find_near_duplicates`: the up-to-`limit`
    /// most-similar active memories whose cosine score is at or above
    /// `threshold`, best first.
    pub fn memory_find_near_duplicates_semantic(
        &self,
        query_embedding: &[f32],
        threshold: f32,
        limit: usize,
    ) -> Result<Vec<SimilarMemory>> {
        Ok(self
            .memory_similar(query_embedding, limit)?
            .into_iter()
            .filter(|sm| sm.score >= threshold)
            .collect())
    }

    pub fn memory_delete(&self, key: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM memory WHERE key = ?1", params![key])?;
        Ok(n > 0)
    }

    /// Mark a memory as stale (no longer reliable). Appends an optional reason to the value.
    /// Stale memories are excluded from default list/search but preserved for history.
    pub fn memory_stale(&self, key: &str, reason: Option<&str>) -> Result<bool> {
        let note = reason
            .map(|r| format!("\n[stale: {r}]"))
            .unwrap_or_default();
        let n = self.conn.execute(
            "UPDATE memory
             SET status = 'stale',
                 value = value || ?1,
                 updated_at = datetime('now')
             WHERE key = ?2 AND status = 'active'",
            params![note, key],
        )?;
        Ok(n > 0)
    }

    /// Replace an old memory with a new one in a single atomic operation.
    ///
    /// Requires an ACTIVE row under `old_key` (returns false otherwise); that
    /// row is marked superseded with its pointer set to the new row's id. The
    /// new entry upserts against the active-only key index, so repeated
    /// supersedes can converge on one canonical key without ever resurrecting
    /// a retired row. Errors when old and new keys are the same - that is an
    /// in-place update, not a supersession.
    pub fn memory_supersede(
        &self,
        old_key: &str,
        new_key: &str,
        new_value: &str,
        memory_type: &str,
        source: &str,
    ) -> Result<bool> {
        self.memory_supersede_impl(old_key, new_key, new_value, memory_type, source, false)
    }

    /// [`memory_supersede`](Self::memory_supersede) for background writers:
    /// also returns false, changing nothing, when `new_key` is an active
    /// hand-written row. See [`memory_set_unless_manual`](Self::memory_set_unless_manual).
    pub fn memory_supersede_unless_manual(
        &self,
        old_key: &str,
        new_key: &str,
        new_value: &str,
        memory_type: &str,
        source: &str,
    ) -> Result<bool> {
        self.memory_supersede_impl(old_key, new_key, new_value, memory_type, source, true)
    }

    fn memory_supersede_impl(
        &self,
        old_key: &str,
        new_key: &str,
        new_value: &str,
        memory_type: &str,
        source: &str,
        protect_manual: bool,
    ) -> Result<bool> {
        if old_key == new_key {
            anyhow::bail!(
                "old and new keys are both '{old_key}'; use `sclerox memory set` to update in place"
            );
        }

        let tx = self.conn.unchecked_transaction()?;
        let old_id: Option<i64> = tx
            .query_row(
                "SELECT id FROM memory WHERE key = ?1 AND status = 'active'",
                params![old_key],
                |r| r.get(0),
            )
            .optional()?;
        let Some(old_id) = old_id else {
            return Ok(false);
        };

        // Superseding into an existing active key replaces its value; keep it.
        // A refused write returns before commit, which rolls the copy back too.
        snapshot_active(&tx, new_key, new_value, memory_type, None, "", None)?;
        let written = tx.execute(
            &format!(
                "INSERT INTO memory (key, value, memory_type, source)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(key) WHERE status = 'active' DO UPDATE SET
                   value = excluded.value,
                   embedding = CASE WHEN memory.value IS excluded.value
                                    THEN memory.embedding END,
                   memory_type = excluded.memory_type,
                   updated_at = datetime('now')
                 {}",
                if protect_manual {
                    "WHERE memory.source != 'manual'"
                } else {
                    ""
                }
            ),
            params![new_key, new_value, memory_type, source],
        )?;
        if written == 0 {
            // Dropping the transaction rolls back; the old row stays active.
            return Ok(false);
        }
        let new_id: i64 = tx.query_row(
            "SELECT id FROM memory WHERE key = ?1 AND status = 'active'",
            params![new_key],
            |r| r.get(0),
        )?;
        tx.execute(
            "UPDATE memory
             SET status = 'superseded',
                 superseded_by = ?1,
                 updated_at = datetime('now')
             WHERE id = ?2",
            params![new_id, old_id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Record that `memory_id` was distilled as a near-duplicate of
    /// `matched_id` but was too ambiguous to auto-supersede. First sighting of
    /// a pair wins; re-flagging is a no-op.
    pub fn memory_conflict_add(
        &self,
        memory_id: i64,
        matched_id: i64,
        score: Option<f64>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO memory_conflicts (memory_id, matched_id, score)
             VALUES (?1, ?2, ?3)",
            params![memory_id, matched_id, score],
        )?;
        Ok(())
    }

    /// Unresolved near-duplicate conflicts, oldest first. Pairs whose sides
    /// are no longer both active are pruned here rather than tracked: merging
    /// (supersede) or staling either side resolves a conflict implicitly.
    /// Record that two memories are genuinely different facts, so the pair is
    /// never flagged as a conflict and distillation never merges one into the
    /// other.
    ///
    /// Without this a conflict could only clear by one side ceasing to be
    /// active, so "these are distinct, keep both" was inexpressible: the pair
    /// stayed flagged and the only way to silence it was to merge, which is the
    /// opposite of the judgement being made.
    ///
    /// Symmetric: the pair is normalized so argument order never matters.
    /// Marking an already-marked pair refreshes the reason rather than failing.
    pub fn memory_distinct_add(&self, a_id: i64, b_id: i64, reason: Option<&str>) -> Result<bool> {
        if a_id == b_id {
            return Ok(false);
        }
        let (lo, hi) = if a_id < b_id {
            (a_id, b_id)
        } else {
            (b_id, a_id)
        };
        self.conn.execute(
            "INSERT INTO memory_distinct (lo_id, hi_id, reason) VALUES (?1, ?2, ?3)
             ON CONFLICT(lo_id, hi_id) DO UPDATE SET reason = excluded.reason",
            params![lo, hi, reason],
        )?;
        // The pair may already be flagged; drop that row now it is resolved.
        self.conn.execute(
            "DELETE FROM memory_conflicts
             WHERE (memory_id = ?1 AND matched_id = ?2)
                OR (memory_id = ?2 AND matched_id = ?1)",
            params![lo, hi],
        )?;
        Ok(true)
    }

    /// Undo a distinct marking, so the pair can be flagged and merged again.
    pub fn memory_distinct_remove(&self, a_id: i64, b_id: i64) -> Result<bool> {
        let (lo, hi) = if a_id < b_id {
            (a_id, b_id)
        } else {
            (b_id, a_id)
        };
        let n = self.conn.execute(
            "DELETE FROM memory_distinct WHERE lo_id = ?1 AND hi_id = ?2",
            params![lo, hi],
        )?;
        Ok(n > 0)
    }

    /// True if this pair has been marked as genuinely different.
    pub fn memory_is_distinct(&self, a_id: i64, b_id: i64) -> Result<bool> {
        let (lo, hi) = if a_id < b_id {
            (a_id, b_id)
        } else {
            (b_id, a_id)
        };
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM memory_distinct WHERE lo_id = ?1 AND hi_id = ?2",
                params![lo, hi],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Every distinct-marked pair, as (key_a, key_b, reason), for `sclerox
    /// memory distinct list`. Rows whose memories were deleted are skipped.
    pub fn memory_distinct_list(&self) -> Result<Vec<(String, String, Option<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT a.key, b.key, d.reason
             FROM memory_distinct d
             JOIN memory a ON a.id = d.lo_id
             JOIN memory b ON b.id = d.hi_id
             ORDER BY d.created_at, d.id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Re-score every scored conflict against the vectors its two memories
    /// hold now, then drop the ones below `floor`.
    ///
    /// The stored score is a snapshot from the moment the pair was flagged.
    /// Editing either memory moves the real score away from it in both
    /// directions, so deleting on the snapshot would discard pairs that have
    /// since become duplicates. Rows with no score, or whose memories are
    /// missing a vector or carry one of another width, are left alone: there
    /// is nothing to compare, and silence is not evidence that they agree.
    /// An edit clears the memory's vector until it is re-embedded, so a pair
    /// is never judged on the value it used to hold. Nothing is re-scored
    /// while the vectors come from a model other than the configured one.
    fn rescore_conflicts(&self, floor: f64) -> Result<()> {
        // The floor is calibrated for the configured model; vectors from
        // another one would be judged on the wrong scale.
        if !self.semantic_ready() {
            return Ok(());
        }
        // Take the write lock before reading, so a commit from another
        // connection can't invalidate the snapshot the writes depend on.
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let rows: Vec<(i64, f64, Vec<u8>, Vec<u8>)> = tx
            .prepare(
                "SELECT c.id, c.score, a.embedding, b.embedding
                   FROM memory_conflicts c
                   JOIN memory a ON a.id = c.memory_id
                   JOIN memory b ON b.id = c.matched_id
                  WHERE c.score IS NOT NULL
                    AND a.embedding IS NOT NULL
                    AND b.embedding IS NOT NULL",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<Result<Vec<_>, _>>()?;

        for (id, stored, a, b) in rows {
            let Some(score) = cosine_of_bytes(&a, &b).map(f64::from) else {
                continue;
            };
            if score < floor {
                tx.execute("DELETE FROM memory_conflicts WHERE id = ?1", params![id])?;
            } else if (score - stored).abs() > 1e-6 {
                tx.execute(
                    "UPDATE memory_conflicts SET score = ?1 WHERE id = ?2",
                    params![score, id],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Flagged near-duplicate pairs, after dropping the ones that are no
    /// longer conflicts.
    ///
    /// `floor` is the caller's current `dedup.cosine_threshold`. Pairs are
    /// re-scored against the vectors the two memories hold now and dropped
    /// below it, so lowering the threshold stops flagging new noise *and*
    /// clears the noise already queued. Pass `None` to read the queue without
    /// re-scoring it.
    pub fn memory_conflicts(&self, floor: Option<f64>) -> Result<Vec<MemoryConflict>> {
        self.conn.execute(
            "DELETE FROM memory_conflicts WHERE id IN (
                 SELECT c.id FROM memory_conflicts c
                 LEFT JOIN memory a ON a.id = c.memory_id
                 LEFT JOIN memory b ON b.id = c.matched_id
                 WHERE COALESCE(a.status, '') != 'active'
                    OR COALESCE(b.status, '') != 'active'
                    OR EXISTS (
                        SELECT 1 FROM memory_distinct d
                        WHERE d.lo_id = MIN(c.memory_id, c.matched_id)
                          AND d.hi_id = MAX(c.memory_id, c.matched_id)))",
            [],
        )?;
        if let Some(floor) = floor {
            self.rescore_conflicts(floor)?;
        }
        let mut stmt = self.conn.prepare(
            "SELECT id, memory_id, matched_id, score, created_at
             FROM memory_conflicts ORDER BY created_at, id",
        )?;
        let rows: Vec<(i64, i64, i64, Option<f64>, String)> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut conflicts = Vec::with_capacity(rows.len());
        for (id, memory_id, matched_id, score, created_at) in rows {
            let (Some(memory), Some(matched)) = (
                self.memory_get_by_id(memory_id)?,
                self.memory_get_by_id(matched_id)?,
            ) else {
                continue;
            };
            conflicts.push(MemoryConflict {
                id,
                score,
                created_at,
                memory,
                matched,
            });
        }
        Ok(conflicts)
    }

    /// Mark a memory's ACTIVE row as reviewed (you've confirmed it's still accurate).
    pub fn memory_review(&self, key: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE memory SET reviewed_at = datetime('now')
             WHERE key = ?1 AND status = 'active'",
            params![key],
        )?;
        Ok(n > 0)
    }

    /// List active memories not reviewed in the last `days` days (or never reviewed).
    pub fn memory_review_needed(&self, days: u32) -> Result<Vec<MemoryEntry>> {
        let sql = format!(
            "SELECT {MEMORY_COLS} FROM memory
             WHERE status = 'active'
               AND (reviewed_at IS NULL
                    OR reviewed_at < datetime('now', '-{days} days'))
             ORDER BY COALESCE(reviewed_at, created_at) ASC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_memory)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn memory_link_person(&self, memory_key: &str, person_id: i64) -> Result<bool> {
        let memory_id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM memory WHERE key = ?1 AND status = 'active'",
                params![memory_key],
                |r| r.get(0),
            )
            .ok();
        let Some(memory_id) = memory_id else {
            return Ok(false);
        };
        self.conn.execute(
            "INSERT OR IGNORE INTO memory_people (memory_id, person_id) VALUES (?1, ?2)",
            params![memory_id, person_id],
        )?;
        Ok(true)
    }

    pub fn memory_unlink_person(&self, memory_key: &str, person_id: i64) -> Result<bool> {
        let memory_id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM memory WHERE key = ?1 AND status = 'active'",
                params![memory_key],
                |r| r.get(0),
            )
            .ok();
        let Some(memory_id) = memory_id else {
            return Ok(false);
        };
        let n = self.conn.execute(
            "DELETE FROM memory_people WHERE memory_id = ?1 AND person_id = ?2",
            params![memory_id, person_id],
        )?;
        Ok(n > 0)
    }

    pub fn memory_people(&self, memory_key: &str) -> Result<Vec<crate::db::people::Person>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.name, p.notes, p.created_at, p.updated_at
             FROM people p
             JOIN memory_people mp ON p.id = mp.person_id
             JOIN memory m ON mp.memory_id = m.id
             WHERE m.key = ?1 AND m.status = 'active'
             ORDER BY p.name",
        )?;
        let rows = stmt.query_map(params![memory_key], |row| {
            Ok(crate::db::people::Person {
                id: row.get(0)?,
                name: row.get(1)?,
                notes: row.get(2)?,
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

/// Significant lowercase tokens of a string: alphanumeric words of 3+ chars,
/// minus common stopwords. Used for near-duplicate scoring.
fn significant_tokens(s: &str) -> std::collections::HashSet<String> {
    const STOPWORDS: &[&str] = &[
        "the", "and", "for", "are", "was", "were", "with", "that", "this", "from", "have", "has",
        "had", "not", "but", "its", "into", "when", "then", "than", "them", "they", "use", "used",
        "uses", "via", "per", "can", "will", "should", "must", "which", "each", "any", "all",
        "one", "now", "how", "why", "what", "who", "our", "out", "off", "get", "set",
    ];
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3 && !STOPWORDS.contains(w))
        .map(|w| w.to_string())
        .collect()
}

/// Fraction of `proposed`'s significant tokens that also appear in `existing`
/// (0.0-1.0). Asymmetric on purpose: a short new fact fully contained in a
/// longer existing one scores 1.0 and is treated as a duplicate.
pub fn token_overlap(proposed: &str, existing: &str) -> f64 {
    let a = significant_tokens(proposed);
    if a.is_empty() {
        return 0.0;
    }
    let b = significant_tokens(existing);
    let shared = a.iter().filter(|t| b.contains(*t)).count();
    shared as f64 / a.len() as f64
}

fn row_to_memory(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryEntry> {
    let tags_json: Option<String> = row.get(4)?;
    let tags = tags_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());
    Ok(MemoryEntry {
        id: row.get(0)?,
        key: row.get(1)?,
        value: row.get(2)?,
        memory_type: row.get(3)?,
        tags,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
        status: row.get(7).unwrap_or_else(|_| "active".to_string()),
        source: row.get(8).unwrap_or_else(|_| "manual".to_string()),
        superseded_by: row.get(9).unwrap_or(None),
        reviewed_at: row.get(10).unwrap_or(None),
    })
}

/// Before `key`'s active row is updated in place, copy its current version
/// into a retired (`superseded`) row pointing at it, so `memory get --history`
/// still shows what it said. The copy is dated when that version was written
/// (the active row's `updated_at`) and retired now.
///
/// The active row keeps its id rather than being replaced by a new one:
/// people links, conflicts, distinct marks and its vector all hang off that
/// id. No-op when there is no active row or the update changes nothing.
/// `tags` is `None` for writes that leave tags alone.
fn snapshot_active(
    conn: &Connection,
    key: &str,
    value: &str,
    memory_type: &str,
    tags: Option<Option<&str>>,
    guard: &str,
    distinct_from: Option<i64>,
) -> Result<()> {
    // `guard` is the upsert's `DO UPDATE ... WHERE` condition (over `memory.*`,
    // with the distinct-from id as ?6): copy only a row the update will change.
    let sql = format!(
        "INSERT INTO memory (key, value, memory_type, tags, created_at, updated_at,
                             status, source, superseded_by, reviewed_at)
         SELECT key, value, memory_type, tags, updated_at, datetime('now'),
                'superseded', source, id, reviewed_at
         FROM memory
         WHERE key = ?1 AND status = 'active'
           AND (value IS NOT ?2 OR memory_type IS NOT ?3 OR (?4 AND tags IS NOT ?5))
           {}",
        if guard.is_empty() {
            String::new()
        } else {
            format!("AND ({guard})")
        }
    );
    let (is_set, tags) = (tags.is_some(), tags.flatten());
    match distinct_from {
        Some(original) => conn.execute(
            &sql,
            params![key, value, memory_type, is_set, tags, original],
        )?,
        None => conn.execute(&sql, params![key, value, memory_type, is_set, tags])?,
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_set_get() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("test_key", "test value", "general", None)
            .unwrap();
        let entry = db.memory_get("test_key").unwrap().unwrap();
        assert_eq!(entry.key, "test_key");
        assert_eq!(entry.value, "test value");
        assert_eq!(entry.memory_type, "general");
    }

    #[test]
    fn test_memory_upsert() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("k", "v1", "user", None).unwrap();
        db.memory_set("k", "v2", "feedback", None).unwrap();
        let entry = db.memory_get("k").unwrap().unwrap();
        assert_eq!(entry.value, "v2");
        assert_eq!(entry.memory_type, "feedback");
    }

    #[test]
    fn test_memory_list_by_type() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("a", "val", "user", None).unwrap();
        db.memory_set("b", "val", "feedback", None).unwrap();
        db.memory_set("c", "val", "user", None).unwrap();

        let user = db.memory_list(Some("user"), None).unwrap();
        assert_eq!(user.len(), 2);

        let all = db.memory_list(None, None).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn test_memory_search_treats_like_wildcards_literally() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("a", "retry budget is 3", "project", None)
            .unwrap();
        db.memory_set("b", "cpu at 95% for an hour", "project", None)
            .unwrap();
        db.memory_set("c", "use snake_case names", "project", None)
            .unwrap();

        let keys = |q: &str| -> Vec<String> {
            db.memory_search(q)
                .unwrap()
                .into_iter()
                .map(|m| m.key)
                .collect()
        };
        assert_eq!(keys("_"), ["c"]);
        assert_eq!(keys("%"), ["b"]);
    }

    #[test]
    fn test_memory_search_ranks_by_relevance_not_recency() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set(
            "deploy-rollback",
            "Rollback procedure: rollback via deploy tool; rollback window 5 min.",
            "project",
            None,
        )
        .unwrap();
        // Written later, so a recency sort would list them first.
        for n in 0..3 {
            db.memory_set(
                &format!("misc-{n}"),
                &format!("Unrelated note {n} that mentions rollback in passing."),
                "project",
                None,
            )
            .unwrap();
            db.conn
                .execute(
                    "UPDATE memory SET updated_at = datetime('now', '+1 hour') WHERE key = ?1",
                    params![format!("misc-{n}")],
                )
                .unwrap();
        }

        let results = db.memory_search("rollback").unwrap();
        assert_eq!(results.len(), 4);
        assert_eq!(results[0].key, "deploy-rollback");
    }

    #[test]
    fn test_unless_manual_writes_leave_hand_written_rows_alone() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("hand", "written by a human", "project", None)
            .unwrap();
        db.memory_set_full("auto", "v1", "project", None, "session")
            .unwrap();

        assert!(db
            .memory_set_unless_manual("hand", "distilled", "project", None, "session")
            .unwrap()
            .is_none());
        assert!(!db
            .memory_supersede_unless_manual("auto", "hand", "distilled", "project", "session")
            .unwrap());
        assert_eq!(
            db.memory_get("hand").unwrap().unwrap().value,
            "written by a human"
        );
        // The rolled-back supersede left the old side active too.
        assert_eq!(db.memory_get("auto").unwrap().unwrap().status, "active");

        // Non-manual and new keys are written as usual.
        assert!(db
            .memory_set_unless_manual("auto", "v2", "project", None, "session")
            .unwrap()
            .is_some());
        assert_eq!(db.memory_get("auto").unwrap().unwrap().value, "v2");
        assert!(db
            .memory_set_unless_manual("fresh", "v", "project", None, "session")
            .unwrap()
            .is_some());
    }

    #[test]
    fn test_sibling_write_leaves_a_distinct_marked_row_alone() {
        let db = Database::open_in_memory().unwrap();
        let original = db.memory_set("k", "original", "project", None).unwrap();
        let sib = db
            .memory_set_full("k-distilled", "curated", "project", None, "session")
            .unwrap();

        // Not marked: the sibling is updated as usual.
        assert_eq!(
            db.memory_set_unless_manual_or_distinct(
                "k-distilled",
                "v2",
                "project",
                "session",
                original
            )
            .unwrap(),
            Some(sib)
        );
        // Marked distinct (in either order): left alone.
        db.memory_distinct_add(original, sib, None).unwrap();
        assert!(db
            .memory_set_unless_manual_or_distinct(
                "k-distilled",
                "v3",
                "project",
                "session",
                original
            )
            .unwrap()
            .is_none());
        assert_eq!(db.memory_get("k-distilled").unwrap().unwrap().value, "v2");
        // A new key has no row to protect.
        assert!(db
            .memory_set_unless_manual_or_distinct(
                "k-distilled-2",
                "v",
                "project",
                "session",
                original
            )
            .unwrap()
            .is_some());
    }

    #[test]
    fn test_memory_set_keeps_replaced_value_in_history() {
        let db = Database::open_in_memory().unwrap();
        let id = db
            .memory_set("db-choice", "Use Postgres. Why: JSONB.", "project", None)
            .unwrap();
        // Rewriting the same value is not a new version.
        db.memory_set("db-choice", "Use Postgres. Why: JSONB.", "project", None)
            .unwrap();
        assert_eq!(db.memory_history("db-choice").unwrap().len(), 1);

        let id2 = db
            .memory_set("db-choice", "Use SQLite.", "project", None)
            .unwrap();
        // Updated in place: links keyed by id (people, conflicts, distinct
        // marks, the vector) stay attached.
        assert_eq!(id2, id);

        let hist = db.memory_history("db-choice").unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!((hist[0].id, hist[0].status.as_str()), (id, "active"));
        assert_eq!(hist[0].value, "Use SQLite.");
        assert_eq!(hist[1].status, "superseded");
        assert_eq!(hist[1].value, "Use Postgres. Why: JSONB.");
        assert_eq!(hist[1].superseded_by, Some(id));

        // Only the current value is searchable by default.
        assert!(db.memory_search("Postgres").unwrap().is_empty());
        assert_eq!(
            db.memory_search_filtered("Postgres", "all").unwrap().len(),
            1
        );
        assert_eq!(
            db.memory_get("db-choice").unwrap().unwrap().value,
            "Use SQLite."
        );
    }

    #[test]
    fn test_memory_set_type_or_tag_change_is_a_new_version() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("k", "v", "project", None).unwrap();
        db.memory_set("k", "v", "feedback", None).unwrap();
        db.memory_set("k", "v", "feedback", Some(&["x".to_string()]))
            .unwrap();
        db.memory_set("k", "v", "feedback", Some(&["x".to_string()]))
            .unwrap();
        assert_eq!(db.memory_history("k").unwrap().len(), 3);
    }

    #[test]
    fn test_supersede_into_existing_key_keeps_its_value() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("canonical", "first wording", "project", None)
            .unwrap();
        db.memory_set("dup", "second wording", "project", None)
            .unwrap();
        db.memory_supersede("dup", "canonical", "merged wording", "project", "manual")
            .unwrap();

        let values: Vec<String> = db
            .memory_history("canonical")
            .unwrap()
            .into_iter()
            .map(|e| e.value)
            .collect();
        assert_eq!(values[0], "merged wording");
        assert!(values.contains(&"first wording".to_string()), "{values:?}");
    }

    #[test]
    fn test_refused_guarded_write_keeps_no_history_copy() {
        let db = Database::open_in_memory().unwrap();
        let hand = db
            .memory_set("hand", "written by a human", "project", None)
            .unwrap();
        let sib = db
            .memory_set_full("hand-distilled", "curated", "project", None, "session")
            .unwrap();
        db.memory_distinct_add(hand, sib, None).unwrap();

        // Refused: hand-written, then marked distinct from the original.
        assert!(db
            .memory_set_unless_manual("hand", "distilled", "project", None, "session")
            .unwrap()
            .is_none());
        assert!(db
            .memory_set_unless_manual_or_distinct(
                "hand-distilled",
                "v2",
                "project",
                "session",
                hand
            )
            .unwrap()
            .is_none());
        assert!(!db
            .memory_supersede_unless_manual("hand-distilled", "hand", "v3", "project", "session")
            .unwrap());
        assert_eq!(db.memory_history("hand").unwrap().len(), 1);
        assert_eq!(db.memory_history("hand-distilled").unwrap().len(), 1);

        // An accepted guarded write keeps the copy as usual.
        db.memory_set_full("auto", "v1", "project", None, "session")
            .unwrap();
        db.memory_set_unless_manual("auto", "v2", "project", None, "session")
            .unwrap();
        assert_eq!(db.memory_history("auto").unwrap().len(), 2);
    }

    #[test]
    fn test_memory_search() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set(
            "rust-pref",
            "prefer Rust for systems code",
            "feedback",
            None,
        )
        .unwrap();
        db.memory_set("py-pref", "use Python for scripts", "feedback", None)
            .unwrap();

        let results = db.memory_search("Rust").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "rust-pref");
    }

    #[test]
    fn test_memory_delete() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("del_me", "x", "general", None).unwrap();
        assert!(db.memory_delete("del_me").unwrap());
        assert!(!db.memory_delete("del_me").unwrap());
        assert!(db.memory_get("del_me").unwrap().is_none());
    }

    #[test]
    fn test_memory_link_people() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("auth-pref", "prefer JWT", "project", None)
            .unwrap();
        let alice = db.people_add("Alice", None).unwrap();
        let bob = db.people_add("Bob", None).unwrap();

        assert!(db.memory_link_person("auth-pref", alice).unwrap());
        assert!(db.memory_link_person("auth-pref", bob).unwrap());

        let people = db.memory_people("auth-pref").unwrap();
        assert_eq!(people.len(), 2);
        let names: Vec<&str> = people.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"Alice"));
        assert!(names.contains(&"Bob"));
    }

    #[test]
    fn test_memory_link_missing_key_returns_false() {
        let db = Database::open_in_memory().unwrap();
        let person_id = db.people_add("Carol", None).unwrap();
        assert!(!db.memory_link_person("no-such-key", person_id).unwrap());
    }

    #[test]
    fn test_memory_unlink_person() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("k", "v", "general", None).unwrap();
        let person_id = db.people_add("Dave", None).unwrap();

        db.memory_link_person("k", person_id).unwrap();
        assert_eq!(db.memory_people("k").unwrap().len(), 1);

        assert!(db.memory_unlink_person("k", person_id).unwrap());
        assert!(db.memory_people("k").unwrap().is_empty());
        assert!(!db.memory_unlink_person("k", person_id).unwrap());
    }

    #[test]
    fn test_token_overlap() {
        // Same fact, different phrasing -> high overlap.
        let a = "Chunk size for embeddings capped at 800 chars for AllMiniLM";
        let b = "Embeddings chunk size limited to 800 chars because AllMiniLM";
        assert!(
            token_overlap(a, b) >= 0.6,
            "overlap was {}",
            token_overlap(a, b)
        );

        // Unrelated facts -> low overlap.
        let c = "Stop hook reads Claude Code JSON from stdin";
        assert!(token_overlap(a, c) < 0.3);

        // Empty proposed -> zero.
        assert_eq!(token_overlap("the and for", "anything here"), 0.0);
    }

    /// Two active memories plus a flagged conflict between them.
    fn db_with_conflicting_pair() -> (Database, i64, i64) {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("alpha", "first fact", "project", None)
            .unwrap();
        db.memory_set("beta", "second fact", "project", None)
            .unwrap();
        let a = db.memory_get("alpha").unwrap().unwrap().id;
        let b = db.memory_get("beta").unwrap().unwrap().id;
        db.memory_conflict_add(a, b, Some(0.9)).unwrap();
        (db, a, b)
    }

    #[test]
    fn distinct_marking_clears_and_suppresses_the_conflict() {
        let (db, a, b) = db_with_conflicting_pair();
        assert_eq!(db.memory_conflicts(None).unwrap().len(), 1);

        db.memory_distinct_add(a, b, Some("different facts"))
            .unwrap();
        assert!(
            db.memory_conflicts(None).unwrap().is_empty(),
            "marking distinct clears the existing flag"
        );

        // And a later re-flag of the same pair does not come back.
        db.memory_conflict_add(a, b, Some(0.9)).unwrap();
        assert!(
            db.memory_conflicts(None).unwrap().is_empty(),
            "a re-flagged distinct pair stays suppressed"
        );
    }

    #[test]
    fn distinct_marking_is_symmetric() {
        let (db, a, b) = db_with_conflicting_pair();
        db.memory_distinct_add(b, a, None).unwrap();
        assert!(db.memory_is_distinct(a, b).unwrap());
        assert!(db.memory_is_distinct(b, a).unwrap());
    }

    /// Two unit vectors `theta` apart, as a stand-in for a memory pair at a
    /// known cosine.
    fn pair_at(db: &Database, cos: f32) -> (i64, i64) {
        let mut a = vec![0.0f32; 384];
        a[0] = 1.0;
        let mut b = vec![0.0f32; 384];
        b[0] = cos;
        b[1] = (1.0 - cos * cos).sqrt();
        db.memory_set("alpha", "first fact", "project", None)
            .unwrap();
        db.memory_set("beta", "second fact", "project", None)
            .unwrap();
        db.memory_set_embedding("alpha", &a).unwrap();
        db.memory_set_embedding("beta", &b).unwrap();
        (
            db.memory_get("alpha").unwrap().unwrap().id,
            db.memory_get("beta").unwrap().unwrap().id,
        )
    }

    #[test]
    fn conflicts_below_the_floor_are_dropped_on_read() {
        let db = Database::open_in_memory().unwrap();
        let (a, b) = pair_at(&db, 0.91);
        db.memory_conflict_add(a, b, Some(0.91)).unwrap();

        assert_eq!(
            db.memory_conflicts(None).unwrap().len(),
            1,
            "no floor, kept"
        );
        assert_eq!(db.memory_conflicts(Some(0.90)).unwrap().len(), 1, "above");
        assert!(
            db.memory_conflicts(Some(0.93)).unwrap().is_empty(),
            "0.91 does not clear a 0.93 floor"
        );
    }

    /// The stored score is a snapshot. Editing a memory moves the real score,
    /// so the floor has to be applied to what the vectors say now: this pair
    /// was filed at 0.88 and would be swept on its stored score, but it reads
    /// 0.96 today and is a duplicate worth showing.
    #[test]
    fn a_stale_low_score_is_refreshed_rather_than_swept() {
        let db = Database::open_in_memory().unwrap();
        let (a, b) = pair_at(&db, 0.96);
        db.memory_conflict_add(a, b, Some(0.88)).unwrap();

        let conflicts = db.memory_conflicts(Some(0.93)).unwrap();
        assert_eq!(
            conflicts.len(),
            1,
            "kept on its current score, not its old one"
        );
        let score = conflicts[0].score.expect("re-scored");
        assert!(
            (score - 0.96).abs() < 0.001,
            "stored score refreshed to {score}, expected ~0.96"
        );
    }

    /// No score means it was never scored, not that it scored low.
    #[test]
    fn unscored_conflicts_survive_the_floor() {
        let db = Database::open_in_memory().unwrap();
        let (a, b) = pair_at(&db, 0.10);
        db.memory_conflict_add(a, b, None).unwrap();
        assert_eq!(db.memory_conflicts(Some(0.93)).unwrap().len(), 1);
    }

    /// Nothing to compare: a pair is never dropped for want of a vector.
    #[test]
    fn conflicts_without_vectors_survive_the_floor() {
        let (db, a, b) = db_with_conflicting_pair();
        assert!(db.memory_needing_embedding().unwrap().len() >= 2);
        let _ = (a, b);
        assert_eq!(db.memory_conflicts(Some(0.99)).unwrap().len(), 1);
    }

    /// An edit leaves no vector of the old value behind to be judged on.
    #[test]
    fn editing_a_memory_clears_its_vector_and_keeps_the_conflict() {
        let db = Database::open_in_memory().unwrap();
        let (a, b) = pair_at(&db, 0.10);
        db.memory_conflict_add(a, b, Some(0.95)).unwrap();

        db.memory_set("alpha", "first fact, restated", "project", None)
            .unwrap();
        let needing: Vec<_> = db
            .memory_needing_embedding()
            .unwrap()
            .into_iter()
            .map(|m| m.key)
            .collect();
        assert_eq!(needing, ["alpha"]);
        assert_eq!(db.memory_conflicts(Some(0.93)).unwrap().len(), 1);

        // Rewriting the same value keeps the vector.
        db.memory_set("beta", "second fact", "project", None)
            .unwrap();
        assert_eq!(db.memory_needing_embedding().unwrap().len(), 1);
    }

    /// The floor belongs to the configured model; vectors from another one
    /// are not judged against it.
    #[test]
    fn conflicts_are_not_rescored_under_another_model() {
        let db = Database::open_in_memory().unwrap();
        let (a, b) = pair_at(&db, 0.10);
        db.memory_conflict_add(a, b, Some(0.95)).unwrap();
        db.set_meta(crate::embed::models::META_KEY, "other-model")
            .unwrap();

        let conflicts = db.memory_conflicts(Some(0.93)).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].score, Some(0.95), "score left as filed");
    }

    #[test]
    fn distinct_marking_can_be_undone() {
        let (db, a, b) = db_with_conflicting_pair();
        db.memory_distinct_add(a, b, None).unwrap();
        assert!(
            db.memory_distinct_remove(b, a).unwrap(),
            "order-independent"
        );
        assert!(!db.memory_is_distinct(a, b).unwrap());

        // Removing restores normal behaviour: the pair can be flagged again.
        db.memory_conflict_add(a, b, Some(0.9)).unwrap();
        assert_eq!(db.memory_conflicts(None).unwrap().len(), 1);
        assert!(
            !db.memory_distinct_remove(a, b).unwrap(),
            "second remove is a no-op"
        );
    }

    #[test]
    fn distinct_add_is_idempotent_and_refreshes_reason() {
        let (db, a, b) = db_with_conflicting_pair();
        db.memory_distinct_add(a, b, Some("first reason")).unwrap();
        db.memory_distinct_add(a, b, Some("better reason")).unwrap();
        let pairs = db.memory_distinct_list().unwrap();
        assert_eq!(pairs.len(), 1, "no duplicate row");
        assert_eq!(pairs[0].2.as_deref(), Some("better reason"));
    }

    #[test]
    fn distinct_rejects_a_memory_against_itself() {
        let (db, a, _b) = db_with_conflicting_pair();
        assert!(!db.memory_distinct_add(a, a, None).unwrap());
        assert!(db.memory_distinct_list().unwrap().is_empty());
    }

    #[test]
    fn test_find_near_duplicates() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set(
            "chunk-size-fix",
            "Embedding chunk size limited to 800 chars because AllMiniLM max is 256 tokens",
            "project",
            None,
        )
        .unwrap();
        db.memory_set(
            "hook-stdin",
            "Stop hook reads Claude Code JSON from stdin to avoid broken pipe",
            "project",
            None,
        )
        .unwrap();

        // A near-duplicate of the chunk-size fact should be found.
        let dups = db
            .memory_find_near_duplicates(
                "Chunk size for embeddings capped at 800 chars for the AllMiniLM model",
                0.6,
            )
            .unwrap();
        assert_eq!(dups.len(), 1);
        assert_eq!(dups[0].entry.key, "chunk-size-fix");

        // An unrelated new fact should not match.
        let none = db
            .memory_find_near_duplicates("User prefers dark mode in the terminal editor", 0.6)
            .unwrap();
        assert!(none.is_empty());

        // Two existing restatements of the same fact both match, best first.
        db.memory_set(
            "chunk-size-cap",
            "Chunk size for embedding is capped at 800 chars for the AllMiniLM window",
            "project",
            None,
        )
        .unwrap();
        let dups = db
            .memory_find_near_duplicates(
                "Chunk size for embeddings capped at 800 chars for the AllMiniLM model",
                0.6,
            )
            .unwrap();
        assert_eq!(dups.len(), 2);
        assert!(dups[0].score >= dups[1].score);
    }

    #[test]
    fn test_superseded_key_reuse_does_not_resurrect() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("fact", "v1", "project", None).unwrap();
        let old_id = db.memory_get("fact").unwrap().unwrap().id;

        assert!(db
            .memory_supersede("fact", "fact-v2", "v2", "project", "manual")
            .unwrap());

        // Re-learning under the retired key creates a NEW active row; the old
        // row stays superseded with its pointer intact.
        let new_id = db.memory_set("fact", "v3", "project", None).unwrap();
        assert_ne!(new_id, old_id);
        let active = db.memory_get("fact").unwrap().unwrap();
        assert_eq!(active.id, new_id);
        assert_eq!(active.status, "active");
        assert_eq!(active.superseded_by, None);

        let history = db.memory_history("fact").unwrap();
        let old = history.iter().find(|e| e.id == old_id).unwrap();
        assert_eq!(old.status, "superseded");
        let successor_id = old.superseded_by.unwrap();
        let successor = db.memory_get_by_id(successor_id).unwrap().unwrap();
        assert_eq!(successor.key, "fact-v2");
        // The chain member fact-v2 is reachable from the key's history.
        assert!(history.iter().any(|e| e.key == "fact-v2"));
    }

    #[test]
    fn test_supersede_requires_active_old_and_distinct_keys() {
        let db = Database::open_in_memory().unwrap();
        assert!(!db
            .memory_supersede("missing", "new", "v", "project", "manual")
            .unwrap());

        db.memory_set("k", "v", "project", None).unwrap();
        assert!(db
            .memory_supersede("k", "k", "v2", "project", "manual")
            .is_err());

        // A retired key can't be superseded again.
        db.memory_supersede("k", "k2", "v2", "project", "manual")
            .unwrap();
        assert!(!db
            .memory_supersede("k", "k3", "v3", "project", "manual")
            .unwrap());
    }

    #[test]
    fn test_supersede_converges_on_canonical() {
        let db = Database::open_in_memory().unwrap();
        let a = db
            .memory_set("dup-a", "the fact, worded one way", "project", None)
            .unwrap();
        let b = db
            .memory_set("dup-b", "the fact, worded another way", "project", None)
            .unwrap();

        db.memory_supersede("dup-a", "canonical", "the fact", "project", "manual")
            .unwrap();
        db.memory_supersede("dup-b", "canonical", "the fact", "project", "manual")
            .unwrap();

        // One active canonical row with no pointer; both dups point at it.
        let canonical = db.memory_get("canonical").unwrap().unwrap();
        assert_eq!(canonical.status, "active");
        assert_eq!(canonical.superseded_by, None);
        for id in [a, b] {
            let e = db.memory_get_by_id(id).unwrap().unwrap();
            assert_eq!(e.status, "superseded");
            assert_eq!(e.superseded_by, Some(canonical.id));
        }
        let actives = db.memory_list(None, None).unwrap();
        assert_eq!(actives.len(), 1);
    }

    #[test]
    fn test_memory_conflicts_flag_and_self_prune() {
        let db = Database::open_in_memory().unwrap();
        let a = db.memory_set("a", "fact one", "project", None).unwrap();
        let b = db
            .memory_set("b", "fact one restated", "project", None)
            .unwrap();

        db.memory_conflict_add(a, b, Some(0.91)).unwrap();
        db.memory_conflict_add(a, b, Some(0.99)).unwrap(); // duplicate pair: no-op

        let conflicts = db.memory_conflicts(None).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].memory.key, "a");
        assert_eq!(conflicts[0].matched.key, "b");
        assert_eq!(conflicts[0].score, Some(0.91));

        // Resolving one side (supersede) makes the conflict disappear.
        db.memory_supersede("b", "a-and-b", "fact one, merged", "project", "manual")
            .unwrap();
        assert!(db.memory_conflicts(None).unwrap().is_empty());
    }

    #[test]
    fn test_memory_similar_and_embedding_backfill() {
        let db = Database::open_in_memory().unwrap();
        db.memory_set("a", "alpha", "project", None).unwrap();
        db.memory_set("b", "beta", "project", None).unwrap();
        db.memory_set("c", "gamma", "project", None).unwrap();

        // Before embedding: all three need backfill.
        assert_eq!(db.memory_needing_embedding().unwrap().len(), 3);

        // 384-dim mock embeddings (the vec0 index enforces the model dim).
        // 'c' is close to 'a', 'b' is orthogonal.
        let unit = |i: usize| {
            let mut v = vec![0.0f32; 384];
            v[i] = 1.0;
            v
        };
        let mut mix = vec![0.0f32; 384];
        mix[0] = 0.9;
        mix[1] = 0.1;
        db.memory_set_embedding("a", &unit(0)).unwrap();
        db.memory_set_embedding("b", &unit(1)).unwrap();
        db.memory_set_embedding("c", &mix).unwrap();
        assert!(db.memory_needing_embedding().unwrap().is_empty());

        // Ranking: exact match first, then the near neighbour.
        let res = db.memory_similar(&unit(0), 2).unwrap();
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].entry.key, "a");
        assert_eq!(res[1].entry.key, "c");

        // Semantic near-dups honour the threshold, best first: both 'a'
        // (exact) and 'c' (cosine ~0.99) clear 0.95, while 'b' is orthogonal.
        let hits = db
            .memory_find_near_duplicates_semantic(&unit(0), 0.95, 8)
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].entry.key, "a");
        assert_eq!(hits[1].entry.key, "c");
        let misses = db
            .memory_find_near_duplicates_semantic(&unit(2), 0.95, 8)
            .unwrap();
        assert!(misses.is_empty());

        // Setting a key that doesn't exist returns false.
        assert!(!db.memory_set_embedding("nope", &unit(0)).unwrap());
    }

    #[test]
    fn test_memory_tags() {
        let db = Database::open_in_memory().unwrap();
        let tags = vec!["rust".to_string(), "systems".to_string()];
        db.memory_set("tagged", "some value", "reference", Some(&tags))
            .unwrap();
        let entry = db.memory_get("tagged").unwrap().unwrap();
        assert_eq!(entry.tags.unwrap(), tags);
    }
}
