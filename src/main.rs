mod cli;
mod config;
mod db;
mod embed;
mod index;
mod logging;
mod migrate;
mod output;
mod search;
mod xdg;

use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    logging::init(cli.log_level);
    // Load settings once and bridge config-only values (e.g. index file-size
    // limit) into the mechanisms that can't read settings() directly.
    config::init();
    log::debug!("sclerox starting");
    cli::run(cli)
}
