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
                install_skill(&dir.join("skills"), "ol-kb.md", "claude", args.dry_run)?;
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
                install_skill(&dir.join("skills"), "ol-kb.md", "opencode", args.dry_run)?;
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
                remove_if_exists(
                    &dir.join("plugins").join("ol-session.js"),
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

fn install_skill(dir: &Path, filename: &str, agent_bin: &str, dry_run: bool) -> Result<()> {
    let path = dir.join(filename);
    if dry_run {
        println!("  would write: {}", path.display());
    } else {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        std::fs::write(&path, skill_file_content(agent_bin))
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

fn skill_file_content(agent_bin: &str) -> String {
    format!(
        r#"# ol - Operating Layer Knowledge Base

Use when the user asks about people, meetings, projects, todos, past decisions, research, or code — or when knowledge base context would help.

## When to use

- Starting work: search for related meetings, todos, investigations, project context
- Colleague mentioned: look them up for contact details
- Past decision referenced: search memory and investigations
- After learning something: save to memory for future sessions
- When a memory is wrong or outdated: mark it stale or supersede it
- Looking for code: search symbols across all indexed repos

## Commands

```bash
# Global search (memory, people, meetings, projects, todos, investigations)
ol search "<query>"

# Memory
ol memory set <key> "<value>" --type user|feedback|project|reference|session
ol memory get <key>
ol memory search "<query>"           # active only by default
ol memory search "<query>" --all     # include stale/superseded
ol memory stale <key> [--reason "why it's no longer valid"]
ol memory supersede <old_key> <new_key> "<new_value>"
ol memory review <key>               # confirm memory is still accurate
ol memory needs-review [--days 30]   # list memories not reviewed recently
ol memory distill <key>              # compress verbose entry via {agent_bin}
ol memory distill --from <file>      # extract memories from a file via {agent_bin}
ol memory distill --from <file> --model <model>  # specify model explicitly
ol memory import --agent claude      # import from Claude Code auto-memory
ol memory import --path <dir>        # import from any directory of .md files
ol memory people add|remove|list <key> <person_id>

# People
ol people search "<name or email>"
ol people add --name "<name>" --email "<email>"

# Meetings
ol meeting search "<topic>"
ol meeting add --title "<title>" --date <YYYY-MM-DD> --notes "<notes>"
ol meeting similar "<description>"   # semantic search (needs --embed on add)
ol meeting people add|remove|list <meeting_id> <person_id> [--role "<role>"]

# Todos
ol todo list                         # open todos
ol todo add --title "<title>" [--category slack|github|email|meeting|general]
ol todo update <id> [--title] [--notes] [--deadline] [--category]
ol todo done <id> [--note "<resolution>"]
ol todo watch <id>                   # monitor without action
ol todo reopen <id>
ol todo history [<query>]            # search completed todos
ol todo search "<query>"
ol todo people add|remove|list <todo_id> <person_id>
ol todo projects add|remove|list <todo_id> <project_id>

# Research / Investigations
ol research list                     # open investigations (default)
ol research start --name "<name>" --slug "<slug>" [--plan "<scope>"]
ol research add-source <id> --url "<url>" --label "<label>" [--notes "<notes>"]
ol research sources <id>             # list evidence sources
ol research update <id> [--plan "<text>"] [--findings "<text>"]
ol research conclude <id> --findings "<findings>"
ol research reopen <id>
ol research search "<query>"         # searches name, plan, and findings
ol research people add|remove|list <id> <person_id>
ol research projects add|remove|list <id> <project_id>

# Projects
ol project search "<description>"
ol project get <id>
ol project add --name "<name>" [--description "<desc>"] [--link "url|label"]
ol project people add|remove|list <project_id> <person_id> [--role "<role>"]
ol project meetings add|remove|list <project_id> <meeting_id>
ol project repos add|remove|list <project_id> <repo_id>

# Repos (code)
ol repo list                         # all indexed repos
ol repo search-symbols "<query>"     # search symbols across all repos
ol repo show [path] [--symbols "<query>"]
ol repo index [path]                 # index (or re-index) a repo
ol repo sync                         # heal registry: remove stale, reindex missing
```

## Patterns

**Before any task:** `ol search "<topic>"`
**Finding code:** `ol repo search-symbols "<function or type name>"`
**After a decision:** `ol memory set "<key>" "<decision>" --type project`
**When a memory is wrong:** `ol memory stale <key> --reason "<why>"`
**When a memory is outdated:** `ol memory supersede <old> <new> "<updated value>"`
**After research:** `ol research conclude <id> --findings "<findings>"`
**Attributing a memory:** `ol memory people add <key> <person_id>`
**Session summary:** `ol memory set "session/<YYYY-MM-DD>/<slug>" "<what was done>" --type session`

## Workflows

### When working on a repo

After `ol repo index` or when the user asks about a codebase:
1. Check whether a project already tracks this repo: `ol project search "<repo name>"`
2. **If found:** link if not already linked: `ol project repos add <project_id> <repo_id>`
3. **If not found:** create one from the repo metadata, then link:
   ```
   ol project add --name "<repo name>" --description "<what this service does>" \
     [--link "<git remote url>|GitHub"]
   ol project repos add <new_project_id> <repo_id>
   ```
   Use `git remote get-url origin` to get the remote URL for the project link.

### When starting a research investigation

After `ol research start`:
1. Search for a related project: `ol project search "<investigation topic>"`
2. If found, link the investigation: `ol research projects add <investigation_id> <project_id>`
3. When the investigation concludes, save the key finding as a memory:
   `ol memory set "research/<slug>/finding" "<one-line finding>" --type project`

### When creating a todo that belongs to a project

1. Add the todo: `ol todo add --title "..." --category github`
2. Find or confirm the project: `ol project search "<area>"`
3. Link it: `ol todo projects add <todo_id> <project_id>`

### When meeting notes mention a project

After `ol meeting add`:
1. Search for the project: `ol project search "<project name from notes>"`
2. Link the meeting: `ol project meetings add <project_id> <meeting_id>`
"#,
        agent_bin = agent_bin
    )
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
