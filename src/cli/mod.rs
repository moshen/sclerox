pub mod completions;
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

use crate::config::Config;
use crate::db::Database;
use crate::output::OutputFormat;

#[derive(Parser)]
#[command(
    name = "ol",
    about = "Operating Layer CLI - your persistent knowledge base",
    version
)]
pub struct Cli {
    /// Output format
    #[arg(long, global = true, default_value = "text", value_enum)]
    pub output: OutputFormat,

    /// Log level (also reads $OL_LOG). Logs go to ~/.ol/logs/ol-YYYY-MM-DD.log
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

    /// Full-text search across all tables
    Search(search::SearchArgs),

    /// Show database schema version and migration status
    Migrate(migrate::MigrateArgs),

    /// Generate shell completions
    Completions(completions::CompletionsArgs),

    /// Internal: run as a Claude Code lifecycle hook (reads hook JSON from stdin)
    #[command(subcommand)]
    Hook(hook::HookCommand),

    /// Install ol into your AI tool setup (Claude Code, OpenCode, Codex)
    Install(install::InstallArgs),

    /// Remove ol integrations from your AI tool setup
    Uninstall(install::InstallArgs),
}

pub fn run(cli: Cli) -> Result<()> {
    let format = cli.output;

    match cli.command {
        Commands::Install(args) => install::run_install(args),
        Commands::Uninstall(args) => install::run_uninstall(args),
        Commands::Completions(args) => completions::run(args),
        Commands::Hook(cmd) => {
            let config = Config::from_env();
            let db = Database::open(&config.db_path)?;
            hook::run(&db, cmd)
        }
        cmd => {
            let config = Config::from_env();
            let db = Database::open(&config.db_path)?;
            match cmd {
                Commands::Memory(c) => memory::run(&db, c, format),
                Commands::People(c) => people::run(&db, c, format),
                Commands::Meeting(c) => meetings::run(&db, c),
                Commands::Project(c) => projects::run(&db, c),
                Commands::Todo(c) => todos::run(&db, c, format),
                Commands::Research(c) => research::run(&db, c, format),
                Commands::Repo(c) => repos::run(&db, c),
                Commands::Search(a) => search::run(&db, a, format),
                Commands::Migrate(a) => migrate::run(&db, a),
                Commands::Install(_)
                | Commands::Uninstall(_)
                | Commands::Completions(_)
                | Commands::Hook(_) => unreachable!(),
            }
        }
    }
}
