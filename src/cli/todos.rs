use anyhow::Result;
use clap::Subcommand;

use crate::db::{todos::TodoStatus, Database};
use crate::output::{print_output, OutputFormat};

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
    Get { id: i64 },
    /// List todos
    List {
        /// Filter by status (open, done, watch, all)
        #[arg(long, default_value = "open")]
        status: String,
        /// Filter by category
        #[arg(long)]
        category: Option<String>,
    },
    /// Full-text search todos (includes done items)
    Search { query: String },
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
    /// Search completed todos (history)
    History {
        /// Optional search query
        query: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
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
                println!("Updated todo #{id}");
            } else {
                println!("Todo #{id} not found or no changes");
            }
        }

        TodoCommand::Get { id } => match db.todo_get(id)? {
            Some(t) => print_output(format, &t, || print_todo_detail(&t)),
            None => println!("Todo #{id} not found"),
        },

        TodoCommand::List { status, category } => {
            let mut todos = db.todo_list(Some(&status))?;
            if let Some(cat) = &category {
                todos.retain(|t| &t.category == cat);
            }
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

        TodoCommand::Search { query } => {
            let results = db.todo_search(&query)?;
            print_output(format, &results, || {
                if results.is_empty() {
                    println!("No matches for: {query}");
                } else {
                    for t in &results {
                        print_todo_line(t);
                    }
                }
            });
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
                println!("Todo #{id} not found or already done");
            }
        }

        TodoCommand::Watch { id } => {
            if db.todo_set_status(id, TodoStatus::Watch)? {
                println!("Watching: #{id}");
            } else {
                println!("Todo #{id} not found");
            }
        }

        TodoCommand::Reopen { id } => {
            if db.todo_set_status(id, TodoStatus::Open)? {
                println!("Reopened: #{id}");
            } else {
                println!("Todo #{id} not found");
            }
        }

        TodoCommand::History { query, from, to } => {
            let results = db.todo_history(query.as_deref(), from.as_deref(), to.as_deref())?;
            print_output(format, &results, || {
                if results.is_empty() {
                    println!("No history found.");
                } else {
                    for t in &results {
                        let done_at = t.completed_at.as_deref().unwrap_or("?");
                        println!("[done {}] #{} {}", done_at, t.id, t.title);
                    }
                    println!("\n{} items", results.len());
                }
            });
        }

        TodoCommand::Delete { id } => {
            if db.todo_delete(id)? {
                println!("Deleted #{id}");
            } else {
                println!("Todo #{id} not found");
            }
        }

        TodoCommand::People(sub) => match sub {
            PeopleCmd::Add { todo_id, person_id } => {
                db.todo_link_person(todo_id, person_id)?;
                println!("Linked person #{person_id} to todo #{todo_id}");
            }
            PeopleCmd::Remove { todo_id, person_id } => {
                if db.todo_unlink_person(todo_id, person_id)? {
                    println!("Removed person #{person_id} from todo #{todo_id}");
                } else {
                    println!("Link not found");
                }
            }
            PeopleCmd::List { todo_id } => {
                let people = db.todo_people(todo_id)?;
                print_output(format, &people, || {
                    if people.is_empty() {
                        println!("No people linked to todo #{todo_id}");
                    } else {
                        for p in &people {
                            let email = p.email.as_deref().unwrap_or("-");
                            println!("#{} {} <{}>", p.id, p.name, email);
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
                println!("Linked project #{project_id} to todo #{todo_id}");
            }
            TodoProjectsCmd::Remove {
                todo_id,
                project_id,
            } => {
                if db.todo_unlink_project(todo_id, project_id)? {
                    println!("Removed project #{project_id} from todo #{todo_id}");
                } else {
                    println!("Link not found");
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

fn print_todo_detail(t: &crate::db::todos::Todo) {
    let checkbox = match t.status.as_str() {
        "done" => "[x]",
        "watch" => "[~]",
        _ => "[ ]",
    };
    println!("{checkbox} #{} {}", t.id, t.title);
    println!("  Category:   {}", t.category);
    println!("  Originated: {}", t.originated_date);
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
