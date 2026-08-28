// Library target: exposes DB, search, embed, and index modules for integration
// tests and benchmarks. The CLI (cli/, config.rs) is binary-only.
pub mod db;
pub mod embed;
pub mod index;
pub mod migrate;
pub mod output;
pub mod search;
// Shared with the binary because `index` resolves `~`-relative paths.
pub mod xdg;
