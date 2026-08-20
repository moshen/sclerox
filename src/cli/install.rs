use anyhow::{Context, Result};
use clap::{Args, ValueEnum};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum InstallTarget {
    /// Claude Code CLI (~/.claude/)
    Claude,
    /// OpenCode (~/.config/opencode/)
    Opencode,
    /// OpenAI Codex CLI (~/.codex/)
    Codex,
    /// All supported tools
    All,
}

#[derive(Args)]
pub struct InstallArgs {
    /// Which tool to install for
    #[arg(long, default_value = "claude", value_enum)]
    pub target: InstallTarget,
    /// Skip writing the skill file
    #[arg(long)]
    no_skill: bool,
    /// Skip adding the Stop hook (Claude Code only)
    #[arg(long)]
    no_hooks: bool,
    /// Skip appending sclerox-kb instructions to the global instructions file
    /// (~/.claude/CLAUDE.md, ~/.config/opencode/AGENTS.md, ~/.codex/instructions.md)
    #[arg(long)]
    no_instructions: bool,
    /// Show what would be done without writing anything
    #[arg(long)]
    dry_run: bool,
}

pub fn run_install(args: InstallArgs) -> Result<()> {
    let sclerox_bin = current_binary_path()?;
    for target in resolve_targets(args.target) {
        println!("Installing for {}...", target_name(target));
        install_for_target(target, &sclerox_bin, &args)?;
    }
    install_shell_completions(args.dry_run)?;
    install_global_gitignore(args.dry_run)?;
    // Create a commented ~/.config/sclerox/config.toml so the tunables are discoverable.
    // Never overwrites an existing file; every key ships commented, so this
    // changes no behaviour.
    super::config_cmd::install_default_config(args.dry_run)?;
    if super::migrate::legacy_data_present() {
        println!(
            "\nDetected an old `ol` install at ~/.ol — run `sclerox migrate` to move it over."
        );
    }
    if args.dry_run {
        println!("\n(dry-run: nothing was written)");
    } else {
        println!("\nDone. Reload any open sessions to pick up the new skill.");
    }
    Ok(())
}

pub fn run_uninstall(args: InstallArgs) -> Result<()> {
    for target in resolve_targets(args.target) {
        println!("Uninstalling for {}...", target_name(target));
        uninstall_for_target(target, &args)?;
    }
    // Deliberately keep ~/.config/sclerox/config.toml — it holds user edits.
    let cfg = crate::config::config_path();
    if cfg.exists() {
        println!(
            "config: kept {} (remove it manually if desired)",
            cfg.display()
        );
    }
    if args.dry_run {
        println!("\n(dry-run: nothing was written)");
    }
    Ok(())
}

fn resolve_targets(target: InstallTarget) -> Vec<InstallTarget> {
    match target {
        InstallTarget::All => vec![
            InstallTarget::Claude,
            InstallTarget::Opencode,
            InstallTarget::Codex,
        ],
        t => vec![t],
    }
}

fn install_for_target(target: InstallTarget, sclerox_bin: &str, args: &InstallArgs) -> Result<()> {
    // Persistent per-artifact overwrite policy. A `--no-*` flag skips outright;
    // an `overwrite_* = false` protects an artifact that already exists (a fresh
    // install still creates it). See `[install]` in ~/.config/sclerox/config.toml.
    let policy = &crate::config::settings().install;
    match target {
        InstallTarget::Claude => {
            let dir = claude_dir()?;
            if !args.no_skill {
                install_skill(&dir.join("skills"), policy.overwrite_skill, args.dry_run)?;
            }
            if !args.no_hooks {
                // Hook uses full path - runs without the user's shell profile.
                install_claude_hook(&dir, sclerox_bin, policy.overwrite_hooks, args.dry_run)?;
            }
            if !args.no_instructions {
                // Write to the global user CLAUDE.md, never to a per-repo file.
                append_or_create_section(
                    &dir.join("CLAUDE.md"),
                    &project_md_section(),
                    policy.overwrite_instructions,
                    args.dry_run,
                )?;
            }
        }
        InstallTarget::Opencode => {
            let dir = opencode_dir()?;
            if !args.no_skill {
                install_skill(&dir.join("skills"), policy.overwrite_skill, args.dry_run)?;
            }
            if !args.no_hooks {
                install_opencode_plugin(&dir, policy.overwrite_hooks, args.dry_run)?;
            }
            if !args.no_instructions {
                // Global OpenCode instructions file, not a per-repo AGENTS.md.
                append_or_create_section(
                    &dir.join("AGENTS.md"),
                    &project_md_section(),
                    policy.overwrite_instructions,
                    args.dry_run,
                )?;
            }
        }
        InstallTarget::Codex => {
            if !args.no_instructions {
                // ~/.codex/instructions.md is the global Codex instructions file.
                append_or_create_section(
                    &codex_dir()?.join("instructions.md"),
                    &project_md_section(),
                    policy.overwrite_instructions,
                    args.dry_run,
                )?;
            }
        }
        InstallTarget::All => unreachable!(),
    }
    Ok(())
}

