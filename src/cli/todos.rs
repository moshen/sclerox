use anyhow::Result;
use clap::Subcommand;

use crate::cli::text_chunks;
use crate::config::settings;
use crate::db::dates::Dates;
use crate::db::todos::{SimilarTodo, Todo, TodoStatus};
use crate::db::{Database, TextChunkKind};
use crate::output::{print_miss, print_output, truncate, OutputFormat};

#[derive(Subcommand)]
pub enum TodoCommand {
    /// Add a new todo item
    Add {
        #[arg(long)]
        title: String,
        #[arg(long)]
        notes: Option<String>,
        /// Item category
        #[arg(long, default_value = "general",
              value_parser = ["slack","github","email","meeting","project","general"])]
        category: String,
        /// Link to original source (Slack permalink, PR URL, etc.)
        #[arg(long)]
        source_url: Option<String>,
        /// When this work originated (defaults to today)
        #[arg(long)]
        originated: Option<String>,
        /// Hard deadline (YYYY-MM-DD)
        #[arg(long)]
        deadline: Option<String>,
        /// Create as a watch item rather than an actionable todo
        #[arg(long)]
        watch: bool,
    },
    /// Get a todo by ID
    #[command(alias = "show")]
    Get { id: i64 },
    /// List todos
    List {
        /// Filter by status (open, done, watch, all)
        #[arg(long, default_value = "open")]
        status: String,
        /// Filter by category
        #[arg(long)]
        category: Option<String>,
        /// Show only items with a deadline within N days
        #[arg(long)]
        deadline_within: Option<u32>,
        #[command(flatten)]
        dates: crate::cli::dates::DateArgs,
    },
    /// Full-text search todos (includes done items)
    Search {
        query: String,
        /// Only todos with this status
        #[arg(long, default_value = "all", value_parser = ["open", "done", "watch", "all"])]
        status: String,
        #[command(flatten)]
        dates: crate::cli::dates::DateArgs,
    },
    /// Edit a todo's fields
    Update {
        id: i64,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        notes: Option<String>,
        #[arg(long)]
        source_url: Option<String>,
        #[arg(long)]
        deadline: Option<String>,
        #[arg(long, value_parser = ["slack","github","email","meeting","project","general"])]
        category: Option<String>,
    },
    /// Mark a todo as done
    Done {
        id: i64,
        /// Brief resolution note
        #[arg(long)]
        note: Option<String>,
    },
    /// Convert a todo to a watch item (no longer actionable, just monitoring)
    Watch { id: i64 },
    /// Reopen a done or watch item
    Reopen { id: i64 },
    /// `todo list --status done`, or with a query `todo search --status done`
    #[command(hide = true)]
    History {
        query: Option<String>,
        #[command(flatten)]
        dates: crate::cli::dates::DateArgs,
    },
    /// Delete a todo permanently
    Delete { id: i64 },
    /// Manage people linked to a todo
    #[command(subcommand)]
    People(PeopleCmd),
    /// Manage projects linked to a todo
    #[command(subcommand)]
    Projects(TodoProjectsCmd),
}

#[derive(clap::Subcommand)]
pub enum PeopleCmd {
    /// Link a person to this todo
    Add { todo_id: i64, person_id: i64 },
    /// Remove a person link from this todo
    Remove { todo_id: i64, person_id: i64 },
    /// List people linked to this todo
    List { todo_id: i64 },
}

#[derive(clap::Subcommand)]
pub enum TodoProjectsCmd {
    /// Link a project to this todo
    Add { todo_id: i64, project_id: i64 },
    /// Remove a project link from this todo
    Remove { todo_id: i64, project_id: i64 },
    /// List projects linked to this todo
    List { todo_id: i64 },
}

