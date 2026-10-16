use anyhow::{Context, Result};
use clap::Subcommand;
use std::fs;
use std::path::{Path, PathBuf};

use crate::db::Database;

#[derive(Subcommand)]
pub enum DbCommand {
    /// Show database schema version, pending migrations, and embedding model.
    Migrate {
        /// Also show migration status for all indexed repos.
        #[arg(long)]
        repos: bool,
    },

    /// Export the knowledge base as plaintext markdown.
    ///
    /// Writes one file per record under <dir>/memory, /people, /projects,
    /// /meetings, /todos, /research, /repos. Files use YAML frontmatter
    /// and Obsidian-style [[wikilinks]] for cross-references.
    ///
    /// Suitable for: git-tracked vaults (Obsidian, Logseq), feeding other
    /// tools, backup, or sharing with teammates.
    ///
    /// The export rewrites owned subdirs on every run (idempotent). Files
    /// at the vault root (.git, README, user notes) are left untouched.
    Export {
        /// Target directory (created if missing).
        dir: PathBuf,
    },

    /// Re-embed stored vectors with the configured embedding model.
    ///
    /// Run after changing `embed.model`, or after upgrading from a version
    /// that used MiniLM. Until then semantic search and dedup are off for any
    /// database embedded with another model (keyword search still works).
    ///
    /// Covers the main database (memories, meetings, todos, research, repo
    /// descriptions) and every indexed repo's code chunks, embedding the stored
    /// text in place - nothing is re-parsed. Each database switches
    /// all-or-nothing: an interrupted run leaves it on the old model, and
    /// running again starts it over.
    ///
    /// Meeting, todo and research text is first re-cut into chunks when it was
    /// cut at other `embed.chunk_size` / `chunk_overlap` values (always with
    /// --force).
    ///
    /// Also embeds main-database items saved without a vector (while the
    /// model was unavailable, or with --no-embed). Code chunks indexed without
    /// vectors are left to `sclerox repo reembed` and the session hooks'
    /// background embed.
    ///
    /// Before switching the main database it waits (up to 5 minutes) for any
    /// running session distillation, which might be from the sclerox you just
    /// upgraded from and would write old-model vectors after the switch.
    Reembed {
        /// Re-embed databases already on the configured model too.
        #[arg(long)]
        force: bool,
    },
}

pub fn run(db: &Database, cmd: DbCommand) -> Result<()> {
    match cmd {
        DbCommand::Migrate { repos } => migrate(db, repos),
        DbCommand::Export { dir } => export(db, &dir),
        DbCommand::Reembed { force } => reembed(db, force),
    }
}

// ---------- reembed ----------

/// Whether `reembed` has anything to do: a database (main or repo) holding
/// vectors from another model, or main-database rows left without a vector.
pub(crate) fn embeddings_need_work(db: &Database) -> bool {
    use crate::embed::models::ModelCheck;
    use crate::index::repo_db::RepoDb;

    let mismatched = |c: Result<ModelCheck>| matches!(c, Ok(ModelCheck::Mismatch { .. }));
    if mismatched(db.embedding_model_check())
        || db.missing_vectors().unwrap_or(0) > 0
        || super::text_chunks::stale(db)
    {
        return true;
    }
    db.repo_list().unwrap_or_default().iter().any(|repo| {
        let path = PathBuf::from(&repo.db_path);
        path.exists() && RepoDb::open(&path).is_ok_and(|r| mismatched(r.embedding_model_check()))
    })
}

