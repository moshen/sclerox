use anyhow::Result;
use clap::Args;

use crate::db::Database;

#[derive(Args)]
pub struct MigrateArgs {
    /// Also show migration status for all indexed repos
    #[arg(long)]
    repos: bool,
}

pub fn run(db: &Database, args: MigrateArgs) -> Result<()> {
    let (current, target, pending) = db.migration_status()?;

    println!("Primary database (~/.ol/ol.db)");
    println!("  schema version : {current}");
    println!("  binary expects : {target}");
    if pending == 0 {
        println!("  status         : up to date");
    } else {
        println!("  status         : {pending} migration(s) will apply on next open");
    }

    if args.repos {
        use crate::index::repo_db::RepoDb;
        use std::path::PathBuf;

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
                            println!("  {} v{cur}/{tgt} ({status})", repo.name);
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
