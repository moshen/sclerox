use anyhow::{Context, Result};
use clap::Subcommand;
use std::path::Path;

use crate::config::{config_path, settings, Settings};
use crate::output::{print_output, OutputFormat};

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Show the effective settings (file + env + defaults, merged)
    Show,
    /// Write a commented ~/.config/sclerox/config.toml with every key at its default
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
            format!("no file at {} - using defaults", path.display())
        };
        println!("# sclerox effective settings ({source})");
        let overrides = active_env_overrides();
        if !overrides.is_empty() {
            println!("# env overrides active: {}", overrides.join(", "));
        }
        let off = |v: Option<f64>| v.map_or("off".to_string(), |v| v.to_string());
        if !s.embed.enabled {
            println!("# embeddings are off (embed.enabled = false): keyword search only");
        }
        println!(
            "# effective thresholds for {}: semantic {}, margin {}, dedup cosine {}, merge {}",
            s.embed.model,
            s.semantic_threshold(),
            off(s.semantic_margin()),
            s.dedup_cosine_threshold(),
            off(s.merge_threshold()),
        );
        match toml::to_string_pretty(s) {
            Ok(t) => print!("{t}"),
            Err(e) => eprintln!("could not render settings as TOML: {e}"),
        }
    });
    Ok(())
}

fn active_env_overrides() -> Vec<&'static str> {
    [
        "SCLEROX_DB",
        "SCLEROX_AI_COMMAND",
        "SCLEROX_AI_MODEL",
        "SCLEROX_MAX_INDEX_FILE_BYTES",
        "SCLEROX_MAX_INDEX_FILES",
        "SCLEROX_EMBED_MODEL",
        "SCLEROX_EMBED_ENABLED",
        "SCLEROX_EMBED_MEMORY_CHARS",
        "SCLEROX_CODE_CHUNK_CHARS",
        "SCLEROX_CODE_CHUNK_LINES",
        "SCLEROX_EMBED_SERVER",
        "SCLEROX_SERVER_IDLE_MINUTES",
        "SCLEROX_SERVER_MAX_MEMORY_MB",
        "SCLEROX_SERVER_HARD_MEMORY_MB",
        "SCLEROX_LOG",
        "SCLEROX_CONFIG",
    ]
    .into_iter()
    .filter(|v| std::env::var(v).map(|s| !s.is_empty()).unwrap_or(false))
    // Values startup copied from config.toml for library code aren't overrides.
    .filter(|v| !crate::config::is_bridged(v))
    .collect()
}

