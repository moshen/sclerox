use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum InstallCommand {
    /// Install skill, hooks, and project CLAUDE.md into your Claude Code setup
    Install(InstallArgs),
    /// Remove ol integrations from your Claude Code setup
    Uninstall(InstallArgs),
}

#[derive(Args)]
pub struct InstallArgs {
    /// Skip writing the skill file to ~/.claude/skills/ol-kb.md
    #[arg(long)]
    no_skill: bool,

    /// Skip adding the Stop hook to ~/.claude/settings.json
    #[arg(long)]
    no_hooks: bool,

    /// Skip creating CLAUDE.md in the current project directory
    #[arg(long)]
    no_project_md: bool,

    /// Show what would be done without writing anything
    #[arg(long)]
    dry_run: bool,
}

pub fn run_install(args: InstallArgs) -> Result<()> {
    let ol_bin = current_binary_path()?;
    let claude_dir = claude_dir()?;

    if !args.no_skill {
        install_skill(&claude_dir, &ol_bin, args.dry_run)?;
    }
    if !args.no_hooks {
        install_hooks(&claude_dir, &ol_bin, args.dry_run)?;
    }
    if !args.no_project_md {
        install_project_md(&ol_bin, args.dry_run)?;
    }

    if args.dry_run {
        println!("\n(dry-run: nothing was written)");
    } else {
        println!("\nInstallation complete. Claude Code will use `ol` as your knowledge base.");
        println!("Reload any open Claude Code sessions to pick up the new skill.");
    }
    Ok(())
}

pub fn run_uninstall(args: InstallArgs) -> Result<()> {
    let claude_dir = claude_dir()?;

    if !args.no_skill {
        uninstall_skill(&claude_dir, args.dry_run)?;
    }
    if !args.no_hooks {
        uninstall_hooks(&claude_dir, args.dry_run)?;
    }
    if !args.no_project_md {
        uninstall_project_md(args.dry_run)?;
    }

    if args.dry_run {
        println!("\n(dry-run: nothing was written)");
    } else {
        println!("\nUninstall complete.");
    }
    Ok(())
}

// --- Skill ---

fn install_skill(claude_dir: &Path, ol_bin: &str, dry_run: bool) -> Result<()> {
    let skills_dir = claude_dir.join("skills");
    let skill_path = skills_dir.join("ol-kb.md");

    let content = skill_file_content(ol_bin);

    if dry_run {
        println!("Would write skill: {}", skill_path.display());
        println!("--- ol-kb.md ---");
        println!("{content}");
        println!("---");
    } else {
        std::fs::create_dir_all(&skills_dir).context("failed to create ~/.claude/skills/")?;
        std::fs::write(&skill_path, &content)
            .with_context(|| format!("failed to write {}", skill_path.display()))?;
        println!("Wrote skill: {}", skill_path.display());
    }
    Ok(())
}

fn uninstall_skill(claude_dir: &Path, dry_run: bool) -> Result<()> {
    let skill_path = claude_dir.join("skills").join("ol-kb.md");
    if skill_path.exists() {
        if dry_run {
            println!("Would remove: {}", skill_path.display());
        } else {
            std::fs::remove_file(&skill_path)?;
            println!("Removed: {}", skill_path.display());
        }
    } else {
        println!("Skill not installed, skipping.");
    }
    Ok(())
}