/// Re-embed every database recorded under another model (all of them with
/// `force`), and embed main-database rows left without a vector. Shared by
/// `sclerox db reembed` and `sclerox install`.
pub(crate) fn reembed(db: &Database, force: bool) -> Result<()> {
    use crate::index::repo_db::RepoDb;

    let mut embedder = crate::embed::required()?;
    let model = embedder.model();
    println!("Embedding model: {} ({})", model.name, model.id);

    let mut rebuilt = 0usize;
    let mut failed = 0usize;

    let main_state = db.embedding_model_check()?;
    println!("\nMain database:");
    let mut main_ok = true;
    let mut main_done = false;
    // Re-cut text chunks first. On the configured model the new chunks are
    // embedded before they are swapped in, and the rebuild below leaves them
    // alone; on another model they go in without vectors and the rebuild
    // embeds them. Either way each is embedded once.
    let mut text_chunks_embedded = false;
    if force || super::text_chunks::stale(db) {
        let mismatched = matches!(
            main_state,
            crate::embed::models::ModelCheck::Mismatch { .. }
        );
        let rechunked = if mismatched {
            super::text_chunks::rechunk_all(db, None, &mut |_, _, _| {})
        } else {
            let mut out = None;
            rebuild_one(&mut embedder, |_, embed, progress| {
                let r = super::text_chunks::rechunk_all(db, Some(embed), progress)?;
                let n = r.chunks;
                out = Some(r);
                Ok(n)
            })
            .and_then(|_| out.ok_or_else(|| anyhow::anyhow!("re-chunk returned nothing")))
        };
        match rechunked {
            Ok(r) => {
                let cfg = &crate::config::settings().embed;
                println!(
                    "  re-chunked {} meeting(s), {} todo(s) and {} research item(s) \
                     at chunk_size {}, overlap {}",
                    r.meetings, r.todos, r.research, cfg.chunk_size, cfg.chunk_overlap
                );
                main_done = true;
                text_chunks_embedded = !mismatched;
            }
            Err(e) => {
                println!("  could not re-chunk text (run again to retry): {e:#}");
                main_ok = false;
            }
        }
    }
    // A failed re-chunk leaves the database as it was; don't switch its model
    // on top of that.
    let rebuild_main = main_ok && needs_rebuild(&main_state, force);
    if rebuild_main && !wait_for_distillations() {
        println!(
            "  skipped: a session distillation is still running and could write \
             vectors from the old model after the switch; run `sclerox db reembed` \
             once it finishes"
        );
        main_ok = false;
    } else if rebuild_main {
        let skip: &[&str] = if text_chunks_embedded {
            &crate::db::TEXT_CHUNK_TABLES
        } else {
            &[]
        };
        match rebuild_one(&mut embedder, |id, embed, progress| {
            db.reembed_all_except(id, skip, embed, progress)
        }) {
            Ok(n) => {
                println!("  re-embedded: {n} vectors");
                main_done = true;
            }
            Err(e) => {
                println!("  failed, left on its previous model: {e:#}");
                main_ok = false;
            }
        }
    }
    // Then any rows without a vector: saved while the model was unavailable
    // or the database mismatched, with --no-embed, or during the rebuild.
    let missing = if main_ok { db.missing_vectors()? } else { 0 };
    if missing > 0 {
        println!("  {missing} item(s) without a vector:");
        match rebuild_one(&mut embedder, |id, embed, progress| {
            db.fill_missing_vectors(id, embed, progress)
        }) {
            Ok(n) => {
                println!("  filled: {n} vectors");
                main_done = true;
            }
            Err(e) => {
                println!("  could not embed them (run again to retry): {e:#}");
                main_ok = false;
            }
        }
    }
    match (main_ok, main_done) {
        (false, _) => failed += 1,
        (true, true) => rebuilt += 1,
        (true, false) => println!("  {}", describe_skip(&main_state)),
    }

    let pruned = db.repo_prune_gone()?;
    let repos = db.repo_list()?;
    if !repos.is_empty() || !pruned.is_empty() {
        println!("\nRepo indexes:");
    }
    for repo in &pruned {
        println!("  {}: removed stale entry (directory gone)", repo.name);
    }
    for repo in &repos {
        let db_path = PathBuf::from(&repo.db_path);
        if !db_path.exists() {
            println!("  {}: index missing ({})", repo.name, repo.db_path);
            continue;
        }
        let rdb = match RepoDb::open(&db_path) {
            Ok(r) => r,
            Err(e) => {
                println!("  {}: could not open ({e})", repo.name);
                failed += 1;
                continue;
            }
        };
        let state = match rdb.embedding_model_check() {
            Ok(s) => s,
            Err(e) => {
                println!(
                    "  {}: could not read its embedding model ({e:#})",
                    repo.name
                );
                failed += 1;
                continue;
            }
        };
        if !needs_rebuild(&state, force) {
            println!("  {}: {}", repo.name, describe_skip(&state));
            continue;
        }
        println!("  {}:", repo.name);
        match rebuild_one(&mut embedder, |id, embed, progress| {
            rdb.reembed_all(id, embed, progress)
        }) {
            Ok(n) => {
                println!("    done: {n} vectors");
                rebuilt += 1;
            }
            Err(e) => {
                println!("    failed, left on its previous model: {e:#}");
                failed += 1;
            }
        }
    }

    println!();
    match (rebuilt, failed) {
        (0, 0) => println!("Nothing to re-embed."),
        (n, 0) => println!("Updated {n} database(s) with {}.", model.name),
        (n, f) => {
            println!("Updated {n} database(s); {f} failed. Run again to retry them.");
            anyhow::bail!("{f} database(s) could not be re-embedded");
        }
    }
    Ok(())
}