/// The commented `config.toml` template. Values are interpolated from
/// `Settings::default()` so the template never drifts from the real defaults.
pub fn config_template() -> String {
    let d = Settings::default();
    let dm = d.embed_model().thresholds;
    format!(
        "# sclerox configuration - all keys optional; defaults shown.\n\
         # Precedence: CLI flag > env var > this file > built-in default.\n\
         # Uncomment a line to change it. `sclerox install` refreshes this file and\n\
         # preserves any values you've set.\n\
         \n\
         # db_path = \"~/.local/share/sclerox/sclerox.db\"            # env: SCLEROX_DB\n\
         \n\
         [ai]\n\
         # Full distillation command; the transcript prompt is appended as the\n\
         # final argument. If unset, sclerox uses the built-in default for the agent\n\
         # that invoked it:\n\
         #   claude -p --safe-mode --no-session-persistence --tools=\n\
         #   opencode run --pure\n\
         # Windows: an npm-installed CLI is a .cmd shim, which the bare name\n\
         # won't resolve. Point command at the shim explicitly, e.g.\n\
         #   command = \"claude.cmd -p --safe-mode --no-session-persistence --tools=\"\n\
         # command = \"\"   # full command incl. flags; env: SCLEROX_AI_COMMAND\n\
         # model = \"\"     # appended to the DEFAULT command only; env: SCLEROX_AI_MODEL\n\
         \n\
         [search]\n\
         # Unset thresholds here and in [dedup] follow embed.model, because each\n\
         # model scores on its own scale:\n\
         {model_thresholds}\
         # semantic_threshold = {sem_thr}          # cosine floor for semantic search hits\n\
         # semantic_limit = {sem_lim}                # max semantic hits per entity type\n\
         # semantic_margin = {sem_margin}            # drop hits this far below the best one; 1.0 = off\n\
         \n\
         [dedup]\n\
         # cosine_threshold = {cos_thr}            # semantic near-dup => supersede\n\
         # lexical_threshold = {lex_thr}            # token-overlap fallback (no embedder)\n\
         # merge_threshold = {merge_thr}              # merge into best match even when several match; unset = the model's (off for granite-small-r2)\n\
         \n\
         [memory]\n\
         # max_value_chars = {max_val}              # warn (not reject) above this length\n\
         \n\
         [session_context]\n\
         # max_tokens = {ctx_tokens}                 # token budget (real MiniLM tokenizer) injected at session start\n\
         # max_chars = {ctx_max}                   # hard byte backstop on that payload\n\
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
         # context_memories = {ctx_mem}               # related memories shown to the distiller for key reuse\n\
         \n\
         [embed]\n\
         # enabled = true                # false: never load the model; keyword search only. env: SCLEROX_EMBED_ENABLED\n\
         # model = \"{embed_model}\"   # {model_names}; switching needs `sclerox db reembed`. env: SCLEROX_EMBED_MODEL\n\
         # Text past a model's token limit is not embedded; sizes past its\n\
         # estimated char limit warn. Larger inputs cost more time and memory.\n\
         {model_limits}\
         # chunk_size = {embed_size}                 # chars per embedded chunk of meeting/todo/research text\n\
         # chunk_overlap = {embed_overlap}               # chars repeated from the previous chunk\n\
         # memory_chars = {mem_chars}               # chars of each memory embedded; unset = the model's. env: SCLEROX_EMBED_MEMORY_CHARS\n\
         # server = \"{embed_server}\"               # auto: share one loaded model between commands via `sclerox server`; off: load it in each. env: SCLEROX_EMBED_SERVER\n\
         \n\
         [server]\n\
         # idle_minutes = {idle_minutes}               # the model server exits after this long without requests. env: SCLEROX_SERVER_IDLE_MINUTES\n\
         # max_memory_mb = {max_memory_mb}            # over this (MB), the model server exits once unused for a minute, giving memory back; 0: no limit. env: SCLEROX_SERVER_MAX_MEMORY_MB\n\
         # hard_memory_mb = {hard_memory_mb}              # over this (MB), it exits after the request in progress, even mid bulk work; 0: no limit. env: SCLEROX_SERVER_HARD_MEMORY_MB\n\
         \n\
         [index]\n\
         # max_file_bytes = {max_bytes}           # env: SCLEROX_MAX_INDEX_FILE_BYTES\n\
         # auto = \"{auto}\"                          # session-hook indexing: git|off\n\
         # max_files = {max_files}                    # reject folders over this many files (--force to override); env: SCLEROX_MAX_INDEX_FILES\n\
         # chunk_chars = {code_chars}                # chars per embedded code chunk; changing it re-chunks on the next index. env: SCLEROX_CODE_CHUNK_CHARS\n\
         # chunk_lines = {code_lines}                 # lines per chunk without a tree-sitter grammar. env: SCLEROX_CODE_CHUNK_LINES\n\
         \n\
         [install]\n\
         # Refresh each managed artifact on a re-install/upgrade. Set false to keep\n\
         # your customizations; a fresh install still creates a missing artifact.\n\
         # overwrite_skill = {ow_skill}               # skills/sclerox-kb.md\n\
         # overwrite_hooks = {ow_hooks}               # SessionStart/Stop hooks + opencode plugin\n\
         # overwrite_instructions = {ow_instr}        # the <!-- sclerox-kb --> section body\n\
         \n\
         [log]\n\
         # level = \"{log_level}\"                     # off|error|warn|info|debug|trace → ~/.local/state/sclerox/logs/. env: SCLEROX_LOG\n\
         # retain_days = {retain_days}                   # delete daily logs older than this (0 = keep forever)\n",
        model_thresholds = model_thresholds_doc(),
        sem_thr = dm.semantic,
        sem_lim = d.search.semantic_limit,
        // MiniLM has no margin; show Granite's as the example value.
        sem_margin = dm
            .top_margin
            .or(crate::embed::models::GRANITE_SMALL_R2.thresholds.top_margin)
            .unwrap_or(0.05),
        cos_thr = dm.dedup_cosine,
        lex_thr = d.dedup.lexical_threshold,
        // Granite has no merge default; show MiniLM's calibrated value as the
        // example, since that is what an explicit value turns back on.
        merge_thr = dm
            .merge
            .or(crate::embed::models::MINILM_L6_V2.thresholds.merge)
            .unwrap_or(0.88),
        max_val = d.memory.max_value_chars,
        ctx_tokens = d.session_context.max_tokens,
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
        ctx_mem = d.distill.context_memories,
        embed_model = d.embed.model,
        model_names = crate::embed::models::names().join("|"),
        embed_size = d.embed.chunk_size,
        embed_overlap = d.embed.chunk_overlap,
        model_limits = model_limits_doc(),
        mem_chars = d.memory_embed_chars(),
        embed_server = d.embed.server,
        idle_minutes = d.server.idle_minutes,
        max_memory_mb = d.server.max_memory_mb,
        hard_memory_mb = d.server.hard_memory_mb,
        code_chars = d.index.chunk_chars,
        code_lines = d.index.chunk_lines,
        max_bytes = d.index.max_file_bytes,
        auto = d.index.auto,
        max_files = d.index.max_files,
        ow_skill = d.install.overwrite_skill,
        ow_hooks = d.install.overwrite_hooks,
        ow_instr = d.install.overwrite_instructions,
        log_level = d.log.level,
        retain_days = d.log.retain_days,
    )
}

