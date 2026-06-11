# Do when writing code

- Run `cargo fmt`, fix all formatting issues
- Run `cargo clippy`, fix all clippy issues
- Run `cargo test`, fix all failing tests

# When adding or changing CLI commands

Update the skill file in `src/cli/install.rs` (`skill_file_content()`):

- Every new subcommand must appear in the Commands section
- Every new pattern or workflow must appear in the Patterns section
- The skill file is what Claude reads to know how to use `ol` — if it's not there, Claude won't use it

Check that `ol <command> --help` output matches what the skill describes.