pub fn run(db: &Database, cmd: TodoCommand, format: OutputFormat) -> Result<()> {
    match cmd {
        TodoCommand::Add {
            title,
            notes,
            category,
            source_url,
            originated,
            deadline,
            watch,
        } => {
            let status = if watch {
                TodoStatus::Watch
            } else {
                TodoStatus::Open
            };
            let id = db.todo_add(
                &title,
                notes.as_deref(),
                status,
                source_url.as_deref(),
                &category,
                originated.as_deref(),
                deadline.as_deref(),
            )?;
            embed_todo(db, id, &title, notes.as_deref());
            let todo = db.todo_get(id)?.unwrap();
            print_output(format, &todo, || {
                println!("Added todo #{id}: {title}");
            });
        }

        TodoCommand::Update {
            id,
            title,
            notes,
            source_url,
            deadline,
            category,
        } => {
            if db.todo_update(
                id,
                title.as_deref(),
                notes.as_deref(),
                source_url.as_deref(),
                deadline.as_deref(),
                category.as_deref(),
            )? {
                if title.is_some() || notes.is_some() {
                    if let Some(t) = db.todo_get(id)? {
                        embed_todo(db, id, &t.title, t.notes.as_deref());
                    }
                }
                let t = db.todo_get(id)?;
                print_output(format, &t, || println!("Updated todo #{id}"));
            } else {
                print_miss(format, &format!("Todo #{id} not found or no changes"));
            }
        }

        TodoCommand::Get { id } => match db.todo_get(id)? {
            Some(t) => {
                print_output(format, &t, || print_todo_detail(&t));
                crate::cli::usage::read(db, "todo get", "todo", id);
            }
            None => print_miss(format, &format!("Todo #{id} not found")),
        },

        TodoCommand::List {
            status,
            category,
            deadline_within,
            dates,
        } => {
            let dates = dates.dates()?;
            let mut todos = if let Some(days) = deadline_within {
                db.todo_list_due_within(days, Some(&status))?
            } else {
                db.todo_list(Some(&status))?
            };
            todos.retain(|t| {
                category.as_ref().is_none_or(|c| &t.category == c)
                    && dates.keeps(&t.updated_at, &t.created_at)
            });
            print_output(format, &todos, || {
                if todos.is_empty() {
                    println!("No todos.");
                } else {
                    for t in &todos {
                        print_todo_line(t);
                    }
                    println!("\n{} items", todos.len());
                }
            });
        }

        TodoCommand::Search {
            query,
            status,
            dates,
        } => {
            let status = (status != "all").then_some(status.as_str());
            let (fts_results, semantic) = find(db, &query, status, &dates.dates()?)?;

            print_output(
                format,
                &serde_json::json!({
                    "fts": fts_results,
                    "semantic": semantic.iter().map(|r| &r.todo).collect::<Vec<_>>()
                }),
                || {
                    if fts_results.is_empty() && semantic.is_empty() {
                        println!("No matches for: {query}");
                    } else {
                        for t in &fts_results {
                            let at = crate::cli::format::stamp("updated", &t.updated_at);
                            println!("{}  {at}", crate::cli::format::todo_line(t));
                        }
                        if !semantic.is_empty() {
                            if !fts_results.is_empty() {
                                println!();
                            }
                            for r in &semantic {
                                println!(
                                    "{}  ({:.0}% match)  {}",
                                    crate::cli::format::todo_line(&r.todo),
                                    r.score * 100.0,
                                    crate::cli::format::stamp("updated", &r.todo.updated_at)
                                );
                                println!("  > {}", truncate(&r.matched_chunk, 100));
                            }
                        }
                    }
                },
            );
            let logged: Vec<_> = fts_results
                .iter()
                .map(|t| crate::cli::usage::keyword("todo", t.id))
                .chain(
                    semantic
                        .iter()
                        .map(|r| crate::cli::usage::semantic("todo", r.todo.id, r.score)),
                )
                .collect();
            crate::cli::usage::search(db, "todo search", &query, &logged);
        }

        TodoCommand::Done { id, note } => {
            if db.todo_done(id, note.as_deref())? {
                if format == OutputFormat::Json {
                    if let Some(t) = db.todo_get(id)? {
                        println!("{}", serde_json::to_string_pretty(&t)?);
                    }
                } else {
                    println!("Done: #{id}");
                }
            } else {
                print_miss(format, &format!("Todo #{id} not found or already done"));
            }
        }

        TodoCommand::Watch { id } => {
            if db.todo_set_status(id, TodoStatus::Watch)? {
                let t = db.todo_get(id)?;
                print_output(format, &t, || println!("Watching: #{id}"));
            } else {
                print_miss(format, &format!("Todo #{id} not found"));
            }
        }

        TodoCommand::Reopen { id } => {
            if db.todo_set_status(id, TodoStatus::Open)? {
                let t = db.todo_get(id)?;
                print_output(format, &t, || println!("Reopened: #{id}"));
            } else {
                print_miss(format, &format!("Todo #{id} not found"));
            }
        }

        TodoCommand::History { query, dates } => {
            let status = "done".to_string();
            let cmd = match query {
                Some(query) => TodoCommand::Search {
                    query,
                    status,
                    dates,
                },
                None => TodoCommand::List {
                    status,
                    category: None,
                    deadline_within: None,
                    dates,
                },
            };
            return run(db, cmd, format);
        }

        TodoCommand::Delete { id } => {
            if db.todo_delete(id)? {
                print_output(format, &serde_json::json!({"deleted": id}), || {
                    println!("Deleted #{id}")
                });
            } else {
                print_miss(format, &format!("Todo #{id} not found"));
            }
        }

        TodoCommand::People(sub) => match sub {
            PeopleCmd::Add { todo_id, person_id } => {
                db.todo_link_person(todo_id, person_id)?;
                let people = db.todo_people(todo_id)?;
                print_output(format, &people, || {
                    println!("Linked person #{person_id} to todo #{todo_id}")
                });
            }
            PeopleCmd::Remove { todo_id, person_id } => {
                if db.todo_unlink_person(todo_id, person_id)? {
                    let people = db.todo_people(todo_id)?;
                    print_output(format, &people, || {
                        println!("Removed person #{person_id} from todo #{todo_id}")
                    });
                } else {
                    print_miss(format, "Link not found");
                }
            }
            PeopleCmd::List { todo_id } => {
                let people = db.todo_people(todo_id)?;
                print_output(format, &people, || {
                    if people.is_empty() {
                        println!("No people linked to todo #{todo_id}");
                    } else {
                        for p in &people {
                            println!("#{} {}", p.id, p.name);
                        }
                    }
                });
            }
        },

        TodoCommand::Projects(sub) => match sub {
            TodoProjectsCmd::Add {
                todo_id,
                project_id,
            } => {
                db.todo_link_project(todo_id, project_id)?;
                let projects = db.todo_projects(todo_id)?;
                print_output(format, &projects, || {
                    println!("Linked project #{project_id} to todo #{todo_id}")
                });
            }
            TodoProjectsCmd::Remove {
                todo_id,
                project_id,
            } => {
                if db.todo_unlink_project(todo_id, project_id)? {
                    let projects = db.todo_projects(todo_id)?;
                    print_output(format, &projects, || {
                        println!("Removed project #{project_id} from todo #{todo_id}")
                    });
                } else {
                    print_miss(format, "Link not found");
                }
            }
            TodoProjectsCmd::List { todo_id } => {
                let projects = db.todo_projects(todo_id)?;
                print_output(format, &projects, || {
                    if projects.is_empty() {
                        println!("No projects linked to todo #{todo_id}");
                    } else {
                        for p in &projects {
                            println!("#{} {}", p.id, p.name);
                        }
                    }
                });
            }
        },
    }
    Ok(())
}