/// One indented, commented line per registry model listing its default
/// thresholds. Indented so the upgrade path never mistakes it for a key.
fn model_thresholds_doc() -> String {
    crate::embed::models::MODELS
        .iter()
        .map(|m| {
            let t = m.thresholds;
            let off = |v: Option<f64>| v.map_or("off".to_string(), |v| v.to_string());
            format!(
                "#   {:<18} semantic {}, margin {}, cosine {}, merge {}\n",
                format!("{}:", m.name),
                t.semantic,
                off(t.top_margin),
                t.dedup_cosine,
                off(t.merge)
            )
        })
        .collect()
}

/// One commented line per registry model giving its input limit and default
/// memory embedding size.
fn model_limits_doc() -> String {
    crate::embed::models::MODELS
        .iter()
        .map(|m| {
            format!(
                "#   {:<18} reads {} tokens (about {} chars); memory_chars {}\n",
                format!("{}:", m.name),
                m.max_tokens,
                m.max_chars(),
                m.memory_chars
            )
        })
        .collect()
}

/// Write the template to `path`. If the file exists and `overwrite` is false,
/// leaves it untouched. Used by `sclerox config init`.
pub fn write_config_template(path: &Path, overwrite: bool, dry_run: bool) -> Result<()> {
    if path.exists() && !overwrite {
        println!(
            "config: {} already exists, leaving it untouched (run `sclerox install` to upgrade it)",
            path.display()
        );
        return Ok(());
    }
    if dry_run {
        println!("  would create: {}", path.display());
        return Ok(());
    }
    write(path, &config_template())?;
    println!("config: wrote {}", path.display());
    Ok(())
}

