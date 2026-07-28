use anyhow::Result;
use clap::Subcommand;

use crate::config::settings;
use crate::db::Database;
use crate::embed::Embedder;
use crate::output::{print_output, OutputFormat};

#[derive(Subcommand)]
pub enum MemoryCommand {
    /// Set or update a memory entry
    Set {
        key: String,
        value: String,
        #[arg(long, default_value = "general", value_parser = ["general","user","feedback","project","reference"])]
        r#type: String,
        /// Comma-separated tags
        #[arg(long)]
        tags: Option<String>,
        /// Skip computing the semantic embedding for this entry
        #[arg(long)]
        no_embed: bool,
    },
    /// Get a memory entry by key
    #[command(alias = "show")]
    Get { key: String },
    /// List memory entries (active only by default)
    List {
        #[arg(long, value_parser = ["general","user","feedback","project","reference","session"])]
        r#type: Option<String>,
        /// Include stale and superseded entries
        #[arg(long)]
        all: bool,
    },
    /// Full-text search memory (active only by default)
    Search {
        query: String,
        /// Also search stale and superseded entries
        #[arg(long)]
        all: bool,
    },
    /// Delete a memory entry permanently
    Delete { key: String },
    /// Mark a memory as stale (no longer reliable, but kept for history)
    Stale {
        key: String,
        /// Brief reason why this memory is stale
        #[arg(long)]
        reason: Option<String>,
    },
    /// Replace a memory with an updated version, marking the old one as superseded
    Supersede {
        /// Key of the memory being replaced
        old_key: String,
        /// Key for the new memory entry
        new_key: String,
        new_value: String,
        #[arg(long, default_value = "project", value_parser = ["general","user","feedback","project","reference","session"])]
        r#type: String,
    },
    /// Backfill semantic embeddings for memories that don't have one yet
    Reembed {
        /// Re-embed ALL active memories, not just those missing an embedding
        #[arg(long)]
        force: bool,
    },
    /// Mark a memory as reviewed (you've confirmed it's still accurate)
    Review { key: String },
    /// List memories that haven't been reviewed recently
    NeedsReview {
        /// Flag memories not reviewed in this many days (default: 30)
        #[arg(long, default_value = "30")]
        days: u32,
    },
    /// Distill text into structured memories using an AI CLI
    ///
    /// Compresses a verbose memory entry OR extracts multiple memories from a
    /// transcript/document file. Shells out to an AI CLI; the prompt is
    /// appended as the final argument of the command.
    ///
    /// Command resolution order:
    ///   1. --via "<full command>"
    ///   2. [ai].command / $OL_AI_COMMAND
    ///   3. built-in default (claude with headless flags)
    ///
    /// Model (applies to the DEFAULT command only): --model > [ai].model /
    /// $OL_AI_MODEL > agent default. Bake the model into a custom --via command.
    Distill {
        /// Compress an existing memory entry (supersedes it with the distilled version)
        key: Option<String>,
        /// Extract memories from a transcript or document file
        #[arg(long)]
        from: Option<String>,
        /// Full AI command to run, e.g. "claude -p --safe-mode --tools="
        /// (default: built-in claude command, or [ai].command / $OL_AI_COMMAND)
        #[arg(long)]
        via: Option<String>,
        /// Model for the default command (ignored when --via is a custom command)
        #[arg(long)]
        model: Option<String>,
        /// Show extracted memories without writing to the database
        #[arg(long)]
        dry_run: bool,
    },
    /// Import memories from markdown files (any agent or custom directory)
    Import {
        /// Directory to scan for .md memory files.
        /// Use --agent as a shorthand for known agent defaults.
        #[arg(long)]
        path: Option<String>,
        /// Resolve the default memory path for a known agent:
        ///   claude   → ~/.claude/projects  (Claude Code auto-memory)
        ///   opencode → ~/.config/opencode/memory
        ///   codex    → ~/.codex
        #[arg(long, value_parser = ["claude", "opencode", "codex"])]
        agent: Option<String>,
        /// Show what would be imported without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// Manage people linked to a memory entry
    #[command(subcommand)]
    People(MemoryPeopleCmd),
}

#[derive(clap::Subcommand)]
pub enum MemoryPeopleCmd {
    /// Link a person to a memory entry
    Add { key: String, person_id: i64 },
    /// Remove a person link from a memory entry
    Remove { key: String, person_id: i64 },
    /// List people linked to a memory entry
    List { key: String },
}

pub fn run(db: &Database, cmd: MemoryCommand, format: OutputFormat) -> Result<()> {
    match cmd {
        MemoryCommand::Set {
            key,
            value,
            r#type,
            tags,
            no_embed,
        } => {
            let tag_list: Option<Vec<String>> = tags
                .as_deref()
                .map(|t| t.split(',').map(|s| s.trim().to_string()).collect());
            warn_if_long(&key, &value);
            db.memory_set(&key, &value, &r#type, tag_list.as_deref())?;
            if !no_embed {
                if let Ok(mut embedder) = Embedder::new() {
                    embed_and_store(db, &mut embedder, &key, &value);
                }
            }
            println!("Set: {key}");
        }

        MemoryCommand::Get { key } => match db.memory_get(&key)? {
            Some(entry) => print_output(format, &entry, || {
                println!("Key:     {}", entry.key);
                println!("Value:   {}", entry.value);
                println!("Type:    {}", entry.memory_type);
                if let Some(tags) = &entry.tags {
                    println!("Tags:    {}", tags.join(", "));
                }
                println!("Updated: {}", entry.updated_at);
            }),
            None => println!("Not found: {key}"),
        },

        MemoryCommand::List { r#type, all } => {
            let status = if all { Some("all") } else { None };
            let entries = db.memory_list(r#type.as_deref(), status)?;
            print_output(format, &entries, || {
                if entries.is_empty() {
                    println!("No entries.");
                } else {
                    for e in &entries {
                        let tags = e
                            .tags
                            .as_ref()
                            .map(|t| format!(" [{}]", t.join(", ")))
                            .unwrap_or_default();
                        println!(
                            "[{}] {} - {}{}",
                            e.memory_type,
                            e.key,
                            truncate(&e.value, 60),
                            tags
                        );
                    }
                    println!("\n{} entries", entries.len());
                }
            });
        }

        MemoryCommand::Search { query, all } => {
            let mut results = if all {
                db.memory_search_filtered(&query, "all")?
            } else {
                db.memory_search(&query)?
            };
            // Add a semantic tier for active search: embeddings live only on
            // active rows, so this is skipped for --all (which includes stale).
            if !all {
                let seen: std::collections::HashSet<i64> = results.iter().map(|m| m.id).collect();
                if let Ok(mut embedder) = Embedder::new() {
                    if let Ok(qe) = embedder.embed_one(&query) {
                        let sem = &settings().search;
                        let floor = sem.semantic_threshold as f32;
                        for r in db
                            .memory_similar(&qe, sem.semantic_limit)
                            .unwrap_or_default()
                        {
                            if !seen.contains(&r.entry.id) && r.score >= floor {
                                results.push(r.entry);
                            }
                        }
                    }
                }
            }
            print_output(format, &results, || {
                if results.is_empty() {
                    println!("No matches for: {query}");
                } else {
                    for e in &results {
                        println!("[{}] {} - {}", e.memory_type, e.key, truncate(&e.value, 80));
                    }
                    println!("\n{} results", results.len());
                }
            });
        }

        MemoryCommand::Delete { key } => {
            if db.memory_delete(&key)? {
                println!("Deleted: {key}");
            } else {
                println!("Not found: {key}");
            }
        }

        MemoryCommand::Stale { key, reason } => {
            if db.memory_stale(&key, reason.as_deref())? {
                println!("Marked stale: {key}");
            } else {
                println!("Not found or already stale: {key}");
            }
        }

        MemoryCommand::Supersede {
            old_key,
            new_key,
            new_value,
            r#type,
        } => {
            warn_if_long(&new_key, &new_value);
            if db.memory_supersede(&old_key, &new_key, &new_value, &r#type)? {
                if let Ok(mut embedder) = Embedder::new() {
                    embed_and_store(db, &mut embedder, &new_key, &new_value);
                }
                println!("Superseded '{old_key}' → '{new_key}'");
            } else {
                println!("Not found: {old_key}");
            }
        }

        MemoryCommand::Reembed { force } => {
            let targets = if force {
                db.memory_list(None, Some("active"))?
            } else {
                db.memory_needing_embedding()?
            };
            if targets.is_empty() {
                println!("All active memories already embedded.");
                return Ok(());
            }
            let mut embedder = Embedder::new()?;
            let mut done = 0usize;
            for m in &targets {
                if embed_and_store(db, &mut embedder, &m.key, &m.value) {
                    done += 1;
                }
            }
            println!("Embedded {done} of {} memories.", targets.len());
        }

        MemoryCommand::Review { key } => {
            if db.memory_review(&key)? {
                println!("Marked reviewed: {key}");
            } else {
                println!("Not found: {key}");
            }
        }

        MemoryCommand::NeedsReview { days } => {
            let entries = db.memory_review_needed(days)?;
            print_output(format, &entries, || {
                if entries.is_empty() {
                    println!("All memories reviewed within {days} days.");
                } else {
                    for e in &entries {
                        let last = e.reviewed_at.as_deref().unwrap_or("never");
                        println!("[{}] {} (last reviewed: {})", e.memory_type, e.key, last);
                        println!("  {}", truncate(&e.value, 80));
                    }
                    println!("\n{} entries need review", entries.len());
                }
            });
        }

        MemoryCommand::Distill {
            key,
            from,
            via,
            model,
            dry_run,
        } => {
            // Precedence: --via/--model flag > settings.ai (which folds in the
            // OL_AI_COMMAND / OL_AI_MODEL env vars) > built-in default. Manual
            // `ol memory distill` defaults to the claude command.
            let cfg_ai = &settings().ai;
            let command = via.as_deref().or(cfg_ai.command.as_deref());
            let resolved_model = model.as_deref().or(cfg_ai.model.as_deref());
            let argv = resolve_distill_command(command, "claude", resolved_model)?;

            let (text, existing_key) = match (&key, &from) {
                (Some(k), None) => {
                    let entry = db
                        .memory_get(k)?
                        .ok_or_else(|| anyhow::anyhow!("memory key '{k}' not found"))?;
                    (entry.value.clone(), Some(k.clone()))
                }
                (None, Some(f)) => (std::fs::read_to_string(f)?, None),
                (Some(_), Some(_)) => anyhow::bail!("use either <key> or --from, not both"),
                (None, None) => anyhow::bail!("provide a memory key or --from <file>"),
            };

            let memories = distill_with_ai(&argv, &text)?;

            if memories.is_empty() {
                println!("No memories extracted.");
                return Ok(());
            }

            for m in &memories {
                println!("[{}] {} - {}", m.memory_type, m.key, truncate(&m.value, 80));
            }

            if dry_run {
                println!("\n{} memories (dry-run: nothing written)", memories.len());
            } else {
                let mut embedder = Embedder::new().ok();
                for m in &memories {
                    warn_if_long(&m.key, &m.value);
                    if let Some(ref old_key) = existing_key {
                        // Compressing a single existing entry: supersede it
                        db.memory_supersede(old_key, &m.key, &m.value, &m.memory_type)?;
                    } else {
                        db.memory_set_full(&m.key, &m.value, &m.memory_type, None, "distilled")?;
                    }
                    if let Some(e) = embedder.as_mut() {
                        embed_and_store(db, e, &m.key, &m.value);
                    }
                }
                println!("\nSaved {} memories.", memories.len());
            }
        }

        MemoryCommand::Import {
            path,
            agent,
            dry_run,
        } => {
            let resolved = resolve_import_path(path.as_deref(), agent.as_deref())?;
            import_memories(db, &resolved, dry_run)?;
        }

        MemoryCommand::People(sub) => match sub {
            MemoryPeopleCmd::Add { key, person_id } => {
                if db.memory_link_person(&key, person_id)? {
                    println!("Linked person #{person_id} to memory '{key}'");
                } else {
                    println!("Memory key '{key}' not found");
                }
            }
            MemoryPeopleCmd::Remove { key, person_id } => {
                if db.memory_unlink_person(&key, person_id)? {
                    println!("Removed person #{person_id} from memory '{key}'");
                } else {
                    println!("Link not found");
                }
            }
            MemoryPeopleCmd::List { key } => {
                let people = db.memory_people(&key)?;
                print_output(format, &people, || {
                    if people.is_empty() {
                        println!("No people linked to memory '{key}'");
                    } else {
                        for p in &people {
                            println!("#{} {}", p.id, p.name);
                        }
                    }
                });
            }
        },
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let boundary = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
        format!("{}...", &s[..boundary])
    }
}

/// Warn (to stderr) when a memory value exceeds the recommended length.
/// The value is still stored — this is an assist, not a hard limit — but over-
/// long values embed worse (the vector is capped at the model window) and crowd
/// the session-start context, so we nudge the writer to shorten them.
fn warn_if_long(key: &str, value: &str) {
    let len = value.chars().count();
    if len > settings().memory.max_value_chars {
        eprintln!(
            "warning: memory '{key}' is {len} chars (over {} recommended). \
             Stored anyway, but consider shortening: long values embed worse \
             and crowd session context.",
            settings().memory.max_value_chars
        );
    }
}

/// Embed a memory value and store the vector on its row. Best-effort: the
/// value is truncated to the model window first, and any embedding failure is
/// swallowed so a memory write is never blocked by embedding. Returns whether a
/// vector was stored. Shared by the CLI write paths and the background hook.
pub(crate) fn embed_and_store(
    db: &Database,
    embedder: &mut Embedder,
    key: &str,
    value: &str,
) -> bool {
    let capped: String = value.chars().take(crate::index::MAX_EMBED_CHARS).collect();
    match embedder.embed_one(&capped) {
        Ok(emb) => db.memory_set_embedding(key, &emb).unwrap_or(false),
        Err(_) => false,
    }
}

/// Resolve the search root from explicit --path or --agent shorthand.
fn resolve_import_path(
    path: Option<&str>,
    agent: Option<&str>,
) -> anyhow::Result<std::path::PathBuf> {
    if let Some(p) = path {
        return Ok(std::path::PathBuf::from(p));
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    let resolved = match agent {
        Some("claude") | None => home.join(".claude").join("projects"),
        Some("opencode") => home.join(".config").join("opencode").join("memory"),
        Some("codex") => home.join(".codex"),
        Some(other) => anyhow::bail!(
            "unknown agent '{other}'. Use --path to specify a directory directly, \
             or --agent claude|opencode|codex"
        ),
    };
    Ok(resolved)
}

/// Parse a markdown memory file with YAML-ish frontmatter.
/// Returns (key_slug, value, memory_type) or None if unparseable / should be skipped.
///
/// Supported frontmatter fields (any agent):
///   type / memory_type  → memory type  (fallback: "project")
///   description         → used as value when body is empty
///   name / title        → used as key slug override
fn parse_memory_file(path: &std::path::Path) -> Option<(String, String, String)> {
    let content = std::fs::read_to_string(path).ok()?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    // Skip index/meta files
    if matches!(stem, "MEMORY" | "AGENTS" | "README" | "INDEX") {
        return None;
    }

    let parts: Vec<&str> = content.splitn(3, "---").collect();
    let (frontmatter, body) = if parts.len() >= 3 {
        (parts[1], parts[2].trim())
    } else {
        ("", content.trim())
    };

    // Key: use frontmatter name/slug if present, else filename stem
    let key = frontmatter
        .lines()
        .find_map(|line| {
            let line = line.trim();
            line.strip_prefix("name:")
                .or_else(|| line.strip_prefix("slug:"))
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| stem.to_string());

    // Type: accept "type:", "memory_type:", or nested under metadata
    let memory_type = frontmatter
        .lines()
        .find_map(|line| {
            let line = line.trim();
            line.strip_prefix("type:")
                .or_else(|| line.strip_prefix("memory_type:"))
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "project".to_string());

    // Value: body if present, else description field
    let value = if !body.is_empty() {
        body.to_string()
    } else {
        frontmatter
            .lines()
            .find_map(|line| {
                let line = line.trim();
                line.strip_prefix("description:")
                    .or_else(|| line.strip_prefix("title:"))
                    .map(|s| s.trim().to_string())
            })
            .unwrap_or_default()
    };

    if value.is_empty() {
        return None;
    }

    Some((key, value, memory_type))
}

/// Recursively collect all .md files under a directory.
fn collect_md_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(collect_md_files(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            files.push(path);
        }
    }
    files
}

fn import_memories(
    db: &crate::db::Database,
    root: &std::path::Path,
    dry_run: bool,
) -> anyhow::Result<()> {
    if !root.exists() {
        println!("Path not found: {}", root.display());
        println!("Tip: use --path <dir> to point at any directory of .md memory files.");
        return Ok(());
    }

    let files = collect_md_files(root);
    if files.is_empty() {
        println!("No .md files found under {}", root.display());
        return Ok(());
    }

    let mut imported = 0usize;
    let mut skipped = 0usize;
    let mut embedder = if dry_run { None } else { Embedder::new().ok() };

    for path in &files {
        let Some((key, value, memory_type)) = parse_memory_file(path) else {
            continue;
        };

        if dry_run {
            println!("[would import] {key} ({memory_type})");
            println!("  {}", truncate(&value, 80));
            imported += 1;
        } else {
            if db.memory_get(&key)?.is_some() {
                skipped += 1;
                continue;
            }
            warn_if_long(&key, &value);
            db.memory_set_full(&key, &value, &memory_type, None, "imported")?;
            if let Some(e) = embedder.as_mut() {
                embed_and_store(db, e, &key, &value);
            }
            imported += 1;
        }
    }

    if dry_run {
        println!("\n{imported} entries would be imported (dry-run: nothing written)");
    } else {
        println!("Imported {imported} entries, skipped {skipped} (already exist)");
        if imported > 0 {
            println!("Run `ol memory list` to see imported entries.");
        }
    }
    Ok(())
}

// ─── AI distillation ─────────────────────────────────────────────────────────

pub struct DistilledMemory {
    pub key: String,
    pub value: String,
    pub memory_type: String,
}

const DISTILL_PROMPT: &str = r#"You are extracting structured memory entries from the provided text.

Extract 1-8 concise, factual memory entries. Each entry should capture a single
distinct fact, preference, decision, or piece of context worth remembering.

Keep each "value" concise: 1-3 sentences and UNDER 800 characters. If a fact
needs more than that, it is really several facts — split it into separate
entries rather than writing one long value.

Output ONLY a JSON array with no surrounding text or markdown fences:
[
  {"key": "kebab-case-slug", "value": "concise 1-3 sentence statement", "type": "feedback|project|user|reference|session"},
  ...
]

Type guide:
  feedback    - preferences, constraints, lessons learned
  project     - decisions, facts specific to a project
  user        - facts about the user's role, context, or goals
  reference   - pointers to external resources
  session     - summary of what happened in a session

Key naming:
  Use stable, descriptive kebab-case slugs. If a fact updates or corrects an
  earlier fact, reuse the same key you would have used before rather than
  inventing a new near-identical slug (e.g. reuse "chunk-size-limit", do not
  add "chunk-size-fix" then "chunk-size-limit-v2"). Prefer the plainest slug
  that names the fact.

Text to distill:
"#;

/// Built-in default distillation command (argv, WITHOUT the prompt) for a known
/// agent. The prompt is appended as the final argument by `distill_with_ai`.
fn default_command(agent: &str) -> Vec<String> {
    let parts: &[&str] = match agent {
        // opencode run --pure "prompt"
        "opencode" => &["opencode", "run", "--pure"],
        // claude (default): headless flags that skip hooks/skills/persistence.
        // NOTE: --tools= must be the single-token equals form. The space form
        // (`--tools ""`) broke when the claude CLI made --tools variadic — it
        // swallowed the prompt argument and every distillation failed.
        _ => &[
            "claude",
            "-p",
            "--safe-mode",
            "--no-session-persistence",
            "--tools=",
        ],
    };
    parts.iter().map(|s| s.to_string()).collect()
}

/// Resolve the distillation command into argv (program + args, WITHOUT the prompt).
///
/// - `command`: an explicit full command (from `--via`, `[ai].command`, or
///   `OL_AI_COMMAND`). Parsed shell-style and used verbatim; `model` is ignored
///   (bake the model flag into a custom command yourself).
/// - `default_agent`: which built-in default to use when `command` is `None` —
///   `"claude"` for the Claude paths, `"opencode"` for the OpenCode hook.
/// - `model`: appended to the DEFAULT command only, per the agent's flag
///   convention (`--model` for claude, `-m` for opencode).
pub fn resolve_distill_command(
    command: Option<&str>,
    default_agent: &str,
    model: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    if let Some(cmd) = command {
        let argv = shlex::split(cmd).ok_or_else(|| {
            anyhow::anyhow!("could not parse AI command (unbalanced quotes?): {cmd}")
        })?;
        if argv.is_empty() {
            anyhow::bail!("AI command is empty");
        }
        return Ok(argv);
    }
    let mut argv = default_command(default_agent);
    if let Some(m) = model {
        if default_agent == "opencode" {
            argv.push("-m".to_string());
        } else {
            argv.push("--model".to_string());
        }
        argv.push(m.to_string());
    }
    Ok(argv)
}

/// Run the resolved AI command (argv) with the distillation prompt appended as
/// the final argument, and parse the JSON response.
pub fn distill_with_ai_pub(argv: &[String], text: &str) -> anyhow::Result<Vec<DistilledMemory>> {
    distill_with_ai(argv, text)
}

/// Search `path_var` × `pathext` for `program`, returning the first candidate
/// for which `exists` is true. Pure so it is testable without a real PATH; the
/// Windows resolver below supplies the real env values and a filesystem check.
#[cfg(any(windows, test))]
fn resolve_via_pathext(
    program: &str,
    path_var: &str,
    pathext: &str,
    exists: impl Fn(&std::path::Path) -> bool,
) -> Option<String> {
    for dir in std::env::split_paths(path_var) {
        for ext in pathext.split(';').filter(|e| !e.is_empty()) {
            let cand = dir.join(format!("{program}{ext}"));
            if exists(&cand) {
                return Some(cand.to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// Resolve the AI command's program token to a concrete path.
///
/// `Command::new` on Windows only auto-appends `.exe`, so a bare name like
/// `claude` or `opencode` never finds the `.cmd`/`.bat` shims npm installs.
/// Resolve bare names against PATH + PATHEXT ourselves; std then routes a
/// resolved `.cmd`/`.bat` through cmd.exe with hardened arg escaping. A name
/// that already has an extension or a path separator is trusted as given, and
/// Unix needs nothing (Command::new already searches PATH and honors shebangs).
fn resolve_program(program: &str) -> String {
    #[cfg(windows)]
    {
        let bare =
            std::path::Path::new(program).extension().is_none() && !program.contains(['/', '\\']);
        if bare {
            let pathext =
                std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
            if let Ok(path) = std::env::var("PATH") {
                if let Some(found) = resolve_via_pathext(program, &path, &pathext, |p| p.is_file())
                {
                    return found;
                }
            }
        }
    }
    program.to_string()
}

fn distill_with_ai(argv: &[String], text: &str) -> anyhow::Result<Vec<DistilledMemory>> {
    let prompt = format!("{DISTILL_PROMPT}{text}");
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("AI command is empty"))?;

    // On Windows this finds claude.cmd/opencode.cmd; elsewhere it's a no-op.
    let resolved = resolve_program(program);
    let mut cmd = std::process::Command::new(&resolved);
    cmd.args(args);
    cmd.arg(&prompt);

    let output = cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!(
                "AI command '{program}' not found in PATH. \
                 Set [ai].command in ~/.ol/config.toml (or $OL_AI_COMMAND). \
                 On Windows an npm CLI is a .cmd shim the bare name can't \
                 resolve — point it at e.g. '{program}.cmd'."
            )
        } else {
            anyhow::anyhow!("failed to run '{program}': {e}")
        }
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("'{program}' exited with error:\n{stderr}");
    }

    let response = String::from_utf8_lossy(&output.stdout);
    parse_distilled_json(&response)
}

/// Extract a JSON array from LLM output that may contain prose or markdown fences.
fn parse_distilled_json(response: &str) -> anyhow::Result<Vec<DistilledMemory>> {
    // Find the first '[' and last ']' to tolerate leading/trailing prose
    let start = response
        .find('[')
        .ok_or_else(|| anyhow::anyhow!("no JSON array found in response"))?;
    let end = response
        .rfind(']')
        .ok_or_else(|| anyhow::anyhow!("unclosed JSON array in response"))?;

    let json_slice = &response[start..=end];
    let raw: Vec<serde_json::Value> = serde_json::from_str(json_slice)
        .map_err(|e| anyhow::anyhow!("failed to parse JSON from response: {e}\n{json_slice}"))?;

    let mut memories = Vec::new();
    for item in raw {
        let key = item["key"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("entry missing 'key' field"))?
            .to_string();
        let value = item["value"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("entry missing 'value' field"))?
            .to_string();
        let memory_type = item["type"].as_str().unwrap_or("project").to_string();

        if !key.is_empty() && !value.is_empty() {
            memories.push(DistilledMemory {
                key,
                value,
                memory_type,
            });
        }
    }

    Ok(memories)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_via_pathext_finds_shim_and_respects_order() {
        let dir = tempfile::TempDir::new().unwrap();
        // Only a .cmd shim exists (the npm-on-Windows shape).
        std::fs::write(dir.path().join("claude.cmd"), "").unwrap();
        let path_var = dir.path().to_string_lossy().into_owned();

        let found = resolve_via_pathext("claude", &path_var, ".exe;.cmd", |p| p.is_file()).unwrap();
        assert!(found.ends_with("claude.cmd"), "resolved to the .cmd shim");

        // A bare name with no matching file resolves to nothing.
        assert!(resolve_via_pathext("opencode", &path_var, ".exe;.cmd", |p| p.is_file()).is_none());

        // .exe wins when both exist (PATHEXT order honored).
        std::fs::write(dir.path().join("claude.exe"), "").unwrap();
        let found = resolve_via_pathext("claude", &path_var, ".exe;.cmd", |p| p.is_file()).unwrap();
        assert!(found.ends_with("claude.exe"), "earlier PATHEXT entry wins");
    }

    #[test]
    fn resolve_program_is_identity_for_explicit_names() {
        // Names with an extension or path separator are trusted as-is on every
        // platform (and the whole thing is a no-op on non-Windows).
        assert_eq!(resolve_program("claude.cmd"), "claude.cmd");
        #[cfg(not(windows))]
        assert_eq!(resolve_program("claude"), "claude");
    }

    #[test]
    fn default_claude_command_with_flags() {
        let argv = resolve_distill_command(None, "claude", None).unwrap();
        assert_eq!(
            argv,
            vec![
                "claude",
                "-p",
                "--safe-mode",
                "--no-session-persistence",
                "--tools=",
            ]
        );
        // Regression guard: --tools must be the single-token equals form. The
        // claude CLI treats a bare `--tools ""` as variadic and swallows the
        // prompt argument.
        assert!(!argv.contains(&"--tools".to_string()));
    }

    #[test]
    fn default_command_appends_model_per_agent() {
        let claude = resolve_distill_command(None, "claude", Some("sonnet")).unwrap();
        assert_eq!(&claude[claude.len() - 2..], &["--model", "sonnet"]);

        let oc = resolve_distill_command(None, "opencode", Some("x/y")).unwrap();
        assert_eq!(oc[0], "opencode");
        assert_eq!(&oc[oc.len() - 2..], &["-m", "x/y"]);
    }

    #[test]
    fn custom_command_is_parsed_and_ignores_model() {
        // Quoted empty arg must survive parsing.
        let argv = resolve_distill_command(Some("claude -p --tools ''"), "claude", Some("ignored"))
            .unwrap();
        assert_eq!(argv, vec!["claude", "-p", "--tools", ""]);
    }

    #[test]
    fn empty_or_unparseable_command_errors() {
        assert!(resolve_distill_command(Some("   "), "claude", None).is_err());
        assert!(resolve_distill_command(Some("claude 'unbalanced"), "claude", None).is_err());
    }
}
