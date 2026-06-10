use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{bytes_to_embedding, embedding_to_bytes, fts, Database};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meeting {
    pub id: i64,
    pub title: String,
    pub meeting_date: Option<String>,
    pub transcript: Option<String>,
    pub notes: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeetingPerson {
    pub meeting_id: i64,
    pub person_id: i64,
    pub role: Option<String>,
    pub person_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimilarMeeting {
    pub meeting: Meeting,
    pub score: f32,
    pub matched_chunk: String,
}

impl Database {
    pub fn meeting_add(
        &self,
        title: &str,
        meeting_date: Option<&str>,
        transcript: Option<&str>,
        notes: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO meetings (title, meeting_date, transcript, notes)
             VALUES (?1, ?2, ?3, ?4)",
            params![title, meeting_date, transcript, notes],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn meeting_get(&self, id: i64) -> Result<Option<Meeting>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, meeting_date, transcript, notes, created_at
             FROM meetings WHERE id = ?1",
        )?;
        match stmt.query_row(params![id], row_to_meeting) {
            Ok(m) => Ok(Some(m)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn meeting_list(
        &self,
        date_from: Option<&str>,
        date_to: Option<&str>,
    ) -> Result<Vec<Meeting>> {
        let sql = match (date_from, date_to) {
            (Some(_), Some(_)) => {
                "SELECT id, title, meeting_date, transcript, notes, created_at
                 FROM meetings WHERE meeting_date >= ?1 AND meeting_date <= ?2
                 ORDER BY meeting_date DESC"
            }
            (Some(_), None) => {
                "SELECT id, title, meeting_date, transcript, notes, created_at
                 FROM meetings WHERE meeting_date >= ?1
                 ORDER BY meeting_date DESC"
            }
            (None, Some(_)) => {
                "SELECT id, title, meeting_date, transcript, notes, created_at
                 FROM meetings WHERE meeting_date <= ?2
                 ORDER BY meeting_date DESC"
            }
            (None, None) => {
                "SELECT id, title, meeting_date, transcript, notes, created_at
                 FROM meetings ORDER BY meeting_date DESC"
            }
        };

        let mut stmt = self.conn.prepare(sql)?;
        let rows = match (date_from, date_to) {
            (Some(f), Some(t)) => stmt.query_map(params![f, t], row_to_meeting)?,
            (Some(f), None) => stmt.query_map(params![f], row_to_meeting)?,
            (None, Some(t)) => stmt.query_map(params![t], row_to_meeting)?,
            (None, None) => stmt.query_map([], row_to_meeting)?,
        };
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn meeting_search(&self, query: &str) -> Result<Vec<Meeting>> {
        let query = fts::sanitize(query);
        let mut stmt = self.conn.prepare(
            "SELECT id, title, meeting_date, transcript, notes, created_at
             FROM meetings
             WHERE id IN (SELECT rowid FROM meetings_fts WHERE meetings_fts MATCH ?1)
             ORDER BY meeting_date DESC",
        )?;
        let rows = stmt.query_map(params![query], row_to_meeting)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn meeting_delete(&self, id: i64) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM meetings WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    pub fn meeting_link_person(
        &self,
        meeting_id: i64,
        person_id: i64,
        role: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meeting_people (meeting_id, person_id, role)
             VALUES (?1, ?2, ?3)",
            params![meeting_id, person_id, role],
        )?;
        Ok(())
    }

    pub fn meeting_unlink_person(&self, meeting_id: i64, person_id: i64) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM meeting_people WHERE meeting_id = ?1 AND person_id = ?2",
            params![meeting_id, person_id],
        )?;
        Ok(n > 0)
    }

    pub fn meeting_people(&self, meeting_id: i64) -> Result<Vec<MeetingPerson>> {
        let mut stmt = self.conn.prepare(
            "SELECT mp.meeting_id, mp.person_id, mp.role, p.name
             FROM meeting_people mp
             JOIN people p ON mp.person_id = p.id
             WHERE mp.meeting_id = ?1",
        )?;
        let rows = stmt.query_map(params![meeting_id], |row| {
            Ok(MeetingPerson {
                meeting_id: row.get(0)?,
                person_id: row.get(1)?,
                role: row.get(2)?,
                person_name: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Store pre-chunked text with optional embedding for a meeting.
    pub fn meeting_store_chunks(
        &self,
        meeting_id: i64,
        chunks: &[(String, Option<Vec<f32>>)],
    ) -> Result<()> {
        self.conn.execute(
            "DELETE FROM meeting_chunks WHERE meeting_id = ?1",
            params![meeting_id],
        )?;
        let mut stmt = self.conn.prepare(
            "INSERT INTO meeting_chunks (meeting_id, chunk_index, chunk_text, embedding)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (i, (text, emb)) in chunks.iter().enumerate() {
            let emb_bytes = emb.as_ref().map(|e| embedding_to_bytes(e));
            stmt.execute(params![meeting_id, i as i64, text, emb_bytes])?;
        }
        Ok(())
    }

    pub fn meeting_similar(
        &self,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<SimilarMeeting>> {
        // Load all chunks with embeddings
        let mut stmt = self.conn.prepare(
            "SELECT mc.meeting_id, mc.chunk_text, mc.embedding,
                    m.id, m.title, m.meeting_date, m.transcript, m.notes, m.created_at
             FROM meeting_chunks mc
             JOIN meetings m ON mc.meeting_id = m.id
             WHERE mc.embedding IS NOT NULL",
        )?;

        let mut scored: Vec<(f32, String, Meeting)> = stmt
            .query_map([], |row| {
                let emb_bytes: Vec<u8> = row.get(2)?;
                let chunk_text: String = row.get(1)?;
                let meeting = Meeting {
                    id: row.get(3)?,
                    title: row.get(4)?,
                    meeting_date: row.get(5)?,
                    transcript: row.get(6)?,
                    notes: row.get(7)?,
                    created_at: row.get(8)?,
                };
                Ok((emb_bytes, chunk_text, meeting))
            })?
            .filter_map(|r| r.ok())
            .map(|(emb_bytes, chunk, meeting)| {
                let emb = bytes_to_embedding(&emb_bytes);
                let score = crate::search::similarity::cosine_similarity(query_embedding, &emb);
                (score, chunk, meeting)
            })
            .collect();

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);

        Ok(scored
            .into_iter()
            .map(|(score, matched_chunk, meeting)| SimilarMeeting {
                meeting,
                score,
                matched_chunk,
            })
            .collect())
    }
}

fn row_to_meeting(row: &rusqlite::Row<'_>) -> rusqlite::Result<Meeting> {
    Ok(Meeting {
        id: row.get(0)?,
        title: row.get(1)?,
        meeting_date: row.get(2)?,
        transcript: row.get(3)?,
        notes: row.get(4)?,
        created_at: row.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_meeting_add_get() {
        let db = Database::open_in_memory().unwrap();
        let id = db
            .meeting_add("Standup", Some("2026-06-01"), None, Some("discussed X"))
            .unwrap();
        let m = db.meeting_get(id).unwrap().unwrap();
        assert_eq!(m.title, "Standup");
        assert_eq!(m.meeting_date.as_deref(), Some("2026-06-01"));
        assert_eq!(m.notes.as_deref(), Some("discussed X"));
    }

    #[test]
    fn test_meeting_search() {
        let db = Database::open_in_memory().unwrap();
        db.meeting_add(
            "Architecture Review",
            None,
            None,
            Some("we discussed microservices"),
        )
        .unwrap();
        db.meeting_add("1:1 Sync", None, None, Some("career growth topics"))
            .unwrap();

        let results = db.meeting_search("microservices").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Architecture Review");
    }

    #[test]
    fn test_meeting_link_person() {
        let db = Database::open_in_memory().unwrap();
        let person_id = db
            .people_add("Alice", None, None, None, None, None, None)
            .unwrap();
        let meeting_id = db.meeting_add("Planning", None, None, None).unwrap();

        db.meeting_link_person(meeting_id, person_id, Some("organizer"))
            .unwrap();

        let people = db.meeting_people(meeting_id).unwrap();
        assert_eq!(people.len(), 1);
        assert_eq!(people[0].person_name, "Alice");
        assert_eq!(people[0].role.as_deref(), Some("organizer"));
    }

    #[test]
    fn test_meeting_chunks_and_similarity() {
        let db = Database::open_in_memory().unwrap();
        let id = db
            .meeting_add("Tech Talk", None, Some("Rust is great for systems"), None)
            .unwrap();

        // Store chunks with mock embeddings
        let v1: Vec<f32> = vec![1.0, 0.0, 0.0];
        let v2: Vec<f32> = vec![0.0, 1.0, 0.0];
        db.meeting_store_chunks(
            id,
            &[
                ("Rust is great for systems".to_string(), Some(v1.clone())),
                ("Python is great for scripting".to_string(), Some(v2)),
            ],
        )
        .unwrap();

        // Query with a vector close to v1
        let query = vec![0.9, 0.1, 0.0];
        let results = db.meeting_similar(&query, 2).unwrap();
        assert_eq!(results.len(), 2);
        // First result should be the one matching [1,0,0]
        assert!(results[0].score > results[1].score);
        assert!(results[0].matched_chunk.contains("Rust"));
    }

    #[test]
    fn test_meeting_list_date_filter() {
        let db = Database::open_in_memory().unwrap();
        db.meeting_add("M1", Some("2026-01-01"), None, None)
            .unwrap();
        db.meeting_add("M2", Some("2026-03-01"), None, None)
            .unwrap();
        db.meeting_add("M3", Some("2026-06-01"), None, None)
            .unwrap();

        let results = db
            .meeting_list(Some("2026-02-01"), Some("2026-05-01"))
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "M2");
    }
}
