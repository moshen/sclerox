/// This build's version, set by build.rs: the release tag when the release
/// workflow built it, otherwise Cargo.toml's `0.0.0-dev`, then `+<commit>`.
pub const VERSION: &str = env!("SCLEROX_BUILD_VERSION");
