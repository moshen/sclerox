pub mod code;
pub mod completions;
pub mod config_cmd;
pub mod db;
pub mod format;
pub mod hook;
pub mod install;
pub mod meetings;
pub mod memory;
pub mod migrate;
pub mod people;
pub mod projects;
pub mod repos;
pub mod research;
pub mod search;
pub mod todos;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::config::settings;
use crate::db::Database;
use crate::output::OutputFormat;

#[derive(Parser)]
#[command(
    name = "sclerox",
    about = "Sclerox CLI - your persistent knowledge base",
    version
)]
pub struct Cli {
    /// Output format
    #[arg(long, global = true, default_value = "text", value_enum)]
    pub output: OutputFormat,

    /// Log level (also reads $SCLEROX_LOG). Logs go to ~/.local/state/sclerox/logs/sclerox-YYYY-MM-DD.log
    #[arg(long, global = true, value_parser = parse_level_filter)]
    pub log_level: Option<crate::logging::LevelFilter>,

    #[command(subcommand)]
    pub command: Commands,
}

fn parse_level_filter(s: &str) -> Result<crate::logging::LevelFilter, String> {
    s.parse::<crate::logging::LevelFilter>()
        .map_err(|_| format!("unknown log level '{s}' (try: error, warn, info, debug, trace)"))
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

    /// Track todos with full done history
    #[command(subcommand)]
    Todo(todos::TodoCommand),

    /// Manage research investigations
    #[command(subcommand)]
    Research(research::ResearchCommand),

    /// Index and search code repositories
    #[command(subcommand)]
    Repo(repos::RepoCommand),

    /// Search code symbols across indexed repos
    #[command(subcommand)]
    Code(code::CodeCommand),

    /// View and manage sclerox configuration (~/.config/sclerox/config.toml)
    #[command(subcommand)]
    Config(config_cmd::ConfigCommand),

    /// Database utilities: schema migration status, plaintext export
    #[command(subcommand)]
    Db(db::DbCommand),

    /// Full-text search across all tables
    Search(search::SearchArgs),

    /// Generate shell completions
    Completions(completions::CompletionsArgs),

    /// Internal: run as a Claude Code lifecycle hook (reads hook JSON from stdin)
    #[command(subcommand)]
    Hook(hook::HookCommand),

    /// Install sclerox into your AI tool setup (Claude Code, OpenCode, Codex)
    Install(install::InstallArgs),

    /// Remove sclerox integrations from your AI tool setup
    Uninstall(install::InstallArgs),

    /// One-time cleanup for a machine with an old, pre-rename `ol` install:
    /// moves ~/.ol/* onto the new XDG layout, renames per-repo .ol/ index
    /// directories, and strips stale integrations. Run BEFORE `sclerox install`
    #[command(hide = true)]
    Migrate(migrate::MigrateArgs),
}

pub fn run(cli: Cli) -> Result<()> {
    let format = cli.output;

    match cli.command {
        Commands::Install(args) => install::run_install(args),
        Commands::Uninstall(args) => install::run_uninstall(args),
        Commands::Migrate(args) => migrate::run_migrate(args),
        Commands::Completions(args) => completions::run(args),
        Commands::Config(cmd) => config_cmd::run(cmd, format),
        Commands::Hook(cmd) => {
            let db = Database::open(&settings().db_path)?;
            hook::run(&db, cmd)
        }
        cmd => {
            let db = Database::open(&settings().db_path)?;
            // Run any pending embedding backfills once, right after the DB opens.
            // This is the Rust-level equivalent of what a schema migration can't do
            // (migrations are pure SQL; fastembed is Rust). Runs once after upgrade,
            // then the todos_without_embeddings() check returns empty immediately.
            todos::backfill_todo_embeddings_pub(&db);
            match cmd {
                Commands::Memory(c) => memory::run(&db, c, format),
                Commands::People(c) => people::run(&db, c, format),
                Commands::Meeting(c) => meetings::run(&db, c),
                Commands::Project(c) => projects::run(&db, c),
                Commands::Todo(c) => todos::run(&db, c, format),
                Commands::Research(c) => research::run(&db, c, format),
                Commands::Repo(c) => repos::run(&db, c),
                Commands::Code(c) => code::run(&db, c),
                Commands::Db(c) => db::run(&db, c),
                Commands::Search(a) => search::run(&db, a, format),
                Commands::Install(_)
                | Commands::Uninstall(_)
                | Commands::Migrate(_)
                | Commands::Completions(_)
                | Commands::Config(_)
                | Commands::Hook(_) => unreachable!(),
            }
        }
    }
}
