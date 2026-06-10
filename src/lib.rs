// Library target: exposes DB, search, embed, and index modules for integration
// tests and benchmarks. The CLI (cli/, config.rs) is binary-only.
pub mod db;
pub mod embed;
pub mod error;
pub mod index;
pub mod output;
pub mod search;
