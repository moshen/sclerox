pub const PRIMARY_SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;

-- Memory: Claude-compatible persistent memory entries
CREATE TABLE IF NOT EXISTS memory (
    id INTEGER PRIMARY KEY,
    key TEXT NOT NULL UNIQUE,
    value TEXT NOT NULL,
    memory_type TEXT NOT NULL DEFAULT 'general',
    tags TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
    key, value, memory_type, tags,
    content=memory, content_rowid=id
);
CREATE TRIGGER IF NOT EXISTS memory_ai AFTER INSERT ON memory BEGIN
    INSERT INTO memory_fts(rowid, key, value, memory_type, tags)
    VALUES (new.id, new.key, new.value, new.memory_type, new.tags);
END;
CREATE TRIGGER IF NOT EXISTS memory_au AFTER UPDATE ON memory BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, key, value, memory_type, tags)
    VALUES ('delete', old.id, old.key, old.value, old.memory_type, old.tags);
    INSERT INTO memory_fts(rowid, key, value, memory_type, tags)
    VALUES (new.id, new.key, new.value, new.memory_type, new.tags);
END;
CREATE TRIGGER IF NOT EXISTS memory_ad AFTER DELETE ON memory BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, key, value, memory_type, tags)
    VALUES ('delete', old.id, old.key, old.value, old.memory_type, old.tags);
END;