fn uninstall_for_target(target: InstallTarget, args: &InstallArgs) -> Result<()> {
    match target {
        InstallTarget::Claude => {
            let dir = claude_dir()?;
            if !args.no_skill {
                remove_skill(&dir.join("skills"), args.dry_run)?;
            }
            if !args.no_hooks {
                uninstall_claude_hook(&dir, args.dry_run)?;
            }
            if !args.no_instructions {
                uninstall_section(&dir.join("CLAUDE.md").to_string_lossy(), args.dry_run)?;
            }
        }
        InstallTarget::Opencode => {
            let dir = opencode_dir()?;
            if !args.no_skill {
                remove_skill(&dir.join("skills"), args.dry_run)?;
            }
            if !args.no_hooks {
                remove_if_exists(
                    &dir.join("plugins").join("sclerox-session.js"),
                    args.dry_run,
                )?;
            }
            if !args.no_instructions {
                uninstall_section(&dir.join("AGENTS.md").to_string_lossy(), args.dry_run)?;
            }
        }
        InstallTarget::Codex => {
            if !args.no_instructions {
                let path = codex_dir()?.join("instructions.md");
                uninstall_section(&path.to_string_lossy(), args.dry_run)?;
            }
        }
        InstallTarget::All => unreachable!(),
    }
    Ok(())
}

// ─── Global gitignore ────────────────────────────────────────────────────────

fn global_gitignore_path() -> Option<std::path::PathBuf> {
    // Respect git's configured excludesfile if set
    if let Ok(out) = std::process::Command::new("git")
        .args(["config", "--global", "core.excludesfile"])
        .output()
    {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            // Expand ~ if present
            let path = if s.starts_with('~') {
                dirs::home_dir()?.join(&s[2..])
            } else {
                std::path::PathBuf::from(&s)
            };
            return Some(path);
        }
    }
    // XDG default: ~/.config/git/ignore
    Some(dirs::home_dir()?.join(".config/git/ignore"))
}

fn install_global_gitignore(dry_run: bool) -> Result<()> {
    let Some(path) = global_gitignore_path() else {
        println!("Global gitignore: could not determine path");
        return Ok(());
    };

    let existing = if path.exists() {
        std::fs::read_to_string(&path).unwrap_or_default()
    } else {
        String::new()
    };

    // Check if .sclerox is already ignored (handle variations like /.sclerox, .sclerox/, **/.sclerox)
    if existing.lines().any(|l| {
        let l = l.trim();
        l == ".sclerox"
            || l == "/.sclerox"
            || l == ".sclerox/"
            || l == "**/.sclerox"
            || l == ".sclerox/**"
    }) {
        if dry_run {
            println!(
                "Global gitignore: .sclerox already present in {}",
                path.display()
            );
        }
        return Ok(());
    }

    let entry = "\n# sclerox - Sclerox CLI\n.sclerox/\n";

    if dry_run {
        println!(
            "Global gitignore: would append '.sclerox/' to {}",
            path.display()
        );
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, format!("{}{}", existing.trim_end(), entry))?;
        println!("Global gitignore: added '.sclerox/' to {}", path.display());
    }
    Ok(())
}

// ─── Shell completions ───────────────────────────────────────────────────────

fn detect_shell() -> Option<&'static str> {
    let shell_path = std::env::var("SHELL").unwrap_or_default();
    let name = std::path::Path::new(&shell_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    // Leak is fine for this short-lived CLI process
    match name.as_str() {
        "zsh" => Some("zsh"),
        "bash" => Some("bash"),
        "fish" => Some("fish"),
        // On Windows, $SHELL is normally unset (Git Bash sets it to "bash",
        // handled above). Default the native case to PowerShell.
        _ => {
            #[cfg(windows)]
            {
                Some("powershell")
            }
            #[cfg(not(windows))]
            {
                None
            }
        }
    }
}