/// How long a rebuild of the main database waits for session distillations.
const DISTILL_WAIT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Wait (up to [`DISTILL_WAIT`]) until no session distillation is running.
/// A distiller started by the sclerox from before an upgrade has no model
/// checks and would write old-model vectors into the rebuilt database, where
/// nothing could tell them apart. Ones started since refuse to, but there is
/// no telling which is which, so wait for all of them. False on timeout.
fn wait_for_distillations() -> bool {
    let started = std::time::Instant::now();
    let mut told = false;
    loop {
        let running = super::hook::distillations_in_progress();
        if running == 0 {
            return true;
        }
        if started.elapsed() >= DISTILL_WAIT {
            return false;
        }
        if !told {
            println!(
                "  waiting for {running} session distillation(s) to finish first \
                 (up to {} min)...",
                DISTILL_WAIT.as_secs() / 60
            );
            told = true;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

/// A mismatch always needs a rebuild. A database with nothing recorded has
/// no vectors to convert; `--force` redoes even a matching one.
fn needs_rebuild(state: &crate::embed::models::ModelCheck, force: bool) -> bool {
    use crate::embed::models::ModelCheck;
    match state {
        ModelCheck::Mismatch { .. } => true,
        ModelCheck::Match => force,
        ModelCheck::NotRecorded => false,
    }
}

fn describe_skip(state: &crate::embed::models::ModelCheck) -> &'static str {
    use crate::embed::models::ModelCheck;
    match state {
        ModelCheck::Match => "already on this model",
        ModelCheck::NotRecorded => "no embeddings yet",
        ModelCheck::Mismatch { .. } => "needs re-embedding",
    }
}

/// Run one database's rebuild with `embedder`, printing per-table progress.
fn rebuild_one(
    embedder: &mut crate::embed::Embedder,
    run: impl FnOnce(
        &str,
        &mut crate::db::reembed::EmbedFn<'_>,
        &mut crate::db::reembed::ProgressFn<'_>,
    ) -> Result<usize>,
) -> Result<usize> {
    use std::io::{IsTerminal, Write};

    let model_id = embedder.model().id;
    let interactive = std::io::stdout().is_terminal();
    let started = std::time::Instant::now();
    let mut embed = |texts: &[&str]| embedder.embed_each(texts);
    let mut progress = |label: &str, done: usize, total: usize| {
        if interactive {
            let rate = done as f64 / started.elapsed().as_secs_f64().max(0.001);
            print!("\r    {label}: {done}/{total} ({rate:.0}/s)   ");
            if done == total {
                println!();
            }
            let _ = std::io::stdout().flush();
        } else if done == total {
            println!("    {label}: {total}");
        }
    };
    run(model_id, &mut embed, &mut progress)
}

// ---------- migrate ----------

fn migrate(db: &Database, show_repos: bool) -> Result<()> {
    let (current, target, pending) = db.migration_status()?;

    println!("Primary database (~/.local/share/sclerox/sclerox.db)");
    println!("  schema version : {current}");
    println!("  binary expects : {target}");
    if pending == 0 {
        println!("  status         : up to date");
    } else {
        println!("  status         : {pending} migration(s) will apply on next open");
    }
    println!(
        "  embeddings     : {}",
        describe_model(db.embedding_model().ok().flatten())
    );

    if show_repos {
        use crate::index::repo_db::RepoDb;

        let repos = db.repo_list()?;
        if repos.is_empty() {
            println!("\nNo repos indexed.");
        } else {
            println!("\nRepo databases:");
            for repo in &repos {
                let db_path = PathBuf::from(&repo.db_path);
                if !db_path.exists() {
                    println!("  {} - db missing ({})", repo.name, repo.db_path);
                    continue;
                }
                match RepoDb::open(&db_path) {
                    Ok(rdb) => match rdb.migration_status() {
                        Ok((cur, tgt, pend)) => {
                            let status = if pend == 0 {
                                "up to date".to_string()
                            } else {
                                format!("{pend} pending")
                            };
                            let model = describe_model(rdb.embedding_model().ok().flatten());
                            println!(
                                "  {} v{cur}/{tgt} ({status}; embeddings: {model})",
                                repo.name
                            );
                        }
                        Err(e) => println!("  {} - error: {e}", repo.name),
                    },
                    Err(e) => println!("  {} - could not open: {e}", repo.name),
                }
            }
        }
    }

    Ok(())
}

/// The recorded embedding model, and whether it matches the configured one.
fn describe_model(recorded: Option<String>) -> String {
    use crate::embed::models::{check, display_id, ModelCheck};
    match check(recorded.as_deref()) {
        ModelCheck::NotRecorded => "none yet".to_string(),
        ModelCheck::Match => format!(
            "{} (current)",
            display_id(recorded.as_deref().unwrap_or(""))
        ),
        ModelCheck::Mismatch { recorded } => format!(
            "{} - run `sclerox db reembed` to switch to {}",
            display_id(&recorded),
            display_id(crate::embed::models::configured_id())
        ),
    }
}

// ---------- export ----------

#[derive(Default)]
struct ExportStats {
    memory: usize,
    people: usize,
    projects: usize,
    meetings: usize,
    todos: usize,
    research: usize,
    repos: usize,
}

const OWNED_SUBDIRS: &[&str] = &[
    "memory", "people", "projects", "meetings", "todos", "research", "repos",
];

fn export(db: &Database, dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating export dir {}", dir.display()))?;

    // Wipe owned subdirs so deleted records vanish on re-export.
    for sub in OWNED_SUBDIRS {
        let path = dir.join(sub);
        if path.exists() {
            fs::remove_dir_all(&path).with_context(|| format!("clearing {}", path.display()))?;
        }
    }

    let mut stats = ExportStats::default();
    export_people(db, dir, &mut stats)?;
    export_projects(db, dir, &mut stats)?;
    export_memory(db, dir, &mut stats)?;
    export_meetings(db, dir, &mut stats)?;
    export_todos(db, dir, &mut stats)?;
    export_research(db, dir, &mut stats)?;
    export_repos(db, dir, &mut stats)?;
    write_index(dir, &stats)?;

    println!("Exported to {}", dir.display());
    println!(
        "  {} memory, {} people, {} projects, {} meetings, {} todos, {} research, {} repos",
        stats.memory,
        stats.people,
        stats.projects,
        stats.meetings,
        stats.todos,
        stats.research,
        stats.repos,
    );
    Ok(())
}

fn export_people(db: &Database, dir: &Path, stats: &mut ExportStats) -> Result<()> {
    let people = db.people_list()?;
    if people.is_empty() {
        return Ok(());
    }
    let out_dir = dir.join("people");
    fs::create_dir_all(&out_dir)?;
    for p in &people {
        let slug = slugify(&p.name);
        let idents = db.people_identifiers_for(p.id).unwrap_or_default();

        let mut fm = Frontmatter::new();
        fm.kv("id", &p.id.to_string());
        fm.kv("name", &p.name);
        for i in &idents {
            fm.kv(&i.identifier_type, &i.identifier);
        }
        fm.kv("created", &p.created_at);
        fm.kv("updated", &p.updated_at);

        let mut body = format!("# {}\n\n", p.name);
        if let Some(notes) = &p.notes {
            body.push_str(notes);
            body.push_str("\n\n");
        }
        if !idents.is_empty() {
            body.push_str("## Identifiers\n\n");
            for i in &idents {
                body.push_str(&format!("- **{}**: {}\n", i.identifier_type, i.identifier));
            }
            body.push('\n');
        }

        write_md(&out_dir.join(format!("{slug}.md")), &fm.render(), &body)?;
        stats.people += 1;
    }
    Ok(())
}

fn export_projects(db: &Database, dir: &Path, stats: &mut ExportStats) -> Result<()> {
    let projects = db.project_list()?;
    if projects.is_empty() {
        return Ok(());
    }
    let out_dir = dir.join("projects");
    fs::create_dir_all(&out_dir)?;
    for proj in &projects {
        let slug = slugify(&proj.name);
        let mut fm = Frontmatter::new();
        fm.kv("id", &proj.id.to_string());
        fm.kv("name", &proj.name);
        fm.kv("created", &proj.created_at);
        fm.kv("updated", &proj.updated_at);

        let project_people = db.project_people(proj.id).unwrap_or_default();
        if !project_people.is_empty() {
            let slugs: Vec<String> = project_people
                .iter()
                .map(|p| slugify(&p.person_name))
                .collect();
            fm.list("people", &slugs);
        }
        let meetings = db.project_meetings_list(proj.id).unwrap_or_default();
        if !meetings.is_empty() {
            let slugs: Vec<String> = meetings.iter().map(meeting_slug).collect();
            fm.list("meetings", &slugs);
        }
        let repos = db.project_repos_list(proj.id).unwrap_or_default();
        if !repos.is_empty() {
            let slugs: Vec<String> = repos.iter().map(|r| slugify(&r.name)).collect();
            fm.list("repos", &slugs);
        }

        let mut body = format!("# {}\n\n", proj.name);
        if let Some(desc) = &proj.description {
            body.push_str(desc);
            body.push_str("\n\n");
        }
        if !proj.links.is_empty() {
            body.push_str("## Links\n\n");
            for link in &proj.links {
                let label = link.label.as_deref().unwrap_or(&link.url);
                body.push_str(&format!("- [{label}]({})\n", link.url));
            }
            body.push('\n');
        }
        if !project_people.is_empty() {
            body.push_str("## People\n\n");
            for p in &project_people {
                let s = slugify(&p.person_name);
                let role = p
                    .role
                    .as_deref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default();
                body.push_str(&format!("- [[people/{s}|{}]]{role}\n", p.person_name));
            }
            body.push('\n');
        }
        if !repos.is_empty() {
            body.push_str("## Repos\n\n");
            for r in &repos {
                let s = slugify(&r.name);
                body.push_str(&format!("- [[repos/{s}|{}]]\n", r.name));
            }
        }

        write_md(&out_dir.join(format!("{slug}.md")), &fm.render(), &body)?;
        stats.projects += 1;
    }
    Ok(())
}

fn export_memory(db: &Database, dir: &Path, stats: &mut ExportStats) -> Result<()> {
    let entries = db.memory_list(None, Some("all"))?;
    if entries.is_empty() {
        return Ok(());
    }
    let out_dir = dir.join("memory");
    fs::create_dir_all(&out_dir)?;
    for m in &entries {
        let safe_key = sanitize_key_path(&m.key);
        let file_path = out_dir.join(format!("{safe_key}.md"));
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut fm = Frontmatter::new();
        fm.kv("id", &m.id.to_string());
        fm.kv("key", &m.key);
        fm.kv("type", &m.memory_type);
        fm.kv("status", &m.status);
        fm.kv("source", &m.source);
        if let Some(sb) = m.superseded_by {
            fm.kv("superseded_by", &sb.to_string());
        }
        if let Some(r) = &m.reviewed_at {
            fm.kv("reviewed", r);
        }
        if let Some(tags) = &m.tags {
            if !tags.is_empty() {
                fm.list("tags", tags);
            }
        }
        fm.kv("created", &m.created_at);
        fm.kv("updated", &m.updated_at);

        let linked = db.memory_people(&m.key).unwrap_or_default();
        if !linked.is_empty() {
            let slugs: Vec<String> = linked.iter().map(|p| slugify(&p.name)).collect();
            fm.list("people", &slugs);
        }

        let mut body = format!("# {}\n\n{}\n", m.key, m.value);
        if !linked.is_empty() {
            body.push_str("\n## People\n\n");
            for p in &linked {
                let s = slugify(&p.name);
                body.push_str(&format!("- [[people/{s}|{}]]\n", p.name));
            }
        }

        write_md(&file_path, &fm.render(), &body)?;
        stats.memory += 1;
    }
    Ok(())
}

fn export_meetings(db: &Database, dir: &Path, stats: &mut ExportStats) -> Result<()> {
    let meetings = db.meeting_list(None, None)?;
    if meetings.is_empty() {
        return Ok(());
    }
    let out_dir = dir.join("meetings");
    fs::create_dir_all(&out_dir)?;
    for m in &meetings {
        let slug = meeting_slug(m);
        let mut fm = Frontmatter::new();
        fm.kv("id", &m.id.to_string());
        fm.kv("title", &m.title);
        if let Some(d) = &m.meeting_date {
            fm.kv("date", d);
        }
        fm.kv("created", &m.created_at);

        let people = db.meeting_people(m.id).unwrap_or_default();
        if !people.is_empty() {
            let slugs: Vec<String> = people.iter().map(|p| slugify(&p.person_name)).collect();
            fm.list("people", &slugs);
        }

        let mut body = format!("# {}\n\n", m.title);
        if let Some(d) = &m.meeting_date {
            body.push_str(&format!("**Date:** {d}\n\n"));
        }
        if !people.is_empty() {
            body.push_str("## Attendees\n\n");
            for p in &people {
                let s = slugify(&p.person_name);
                let role = p
                    .role
                    .as_deref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default();
                body.push_str(&format!("- [[people/{s}|{}]]{role}\n", p.person_name));
            }
            body.push('\n');
        }
        if let Some(notes) = &m.notes {
            body.push_str("## Notes\n\n");
            body.push_str(notes);
            body.push_str("\n\n");
        }
        if let Some(transcript) = &m.transcript {
            body.push_str("## Transcript\n\n");
            body.push_str(transcript);
            body.push('\n');
        }

        write_md(&out_dir.join(format!("{slug}.md")), &fm.render(), &body)?;
        stats.meetings += 1;
    }
    Ok(())
}

fn export_todos(db: &Database, dir: &Path, stats: &mut ExportStats) -> Result<()> {
    let todos = db.todo_list(None)?;
    if todos.is_empty() {
        return Ok(());
    }
    let base = dir.join("todos");
    fs::create_dir_all(base.join("open"))?;
    fs::create_dir_all(base.join("done"))?;

    for t in &todos {
        let bucket = if t.status == "done" { "done" } else { "open" };
        let slug = slugify(&t.title);
        let mut fm = Frontmatter::new();
        fm.kv("id", &t.id.to_string());
        fm.kv("title", &t.title);
        fm.kv("status", &t.status);
        fm.kv("category", &t.category);
        fm.kv("originated", &t.originated_date);
        if let Some(d) = &t.deadline_date {
            fm.kv("deadline", d);
        }
        if let Some(c) = &t.completed_at {
            fm.kv("completed", c);
        }
        if let Some(url) = &t.source_url {
            fm.kv("source_url", url);
        }
        fm.kv("created", &t.created_at);
        fm.kv("updated", &t.updated_at);

        let people = db.todo_people(t.id).unwrap_or_default();
        if !people.is_empty() {
            let slugs: Vec<String> = people.iter().map(|p| slugify(&p.name)).collect();
            fm.list("people", &slugs);
        }
        let projects = db.todo_projects(t.id).unwrap_or_default();
        if !projects.is_empty() {
            let slugs: Vec<String> = projects.iter().map(|p| slugify(&p.name)).collect();
            fm.list("projects", &slugs);
        }

        let mut body = format!("# {}\n\n", t.title);
        if let Some(n) = &t.notes {
            body.push_str(n);
            body.push_str("\n\n");
        }
        if !projects.is_empty() {
            body.push_str("## Projects\n\n");
            for p in &projects {
                let s = slugify(&p.name);
                body.push_str(&format!("- [[projects/{s}|{}]]\n", p.name));
            }
            body.push('\n');
        }
        if !people.is_empty() {
            body.push_str("## People\n\n");
            for p in &people {
                let s = slugify(&p.name);
                body.push_str(&format!("- [[people/{s}|{}]]\n", p.name));
            }
        }

        let file = base.join(bucket).join(format!("{}-{slug}.md", t.id));
        write_md(&file, &fm.render(), &body)?;
        stats.todos += 1;
    }
    Ok(())
}

fn export_research(db: &Database, dir: &Path, stats: &mut ExportStats) -> Result<()> {
    let invs = db.investigation_list(None)?;
    if invs.is_empty() {
        return Ok(());
    }
    let out_dir = dir.join("research");
    fs::create_dir_all(&out_dir)?;
    for inv in &invs {
        let mut fm = Frontmatter::new();
        fm.kv("id", &inv.id.to_string());
        fm.kv("name", &inv.name);
        fm.kv("slug", &inv.slug);
        fm.kv("status", &inv.status);
        fm.kv("created", &inv.created_at);
        if let Some(c) = &inv.concluded_at {
            fm.kv("concluded", c);
        }
        fm.kv("updated", &inv.updated_at);

        let people = db.investigation_people(inv.id).unwrap_or_default();
        if !people.is_empty() {
            let slugs: Vec<String> = people.iter().map(|p| slugify(&p.name)).collect();
            fm.list("people", &slugs);
        }
        let projects = db.investigation_projects(inv.id).unwrap_or_default();
        if !projects.is_empty() {
            let slugs: Vec<String> = projects.iter().map(|p| slugify(&p.name)).collect();
            fm.list("projects", &slugs);
        }

        let mut body = format!("# {}\n\n**Status:** {}\n\n", inv.name, inv.status);
        if let Some(plan) = &inv.plan {
            body.push_str("## Plan\n\n");
            body.push_str(plan);
            body.push_str("\n\n");
        }
        if let Some(findings) = &inv.findings {
            body.push_str("## Findings\n\n");
            body.push_str(findings);
            body.push_str("\n\n");
        }
        let sources = db.investigation_sources(inv.id).unwrap_or_default();
        if !sources.is_empty() {
            body.push_str("## Sources\n\n");
            for s in &sources {
                let label = s.label.as_deref().unwrap_or("source");
                body.push_str(&format!("- [{label}]({})\n", s.url));
                if let Some(n) = &s.notes {
                    body.push_str(&format!("  - {n}\n"));
                }
            }
            body.push('\n');
        }
        if !projects.is_empty() {
            body.push_str("## Projects\n\n");
            for p in &projects {
                let s = slugify(&p.name);
                body.push_str(&format!("- [[projects/{s}|{}]]\n", p.name));
            }
            body.push('\n');
        }
        if !people.is_empty() {
            body.push_str("## People\n\n");
            for p in &people {
                let s = slugify(&p.name);
                body.push_str(&format!("- [[people/{s}|{}]]\n", p.name));
            }
        }

        write_md(
            &out_dir.join(format!("{}.md", inv.slug)),
            &fm.render(),
            &body,
        )?;
        stats.research += 1;
    }
    Ok(())
}

fn export_repos(db: &Database, dir: &Path, stats: &mut ExportStats) -> Result<()> {
    let repos = db.repo_list()?;
    if repos.is_empty() {
        return Ok(());
    }
    let out_dir = dir.join("repos");
    fs::create_dir_all(&out_dir)?;
    for r in &repos {
        let slug = slugify(&r.name);
        let mut fm = Frontmatter::new();
        fm.kv("id", &r.id.to_string());
        fm.kv("name", &r.name);
        fm.kv("path", &r.path);
        if let Some(li) = &r.last_indexed {
            fm.kv("last_indexed", li);
        }
        fm.kv("created", &r.created_at);

        let mut body = format!("# {}\n\n", r.name);
        if let Some(d) = &r.description {
            body.push_str(d);
            body.push_str("\n\n");
        }
        body.push_str(&format!("**Path:** `{}`\n", r.path));

        write_md(&out_dir.join(format!("{slug}.md")), &fm.render(), &body)?;
        stats.repos += 1;
    }
    Ok(())
}

fn write_index(dir: &Path, stats: &ExportStats) -> Result<()> {
    let body = format!(
        "# sclerox export\n\n\
         Plaintext export of the sclerox knowledge base. Regenerated by `sclerox db export`.\n\n\
         ## Counts\n\n\
         - {} memory entries\n\
         - {} people\n\
         - {} projects\n\
         - {} meetings\n\
         - {} todos\n\
         - {} research investigations\n\
         - {} repos\n",
        stats.memory,
        stats.people,
        stats.projects,
        stats.meetings,
        stats.todos,
        stats.research,
        stats.repos,
    );
    fs::write(dir.join("README.md"), body)?;
    Ok(())
}

// ---------- helpers ----------

struct Frontmatter {
    lines: Vec<String>,
}

impl Frontmatter {
    fn new() -> Self {
        Self { lines: Vec::new() }
    }
    fn kv(&mut self, key: &str, value: &str) {
        self.lines.push(format!("{key}: {}", yaml_quote(value)));
    }
    fn list(&mut self, key: &str, items: &[String]) {
        self.lines.push(format!("{key}:"));
        for item in items {
            self.lines.push(format!("  - {}", yaml_quote(item)));
        }
    }
    fn render(&self) -> String {
        if self.lines.is_empty() {
            return String::new();
        }
        let mut s = String::from("---\n");
        for line in &self.lines {
            s.push_str(line);
            s.push('\n');
        }
        s.push_str("---\n\n");
        s
    }
}

fn yaml_quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn write_md(path: &Path, frontmatter: &str, body: &str) -> Result<()> {
    let mut content = String::with_capacity(frontmatter.len() + body.len());
    content.push_str(frontmatter);
    content.push_str(body);
    if !content.ends_with('\n') {
        content.push('\n');
    }
    fs::write(path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_was_hyphen = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_hyphen = false;
        } else if !last_was_hyphen {
            out.push('-');
            last_was_hyphen = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "unnamed".to_string()
    } else {
        trimmed
    }
}

fn sanitize_key_path(key: &str) -> String {
    key.split('/')
        .map(slugify)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

fn meeting_slug(m: &crate::db::meetings::Meeting) -> String {
    let date = m.meeting_date.as_deref().unwrap_or("undated");
    format!("{date}-{}", slugify(&m.title))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_slugify_basic() {
        assert_eq!(slugify("Hello World"), "hello-world");
        assert_eq!(slugify("Jane Doe"), "jane-doe");
        assert_eq!(slugify("multi   space"), "multi-space");
        assert_eq!(slugify("Foo/Bar:Baz"), "foo-bar-baz");
        assert_eq!(slugify("---trim---"), "trim");
        assert_eq!(slugify(""), "unnamed");
    }

    #[test]
    fn test_yaml_quote_escapes() {
        assert_eq!(yaml_quote("hi"), "\"hi\"");
        assert_eq!(yaml_quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(yaml_quote("path\\to"), "\"path\\\\to\"");
    }

    #[test]
    fn test_sanitize_key_path_preserves_namespaces() {
        assert_eq!(
            sanitize_key_path("research/graphify-rewrite/finding"),
            "research/graphify-rewrite/finding"
        );
        assert_eq!(
            sanitize_key_path("session/2026-06-12/foo-bar"),
            "session/2026-06-12/foo-bar"
        );
    }

    #[test]
    fn test_frontmatter_render() {
        let mut fm = Frontmatter::new();
        fm.kv("name", "Alice");
        fm.list("tags", &["rust".to_string(), "cli".to_string()]);
        let rendered = fm.render();
        assert!(rendered.starts_with("---\n"));
        assert!(rendered.contains("name: \"Alice\""));
        assert!(rendered.contains("tags:\n  - \"rust\""));
        assert!(rendered.ends_with("---\n\n"));
    }

    #[test]
    fn test_export_writes_owned_subdirs_only() {
        let dir = TempDir::new().unwrap();
        let db = Database::open_in_memory().unwrap();

        let user_file = dir.path().join("my-notes.md");
        fs::write(&user_file, "user content").unwrap();

        let pid = db.people_add("Jane Doe", None).unwrap();
        db.people_identifier_set(pid, "email", "c@x").unwrap();
        let _mid = db
            .memory_set("project/active", "sclerox-cli", "project", None)
            .unwrap();
        let _ = db.todo_add(
            "fix bug",
            None,
            crate::db::todos::TodoStatus::Open,
            None,
            "general",
            None,
            None,
        );

        export(&db, dir.path()).unwrap();

        assert!(dir.path().join("people").exists());
        assert!(dir.path().join("memory").exists());
        assert!(dir.path().join("todos").exists());
        assert!(user_file.exists());
        assert!(dir.path().join("people/jane-doe.md").exists());
        assert!(dir.path().join("memory/project/active.md").exists());

        // Re-run after deleting the person - their file should vanish.
        let _ = db.people_delete(pid);
        export(&db, dir.path()).unwrap();
        assert!(!dir.path().join("people/jane-doe.md").exists());
        assert!(user_file.exists(), "user file must survive re-export");
    }

    #[test]
    fn test_export_memory_key_as_path() {
        let dir = TempDir::new().unwrap();
        let db = Database::open_in_memory().unwrap();
        db.memory_set("research/graphify/finding", "use rust", "project", None)
            .unwrap();
        export(&db, dir.path()).unwrap();
        let expected = dir.path().join("memory/research/graphify/finding.md");
        assert!(expected.exists(), "expected {}", expected.display());
        let content = fs::read_to_string(&expected).unwrap();
        assert!(content.contains("key: \"research/graphify/finding\""));
        assert!(content.contains("use rust"));
    }
}
