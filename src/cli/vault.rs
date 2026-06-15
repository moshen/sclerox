use anyhow::{Context, Result};
use clap::Subcommand;
use std::fs;
use std::path::{Path, PathBuf};

use crate::db::Database;

#[derive(Subcommand)]
pub enum VaultCommand {
    /// Export the entire knowledge base as plaintext markdown for git-backed sync.
    ///
    /// Creates one markdown file per record under <dir>/memory, /people, /projects,
    /// /meetings, /todos, /research, /repos. Each file has YAML frontmatter plus
    /// a body, with cross-links in Obsidian `[[link]]` form.
    ///
    /// The export owns those subdirectories and rewrites them on every run, so
    /// rerunning is safe and idempotent. Files at the vault root (README, .git,
    /// user notes) are preserved.
    Export {
        /// Target vault directory (created if missing).
        dir: PathBuf,
    },
}

pub fn run(db: &Database, cmd: VaultCommand) -> Result<()> {
    match cmd {
        VaultCommand::Export { dir } => export(db, &dir),
    }
}

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
    fs::create_dir_all(dir)
        .with_context(|| format!("creating vault dir {}", dir.display()))?;

    // Wipe owned subdirs so deleted records vanish on re-export.
    for sub in OWNED_SUBDIRS {
        let path = dir.join(sub);
        if path.exists() {
            fs::remove_dir_all(&path)
                .with_context(|| format!("clearing {}", path.display()))?;
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
        let mut fm = Frontmatter::new();
        fm.kv("id", &p.id.to_string());
        fm.kv("name", &p.name);
        if let Some(e) = &p.email {
            fm.kv("email", e);
        }
        if let Some(s) = &p.slack_id {
            fm.kv("slack_id", s);
        }
        if let Some(g) = &p.github_username {
            fm.kv("github", g);
        }
        fm.kv("created", &p.created_at);
        fm.kv("updated", &p.updated_at);

        let mut body = format!("# {}\n\n", p.name);
        if let Some(notes) = &p.notes {
            body.push_str(notes);
            body.push_str("\n\n");
        }
        if let Some(url) = &p.slack_url {
            body.push_str(&format!("- Slack: {url}\n"));
        }
        if let Some(url) = &p.github_url {
            body.push_str(&format!("- GitHub: {url}\n"));
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

        // Linked people, meetings, repos in frontmatter as list of slugs
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
                let role = p.role.as_deref().map(|r| format!(" ({r})")).unwrap_or_default();
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
        // Key is namespace-with-slashes — use it directly as a relative path.
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
        if let Some(sb) = &m.superseded_by {
            fm.kv("superseded_by", sb);
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
                let role = p.role.as_deref().map(|r| format!(" ({r})")).unwrap_or_default();
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

        write_md(&out_dir.join(format!("{}.md", inv.slug)), &fm.render(), &body)?;
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
        "# ol vault\n\n\
         Plaintext export of the ol knowledge base. Regenerated by `ol vault export`.\n\n\
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

/// Builder for YAML frontmatter blocks. Always quotes scalar values to avoid
/// edge cases (colons, leading-dash, numbers-that-look-like-strings).
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
    // Always use double-quoted scalar to avoid ambiguity. Escape backslash + quote.
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
    fs::write(path, content)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Lowercase, ASCII alphanumeric + hyphen, collapsed.
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

/// Memory keys use `/` as namespace separator — preserve as directory structure
/// but slugify each segment.
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
        assert_eq!(slugify("Colin Kennedy"), "colin-kennedy");
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
        assert_eq!(
            sanitize_key_path("feedback/em-dash"),
            "feedback/em-dash"
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

        // User-created file at vault root should survive re-export.
        let user_file = dir.path().join("my-notes.md");
        fs::write(&user_file, "user content").unwrap();

        // Add one of each type
        let pid = db
            .people_add("Colin Kennedy", Some("c@x"), None, None, None, None, None)
            .unwrap();
        let _mid = db.memory_set("project/active", "ol-cli", "project", None).unwrap();
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

        // Owned subdirs exist
        assert!(dir.path().join("people").exists());
        assert!(dir.path().join("memory").exists());
        assert!(dir.path().join("todos").exists());

        // User file preserved
        assert!(user_file.exists());

        // Specific files written
        assert!(dir.path().join("people/colin-kennedy.md").exists());
        assert!(dir.path().join("memory/project/active.md").exists());

        // Re-run with the person removed: people dir should be empty (or gone),
        // user file still there.
        let _ = db.people_delete(pid);
        export(&db, dir.path()).unwrap();
        assert!(!dir.path().join("people/colin-kennedy.md").exists());
        assert!(user_file.exists(), "user file must survive re-export");
    }

    #[test]
    fn test_export_memory_uses_key_as_path() {
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
