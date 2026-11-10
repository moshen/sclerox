//! The date flags searches and date-ranged lists share.

use anyhow::Result;
use clap::Args;

use crate::db::dates::Dates;

/// `--from` and `--to`: a range of dates, both ends included.
#[derive(Args, Debug, Clone, Default)]
pub struct Range {
    /// Only what is dated on or after this: a date (2026-06-08, optionally
    /// with a time) or an age (7d, 2w, 3m, 1y), in UTC
    #[arg(long)]
    pub from: Option<String>,
    /// Only what is dated on or before this, the whole day for a date: a date
    /// or an age, as for --from
    #[arg(long)]
    pub to: Option<String>,
}

impl Range {
    pub fn dates(&self) -> Result<Dates> {
        Dates::parse(self.from.as_deref(), self.to.as_deref(), false)
    }
}

/// [`Range`], and `--created` to date records by when they were created
/// rather than when they last changed.
#[derive(Args, Debug, Clone, Default)]
pub struct DateArgs {
    #[command(flatten)]
    pub range: Range,
    /// Date records by when they were created, not when they last changed
    /// (meetings keep their meeting date, and code its last change)
    #[arg(long)]
    pub created: bool,
}

impl DateArgs {
    pub fn dates(&self) -> Result<Dates> {
        Dates::parse(
            self.range.from.as_deref(),
            self.range.to.as_deref(),
            self.created,
        )
    }
}