-- People: v1 baseline retains inline identifier columns.
-- Migration v7 moves them to people_identifiers and drops these columns.
CREATE TABLE IF NOT EXISTS people (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    email TEXT,
    slack_id TEXT,
    slack_url TEXT,
    github_username TEXT,
    github_url TEXT,
    notes TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE VIRTUAL TABLE IF NOT EXISTS people_fts USING fts5(
    name, email, github_username, notes,
    content=people, content_rowid=id
);
CREATE TRIGGER IF NOT EXISTS people_ai AFTER INSERT ON people BEGIN
    INSERT INTO people_fts(rowid, name, email, github_username, notes)
    VALUES (new.id, new.name, new.email, new.github_username, new.notes);
END;
CREATE TRIGGER IF NOT EXISTS people_au AFTER UPDATE ON people BEGIN
    INSERT INTO people_fts(people_fts, rowid, name, email, github_username, notes)
    VALUES ('delete', old.id, old.name, old.email, old.github_username, old.notes);
    INSERT INTO people_fts(rowid, name, email, github_username, notes)
    VALUES (new.id, new.name, new.email, new.github_username, new.notes);
END;
CREATE TRIGGER IF NOT EXISTS people_ad AFTER DELETE ON people BEGIN
    INSERT INTO people_fts(people_fts, rowid, name, email, github_username, notes)
    VALUES ('delete', old.id, old.name, old.email, old.github_username, old.notes);
END;

-- Meetings: with FTS on title/transcript/notes and chunked embeddings
CREATE TABLE IF NOT EXISTS meetings (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    meeting_date TEXT,
    transcript TEXT,
    notes TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE VIRTUAL TABLE IF NOT EXISTS meetings_fts USING fts5(
    title, transcript, notes,
    content=meetings, content_rowid=id
);
CREATE TRIGGER IF NOT EXISTS meetings_ai AFTER INSERT ON meetings BEGIN
    INSERT INTO meetings_fts(rowid, title, transcript, notes)
    VALUES (new.id, new.title, new.transcript, new.notes);
END;
CREATE TRIGGER IF NOT EXISTS meetings_au AFTER UPDATE ON meetings BEGIN
    INSERT INTO meetings_fts(meetings_fts, rowid, title, transcript, notes)
    VALUES ('delete', old.id, old.title, old.transcript, old.notes);
    INSERT INTO meetings_fts(rowid, title, transcript, notes)
    VALUES (new.id, new.title, new.transcript, new.notes);
END;
CREATE TRIGGER IF NOT EXISTS meetings_ad AFTER DELETE ON meetings BEGIN
    INSERT INTO meetings_fts(meetings_fts, rowid, title, transcript, notes)
    VALUES ('delete', old.id, old.title, old.transcript, old.notes);
END;

CREATE TABLE IF NOT EXISTS meeting_people (
    meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    person_id INTEGER NOT NULL REFERENCES people(id) ON DELETE CASCADE,
    role TEXT,
    PRIMARY KEY (meeting_id, person_id)
);

-- Chunked text + embeddings for meeting similarity search
CREATE TABLE IF NOT EXISTS meeting_chunks (
    id INTEGER PRIMARY KEY,
    meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    chunk_index INTEGER NOT NULL,
    chunk_text TEXT NOT NULL,
    embedding BLOB
);

-- Projects: collection of links, description, people/meeting cross-references
CREATE TABLE IF NOT EXISTS projects (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT,
    links TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE VIRTUAL TABLE IF NOT EXISTS projects_fts USING fts5(
    name, description,
    content=projects, content_rowid=id
);
CREATE TRIGGER IF NOT EXISTS projects_ai AFTER INSERT ON projects BEGIN
    INSERT INTO projects_fts(rowid, name, description)
    VALUES (new.id, new.name, new.description);
END;
CREATE TRIGGER IF NOT EXISTS projects_au AFTER UPDATE ON projects BEGIN
    INSERT INTO projects_fts(projects_fts, rowid, name, description)
    VALUES ('delete', old.id, old.name, old.description);
    INSERT INTO projects_fts(rowid, name, description)
    VALUES (new.id, new.name, new.description);
END;
CREATE TRIGGER IF NOT EXISTS projects_ad AFTER DELETE ON projects BEGIN
    INSERT INTO projects_fts(projects_fts, rowid, name, description)
    VALUES ('delete', old.id, old.name, old.description);
END;

CREATE TABLE IF NOT EXISTS project_people (
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    person_id INTEGER NOT NULL REFERENCES people(id) ON DELETE CASCADE,
    role TEXT,
    PRIMARY KEY (project_id, person_id)
);

CREATE TABLE IF NOT EXISTS project_meetings (
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    PRIMARY KEY (project_id, meeting_id)
);

-- Repos registry: points to per-repo SQLite files, FTS + embedding on description
CREATE TABLE IF NOT EXISTS repos (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    description TEXT,
    db_path TEXT NOT NULL,
    last_indexed TEXT,
    description_embedding BLOB,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE VIRTUAL TABLE IF NOT EXISTS repos_fts USING fts5(
    name, description,
    content=repos, content_rowid=id
);
CREATE TRIGGER IF NOT EXISTS repos_ai AFTER INSERT ON repos BEGIN
    INSERT INTO repos_fts(rowid, name, description)
    VALUES (new.id, new.name, new.description);
END;
CREATE TRIGGER IF NOT EXISTS repos_au AFTER UPDATE ON repos BEGIN
    INSERT INTO repos_fts(repos_fts, rowid, name, description)
    VALUES ('delete', old.id, old.name, old.description);
    INSERT INTO repos_fts(rowid, name, description)
    VALUES (new.id, new.name, new.description);
END;
CREATE TRIGGER IF NOT EXISTS repos_ad AFTER DELETE ON repos BEGIN
    INSERT INTO repos_fts(repos_fts, rowid, name, description)
    VALUES ('delete', old.id, old.name, old.description);
END;
";

/// Migration v2: todos and investigations tables.
pub const MIGRATION_V2: &str = "
-- Todos: task tracking with complete done history
-- status: open | done | watch
-- category: slack | github | email | meeting | project | general
CREATE TABLE IF NOT EXISTS todos (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    notes TEXT,
    status TEXT NOT NULL DEFAULT 'open',
    source_url TEXT,
    category TEXT NOT NULL DEFAULT 'general',
    originated_date TEXT NOT NULL DEFAULT (date('now')),
    deadline_date TEXT,
    completed_at TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE VIRTUAL TABLE IF NOT EXISTS todos_fts USING fts5(
    title, notes, category, status,
    content=todos, content_rowid=id
);
CREATE TRIGGER IF NOT EXISTS todos_ai AFTER INSERT ON todos BEGIN
    INSERT INTO todos_fts(rowid, title, notes, category, status)
    VALUES (new.id, new.title, new.notes, new.category, new.status);
END;
CREATE TRIGGER IF NOT EXISTS todos_au AFTER UPDATE ON todos BEGIN
    INSERT INTO todos_fts(todos_fts, rowid, title, notes, category, status)
    VALUES ('delete', old.id, old.title, old.notes, old.category, old.status);
    INSERT INTO todos_fts(rowid, title, notes, category, status)
    VALUES (new.id, new.title, new.notes, new.category, new.status);
END;
CREATE TRIGGER IF NOT EXISTS todos_ad AFTER DELETE ON todos BEGIN
    INSERT INTO todos_fts(todos_fts, rowid, title, notes, category, status)
    VALUES ('delete', old.id, old.title, old.notes, old.category, old.status);
END;

CREATE TABLE IF NOT EXISTS todo_people (
    todo_id INTEGER NOT NULL REFERENCES todos(id) ON DELETE CASCADE,
    person_id INTEGER NOT NULL REFERENCES people(id) ON DELETE CASCADE,
    PRIMARY KEY (todo_id, person_id)
);
CREATE TABLE IF NOT EXISTS todo_projects (
    todo_id INTEGER NOT NULL REFERENCES todos(id) ON DELETE CASCADE,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    PRIMARY KEY (todo_id, project_id)
);

-- Investigations / Research
-- status: planning | active | concluded
CREATE TABLE IF NOT EXISTS investigations (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    slug TEXT NOT NULL UNIQUE,
    status TEXT NOT NULL DEFAULT 'open',
    plan TEXT,
    findings TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    concluded_at TEXT,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE VIRTUAL TABLE IF NOT EXISTS investigations_fts USING fts5(
    name, slug, plan, findings,
    content=investigations, content_rowid=id
);
CREATE TRIGGER IF NOT EXISTS investigations_ai AFTER INSERT ON investigations BEGIN
    INSERT INTO investigations_fts(rowid, name, slug, plan, findings)
    VALUES (new.id, new.name, new.slug, new.plan, new.findings);
END;
CREATE TRIGGER IF NOT EXISTS investigations_au AFTER UPDATE ON investigations BEGIN
    INSERT INTO investigations_fts(investigations_fts, rowid, name, slug, plan, findings)
    VALUES ('delete', old.id, old.name, old.slug, old.plan, old.findings);
    INSERT INTO investigations_fts(rowid, name, slug, plan, findings)
    VALUES (new.id, new.name, new.slug, new.plan, new.findings);
END;
CREATE TRIGGER IF NOT EXISTS investigations_ad AFTER DELETE ON investigations BEGIN
    INSERT INTO investigations_fts(investigations_fts, rowid, name, slug, plan, findings)
    VALUES ('delete', old.id, old.name, old.slug, old.plan, old.findings);
END;

CREATE TABLE IF NOT EXISTS investigation_sources (
    id INTEGER PRIMARY KEY,
    investigation_id INTEGER NOT NULL REFERENCES investigations(id) ON DELETE CASCADE,
    url TEXT NOT NULL,
    label TEXT,
    notes TEXT,
    added_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE IF NOT EXISTS investigation_people (
    investigation_id INTEGER NOT NULL REFERENCES investigations(id) ON DELETE CASCADE,
    person_id INTEGER NOT NULL REFERENCES people(id) ON DELETE CASCADE,
    PRIMARY KEY (investigation_id, person_id)
);
CREATE TABLE IF NOT EXISTS investigation_projects (
    investigation_id INTEGER NOT NULL REFERENCES investigations(id) ON DELETE CASCADE,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    PRIMARY KEY (investigation_id, project_id)
);
";

/// Migration v3: memory_people junction table.
pub const MIGRATION_V3: &str = "
CREATE TABLE IF NOT EXISTS memory_people (
    memory_id INTEGER NOT NULL REFERENCES memory(id) ON DELETE CASCADE,
    person_id INTEGER NOT NULL REFERENCES people(id) ON DELETE CASCADE,
    PRIMARY KEY (memory_id, person_id)
);
";

/// Migration v4: project_repos junction table.
pub const MIGRATION_V4: &str = "
CREATE TABLE IF NOT EXISTS project_repos (
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    repo_id    INTEGER NOT NULL REFERENCES repos(id)    ON DELETE CASCADE,
    PRIMARY KEY (project_id, repo_id)
);
";

/// Migration v5: memory status, source, and supersession chain.
///
/// status:        active | stale | superseded
/// source:        manual | claude-auto | session
/// superseded_by: key of the memory that replaced this one (nullable)
/// reviewed_at:   when the user last confirmed this memory is still valid
pub const MIGRATION_V5: &str = "
ALTER TABLE memory ADD COLUMN status TEXT NOT NULL DEFAULT 'active';
ALTER TABLE memory ADD COLUMN source TEXT NOT NULL DEFAULT 'manual';
ALTER TABLE memory ADD COLUMN superseded_by TEXT;
ALTER TABLE memory ADD COLUMN reviewed_at TEXT;
";

/// Migration v6: investigation_chunks for semantic similarity search.
pub const MIGRATION_V6: &str = "
CREATE TABLE IF NOT EXISTS investigation_chunks (
    id INTEGER PRIMARY KEY,
    investigation_id INTEGER NOT NULL REFERENCES investigations(id) ON DELETE CASCADE,
    chunk_index INTEGER NOT NULL,
    chunk_text TEXT NOT NULL,
    embedding BLOB
);
";

/// Migration v7: people_identifiers + identifier_types; removes old inline identity columns.
pub const MIGRATION_V7: &str = "
-- Catalog of valid identifier types
CREATE TABLE IF NOT EXISTS identifier_types (
    name TEXT PRIMARY KEY,
    description TEXT
);
INSERT OR IGNORE INTO identifier_types (name, description) VALUES
    ('email',      'Email address'),
    ('slack',      'Slack member ID (e.g. U1234ABC)'),
    ('slack_url',  'Slack profile URL'),
    ('github',     'GitHub username'),
    ('github_url', 'GitHub profile URL'),
    ('atlassian',  'Atlassian account ID or email (covers Jira, Confluence, Bitbucket)'),
    ('linear',     'Linear user ID or email'),
    ('linkedin',   'LinkedIn profile URL or handle');

-- Identifier rows per person
CREATE TABLE IF NOT EXISTS people_identifiers (
    id INTEGER PRIMARY KEY,
    person_id INTEGER NOT NULL REFERENCES people(id) ON DELETE CASCADE,
    type TEXT NOT NULL REFERENCES identifier_types(name),
    identifier TEXT NOT NULL,
    UNIQUE(person_id, type)
);
CREATE INDEX IF NOT EXISTS idx_pi_person ON people_identifiers(person_id);
CREATE INDEX IF NOT EXISTS idx_pi_lookup ON people_identifiers(type, identifier);

-- Migrate data from old inline columns (only if they still exist)
INSERT OR IGNORE INTO people_identifiers (person_id, type, identifier)
    SELECT id, 'email', email FROM people WHERE email IS NOT NULL;
INSERT OR IGNORE INTO people_identifiers (person_id, type, identifier)
    SELECT id, 'slack', slack_id FROM people WHERE slack_id IS NOT NULL;
INSERT OR IGNORE INTO people_identifiers (person_id, type, identifier)
    SELECT id, 'slack_url', slack_url FROM people WHERE slack_url IS NOT NULL;
INSERT OR IGNORE INTO people_identifiers (person_id, type, identifier)
    SELECT id, 'github', github_username FROM people WHERE github_username IS NOT NULL;
INSERT OR IGNORE INTO people_identifiers (person_id, type, identifier)
    SELECT id, 'github_url', github_url FROM people WHERE github_url IS NOT NULL;

-- Rebuild FTS without identifier columns
DROP TRIGGER IF EXISTS people_ai;
DROP TRIGGER IF EXISTS people_au;
DROP TRIGGER IF EXISTS people_ad;
DROP TABLE IF EXISTS people_fts;
CREATE VIRTUAL TABLE IF NOT EXISTS people_fts USING fts5(
    name, notes,
    content=people, content_rowid=id
);
INSERT INTO people_fts(rowid, name, notes) SELECT id, name, notes FROM people;
CREATE TRIGGER IF NOT EXISTS people_ai AFTER INSERT ON people BEGIN
    INSERT INTO people_fts(rowid, name, notes) VALUES (new.id, new.name, new.notes);
END;
CREATE TRIGGER IF NOT EXISTS people_au AFTER UPDATE ON people BEGIN
    INSERT INTO people_fts(people_fts, rowid, name, notes)
        VALUES ('delete', old.id, old.name, old.notes);
    INSERT INTO people_fts(rowid, name, notes) VALUES (new.id, new.name, new.notes);
END;
CREATE TRIGGER IF NOT EXISTS people_ad AFTER DELETE ON people BEGIN
    INSERT INTO people_fts(people_fts, rowid, name, notes)
        VALUES ('delete', old.id, old.name, old.notes);
END;

-- Drop old columns (requires SQLite >= 3.35.0, bundled via rusqlite)
ALTER TABLE people DROP COLUMN email;
ALTER TABLE people DROP COLUMN slack_id;
ALTER TABLE people DROP COLUMN slack_url;
ALTER TABLE people DROP COLUMN github_username;
ALTER TABLE people DROP COLUMN github_url;
";

/// Migration v9: todo_chunks for semantic similarity search on todos.
pub const MIGRATION_V9: &str = "
CREATE TABLE IF NOT EXISTS todo_chunks (
    id INTEGER PRIMARY KEY,
    todo_id INTEGER NOT NULL REFERENCES todos(id) ON DELETE CASCADE,
    chunk_index INTEGER NOT NULL,
    chunk_text TEXT NOT NULL,
    embedding BLOB
);
";

/// Migration v10: add embedding to memory for semantic search + dedup.
/// Memory values are single short statements (1-3 sentences), so one vector
/// per row is stored directly on the table rather than in a chunks table.
pub const MIGRATION_V10: &str = "
ALTER TABLE memory ADD COLUMN embedding BLOB;
";

/// Migration v11: sqlite-vec KNN indexes for every embedded primary-DB table
/// (memory, meeting/todo/investigation chunks, repo descriptions), mirroring
/// what REPO_MIGRATION_V4 did for code chunks. The base `embedding` columns stay
/// the source of truth; each `*_vec` vec0 table is kept in sync by triggers and
/// backfilled from existing rows. 384 dims = AllMiniLM-L6-v2; cosine distance to
/// match the previous cosine_similarity ranking. Requires the sqlite-vec
/// extension (registered in Database::open before migrating).
pub const MIGRATION_V11: &str = "
-- memory (embedding on the row itself, keyed by memory.id). memory_similar
-- only ever wants ACTIVE memories, so keep the index active-only: maintain it
-- on both embedding and status changes, and backfill active rows only.
CREATE VIRTUAL TABLE IF NOT EXISTS memory_vec USING vec0(embedding float[384] distance_metric=cosine);
INSERT INTO memory_vec(rowid, embedding)
    SELECT id, embedding FROM memory WHERE embedding IS NOT NULL AND status = 'active';
CREATE TRIGGER IF NOT EXISTS memory_vec_ai AFTER INSERT ON memory
    WHEN new.embedding IS NOT NULL AND new.status = 'active'
BEGIN
    INSERT INTO memory_vec(rowid, embedding) VALUES (new.id, new.embedding);
END;
CREATE TRIGGER IF NOT EXISTS memory_vec_au AFTER UPDATE OF embedding ON memory
BEGIN
    DELETE FROM memory_vec WHERE rowid = old.id;
    INSERT INTO memory_vec(rowid, embedding)
        SELECT new.id, new.embedding
        WHERE new.embedding IS NOT NULL AND new.status = 'active';
END;
CREATE TRIGGER IF NOT EXISTS memory_vec_status AFTER UPDATE OF status ON memory
BEGIN
    DELETE FROM memory_vec WHERE rowid = old.id;
    INSERT INTO memory_vec(rowid, embedding)
        SELECT new.id, new.embedding
        WHERE new.embedding IS NOT NULL AND new.status = 'active';
END;
CREATE TRIGGER IF NOT EXISTS memory_vec_ad AFTER DELETE ON memory
BEGIN
    DELETE FROM memory_vec WHERE rowid = old.id;
END;

-- meeting_chunks
CREATE VIRTUAL TABLE IF NOT EXISTS meeting_chunks_vec USING vec0(embedding float[384] distance_metric=cosine);
INSERT INTO meeting_chunks_vec(rowid, embedding)
    SELECT id, embedding FROM meeting_chunks WHERE embedding IS NOT NULL;
CREATE TRIGGER IF NOT EXISTS meeting_chunks_vec_ai AFTER INSERT ON meeting_chunks
    WHEN new.embedding IS NOT NULL
BEGIN
    INSERT INTO meeting_chunks_vec(rowid, embedding) VALUES (new.id, new.embedding);
END;
CREATE TRIGGER IF NOT EXISTS meeting_chunks_vec_au AFTER UPDATE OF embedding ON meeting_chunks
BEGIN
    DELETE FROM meeting_chunks_vec WHERE rowid = old.id;
    INSERT INTO meeting_chunks_vec(rowid, embedding)
        SELECT new.id, new.embedding WHERE new.embedding IS NOT NULL;
END;
CREATE TRIGGER IF NOT EXISTS meeting_chunks_vec_ad AFTER DELETE ON meeting_chunks
BEGIN
    DELETE FROM meeting_chunks_vec WHERE rowid = old.id;
END;

-- todo_chunks
CREATE VIRTUAL TABLE IF NOT EXISTS todo_chunks_vec USING vec0(embedding float[384] distance_metric=cosine);
INSERT INTO todo_chunks_vec(rowid, embedding)
    SELECT id, embedding FROM todo_chunks WHERE embedding IS NOT NULL;
CREATE TRIGGER IF NOT EXISTS todo_chunks_vec_ai AFTER INSERT ON todo_chunks
    WHEN new.embedding IS NOT NULL
BEGIN
    INSERT INTO todo_chunks_vec(rowid, embedding) VALUES (new.id, new.embedding);
END;
CREATE TRIGGER IF NOT EXISTS todo_chunks_vec_au AFTER UPDATE OF embedding ON todo_chunks
BEGIN
    DELETE FROM todo_chunks_vec WHERE rowid = old.id;
    INSERT INTO todo_chunks_vec(rowid, embedding)
        SELECT new.id, new.embedding WHERE new.embedding IS NOT NULL;
END;
CREATE TRIGGER IF NOT EXISTS todo_chunks_vec_ad AFTER DELETE ON todo_chunks
BEGIN
    DELETE FROM todo_chunks_vec WHERE rowid = old.id;
END;

-- investigation_chunks
CREATE VIRTUAL TABLE IF NOT EXISTS investigation_chunks_vec USING vec0(embedding float[384] distance_metric=cosine);
INSERT INTO investigation_chunks_vec(rowid, embedding)
    SELECT id, embedding FROM investigation_chunks WHERE embedding IS NOT NULL;
CREATE TRIGGER IF NOT EXISTS investigation_chunks_vec_ai AFTER INSERT ON investigation_chunks
    WHEN new.embedding IS NOT NULL
BEGIN
    INSERT INTO investigation_chunks_vec(rowid, embedding) VALUES (new.id, new.embedding);
END;
CREATE TRIGGER IF NOT EXISTS investigation_chunks_vec_au AFTER UPDATE OF embedding ON investigation_chunks
BEGIN
    DELETE FROM investigation_chunks_vec WHERE rowid = old.id;
    INSERT INTO investigation_chunks_vec(rowid, embedding)
        SELECT new.id, new.embedding WHERE new.embedding IS NOT NULL;
END;
CREATE TRIGGER IF NOT EXISTS investigation_chunks_vec_ad AFTER DELETE ON investigation_chunks
BEGIN
    DELETE FROM investigation_chunks_vec WHERE rowid = old.id;
END;

-- repos (description_embedding, keyed by repos.id)
CREATE VIRTUAL TABLE IF NOT EXISTS repos_vec USING vec0(embedding float[384] distance_metric=cosine);
INSERT INTO repos_vec(rowid, embedding)
    SELECT id, description_embedding FROM repos WHERE description_embedding IS NOT NULL;
CREATE TRIGGER IF NOT EXISTS repos_vec_ai AFTER INSERT ON repos
    WHEN new.description_embedding IS NOT NULL
BEGIN
    INSERT INTO repos_vec(rowid, embedding) VALUES (new.id, new.description_embedding);
END;
CREATE TRIGGER IF NOT EXISTS repos_vec_au AFTER UPDATE OF description_embedding ON repos
BEGIN
    DELETE FROM repos_vec WHERE rowid = old.id;
    INSERT INTO repos_vec(rowid, embedding)
        SELECT new.id, new.description_embedding WHERE new.description_embedding IS NOT NULL;
END;
CREATE TRIGGER IF NOT EXISTS repos_vec_ad AFTER DELETE ON repos
BEGIN
    DELETE FROM repos_vec WHERE rowid = old.id;
END;
";

/// Migration v8: merge jira into atlassian — one Atlassian account covers all products.
pub const MIGRATION_V8: &str = "
DELETE FROM identifier_types WHERE name = 'jira';
UPDATE identifier_types
    SET description = 'Atlassian account ID or email (covers Jira, Confluence, Bitbucket)'
    WHERE name = 'atlassian';
";

/// Migration v2: symbol_edges for call graph (callers, callees, graph traversal).
pub const REPO_MIGRATION_V2: &str = "
CREATE TABLE IF NOT EXISTS symbol_edges (
    id INTEGER PRIMARY KEY,
    from_symbol_id INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
    to_name TEXT NOT NULL,
    kind TEXT NOT NULL DEFAULT 'calls',
    line INTEGER
);
CREATE INDEX IF NOT EXISTS idx_edges_from ON symbol_edges(from_symbol_id);
CREATE INDEX IF NOT EXISTS idx_edges_to_name ON symbol_edges(to_name);
";

/// Migration v3: confidence tag on symbol_edges.
/// extracted = seen directly in source via tree-sitter AST node.
/// inferred  = derived through analysis (type inference, dynamic dispatch, etc.).
pub const REPO_MIGRATION_V3: &str =
    "ALTER TABLE symbol_edges ADD COLUMN confidence TEXT NOT NULL DEFAULT 'extracted';";

/// Migration v4: sqlite-vec KNN index over code chunk embeddings.
///
/// `chunks.embedding` (LE-f32 BLOB) stays the source of truth; `chunks_vec` is
/// a vec0 index keyed by chunk id, kept in sync by triggers, and backfilled
/// from existing embeddings. Requires the sqlite-vec extension to be registered
/// on the connection (RepoDb::open does this before migrating). 384 dims =
/// AllMiniLM-L6-v2; cosine distance to match the previous cosine_similarity.
pub const REPO_MIGRATION_V4: &str = "
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_vec USING vec0(
    embedding float[384] distance_metric=cosine
);

INSERT INTO chunks_vec(rowid, embedding)
    SELECT id, embedding FROM chunks WHERE embedding IS NOT NULL;

CREATE TRIGGER IF NOT EXISTS chunks_vec_ai AFTER INSERT ON chunks
    WHEN new.embedding IS NOT NULL
BEGIN
    INSERT INTO chunks_vec(rowid, embedding) VALUES (new.id, new.embedding);
END;

CREATE TRIGGER IF NOT EXISTS chunks_vec_au AFTER UPDATE OF embedding ON chunks
BEGIN
    DELETE FROM chunks_vec WHERE rowid = old.id;
    INSERT INTO chunks_vec(rowid, embedding)
        SELECT new.id, new.embedding WHERE new.embedding IS NOT NULL;
END;

CREATE TRIGGER IF NOT EXISTS chunks_vec_ad AFTER DELETE ON chunks
BEGIN
    DELETE FROM chunks_vec WHERE rowid = old.id;
END;
";

pub const REPO_SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;

CREATE TABLE IF NOT EXISTS repo_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    language TEXT,
    content_hash TEXT NOT NULL,
    last_modified TEXT
);

CREATE TABLE IF NOT EXISTS symbols (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    signature TEXT,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS symbols_fts USING fts5(
    name, kind, signature,
    content=symbols, content_rowid=id
);
CREATE TRIGGER IF NOT EXISTS symbols_ai AFTER INSERT ON symbols BEGIN
    INSERT INTO symbols_fts(rowid, name, kind, signature)
    VALUES (new.id, new.name, new.kind, new.signature);
END;
CREATE TRIGGER IF NOT EXISTS symbols_au AFTER UPDATE ON symbols BEGIN
    INSERT INTO symbols_fts(symbols_fts, rowid, name, kind, signature)
    VALUES ('delete', old.id, old.name, old.kind, old.signature);
    INSERT INTO symbols_fts(rowid, name, kind, signature)
    VALUES (new.id, new.name, new.kind, new.signature);
END;
CREATE TRIGGER IF NOT EXISTS symbols_ad AFTER DELETE ON symbols BEGIN
    INSERT INTO symbols_fts(symbols_fts, rowid, name, kind, signature)
    VALUES ('delete', old.id, old.name, old.kind, old.signature);
END;

CREATE TABLE IF NOT EXISTS chunks (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    chunk_index INTEGER NOT NULL,
    chunk_text TEXT NOT NULL,
    start_line INTEGER,
    end_line INTEGER,
    embedding BLOB
);

-- sqlite-vec KNN index over chunk embeddings (chunks.embedding stays source of
-- truth; triggers keep this in sync). Requires the sqlite-vec extension to be
-- registered on the connection before this schema runs (RepoDb::open does so).
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_vec USING vec0(
    embedding float[384] distance_metric=cosine
);
CREATE TRIGGER IF NOT EXISTS chunks_vec_ai AFTER INSERT ON chunks
    WHEN new.embedding IS NOT NULL
BEGIN
    INSERT INTO chunks_vec(rowid, embedding) VALUES (new.id, new.embedding);
END;
CREATE TRIGGER IF NOT EXISTS chunks_vec_au AFTER UPDATE OF embedding ON chunks
BEGIN
    DELETE FROM chunks_vec WHERE rowid = old.id;
    INSERT INTO chunks_vec(rowid, embedding)
        SELECT new.id, new.embedding WHERE new.embedding IS NOT NULL;
END;
CREATE TRIGGER IF NOT EXISTS chunks_vec_ad AFTER DELETE ON chunks
BEGIN
    DELETE FROM chunks_vec WHERE rowid = old.id;
END;

-- Call graph edges: records calls, inherits, implements relationships between symbols.
-- from_symbol_id is the caller/child; to_name is the callee/parent as written in source.
-- confidence: 'extracted' = seen directly in source AST; 'inferred' = derived by analysis.
-- Deleting a symbol cascades to delete its outbound edges.
CREATE TABLE IF NOT EXISTS symbol_edges (
    id INTEGER PRIMARY KEY,
    from_symbol_id INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
    to_name TEXT NOT NULL,
    kind TEXT NOT NULL DEFAULT 'calls',
    line INTEGER,
    confidence TEXT NOT NULL DEFAULT 'extracted'
);
CREATE INDEX IF NOT EXISTS idx_edges_from ON symbol_edges(from_symbol_id);
CREATE INDEX IF NOT EXISTS idx_edges_to_name ON symbol_edges(to_name);
";