fn skill_file_content(ol_bin: &str) -> String {
    format!(
        r#"# ol - Operating Layer Knowledge Base

Use this skill when the user asks about people, meetings, projects, or past decisions — or when context from the knowledge base would help with the current task.

## When to use

- Starting work on a feature: search for related past meetings and project context
- Mentioning a colleague by name: look them up to get contact details and links
- Referencing a past decision: search meetings and memory for relevant context
- After learning something important: save it to memory so future sessions benefit
- Exploring an unfamiliar codebase: check if it's indexed, search symbols

## Commands

```bash
# Memory (persistent key/value, survives across sessions)
{ol} memory set <key> "<value>" [--type user|feedback|project|reference] [--tags tag1,tag2]
{ol} memory get <key>
{ol} memory search "<query>"
{ol} memory list [--type feedback]

# People
{ol} people search "<name or email>"
{ol} people add --name "<name>" --email "<email>" [--slack-url <url>] [--github-username <handle>]
{ol} people get <id>

# Meetings
{ol} meeting search "<topic>"
{ol} meeting similar "<description>"   # semantic search (requires --embed on add)
{ol} meeting add --title "<title>" --date <YYYY-MM-DD> --notes "<notes>" [--embed]
{ol} meeting link-person <meeting-id> <person-id> [--role <role>]

# Projects
{ol} project search "<name or description>"
{ol} project get <id>
{ol} project add --name "<name>" --description "<desc>" [--link "url|label"]

# Repos
{ol} repo show [path] [--symbols "<query>"]   # explore indexed symbols
{ol} repo search "<description>"
{ol} repo similar "<description>"             # semantic repo search

# Global search across all tables
{ol} search "<query>"
```

## Usage patterns

**Before starting a task:**
```bash
{ol} search "<feature or area>"
{ol} meeting search "<feature or area>"
```

**When a colleague is mentioned:**
```bash
{ol} people search "<name>"
```

**After a decision is made:**
```bash
{ol} memory set "<slug>" "<decision and rationale>" --type project
```

**After learning a team preference:**
```bash
{ol} memory set "<slug>" "<preference>" --type feedback
```
"#,
        ol = ol_bin
    )
}

// --- Hooks ---

const HOOK_COMMAND_PREFIX: &str = "# ol-kb-hook";

fn install_hooks(claude_dir: &Path, ol_bin: &str, dry_run: bool) -> Result<()> {
    let settings_path = claude_dir.join("settings.json");
    let mut settings = read_settings_json(&settings_path)?;

    let hook_command = format!(
        "{HOOK_COMMAND_PREFIX}\n{ol_bin} repo index . --description \"$(basename \"$PWD\") repo\" 2>/dev/null || true",
        ol_bin = ol_bin
    );

    // Build the hook entry
    let new_hook = serde_json::json!({
        "type": "command",
        "command": hook_command
    });

    // Get or create hooks.Stop array
    let hooks = settings
        .as_object_mut()
        .context("settings.json is not an object")?
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));

    let stop_hooks = hooks
        .as_object_mut()
        .context("hooks is not an object")?
        .entry("Stop")
        .or_insert_with(|| serde_json::json!([]));

    let stop_array = stop_hooks
        .as_array_mut()
        .context("hooks.Stop is not an array")?;

    // Remove any existing ol hook, then append fresh
    remove_ol_hooks_from_stop(stop_array);

    // Wrap in a matcher object (empty matcher = all sessions)
    let matcher_entry = serde_json::json!({
        "hooks": [new_hook]
    });
    stop_array.push(matcher_entry);

    if dry_run {
        println!("Would update hooks in: {}", settings_path.display());
        println!("  hooks.Stop: add `ol repo index .` command");
    } else {
        write_settings_json(&settings_path, &settings)?;
        println!("Updated hooks: {}", settings_path.display());
        println!("  Stop hook: `ol repo index .` after each session");
    }
    Ok(())
}

fn uninstall_hooks(claude_dir: &Path, dry_run: bool) -> Result<()> {
    let settings_path = claude_dir.join("settings.json");
    if !settings_path.exists() {
        println!("No settings.json found, skipping hook removal.");
        return Ok(());
    }

    let mut settings = read_settings_json(&settings_path)?;

    let modified = if let Some(stop) = settings
        .pointer_mut("/hooks/Stop")
        .and_then(|v| v.as_array_mut())
    {
        let before = stop.len();
        remove_ol_hooks_from_stop(stop);
        stop.len() < before
    } else {
        false
    };

    if modified {
        if dry_run {
            println!(
                "Would remove ol Stop hook from: {}",
                settings_path.display()
            );
        } else {
            write_settings_json(&settings_path, &settings)?;
            println!("Removed ol Stop hook from: {}", settings_path.display());
        }
    } else {
        println!("No ol hooks found in settings.json, skipping.");
    }
    Ok(())
}

