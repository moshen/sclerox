# Do when writing code

- Run `cargo fmt`, fix all formatting issues
- Run `cargo clippy --all-targets --all-features`, fix **every** warning - unused imports, dead code, style lints, all of it. Zero warnings is the bar, not "no new warnings".
- Run `cargo test`, fix all failing tests

# When changing how a record type is displayed

Update `src/cli/format.rs` - it owns the canonical single-line renderers for
todos and investigations. Do not duplicate formatting logic in individual
command handlers; call the shared function instead.

# When adding or changing CLI commands

The skill file is `src/skill.md` (embedded via `skill_file_content()` in
`src/cli/install.rs`, which just `include_str!`s it). Update `src/skill.md`:

- Every new subcommand or flag must appear in the Commands section
- Every new pattern or workflow must appear in the Patterns section
- The skill file is what Claude reads to know how to use `ol` — if it's
  not there, Claude won't use it

Check that `ol <command> --help` output matches what the skill describes.

# When adding a config setting

`src/config.rs` is binary-only; library modules (`index`, `search`,
`db`, `embed`) stay settings-free. To add a tunable:

- Add the field to the relevant `*Settings` struct, its `Default`, and
  `validate()` in `src/config.rs`
- Surface it in the `ol config` template (`src/cli/config_cmd.rs`)
- If a library module needs it, bridge it to an env var in
  `config::init()` and read that env var from the module — do not make
  library code call `settings()`. See `OL_MAX_INDEX_FILE_BYTES` and
  `OL_MAX_INDEX_FILES`.

# Building

`build.rs` downloads the MiniLM embedding model into `.model-cache/` on
the first build (needs network plus `curl` or `wget`) and sets the
`bundled_model` cfg; the model's tokenizer is then embedded in the binary
for exact token counts. Set `SKIP_MODEL_DOWNLOAD=1` to skip the download —
embeddings then fall back to a runtime download and token counts to a
coarse estimate.
