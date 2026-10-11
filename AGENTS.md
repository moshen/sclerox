# Do when writing code

- Run `cargo fmt`, fix all formatting issues
- Run `cargo clippy --all-targets --all-features`, fix **every** warning - unused imports, dead code, style lints, all of it. Zero warnings is the bar, not "no new warnings".
- Run `cargo test`, fix all failing tests

# When changing how a record type is displayed

Update `src/cli/format.rs` - it owns the canonical single-line renderers for
todos and investigations. Do not duplicate formatting logic in individual
command handlers; call the shared function instead.

# When adding or changing CLI commands

The skill is a progressive-disclosure directory under `src/skill/`, embedded
via `skill_files()` in `src/cli/install.rs`:

- `src/skill/SKILL.md` - the always-loaded behavioral core (when to use,
  recording decisions, the code-search rule, privacy, patterns). Keep it lean.
- `src/skill/reference/<domain>.md` - per-domain commands plus that domain's
  workflow, read on demand (e.g. `todos.md`, `repos-and-code.md`).

When adding or changing a command or flag, update the matching
`reference/<domain>.md`; add a new behavioral rule or a new domain link to
`SKILL.md`. The skill is what the agent reads to use `sclerox` - if it's not there,
the agent won't use it.

Check that `sclerox <command> --help` output matches what the skill describes.

# When adding a config setting

`src/config.rs` is binary-only; library modules (`index`, `search`,
`db`, `embed`) stay settings-free. To add a tunable:

- Add the field to the relevant `*Settings` struct, its `Default`, and
  `validate()` in `src/config.rs`
- Surface it in the `sclerox config` template (`src/cli/config_cmd.rs`)
- If a library module needs it, bridge it to an env var in
  `config::init()` and read that env var from the module - do not make
  library code call `settings()`. See `SCLEROX_MAX_INDEX_FILE_BYTES` and
  `SCLEROX_MAX_INDEX_FILES`.

# Building

Nothing is downloaded at build time. The embedding models and the
token-counting tokenizer download on first use into
`$XDG_CACHE_HOME/sclerox/models/`, pinned to a commit and checked against the
size and sha256 recorded in `src/embed/models.rs` (see `src/embed/download.rs`).
To change a model's files, bump `revision` and every hash together; if the
vectors change, the model `id` must change too so databases get re-embedded.