fn install_shell_completions(dry_run: bool) -> Result<()> {
    use clap_complete::Shell;

    let shell_name = match detect_shell() {
        Some(s) => s,
        None => {
            println!(
                "Completions: unknown shell (set $SHELL). \
                 Run `sclerox completions <bash|zsh|fish>` manually."
            );
            return Ok(());
        }
    };

    let shell: Shell = shell_name
        .parse()
        .map_err(|_| anyhow::anyhow!("unsupported shell: {shell_name}"))?;
    let content = super::completions::generate_to_string(shell);
    let home = dirs::home_dir().context("no home directory")?;

    match shell_name {
        "zsh" => {
            let comp_dir = home.join(".zsh").join("completions");
            let comp_file = comp_dir.join("_sclerox");
            let zshrc = home.join(".zshrc");
            let fpath_line = "fpath=(~/.zsh/completions $fpath)";

            if dry_run {
                println!("Completions (zsh):");
                println!("  would write: {}", comp_file.display());
                // Check if fpath line already present
                let existing = if zshrc.exists() {
                    std::fs::read_to_string(&zshrc).unwrap_or_default()
                } else {
                    String::new()
                };
                let already = existing.contains(fpath_line);
                if !already {
                    println!("  would append to ~/.zshrc: {fpath_line}");
                }
            } else {
                std::fs::create_dir_all(&comp_dir)?;
                std::fs::write(&comp_file, &content)?;
                println!("Completions (zsh): {}", comp_file.display());

                // Append fpath setup to .zshrc if not already present
                let zshrc_content = if zshrc.exists() {
                    std::fs::read_to_string(&zshrc)?
                } else {
                    String::new()
                };
                if !zshrc_content.contains(fpath_line) {
                    let append = format!(
                        "\n# sclerox completions\n{fpath_line}\nautoload -Uz compinit && compinit\n"
                    );
                    std::fs::write(&zshrc, format!("{}{}", zshrc_content.trim_end(), append))?;
                    println!("  appended fpath setup to ~/.zshrc");
                    println!("  run: source ~/.zshrc");
                }
            }
        }
        "bash" => {
            // XDG user completions dir, picked up automatically by bash-completion ≥ 2.x
            let comp_dir = home.join(".local/share/bash-completion/completions");
            let comp_file = comp_dir.join("sclerox");
            if dry_run {
                println!("Completions (bash): would write {}", comp_file.display());
            } else {
                std::fs::create_dir_all(&comp_dir)?;
                std::fs::write(&comp_file, &content)?;
                println!("Completions (bash): {}", comp_file.display());
                println!("  run: source ~/.bashrc  (or open a new shell)");
            }
        }
        "fish" => {
            let comp_dir = home.join(".config/fish/completions");
            let comp_file = comp_dir.join("sclerox.fish");
            if dry_run {
                println!("Completions (fish): would write {}", comp_file.display());
            } else {
                std::fs::create_dir_all(&comp_dir)?;
                std::fs::write(&comp_file, &content)?;
                println!("Completions (fish): {}", comp_file.display());
                println!("  completions active in new fish sessions automatically");
            }
        }
        "powershell" => {
            // $PROFILE varies by PowerShell version/host and Documents may be
            // OneDrive-redirected, so write to a stable path and print the line
            // to add rather than guess-editing the profile.
            let comp_dir = crate::xdg::data_home().join("sclerox").join("completions");
            let comp_file = comp_dir.join("sclerox.ps1");
            if dry_run {
                println!(
                    "Completions (powershell): would write {}",
                    comp_file.display()
                );
                println!("  then dot-source it from $PROFILE");
            } else {
                std::fs::create_dir_all(&comp_dir)?;
                std::fs::write(&comp_file, &content)?;
                println!("Completions (powershell): {}", comp_file.display());
                println!("  add this line to your $PROFILE, then restart PowerShell:");
                println!("    . \"{}\"", comp_file.display());
            }
        }
        _ => {}
    }

    Ok(())
}

