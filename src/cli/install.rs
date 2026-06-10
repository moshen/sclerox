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
                // Skill file uses bare `ol` - it's documentation Claude reads, not executed.
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

    let hook_command = format!(
        "{HOOK_MARKER}\n{ol_bin} repo index . --description \"$(basename \"$PWD\") repo\" 2>/dev/null || true"
    );
    let new_hook = serde_json::json!({ "type": "command", "command": hook_command });

    let stop = settings
        .as_object_mut()
        .context("settings.json is not an object")?
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("hooks is not an object")?
        .entry("Stop")
        .or_insert_with(|| serde_json::json!([]));

    let arr = stop.as_array_mut().context("hooks.Stop is not an array")?;
    strip_ol_hooks(arr);
    arr.push(serde_json::json!({ "hooks": [new_hook] }));

    if dry_run {
        println!("  would add Stop hook to: {}", settings_path.display());
    } else {
        write_json(&settings_path, &settings)?;
        println!("  added Stop hook: {}", settings_path.display());
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
    let modified = settings
        .pointer_mut("/hooks/Stop")
        .and_then(|v| v.as_array_mut())
        .map(|arr| {
            let before = arr.len();
            strip_ol_hooks(arr);
            arr.len() < before
        })
        .unwrap_or(false);

    if modified {
        if dry_run {
            println!("  would remove Stop hook from: {}", settings_path.display());
        } else {
            write_json(&settings_path, &settings)?;
            println!("  removed Stop hook: {}", settings_path.display());
        }
    } else {
        println!("  no ol hook found");
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

fn skill_file_content() -> &'static str {
    r#"# ol - Operating Layer Knowledge Base

Use when the user asks about people, meetings, projects, todos, past decisions, or research — or when knowledge base context would help.

## When to use

- Starting work: search for related meetings, todos, investigations, project context
- Colleague mentioned: look them up for contact details
- Past decision referenced: search memory and investigations
- After learning something: save to memory for future sessions

## Commands

```bash
# Global search across all tables
ol search "<query>"

# Memory
ol memory set <key> "<value>" --type user|feedback|project|reference
ol memory search "<query>"

# People
ol people search "<name or email>"
ol people add --name "<name>" --email "<email>"

# Meetings
ol meeting search "<topic>"
ol meeting add --title "<title>" --date <YYYY-MM-DD> --notes "<notes>"

# Todos
ol todo list                         # open todos
ol todo add --title "<title>" [--category slack|github|email|meeting|general]
ol todo done <id> [--note "<resolution>"]
ol todo history [<query>]            # search completed todos

# Research / Investigations
ol research list                     # open investigations
ol research start --name "<name>" --slug "<slug>" [--plan "<scope>"]
ol research add-source <id> --url "<url>" --label "<label>"
ol research conclude <id> --findings "<findings>"
ol research search "<query>"

# Projects
ol project search "<description>"

# Repos
ol repo show [path] [--symbols "<query>"]
```

## Patterns

**Before any task:** `ol search "<topic>"`
**After a decision:** `ol memory set "<key>" "<decision>" --type project`
**After research:** `ol research conclude <id> --findings "<findings>"`
"#
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
