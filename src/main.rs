mod cli;
mod config;
mod db;
mod embed;
mod error;
mod index;
mod logging;
mod output;
mod search;

use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    logging::init(cli.log_level);
    log::debug!("ol starting");
    cli::run(cli)
}