/// Install-time helper: create the config if missing, or UPGRADE it in place -
/// regenerate the commented template (refreshed docs + any new keys) while
/// preserving every value the user has set. Like the CLAUDE.md / skill refresh,
/// this keeps the file current across sclerox versions. Never loses user settings; a
/// file that can't be parsed is left untouched.
pub fn install_default_config(dry_run: bool) -> Result<()> {
    let path = config_path();

    if !path.exists() {
        // A pre-rename `~/.ol/config.toml` IS the real config. Writing a
        // default beside it would take the destination slot and make a later
        // migrate skip it, stranding the user's settings at the old path.
        //
        // Adopt it rather than refusing to act: blocking on the file's mere
        // existence just defers the problem and leaves install degraded until
        // someone runs a second command. Moving it is non-destructive, ends
        // with the user on the new config, and falls through to the upgrade
        // below so the template refreshes around the values it carried.
        match super::migrate::pending_legacy_config() {
            Some(legacy) if dry_run => {
                println!(
                    "  would move {} -> {} (adopting your existing settings)",
                    legacy.display(),
                    path.display()
                );
                return Ok(());
            }
            Some(legacy) => {
                super::migrate::adopt_legacy_config(&legacy, &path)?;
                println!(
                    "config: moved {} -> {} (adopted your existing settings)",
                    legacy.display(),
                    path.display()
                );
                // Falls through to the upgrade path below.
            }
            None if dry_run => {
                println!("  would create: {}", path.display());
                return Ok(());
            }
            None => {
                write(&path, &config_template())?;
                println!("config: wrote {}", path.display());
                return Ok(());
            }
        }
    }

    let existing =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let Some(values) = flatten_user_values(&existing) else {
        println!(
            "config: {} could not be parsed, leaving it untouched \
             (fix it or run `sclerox config init --force`)",
            path.display()
        );
        return Ok(());
    };

    let upgraded = render_with_overrides(&values);
    if upgraded == existing {
        println!("config: {} is up to date", path.display());
    } else if dry_run {
        println!(
            "  would upgrade: {} (preserving your settings)",
            path.display()
        );
    } else {
        write(&path, &upgraded)?;
        println!("config: upgraded {} (kept your settings)", path.display());
    }
    Ok(())
}

