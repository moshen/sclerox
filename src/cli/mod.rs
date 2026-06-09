pub mod install;
pub mod meetings;
pub mod memory;
pub mod migrate;
pub mod people;
pub mod projects;
pub mod repos;
pub mod search;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::db::Database;

#[derive(Parser)]
#[command(
    name = "ol",
    about = "Operating Layer CLI - your persistent knowledge base",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Manage persistent memory entries (Claude-compatible)
    #[command(subcommand)]
    Memory(memory::MemoryCommand),

    /// Manage people - colleagues, contacts
    #[command(subcommand)]
    People(people::PeopleCommand),

    /// Manage meetings - notes, transcripts, participants
    #[command(subcommand)]
    Meeting(meetings::MeetingCommand),

    /// Manage projects - descriptions, links, people
    #[command(subcommand)]
    Project(projects::ProjectCommand),

    /// Index and search code repositories
    #[command(subcommand)]
    Repo(repos::RepoCommand),

    /// Full-text search across all tables
    Search(search::SearchArgs),

    /// Show database schema version and migration status
    Migrate(migrate::MigrateArgs),

    /// Install ol into your Claude Code setup (skill, hooks, CLAUDE.md)
    Install(install::InstallArgs),

    /// Remove ol integrations from your Claude Code setup
    Uninstall(install::InstallArgs),
}

pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Install(args) => install::run_install(args),
        Commands::Uninstall(args) => install::run_uninstall(args),
        // All other commands need the database
        cmd => {
            let config = Config::from_env();
            let db = Database::open(&config.db_path)?;
            match cmd {
                Commands::Memory(c) => memory::run(&db, c),
                Commands::People(c) => people::run(&db, c),
                Commands::Meeting(c) => meetings::run(&db, c),
                Commands::Project(c) => projects::run(&db, c),
                Commands::Repo(c) => repos::run(&db, c),
                Commands::Search(a) => search::run(&db, a),
                Commands::Migrate(a) => migrate::run(&db, a),
                Commands::Install(_) | Commands::Uninstall(_) => unreachable!(),
            }
        }
    }
}
