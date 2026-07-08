use anyhow::{Context, Result};
use clap::Subcommand;
use std::path::Path;

use crate::config::{config_path, settings, Settings};
use crate::output::{print_output, OutputFormat};

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Show the effective settings (file + env + defaults, merged)
    Show,
    /// Write a commented ~/.ol/config.toml with every key at its default
    Init {
        /// Overwrite an existing config file
        #[arg(long)]
        force: bool,
    },
    /// Print the resolved config file path and whether it exists
    Path,
}

pub fn run(cmd: ConfigCommand, format: OutputFormat) -> Result<()> {
    match cmd {
        ConfigCommand::Show => show(format),
        ConfigCommand::Init { force } => {
            let path = config_path();
            write_config_template(&path, force, false)?;
            Ok(())
        }
        ConfigCommand::Path => {
            let path = config_path();
            let exists = if path.exists() {
                "exists"
            } else {
                "not created yet"
            };
            println!("{} ({exists})", path.display());
            Ok(())
        }
    }
}

fn show(format: OutputFormat) -> Result<()> {
    let s = settings();
    print_output(format, s, || {
        let path = config_path();
        let source = if path.exists() {
            format!("loaded from {}", path.display())
        } else {
            format!("no file at {} — using defaults", path.display())
        };
        println!("# ol effective settings ({source})");
        let overrides = active_env_overrides();
        if !overrides.is_empty() {
            println!("# env overrides active: {}", overrides.join(", "));
        }
        match toml::to_string_pretty(s) {
            Ok(t) => print!("{t}"),
            Err(e) => eprintln!("could not render settings as TOML: {e}"),
        }
    });
    Ok(())
}

fn active_env_overrides() -> Vec<&'static str> {
    [
        "OL_DB",
        "OL_AI_COMMAND",
        "OL_AI_MODEL",
        "OL_MAX_INDEX_FILE_BYTES",
        "OL_CONFIG",
    ]
    .into_iter()
    .filter(|v| std::env::var(v).map(|s| !s.is_empty()).unwrap_or(false))
    .collect()
}

/// The commented `config.toml` template. Values are interpolated from
/// `Settings::default()` so the template never drifts from the real defaults.
pub fn config_template() -> String {
    let d = Settings::default();
    format!(
        "# ol configuration — all keys optional; defaults shown.\n\
         # Precedence: CLI flag > env var > this file > built-in default.\n\
         # Uncomment a line to change it.\n\
         \n\
         # db_path = \"~/.ol/ol.db\"            # env: OL_DB\n\
         \n\
         [ai]\n\
         # Full distillation command; the transcript prompt is appended as the\n\
         # final argument. If unset, ol uses the default for the invoking agent.\n\
         # Uncomment and edit ONE of these defaults to override (env: OL_AI_COMMAND):\n\
         #   command = \"claude -p --safe-mode --no-session-persistence --tools ''\"\n\
         #   command = \"opencode run --pure\"\n\
         # model = \"\"   # appended to the DEFAULT command only; bake into a custom command. env: OL_AI_MODEL\n\
         \n\
         [search]\n\
         # semantic_threshold = {sem_thr}          # cosine floor for semantic search hits\n\
         # semantic_limit = {sem_lim}                 # max semantic hits per entity type\n\
         \n\
         [dedup]\n\
         # cosine_threshold = {cos_thr}            # semantic near-dup => supersede\n\
         # lexical_threshold = {lex_thr}            # token-overlap fallback (no embedder)\n\
         \n\
         [memory]\n\
         # max_value_chars = {max_val}              # warn (not reject) above this length\n\
         \n\
         [session_context]\n\
         # max_chars = {ctx_max}                   # ~750 tokens injected at session start\n\
         # relevant_memories = {rel_mem}              # full-value memories shown\n\
         # feedback_reserved = {fb_res}              # slots guaranteed for feedback type\n\
         # todos_shown = {todos}\n\
         # research_shown = {research}\n\
         # sessions_shown = {sessions}\n\
         # memory_keys_shown = {keys}\n\
         \n\
         [distill]\n\
         # chunk_chars = {chunk_chars}                # transcript chars per AI call\n\
         # min_turns = {min_turns}                     # sessions shorter than this are skipped\n\
         # min_new_turns = {min_new}                 # re-distill only after this much growth\n\
         \n\
         [embed]\n\
         # chunk_size = {embed_size}                 # entity-text chunking for embeddings\n\
         # chunk_overlap = {embed_overlap}\n\
         \n\
         [index]\n\
         # max_file_bytes = {max_bytes}           # env: OL_MAX_INDEX_FILE_BYTES\n",
        sem_thr = d.search.semantic_threshold,
        sem_lim = d.search.semantic_limit,
        cos_thr = d.dedup.cosine_threshold,
        lex_thr = d.dedup.lexical_threshold,
        max_val = d.memory.max_value_chars,
        ctx_max = d.session_context.max_chars,
        rel_mem = d.session_context.relevant_memories,
        fb_res = d.session_context.feedback_reserved,
        todos = d.session_context.todos_shown,
        research = d.session_context.research_shown,
        sessions = d.session_context.sessions_shown,
        keys = d.session_context.memory_keys_shown,
        chunk_chars = d.distill.chunk_chars,
        min_turns = d.distill.min_turns,
        min_new = d.distill.min_new_turns,
        embed_size = d.embed.chunk_size,
        embed_overlap = d.embed.chunk_overlap,
        max_bytes = d.index.max_file_bytes,
    )
}

/// Write the template to `path`. If the file exists and `overwrite` is false,
/// leaves it untouched. Shared by `ol config init` and `ol install`.
pub fn write_config_template(path: &Path, overwrite: bool, dry_run: bool) -> Result<()> {
    if path.exists() && !overwrite {
        println!(
            "config: {} already exists, leaving it untouched",
            path.display()
        );
        return Ok(());
    }
    if dry_run {
        println!("  would create: {}", path.display());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, config_template())
        .with_context(|| format!("writing {}", path.display()))?;
    println!("config: wrote {}", path.display());
    Ok(())
}

/// Install-time helper: create the config file only if it doesn't exist yet.
/// Never overwrites (install must be safe to re-run).
pub fn install_default_config(dry_run: bool) -> Result<()> {
    write_config_template(&config_path(), false, dry_run)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_back_to_defaults() {
        // Every line is commented, so the template parses as an empty config
        // and yields the built-in defaults.
        let parsed = Settings::from_toml_str(&config_template()).unwrap();
        assert_eq!(parsed.search.semantic_threshold, 0.45);
        assert_eq!(parsed.dedup.cosine_threshold, 0.85);
    }

    #[test]
    fn template_mentions_every_section() {
        let t = config_template();
        for section in [
            "[ai]",
            "[search]",
            "[dedup]",
            "[memory]",
            "[session_context]",
            "[distill]",
            "[embed]",
            "[index]",
        ] {
            assert!(t.contains(section), "template missing {section}");
        }
    }
}