fn remove_ol_hooks_from_stop(stop_array: &mut Vec<Value>) {
    stop_array.retain(|entry| {
        let hooks = match entry.get("hooks").and_then(|h| h.as_array()) {
            Some(h) => h,
            None => return true,
        };
        !hooks.iter().any(|h| {
            h.get("command")
                .and_then(|c| c.as_str())
                .map(|c| c.contains(HOOK_COMMAND_PREFIX))
                .unwrap_or(false)
        })
    });
}

// --- Project CLAUDE.md ---

fn install_project_md(ol_bin: &str, dry_run: bool) -> Result<()> {
    let claude_md_path = PathBuf::from("CLAUDE.md");
    let content = project_claude_md_content(ol_bin);

    if claude_md_path.exists() {
        // Check if ol section already present
        let existing = std::fs::read_to_string(&claude_md_path)?;
        if existing.contains("<!-- ol-kb -->") {
            println!("CLAUDE.md already has ol-kb section, skipping.");
            return Ok(());
        }
        // Append ol section
        let appended = format!("{existing}\n{content}");
        if dry_run {
            println!(
                "Would append ol-kb section to: {}",
                claude_md_path.display()
            );
        } else {
            std::fs::write(&claude_md_path, appended)?;
            println!("Appended ol-kb section to: {}", claude_md_path.display());
        }
    } else {
        if dry_run {
            println!("Would create: {}", claude_md_path.display());
            println!("--- CLAUDE.md ---");
            println!("{content}");
            println!("---");
        } else {
            std::fs::write(&claude_md_path, &content)?;
            println!("Created: {}", claude_md_path.display());
        }
    }
    Ok(())
}

fn uninstall_project_md(dry_run: bool) -> Result<()> {
    let claude_md_path = PathBuf::from("CLAUDE.md");
    if !claude_md_path.exists() {
        println!("No CLAUDE.md found, skipping.");
        return Ok(());
    }

    let content = std::fs::read_to_string(&claude_md_path)?;
    if !content.contains("<!-- ol-kb -->") {
        println!("No ol-kb section in CLAUDE.md, skipping.");
        return Ok(());
    }

    // Remove the ol-kb section (everything from the marker to the next H2 or EOF)
    if let Some(start) = content.find("<!-- ol-kb -->") {
        let trimmed = content[..start].trim_end().to_string();
        if dry_run {
            println!("Would remove ol-kb section from CLAUDE.md");
        } else {
            std::fs::write(&claude_md_path, format!("{trimmed}\n"))?;
            println!("Removed ol-kb section from CLAUDE.md");
        }
    }
    Ok(())
}

fn project_claude_md_content(ol_bin: &str) -> String {
    format!(
        r#"<!-- ol-kb -->
# Knowledge Base (ol)

This project uses `ol` as a local knowledge base. Before starting work, search for relevant context:

```bash
{ol} search "<topic>"          # search across all tables
{ol} meeting search "<topic>"  # find past decisions in meeting notes
{ol} repo show . --symbols "<name>"  # find symbols in this codebase
```

Save important learnings during or after your session:

```bash
{ol} memory set "<key>" "<value>" --type feedback
{ol} memory set "<key>" "<value>" --type project
```

The `ol-kb` skill has full command reference.
"#,
        ol = ol_bin
    )
}

// --- Helpers ---

fn claude_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not find home directory")?;
    Ok(home.join(".claude"))
}

fn current_binary_path() -> Result<String> {
    let path = std::env::current_exe().context("could not determine current binary path")?;
    Ok(path.to_string_lossy().into_owned())
}

fn read_settings_json(path: &Path) -> Result<Value> {
    if path.exists() {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("failed to parse {}", path.display()))
    } else {
        Ok(serde_json::json!({}))
    }
}

fn write_settings_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content =
        serde_json::to_string_pretty(value).context("failed to serialize settings.json")?;
    std::fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))
}