fn print_todo_line(t: &crate::db::todos::Todo) {
    println!("{}", crate::cli::format::todo_line(t));
}

/// Embed a single todo's title+notes and store the chunks. Silent on error.
fn embed_todo(db: &Database, id: i64, title: &str, notes: Option<&str>) {
    if let Some(mut embedder) = crate::embed::optional(db.semantic_ready()) {
        store_todo_chunks(db, id, title, notes, &mut embedder);
    }
}

/// Backfill embeddings for todos that have none. Called transparently on search.
pub fn backfill_todo_embeddings_pub(db: &Database) {
    let Ok(pending) = db.todos_without_embeddings() else {
        return;
    };
    if pending.is_empty() {
        return;
    }
    let Some(mut embedder) = crate::embed::optional(db.semantic_ready()) else {
        return;
    };
    log::debug!("backfilling embeddings for {} todos", pending.len());
    for t in pending {
        store_todo_chunks(db, t.id, &t.title, t.notes.as_deref(), &mut embedder);
    }
}

/// Store a todo's chunks with their vectors; nothing when embedding fails.
fn store_todo_chunks(
    db: &Database,
    id: i64,
    title: &str,
    notes: Option<&str>,
    embedder: &mut crate::embed::Embedder,
) {
    let source = text_chunks::todo_source(title, notes);
    if let Err(e) = text_chunks::store(db, TextChunkKind::Todo, id, &source, Some(embedder)) {
        log::warn!("could not embed todo #{id}: {e:#}");
    }
}

fn print_todo_detail(t: &crate::db::todos::Todo) {
    let checkbox = match t.status.as_str() {
        "done" => "[x]",
        "watch" => "[~]",
        _ => "[ ]",
    };
    println!("{checkbox} #{} {}", t.id, t.title);
    println!("  Category:   {}", t.category);
    println!("  Originated: {}", t.originated_date);
    println!("  Created:    {}", t.created_at);
    if let Some(d) = &t.deadline_date {
        println!("  Deadline:   {d}");
    }
    if let Some(u) = &t.source_url {
        println!("  Source:     {u}");
    }
    if let Some(n) = &t.notes {
        println!("  Notes:\n    {}", n.trim().replace('\n', "\n    "));
    }
    if let Some(at) = &t.completed_at {
        println!("  Done at:    {at}");
    }
}

/// Todos with `status` matching `query` within `dates`: keyword matches,
/// then the semantic ones they missed, above the configured cutoff.
fn find(
    db: &Database,
    query: &str,
    status: Option<&str>,
    dates: &Dates,
) -> Result<(Vec<Todo>, Vec<SimilarTodo>)> {
    let fts = db.todo_search(query, status, dates)?;
    let fts_ids: std::collections::HashSet<i64> = fts.iter().map(|t| t.id).collect();
    // Model load costs ~1s; acceptable for a search that already has context.
    let Some(query_emb) =
        crate::embed::optional(db.semantic_ready()).and_then(|mut emb| emb.embed_query(query).ok())
    else {
        return Ok((fts, Vec::new()));
    };
    let s = settings();
    let hits = db
        .todo_similar(&query_emb, s.search.semantic_limit, status, dates)
        .unwrap_or_default();
    let cut = crate::embed::models::semantic_cutoff(
        hits.iter().map(|r| r.score),
        s.semantic_threshold() as f32,
        s.semantic_margin().map(|m| m as f32),
    );
    let semantic = hits
        .into_iter()
        .filter(|r| !fts_ids.contains(&r.todo.id) && r.score >= cut)
        .collect();
    Ok((fts, semantic))
}