fn install_skill(skills_dir: &Path, overwrite: bool, dry_run: bool) -> Result<()> {
    let skill_root = skills_dir.join(SKILL_DIR_NAME);
    let legacy = skills_dir.join(LEGACY_SKILL_FILE);

    // Protect a customized skill: if the current dir or the legacy flat file is
    // present and overwrite is off, leave everything as-is.
    if !overwrite && (skill_root.exists() || legacy.exists()) {
        println!(
            "  skill: kept existing {}/ (install.overwrite_skill = false)",
            skill_root.display()
        );
        return Ok(());
    }

    if dry_run {
        println!(
            "  would write skill: {}/ (SKILL.md + reference/)",
            skill_root.display()
        );
        if legacy.exists() {
            println!("  would remove legacy skill file: {}", legacy.display());
        }
        return Ok(());
    }

    // Migrate away from the pre-directory flat file so it can't shadow the skill.
    if legacy.exists() {
        std::fs::remove_file(&legacy)
            .with_context(|| format!("failed to remove {}", legacy.display()))?;
        println!("  removed legacy skill file: {}", legacy.display());
    }

    for (rel, content) in skill_files() {
        let path = skill_root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::write(&path, content)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    println!("  wrote skill: {}/", skill_root.display());
    Ok(())
}

/// Remove the installed skill: the `sclerox-kb/` directory (current layout) plus the
/// legacy flat `ol-kb.md` if an older install left one.
fn remove_skill(skills_dir: &Path, dry_run: bool) -> Result<()> {
    let skill_root = skills_dir.join(SKILL_DIR_NAME);
    let legacy = skills_dir.join(LEGACY_SKILL_FILE);
    let mut found = false;
    if skill_root.exists() {
        found = true;
        if dry_run {
            println!("  would remove: {}/", skill_root.display());
        } else {
            std::fs::remove_dir_all(&skill_root)
                .with_context(|| format!("failed to remove {}", skill_root.display()))?;
            println!("  removed: {}/", skill_root.display());
        }
    }
    if legacy.exists() {
        found = true;
        remove_if_exists(&legacy, dry_run)?;
    }
    if !found {
        println!("  skill: nothing to remove in {}", skills_dir.display());
    }
    Ok(())
}

fn remove_if_exists(path: &Path, dry_run: bool) -> Result<()> {
    if path.exists() {
        if dry_run {
            println!("  would remove: {}", path.display());
        } else {
            std::fs::remove_file(path)?;
            println!("  removed: {}", path.display());
        }
    } else {
        println!("  not found: {}", path.display());
    }
    Ok(())
}

const HOOK_MARKER: &str = "# sclerox-kb-hook";

fn install_claude_hook(
    claude_dir: &Path,
    sclerox_bin: &str,
    overwrite: bool,
    dry_run: bool,
) -> Result<()> {
    let settings_path = claude_dir.join("settings.json");
    let mut settings = read_json(&settings_path)?;

    if !overwrite && sclerox_hook_present(&settings) {
        println!(
            "  hooks: kept existing in {} (install.overwrite_hooks = false)",
            settings_path.display()
        );
        return Ok(());
    }

    let hooks_obj = settings
        .as_object_mut()
        .context("settings.json is not an object")?
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("hooks is not an object")?;

    // SessionStart: index the repo immediately when entering a git directory.
    let start_cmd = format!("{HOOK_MARKER}\n{sclerox_bin} hook start 2>/dev/null || true");
    let start_arr = hooks_obj
        .entry("SessionStart")
        .or_insert_with(|| serde_json::json!([]));
    let start_arr = start_arr
        .as_array_mut()
        .context("hooks.SessionStart is not an array")?;
    strip_sclerox_hooks(start_arr);
    start_arr.push(serde_json::json!({ "hooks": [{ "type": "command", "command": start_cmd }] }));

    // Stop: index the repo + distill session memories from the transcript.
    let stop_cmd = format!("{HOOK_MARKER}\n{sclerox_bin} hook stop 2>/dev/null || true");
    let stop_arr = hooks_obj
        .entry("Stop")
        .or_insert_with(|| serde_json::json!([]));
    let stop_arr = stop_arr
        .as_array_mut()
        .context("hooks.Stop is not an array")?;
    strip_sclerox_hooks(stop_arr);
    stop_arr.push(serde_json::json!({ "hooks": [{ "type": "command", "command": stop_cmd }] }));

    if dry_run {
        println!(
            "  would add SessionStart + Stop hooks to: {}",
            settings_path.display()
        );
    } else {
        write_json(&settings_path, &settings)?;
        println!(
            "  added SessionStart + Stop hooks: {}",
            settings_path.display()
        );
    }
    Ok(())
}

fn uninstall_claude_hook(claude_dir: &Path, dry_run: bool) -> Result<()> {
    let settings_path = claude_dir.join("settings.json");
    if !settings_path.exists() {
        println!("  no settings.json found");
        return Ok(());
    }
    let mut settings = read_json(&settings_path)?;
    let mut modified = false;

    for event in ["SessionStart", "Stop"] {
        let key = format!("/hooks/{event}");
        if let Some(arr) = settings.pointer_mut(&key).and_then(|v| v.as_array_mut()) {
            let before = arr.len();
            strip_sclerox_hooks(arr);
            if arr.len() < before {
                modified = true;
            }
        }
    }

    if modified {
        if dry_run {
            println!(
                "  would remove SessionStart + Stop hooks from: {}",
                settings_path.display()
            );
        } else {
            write_json(&settings_path, &settings)?;
            println!("  removed hooks: {}", settings_path.display());
        }
    } else {
        println!("  no sclerox hooks found");
    }
    Ok(())
}

/// True if any SessionStart/Stop entry is an sclerox-installed hook (matched by the
/// HOOK_MARKER embedded in its command). Used to protect a customized hook set
/// when `install.overwrite_hooks = false`.
fn sclerox_hook_present(settings: &Value) -> bool {
    ["SessionStart", "Stop"].iter().any(|event| {
        settings
            .pointer(&format!("/hooks/{event}"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter().any(|entry| {
                    entry
                        .get("hooks")
                        .and_then(|h| h.as_array())
                        .map(|hooks| {
                            hooks.iter().any(|h| {
                                h.get("command")
                                    .and_then(|c| c.as_str())
                                    .map(|c| c.contains(HOOK_MARKER))
                                    .unwrap_or(false)
                            })
                        })
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    })
}

fn strip_sclerox_hooks(arr: &mut Vec<Value>) {
    arr.retain(|entry| {
        !entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hooks| {
                hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(|c| c.contains(HOOK_MARKER))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });
}

const OPENCODE_PLUGIN_MARKER: &str = "// sclerox-kb-plugin";

fn install_opencode_plugin(opencode_dir: &Path, overwrite: bool, dry_run: bool) -> Result<()> {
    let plugins_dir = opencode_dir.join("plugins");
    let plugin_path = plugins_dir.join("sclerox-session.js");
    let content = opencode_plugin_content();

    if !overwrite && plugin_path.exists() {
        println!(
            "  plugin: kept existing {} (install.overwrite_hooks = false)",
            plugin_path.display()
        );
        return Ok(());
    }

    if dry_run {
        println!("  would write OpenCode plugin: {}", plugin_path.display());
    } else {
        std::fs::create_dir_all(&plugins_dir)
            .with_context(|| format!("failed to create {}", plugins_dir.display()))?;
        std::fs::write(&plugin_path, content)
            .with_context(|| format!("failed to write {}", plugin_path.display()))?;
        println!("  wrote OpenCode plugin: {}", plugin_path.display());
    }
    Ok(())
}

fn opencode_plugin_content() -> String {
    format!(
        r#"{OPENCODE_PLUGIN_MARKER}
// Sclerox - session hook for OpenCode
// Indexes the current repo and distills session memories on idle.
// Installed by: sclerox install --target opencode
//
// OpenCode plugins default-export an object with a `server(input)` function
// returning a Hooks map. The only event hook is the catch-all `event`; there
// is no per-event-type key, so we filter on `event.type === "session.idle"`.
// The session.idle payload carries `properties.sessionID`.
//
// The `sclerox` binary is resolved at runtime: $SCLEROX_BIN if set, else `sclerox` from PATH
// (Bun's `$` shell looks it up). This keeps the plugin portable across
// machines/users instead of baking in an absolute path at install time.

const SCLEROX_BIN = process.env.SCLEROX_BIN || "sclerox";

export default {{
  id: "sclerox-session",
  server: async ({{ $, directory }}) => ({{
    event: async ({{ event }}) => {{
      if (event.type !== "session.idle") return;
      try {{
        // Guard against recursion if opencode is the distillation binary
        if (process.env.SCLEROX_HOOK_RUNNING) return;
        const sessionID = event.properties?.sessionID;
        if (!sessionID) return;
        await $`${{SCLEROX_BIN}} hook opencode ${{sessionID}} ${{directory}}`.quiet();
      }} catch (_) {{
        // Never block session exit
      }}
    }},
  }}),
}};
"#,
    )
}

const SECTION_MARKER: &str = "<!-- sclerox-kb -->";
const SECTION_END_MARKER: &str = "<!-- /sclerox-kb -->";

/// Create the sclerox-kb section, or UPDATE it in place if already present, so a
/// re-install refreshes stale instructions (like the skill file). The section
/// is bounded by start/end markers; content outside the markers is preserved.
fn append_or_create_section(
    path: &Path,
    block: &str,
    overwrite: bool,
    dry_run: bool,
) -> Result<()> {
    let existed = path.exists();
    let existing = if existed {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };
    // Protect a customized section when asked. A fresh file (no marker yet) is
    // still created — the policy only guards an existing sclerox-kb section.
    if !overwrite && existing.contains(SECTION_MARKER) {
        println!(
            "  {}: kept existing sclerox-kb section (install.overwrite_instructions = false)",
            path.display()
        );
        return Ok(());
    }
    let updated = rebuild_section(&existing, block);

    if existed && updated == existing {
        println!("  {}: sclerox-kb section up to date", path.display());
        return Ok(());
    }
    let verb = if existed { "update" } else { "create" };
    if dry_run {
        println!("  would {verb} sclerox-kb section in: {}", path.display());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, &updated)?;
    println!("  {verb}d sclerox-kb section in: {}", path.display());
    Ok(())
}

/// Insert or replace the sclerox-kb block within `existing`, preserving everything
/// outside the markers. Handles the legacy format (start marker, no end marker,
/// section ran to EOF) by upgrading it to the bounded form.
fn rebuild_section(existing: &str, block: &str) -> String {
    let Some(start) = existing.find(SECTION_MARKER) else {
        // No section yet: append after existing content (or create fresh).
        return if existing.trim().is_empty() {
            format!("{block}\n")
        } else {
            format!("{}\n\n{block}\n", existing.trim_end())
        };
    };

    let before = existing[..start].trim_end();
    let rest = &existing[start..];
    // New format: content after the end marker is user's and must survive.
    // Legacy format (no end marker): the section ran to EOF, so nothing after.
    let after = match rest.find(SECTION_END_MARKER) {
        Some(rel) => existing[start + rel + SECTION_END_MARKER.len()..].trim(),
        None => "",
    };

    let mut out = String::new();
    if !before.is_empty() {
        out.push_str(before);
        out.push_str("\n\n");
    }
    out.push_str(block);
    out.push('\n');
    if !after.is_empty() {
        out.push('\n');
        out.push_str(after);
        out.push('\n');
    }
    out
}

fn uninstall_section(filename: &str, dry_run: bool) -> Result<()> {
    let path = PathBuf::from(filename);
    if !path.exists() {
        println!("  {filename}: not found");
        return Ok(());
    }
    let content = std::fs::read_to_string(&path)?;
    let Some(start) = content.find(SECTION_MARKER) else {
        println!("  {filename}: no sclerox-kb section");
        return Ok(());
    };
    if dry_run {
        println!("  would remove sclerox-kb section from {filename}");
        return Ok(());
    }
    // Remove start..end-marker (new format) or start..EOF (legacy), keeping the
    // text before and any user content after the section.
    let before = content[..start].trim_end();
    let rest = &content[start..];
    let after = match rest.find(SECTION_END_MARKER) {
        Some(rel) => content[start + rel + SECTION_END_MARKER.len()..].trim(),
        None => "",
    };
    let rebuilt = match (before.is_empty(), after.is_empty()) {
        (true, true) => String::new(),
        (true, false) => format!("{after}\n"),
        (false, true) => format!("{before}\n"),
        (false, false) => format!("{before}\n\n{after}\n"),
    };
    std::fs::write(&path, rebuilt)?;
    println!("  removed sclerox-kb section from {filename}");
    Ok(())
}

const SKILL_DIR_NAME: &str = "sclerox-kb";
const LEGACY_SKILL_FILE: &str = "ol-kb.md";

/// The skill as (path-relative-to-the-skill-dir, contents) pairs, baked into the
/// binary. SKILL.md is the always-available core (its frontmatter description is
/// what the agent sees at startup); reference/*.md are read on demand.
fn skill_files() -> &'static [(&'static str, &'static str)] {
    &[
        ("SKILL.md", include_str!("../skill/SKILL.md")),
        (
            "reference/memory.md",
            include_str!("../skill/reference/memory.md"),
        ),
        (
            "reference/people.md",
            include_str!("../skill/reference/people.md"),
        ),
        (
            "reference/meetings.md",
            include_str!("../skill/reference/meetings.md"),
        ),
        (
            "reference/todos.md",
            include_str!("../skill/reference/todos.md"),
        ),
        (
            "reference/research.md",
            include_str!("../skill/reference/research.md"),
        ),
        (
            "reference/projects.md",
            include_str!("../skill/reference/projects.md"),
        ),
        (
            "reference/repos-and-code.md",
            include_str!("../skill/reference/repos-and-code.md"),
        ),
        (
            "reference/config.md",
            include_str!("../skill/reference/config.md"),
        ),
    ]
}

fn project_md_section() -> String {
    format!(
        "{SECTION_MARKER}\n\
<!-- Managed by `sclerox install`: this section is regenerated on upgrade; edits\n\
between these markers are overwritten. To keep changes, edit outside the\n\
markers, or set install.overwrite_instructions = false in ~/.config/sclerox/config.toml. -->\n\
# Knowledge Base (sclerox)\n\nSearch before starting work:\n\n```bash\n\
sclerox search \"<topic>\"           # all tables\n\
sclerox todo list                  # open todos\n\
sclerox research list              # open investigations\n\
sclerox meeting search \"<topic>\"   # past decisions\n\
```\n\n\
Finding code: prefer `sclerox code` over Grep/Glob for symbols in indexed repos:\n\n```bash\n\
sclerox code search \"<symbol>\"      # where is it defined (cross-repo, pre-indexed)\n\
sclerox code refs <symbol>         # what calls it (impact of a change)\n\
```\n\nRecord outcomes:\n\n```bash\n\
sclerox todo done <id> --note \"<resolution>\"\n\
sclerox research conclude <id> --findings \"<findings>\"\n\
sclerox memory set \"<key>\" \"<value>\" --type project\n\
```\n\n{SECTION_END_MARKER}"
    )
}

fn target_name(target: InstallTarget) -> &'static str {
    match target {
        InstallTarget::Claude => "Claude Code",
        InstallTarget::Opencode => "OpenCode",
        InstallTarget::Codex => "Codex",
        InstallTarget::All => "all",
    }
}

pub(crate) fn claude_dir() -> Result<PathBuf> {
    Ok(dirs::home_dir().context("no home")?.join(".claude"))
}

pub(crate) fn opencode_dir() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("no home")?
        .join(".config")
        .join("opencode"))
}

pub(crate) fn codex_dir() -> Result<PathBuf> {
    Ok(dirs::home_dir().context("no home")?.join(".codex"))
}

fn current_binary_path() -> Result<String> {
    Ok(std::env::current_exe()
        .context("could not determine binary path")?
        .to_string_lossy()
        .into_owned())
}

pub(crate) fn read_json(path: &Path) -> Result<Value> {
    if path.exists() {
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str(&s).with_context(|| format!("failed to parse {}", path.display()))
    } else {
        Ok(serde_json::json!({}))
    }
}

pub(crate) fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        path,
        serde_json::to_string_pretty(value).context("failed to serialize")?,
    )
    .with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: &str = "<!-- sclerox-kb -->\nBODY\n<!-- /sclerox-kb -->";

    #[test]
    fn creates_in_empty_file() {
        assert_eq!(rebuild_section("", BLOCK), format!("{BLOCK}\n"));
    }

    #[test]
    fn appends_after_existing_content() {
        assert_eq!(
            rebuild_section("# My notes\n", BLOCK),
            format!("# My notes\n\n{BLOCK}\n")
        );
    }

    #[test]
    fn replaces_legacy_section_running_to_eof() {
        // Legacy format: start marker, no end marker, section was the file tail.
        let existing = "# Notes\n\n<!-- sclerox-kb -->\nOLD CONTENT\nmore old\n";
        let out = rebuild_section(existing, BLOCK);
        assert_eq!(out, format!("# Notes\n\n{BLOCK}\n"));
        assert!(!out.contains("OLD CONTENT"));
    }

    #[test]
    fn replaces_bounded_section_preserving_trailing_user_content() {
        let existing =
            "# Notes\n\n<!-- sclerox-kb -->\nOLD\n<!-- /sclerox-kb -->\n\n# After the section\n";
        let out = rebuild_section(existing, BLOCK);
        assert!(out.contains("BODY") && !out.contains("OLD"));
        assert!(out.contains("# Notes"));
        assert!(out.contains("# After the section"));
    }

    #[test]
    fn is_idempotent() {
        let block = project_md_section();
        let once = rebuild_section("# Notes\n", &block);
        let twice = rebuild_section(&once, &block);
        assert_eq!(once, twice);
    }

    #[test]
    fn real_section_has_both_markers() {
        let s = project_md_section();
        assert!(s.starts_with(SECTION_MARKER));
        assert!(s.trim_end().ends_with(SECTION_END_MARKER));
    }

    #[test]
    fn section_documents_upgrade_behavior() {
        // The delimiter comment must mention the config key so users can find it.
        let s = project_md_section();
        assert!(s.contains("install.overwrite_instructions"));
        assert!(s.contains("regenerated on upgrade"));
    }

    #[test]
    fn overwrite_false_keeps_existing_section() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("CLAUDE.md");
        let original = format!("# Mine\n\n{SECTION_MARKER}\nCUSTOM\n{SECTION_END_MARKER}\n");
        std::fs::write(&path, &original).unwrap();

        append_or_create_section(&path, &project_md_section(), false, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "an existing section must be left byte-for-byte untouched"
        );
    }

    #[test]
    fn overwrite_false_still_creates_missing_section() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("CLAUDE.md"); // absent
        append_or_create_section(&path, &project_md_section(), false, false).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(
            out.contains(SECTION_MARKER),
            "a fresh file is still created even with overwrite disabled"
        );
    }

    #[test]
    fn overwrite_true_refreshes_section() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("CLAUDE.md");
        std::fs::write(
            &path,
            format!("{SECTION_MARKER}\nOLD\n{SECTION_END_MARKER}\n"),
        )
        .unwrap();
        append_or_create_section(&path, &project_md_section(), true, false).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(!out.contains("OLD"), "section refreshed");
        assert!(out.contains("Knowledge Base (sclerox)"));
    }

    #[test]
    fn install_skill_writes_dir_and_respects_overwrite() {
        let dir = tempfile::TempDir::new().unwrap();
        let skills = dir.path().join("skills");
        let skill_md = skills.join(SKILL_DIR_NAME).join("SKILL.md");

        // Fresh install lays down SKILL.md + the reference tree.
        install_skill(&skills, true, false).unwrap();
        assert!(skill_md.exists(), "SKILL.md written");
        assert!(
            skills
                .join(SKILL_DIR_NAME)
                .join("reference/memory.md")
                .exists(),
            "reference files written"
        );

        // overwrite = false keeps a customized SKILL.md.
        std::fs::write(&skill_md, "MY CUSTOM SKILL").unwrap();
        install_skill(&skills, false, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&skill_md).unwrap(),
            "MY CUSTOM SKILL"
        );

        // overwrite = true refreshes it.
        install_skill(&skills, true, false).unwrap();
        assert_ne!(
            std::fs::read_to_string(&skill_md).unwrap(),
            "MY CUSTOM SKILL"
        );
    }

    #[test]
    fn install_skill_migrates_legacy_flat_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let skills = dir.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        let legacy = skills.join(LEGACY_SKILL_FILE);
        std::fs::write(&legacy, "old flat skill").unwrap();

        install_skill(&skills, true, false).unwrap();
        assert!(!legacy.exists(), "legacy flat file removed on migration");
        assert!(skills.join(SKILL_DIR_NAME).join("SKILL.md").exists());
    }

    #[test]
    fn overwrite_false_protects_legacy_flat_file() {
        // A user who customized the old flat file and set overwrite_skill=false
        // must not have it migrated out from under them.
        let dir = tempfile::TempDir::new().unwrap();
        let skills = dir.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        let legacy = skills.join(LEGACY_SKILL_FILE);
        std::fs::write(&legacy, "old flat skill").unwrap();

        install_skill(&skills, false, false).unwrap();
        assert!(
            legacy.exists(),
            "legacy file preserved when overwrite disabled"
        );
    }

    #[test]
    fn sclerox_hook_present_detects_marker() {
        assert!(!sclerox_hook_present(
            &serde_json::json!({ "hooks": { "Stop": [] } })
        ));

        let cmd = format!("{HOOK_MARKER}\nol hook stop");
        let with_hook = serde_json::json!({
            "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": cmd }] }] }
        });
        assert!(sclerox_hook_present(&with_hook));
    }
}
