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
    /// Skip appending ol-kb instructions to the global instructions file
    /// (~/.claude/CLAUDE.md, ~/.config/opencode/AGENTS.md, ~/.codex/instructions.md)
    #[arg(long)]
    no_instructions: bool,
    /// Show what would be done without writing anything
    #[arg(long)]
    dry_run: bool,
}

pub fn run_install(args: InstallArgs) -> Result<()> {
    let ol_bin = current_binary_path()?;
    for target in resolve_targets(args.target) {
        println!("Installing for {}...", target_name(target));
        install_for_target(target, &ol_bin, &args)?;
    }
    install_shell_completions(args.dry_run)?;
    install_global_gitignore(args.dry_run)?;
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

fn install_for_target(target: InstallTarget, ol_bin: &str, args: &InstallArgs) -> Result<()> {
    match target {
        InstallTarget::Claude => {
            let dir = claude_dir()?;
            if !args.no_skill {
                install_skill(&dir.join("skills"), "ol-kb.md", args.dry_run)?;
            }
            if !args.no_hooks {
                // Hook uses full path - runs without the user's shell profile.
                install_claude_hook(&dir, ol_bin, args.dry_run)?;
            }
            if !args.no_instructions {
                // Write to the global user CLAUDE.md, never to a per-repo file.
                append_or_create_section(
                    &dir.join("CLAUDE.md"),
                    &project_md_section(),
                    args.dry_run,
                )?;
            }
        }
        InstallTarget::Opencode => {
            let dir = opencode_dir()?;
            if !args.no_skill {
                install_skill(&dir.join("skills"), "ol-kb.md", args.dry_run)?;
            }
            if !args.no_hooks {
                install_opencode_plugin(&dir, ol_bin, args.dry_run)?;
            }
            if !args.no_instructions {
                // Global OpenCode instructions file, not a per-repo AGENTS.md.
                append_or_create_section(
                    &dir.join("AGENTS.md"),
                    &project_md_section(),
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
                remove_if_exists(&dir.join("skills").join("ol-kb.md"), args.dry_run)?;
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
                remove_if_exists(&dir.join("skills").join("ol-kb.md"), args.dry_run)?;
            }
            if !args.no_hooks {
                remove_if_exists(&dir.join("plugins").join("ol-session.js"), args.dry_run)?;
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

    // Check if .ol is already ignored (handle variations like /.ol, .ol/, **/.ol)
    if existing.lines().any(|l| {
        let l = l.trim();
        l == ".ol" || l == "/.ol" || l == ".ol/" || l == "**/.ol" || l == ".ol/**"
    }) {
        if dry_run {
            println!(
                "Global gitignore: .ol already present in {}",
                path.display()
            );
        }
        return Ok(());
    }

    let entry = "\n# ol - Operating Layer CLI\n.ol/\n";

    if dry_run {
        println!(
            "Global gitignore: would append '.ol/' to {}",
            path.display()
        );
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, format!("{}{}", existing.trim_end(), entry))?;
        println!("Global gitignore: added '.ol/' to {}", path.display());
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
        _ => None,
    }
}

fn install_shell_completions(dry_run: bool) -> Result<()> {
    use clap_complete::Shell;

    let shell_name = match detect_shell() {
        Some(s) => s,
        None => {
            println!(
                "Completions: unknown shell (set $SHELL). \
                 Run `ol completions <bash|zsh|fish>` manually."
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
            let comp_file = comp_dir.join("_ol");
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
                        "\n# ol completions\n{fpath_line}\nautoload -Uz compinit && compinit\n"
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
            let comp_file = comp_dir.join("ol");
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
            let comp_file = comp_dir.join("ol.fish");
            if dry_run {
                println!("Completions (fish): would write {}", comp_file.display());
            } else {
                std::fs::create_dir_all(&comp_dir)?;
                std::fs::write(&comp_file, &content)?;
                println!("Completions (fish): {}", comp_file.display());
                println!("  completions active in new fish sessions automatically");
            }
        }
        _ => {}
    }

    Ok(())
}

fn install_skill(dir: &Path, filename: &str, dry_run: bool) -> Result<()> {
    let path = dir.join(filename);
    if dry_run {
        println!("  would write: {}", path.display());
    } else {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        std::fs::write(&path, skill_file_content())
            .with_context(|| format!("failed to write {}", path.display()))?;
        println!("  wrote skill: {}", path.display());
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

const HOOK_MARKER: &str = "# ol-kb-hook";

fn install_claude_hook(claude_dir: &Path, ol_bin: &str, dry_run: bool) -> Result<()> {
    let settings_path = claude_dir.join("settings.json");
    let mut settings = read_json(&settings_path)?;

    let hooks_obj = settings
        .as_object_mut()
        .context("settings.json is not an object")?
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("hooks is not an object")?;

    // SessionStart: index the repo immediately when entering a git directory.
    let start_cmd = format!("{HOOK_MARKER}\n{ol_bin} hook start 2>/dev/null || true");
    let start_arr = hooks_obj
        .entry("SessionStart")
        .or_insert_with(|| serde_json::json!([]));
    let start_arr = start_arr
        .as_array_mut()
        .context("hooks.SessionStart is not an array")?;
    strip_ol_hooks(start_arr);
    start_arr.push(serde_json::json!({ "hooks": [{ "type": "command", "command": start_cmd }] }));

    // Stop: index the repo + distill session memories from the transcript.
    let stop_cmd = format!("{HOOK_MARKER}\n{ol_bin} hook stop 2>/dev/null || true");
    let stop_arr = hooks_obj
        .entry("Stop")
        .or_insert_with(|| serde_json::json!([]));
    let stop_arr = stop_arr
        .as_array_mut()
        .context("hooks.Stop is not an array")?;
    strip_ol_hooks(stop_arr);
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
            strip_ol_hooks(arr);
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
        println!("  no ol hooks found");
    }
    Ok(())
}

fn strip_ol_hooks(arr: &mut Vec<Value>) {
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

const OPENCODE_PLUGIN_MARKER: &str = "// ol-kb-plugin";

fn install_opencode_plugin(opencode_dir: &Path, ol_bin: &str, dry_run: bool) -> Result<()> {
    let plugins_dir = opencode_dir.join("plugins");
    let plugin_path = plugins_dir.join("ol-session.js");
    let content = opencode_plugin_content(ol_bin);

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

fn opencode_plugin_content(ol_bin: &str) -> String {
    format!(
        r#"{OPENCODE_PLUGIN_MARKER}
// ol Operating Layer - session hook for OpenCode
// Indexes the current repo and distills session memories on idle.
// Installed by: ol install --target opencode

const OL_BIN = "{ol_bin}";

export default function(ctx) {{
  return {{
    "session.idle": async ({{ session }}) => {{
      try {{
        // Guard against recursion if opencode is the distillation binary
        if (process.env.OL_HOOK_RUNNING) return;
        await ctx.$`${{OL_BIN}} hook opencode ${{session.id}} ${{ctx.directory}}`.quiet();
      }} catch (_) {{
        // Never block session exit
      }}
    }},
  }};
}}
"#
    )
}

const SECTION_MARKER: &str = "<!-- ol-kb -->";

fn append_or_create_section(path: &Path, content: &str, dry_run: bool) -> Result<()> {
    if path.exists() {
        let existing = std::fs::read_to_string(path)?;
        if existing.contains(SECTION_MARKER) {
            println!("  {}: already has ol-kb section", path.display());
            return Ok(());
        }
        if dry_run {
            println!("  would append to: {}", path.display());
        } else {
            std::fs::write(path, format!("{}\n{content}", existing.trim_end()))?;
            println!("  appended to: {}", path.display());
        }
    } else if dry_run {
        println!("  would create: {}", path.display());
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, content)?;
        println!("  created: {}", path.display());
    }
    Ok(())
}

fn uninstall_section(filename: &str, dry_run: bool) -> Result<()> {
    let path = PathBuf::from(filename);
    if !path.exists() {
        println!("  {filename}: not found");
        return Ok(());
    }
    let content = std::fs::read_to_string(&path)?;
    if let Some(start) = content.find(SECTION_MARKER) {
        if dry_run {
            println!("  would remove ol-kb section from {filename}");
        } else {
            std::fs::write(&path, format!("{}\n", content[..start].trim_end()))?;
            println!("  removed ol-kb section from {filename}");
        }
    } else {
        println!("  {filename}: no ol-kb section");
    }
    Ok(())
}

fn skill_file_content() -> String {
    include_str!("../skill.md").to_string()
}

fn project_md_section() -> String {
    format!(
        "\n{SECTION_MARKER}\n# Knowledge Base (ol)\n\nSearch before starting work:\n\n```bash\n\
ol search \"<topic>\"           # all tables\n\
ol todo list                  # open todos\n\
ol research list              # open investigations\n\
ol meeting search \"<topic>\"   # past decisions\n\
```\n\nRecord outcomes:\n\n```bash\n\
ol todo done <id> --note \"<resolution>\"\n\
ol research conclude <id> --findings \"<findings>\"\n\
ol memory set \"<key>\" \"<value>\" --type project\n\
```\n"
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

fn claude_dir() -> Result<PathBuf> {
    Ok(dirs::home_dir().context("no home")?.join(".claude"))
}

fn opencode_dir() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("no home")?
        .join(".config")
        .join("opencode"))
}

fn codex_dir() -> Result<PathBuf> {
    Ok(dirs::home_dir().context("no home")?.join(".codex"))
}

fn current_binary_path() -> Result<String> {
    Ok(std::env::current_exe()
        .context("could not determine binary path")?
        .to_string_lossy()
        .into_owned())
}

fn read_json(path: &Path) -> Result<Value> {
    if path.exists() {
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str(&s).with_context(|| format!("failed to parse {}", path.display()))
    } else {
        Ok(serde_json::json!({}))
    }
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        path,
        serde_json::to_string_pretty(value).context("failed to serialize")?,
    )
    .with_context(|| format!("failed to write {}", path.display()))
}
