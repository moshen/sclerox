mod cli;
mod config;
mod db;
mod embed;
mod error;
mod index;
mod search;

use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    cli::run(cli)
}