fn write(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

/// True if `contents` is a generated template with nothing set in it, i.e. it
/// carries no user intent and can be replaced without losing anything.
///
/// `None` when the file isn't valid TOML: unparseable means unknown, and the
/// caller must not treat unknown as safe to overwrite.
pub fn is_pristine_template(contents: &str) -> Option<bool> {
    flatten_user_values(contents).map(|v| v.is_empty())
}

/// Parse a config file into a map of (section, key) -> value for every key the
/// user has actually set (uncommented). Section is "" for top-level keys.
/// Returns None if the file isn't valid TOML (so callers avoid clobbering it).
fn flatten_user_values(
    contents: &str,
) -> Option<std::collections::HashMap<(String, String), toml::Value>> {
    let table: toml::Table = toml::from_str(contents).ok()?;
    let mut out = std::collections::HashMap::new();
    for (k, v) in table {
        match v {
            toml::Value::Table(sub) => {
                for (k2, v2) in sub {
                    out.insert((k.clone(), k2), v2);
                }
            }
            scalar => {
                out.insert((String::new(), k), scalar);
            }
        }
    }
    Some(out)
}

/// Render the template, but uncomment each key the user had set and substitute
/// their value. Keys are matched section-aware; unset keys stay commented so
/// future default changes still reach the user.
fn render_with_overrides(
    values: &std::collections::HashMap<(String, String), toml::Value>,
) -> String {
    let mut section = String::new();
    let mut out = String::new();
    for line in config_template().lines() {
        // Track the current [section] header.
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = trimmed[1..trimmed.len() - 1].to_string();
            out.push_str(line);
            out.push('\n');
            continue;
        }

        if let Some((key, comment)) = commented_key_line(line) {
            if let Some(val) = values.get(&(section.clone(), key.to_string())) {
                let rendered = render_toml_value(val);
                match comment {
                    Some(c) => out.push_str(&format!("{key} = {rendered}   {c}\n")),
                    None => out.push_str(&format!("{key} = {rendered}\n")),
                }
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// If `line` is a commented key line (`# key = default   # comment`), return the
/// key and any trailing `# comment`. Indented example lines and prose are
/// rejected so only real keys are matched.
fn commented_key_line(line: &str) -> Option<(&str, Option<&str>)> {
    let rest = line.strip_prefix("# ")?;
    // Reject indented example lines like "#   claude ...".
    if rest.starts_with(char::is_whitespace) {
        return None;
    }
    let eq = rest.find(" = ")?;
    let key = &rest[..eq];
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let after = &rest[eq + 3..];
    // The value never contains '#', so the first '#' begins a trailing comment.
    let comment = after.find('#').map(|i| after[i..].trim_end());
    Some((key, comment))
}

fn render_toml_value(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => {
            let s = f.to_string();
            // TOML floats need a decimal point; keep 5.0 from becoming int-like "5".
            if s.contains('.') || s.contains('e') || s.contains("inf") || s.contains("nan") {
                s
            } else {
                format!("{s}.0")
            }
        }
        toml::Value::Boolean(b) => b.to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_pristine_template_distinguishes_generated_from_edited() {
        // The shipped template has every key commented out.
        assert_eq!(is_pristine_template(&config_template()), Some(true));
        assert_eq!(
            is_pristine_template("# max_value_chars = 800\n"),
            Some(true)
        );
        assert_eq!(is_pristine_template(""), Some(true));

        assert_eq!(
            is_pristine_template("[memory]\nmax_value_chars = 1200\n"),
            Some(false)
        );
        // Unparseable is unknown, not safe.
        assert_eq!(is_pristine_template("not [valid toml"), None);
    }

    #[test]
    fn template_parses_back_to_defaults() {
        // Every line is commented, so the template parses as an empty config
        // and yields the built-in defaults.
        let parsed = Settings::from_toml_str(&config_template()).unwrap();
        assert_eq!(parsed.search.semantic_threshold, None);
        assert_eq!(parsed.dedup.cosine_threshold, None);
        assert_eq!(parsed.embed.model, "granite-small-r2");
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
            "[server]",
            "[log]",
        ] {
            assert!(t.contains(section), "template missing {section}");
        }
    }

    #[test]
    fn upgrade_preserves_user_values_and_refreshes_docs() {
        // Simulate an "old" config: a couple of set keys, terse/no docs, and
        // missing newer keys entirely.
        let old = "[search]\nsemantic_threshold = 0.9\n\n[ai]\ncommand = \"opencode run --pure\"\n";
        let values = flatten_user_values(old).unwrap();
        let upgraded = render_with_overrides(&values);

        // User's values are carried over, uncommented.
        assert!(upgraded.contains("semantic_threshold = 0.9"));
        assert!(upgraded.contains("command = \"opencode run --pure\""));
        // Untouched keys remain commented at their defaults (docs refreshed).
        assert!(upgraded.contains("# semantic_limit = 25"));
        assert!(upgraded.contains("# cosine_threshold = 0.9"));
        // A newer key absent from the old file now appears (commented).
        assert!(upgraded.contains("# max_file_bytes ="));

        // The result is valid TOML and round-trips to the user's values.
        let s = Settings::from_toml_str(&upgraded).unwrap();
        assert_eq!(s.search.semantic_threshold, Some(0.9));
        assert_eq!(s.ai.command.as_deref(), Some("opencode run --pure"));
        assert_eq!(s.search.semantic_limit, 25); // still default
    }

    #[test]
    fn upgrade_is_idempotent() {
        // Rendering with the values parsed from a rendered file reproduces it.
        let values = flatten_user_values("[dedup]\ncosine_threshold = 0.7\n").unwrap();
        let once = render_with_overrides(&values);
        let twice = render_with_overrides(&flatten_user_values(&once).unwrap());
        assert_eq!(once, twice);
    }

    #[test]
    fn example_command_lines_are_not_treated_as_keys() {
        // The `#   claude ...` example lines must not be mistaken for a
        // `command` key (which would corrupt the [ai] block on upgrade).
        let values = flatten_user_values("[ai]\ncommand = \"custom cmd\"\n").unwrap();
        let out = render_with_overrides(&values);
        // Exactly one active command line; examples stay commented.
        assert_eq!(out.matches("\ncommand = ").count(), 1);
        assert!(out.contains("#   claude -p"));
        assert!(out.contains("#   opencode run --pure"));
    }
}
