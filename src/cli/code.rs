use anyhow::Result;
use clap::Subcommand;
use std::path::PathBuf;

use crate::db::Database;
use crate::index::repo_db::RepoDb;

#[derive(Subcommand)]
pub enum CodeCommand {
    /// Search symbols and code across all indexed repos
    Search {
        /// The symbol name, class, or keyword to look for
        needle: String,
        /// Restrict to repos whose name contains this string
        #[arg(long)]
        repo: Option<String>,
    },
}

pub fn run(db: &Database, cmd: CodeCommand) -> Result<()> {
    match cmd {
        CodeCommand::Search { needle, repo } => {
            let repos = db.repo_list()?;
            if repos.is_empty() {
                println!("No repos indexed. Run `ol repo index [path]` first.");
                return Ok(());
            }

            let mut any = false;
            for entry in &repos {
                // Apply --repo filter
                if let Some(filter) = &repo {
                    if !entry.name.to_lowercase().contains(&filter.to_lowercase()) {
                        continue;
                    }
                }

                let db_path = PathBuf::from(&entry.db_path);
                if !db_path.exists() {
                    continue;
                }
                let repo_db = match RepoDb::open(&db_path) {
                    Ok(r) => r,
                    Err(_) => continue,
                };

                for s in repo_db.search_symbols(&needle)? {
                    let sig = s.signature.as_deref().unwrap_or(&s.name);
                    println!(
                        "[{}] {} ({}/{}:{})",
                        s.kind, sig, entry.name, s.file_path, s.start_line
                    );
                    any = true;
                }
            }

            if !any {
                let scope = repo
                    .as_deref()
                    .map(|r| format!(" in repos matching '{r}'"))
                    .unwrap_or_default();
                println!("No symbols match '{needle}'{scope}");
            }
        }
    }
    Ok(())
}
