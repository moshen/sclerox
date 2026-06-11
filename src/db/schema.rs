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

-- People: with discrete columns for all known identity links
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
";
