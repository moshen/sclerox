/// End-to-end tests: invoke the compiled `sclerox` binary against a real SQLite database.
/// Each test gets an isolated temp directory so tests are fully independent.
use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;
use tempfile::TempDir;

// ─── Test harness ────────────────────────────────────────────────────────────

struct Env {
    _dir: TempDir,
    db: PathBuf,
    config: PathBuf,
}

impl Env {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("sclerox.db");
        let config = dir.path().join("config.toml");
        Self {
            _dir: dir,
            db,
            config,
        }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::cargo_bin("sclerox").unwrap();
        c.env("SCLEROX_DB", &self.db);
        // Isolate from the developer's real ~/.config/sclerox/config.toml. The path starts
        // out nonexistent (pure defaults); `sclerox config init` can create it here.
        c.env("SCLEROX_CONFIG", &self.config);
        // Never write test noise into the developer's real ~/.local/state/sclerox/logs/ (an
        // exported SCLEROX_LOG=debug once made a day's log 100x normal size).
        c.env("SCLEROX_LOG", "off");
        c
    }

    /// Run a command and return trimmed stdout. Panics if it fails.
    fn run(&self, args: &[&str]) -> String {
        let out = self.cmd().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "sclerox {} failed:\nstdout: {}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Parse the ID from "Added <noun> #<id>: ..." output.
    fn run_get_id(&self, args: &[&str]) -> i64 {
        let out = self.run(args);
        out.split('#')
            .nth(1)
            .and_then(|s| s.split(':').next())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or_else(|| panic!("could not parse id from: {out}"))
    }
}

// ─── Memory ──────────────────────────────────────────────────────────────────

#[test]
fn memory_set_get_delete() {
    let e = Env::new();
    e.run(&[
        "memory",
        "set",
        "test-key",
        "test value",
        "--type",
        "feedback",
    ]);

    let out = e.run(&["memory", "get", "test-key"]);
    assert!(out.contains("test value"));
    assert!(out.contains("feedback"));

    e.run(&["memory", "delete", "test-key"]);
    e.cmd()
        .args(["memory", "get", "test-key"])
        .assert()
        .stdout(predicate::str::contains("Not found"));
}

#[test]
fn memory_set_warns_but_stores_long_value() {
    let e = Env::new();
    let long_value = "x".repeat(900); // over the 800-char recommendation

    let out = e
        .cmd()
        .args(["memory", "set", "long-key", &long_value])
        .output()
        .unwrap();

    // Warn-and-store: command succeeds, warns on stderr, value is persisted whole.
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("warning"),
        "expected a warning, got: {stderr}"
    );
    assert!(
        stderr.contains("900"),
        "expected the length in the warning: {stderr}"
    );

    let got = e.run(&["memory", "get", "long-key"]);
    assert!(
        got.contains(&long_value),
        "long value was not stored intact"
    );
}

#[test]
fn config_show_reflects_defaults_and_edits() {
    let e = Env::new();

    // No file yet → show reports defaults.
    let out = e.run(&["config", "show"]);
    assert!(out.contains("semantic_threshold = 0.45"), "got: {out}");
    assert!(
        out.contains("using defaults") || out.contains("no file"),
        "got: {out}"
    );

    // path reports the isolated temp location, not created yet.
    let path = e.run(&["config", "path"]);
    assert!(path.contains("config.toml"));

    // init writes the template; a second init refuses without --force.
    let init = e.run(&["config", "init"]);
    assert!(init.contains("wrote"), "got: {init}");
    let reinit = e.cmd().args(["config", "init"]).output().unwrap();
    assert!(
        !reinit.status.success() || {
            // init without --force on an existing file should error or report skip
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&reinit.stdout),
                String::from_utf8_lossy(&reinit.stderr)
            );
            combined.contains("already exists")
        }
    );

    // Edit a key and confirm `show` echoes the new value.
    std::fs::write(&e.config, "[search]\nsemantic_threshold = 0.9\n").unwrap();
    let edited = e.run(&["config", "show"]);
    assert!(edited.contains("semantic_threshold = 0.9"), "got: {edited}");
}

#[test]
fn memory_reembed_backfills_missing() {
    let e = Env::new();
    // Stored without an embedding via --no-embed.
    e.run(&["memory", "set", "k1", "some value", "--no-embed"]);

    // Backfill embeds exactly the one missing entry.
    let out = e.run(&["memory", "reembed"]);
    assert!(out.contains("Embedded 1"), "got: {out}");

    // Nothing left to embed on a second pass (set auto-embeds by default too).
    let out2 = e.run(&["memory", "reembed"]);
    assert!(out2.contains("already embedded"), "got: {out2}");
}

#[test]
fn memory_list_filters_by_type() {
    let e = Env::new();
    e.run(&["memory", "set", "a", "val", "--type", "user"]);
    e.run(&["memory", "set", "b", "val", "--type", "feedback"]);
    e.run(&["memory", "set", "c", "val", "--type", "user"]);

    let user = e.run(&["memory", "list", "--type", "user"]);
    assert!(user.contains("[user] a"));
    assert!(user.contains("[user] c"));
    assert!(!user.contains("[feedback]"));

    let all = e.run(&["memory", "list"]);
    assert!(all.contains("[feedback] b"));
}

#[test]
fn memory_search() {
    let e = Env::new();
    e.run(&[
        "memory",
        "set",
        "rust-pref",
        "prefer Rust for systems code",
        "--type",
        "feedback",
    ]);
    e.run(&[
        "memory",
        "set",
        "py-pref",
        "Python for scripts",
        "--type",
        "feedback",
    ]);

    let out = e.run(&["memory", "search", "Rust"]);
    assert!(out.contains("rust-pref"));
    assert!(!out.contains("py-pref"));
}

#[test]
fn memory_json_output() {
    let e = Env::new();
    e.run(&[
        "memory", "set", "k", "v", "--type", "project", "--tags", "a,b",
    ]);

    let out = e.run(&["--output", "json", "memory", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("invalid JSON");
    assert!(parsed.is_array());
    assert_eq!(parsed[0]["key"], "k");
    assert_eq!(parsed[0]["memory_type"], "project");
}

// ─── People ──────────────────────────────────────────────────────────────────

#[test]
fn people_lifecycle() {
    let e = Env::new();
    let id = e.run_get_id(&[
        "people",
        "add",
        "--name",
        "Alice Smith",
        "--email",
        "alice@example.com",
        "--github",
        "alicegit",
    ]);

    let out = e.run(&["people", "get", &id.to_string()]);
    assert!(out.contains("Alice Smith"));
    assert!(out.contains("alice@example.com"));
    assert!(out.contains("alicegit"));

    let list = e.run(&["people", "list"]);
    assert!(list.contains("Alice Smith"));

    let search = e.run(&["people", "search", "alice"]);
    assert!(search.contains(&format!("#{id}")));

    e.run(&["people", "delete", &id.to_string()]);
    e.cmd()
        .args(["people", "get", &id.to_string()])
        .assert()
        .stdout(predicate::str::contains("not found"));
}

#[test]
fn people_json_output() {
    let e = Env::new();
    e.run(&[
        "people",
        "add",
        "--name",
        "Bob",
        "--email",
        "bob@example.com",
    ]);

    let out = e.run(&["--output", "json", "people", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("invalid JSON");
    assert!(parsed.is_array());
    assert_eq!(parsed[0]["name"], "Bob");
    // email now lives in people_identifiers, not the person row
    assert!(parsed[0].get("email").is_none());
}

// ─── Meetings ────────────────────────────────────────────────────────────────

#[test]
fn meeting_lifecycle() {
    let e = Env::new();
    let person_id = e.run_get_id(&["people", "add", "--name", "Carol"]);
    let meeting_id = e.run_get_id(&[
        "meeting",
        "add",
        "--title",
        "Sprint Planning",
        "--date",
        "2026-06-09",
        "--notes",
        "discussed auth migration and timeline",
    ]);

    let out = e.run(&["meeting", "get", &meeting_id.to_string()]);
    assert!(out.contains("Sprint Planning"));
    assert!(out.contains("2026-06-09"));
    assert!(out.contains("auth migration"));

    e.run(&[
        "meeting",
        "people",
        "add",
        &meeting_id.to_string(),
        &person_id.to_string(),
        "--role",
        "facilitator",
    ]);
    let detail = e.run(&["meeting", "get", &meeting_id.to_string()]);
    assert!(detail.contains("Carol"));
    assert!(detail.contains("facilitator"));

    let search = e.run(&["meeting", "search", "auth"]);
    assert!(search.contains("Sprint Planning"));

    let list = e.run(&["meeting", "list"]);
    assert!(list.contains("Sprint Planning"));
}

#[test]
fn meeting_update_attaches_transcript() {
    let e = Env::new();
    let id = e.run_get_id(&[
        "meeting",
        "add",
        "--title",
        "Weekly Sync",
        "--date",
        "2026-07-01",
        "--notes",
        "short recap",
    ]);

    // Initially no transcript shown.
    let before = e.run(&["meeting", "get", &id.to_string()]);
    assert!(!before.contains("full transcript body"));

    // Attach a transcript from a file via update.
    let tx = e._dir.path().join("transcript.txt");
    std::fs::write(
        &tx,
        "Alice: the full transcript body goes here\nBob: agreed",
    )
    .unwrap();
    e.run(&[
        "meeting",
        "update",
        &id.to_string(),
        "--transcript-file",
        tx.to_str().unwrap(),
    ]);

    // get now shows the transcript, and the notes summary is preserved.
    let after = e.run(&["meeting", "get", &id.to_string()]);
    assert!(
        after.contains("full transcript body"),
        "transcript not stored: {after}"
    );
    assert!(
        after.contains("short recap"),
        "notes were clobbered: {after}"
    );

    // The transcript text is searchable (it was chunked/indexed).
    let hit = e.run(&["meeting", "search", "transcript body"]);
    assert!(hit.contains("Weekly Sync"));
}

// ─── Todos ───────────────────────────────────────────────────────────────────

#[test]
fn todo_full_lifecycle() {
    let e = Env::new();
    let id = e.run_get_id(&[
        "todo",
        "add",
        "--title",
        "Fix the login bug",
        "--category",
        "github",
        "--source-url",
        "https://github.com/org/repo/issues/42",
    ]);

    // Appears in open list
    let list = e.run(&["todo", "list"]);
    assert!(list.contains("Fix the login bug"));
    assert!(list.contains("[ ]"));

    // Update title and notes
    e.run(&[
        "todo",
        "update",
        &id.to_string(),
        "--title",
        "Fix the login redirect bug",
        "--notes",
        "traced to OAuth callback handler",
    ]);
    let detail = e.run(&["todo", "get", &id.to_string()]);
    assert!(detail.contains("Fix the login redirect bug"));
    assert!(detail.contains("OAuth callback handler"));

    // Mark done with resolution
    e.run(&["todo", "done", &id.to_string(), "--note", "fixed in PR #99"]);
    let done_detail = e.run(&["todo", "get", &id.to_string()]);
    assert!(done_detail.contains("[x]"));
    assert!(done_detail.contains("fixed in PR #99"));

    // No longer in open list
    let open = e.run(&["todo", "list", "--status", "open"]);
    assert!(!open.contains("login redirect"));

    // Appears in history
    let hist = e.run(&["todo", "history"]);
    assert!(hist.contains("login redirect"));

    // History search finds it
    let hist_search = e.run(&["todo", "history", "OAuth"]);
    assert!(hist_search.contains("login redirect"));

    // Reopen
    e.run(&["todo", "reopen", &id.to_string()]);
    let reopened = e.run(&["todo", "list", "--status", "open"]);
    assert!(reopened.contains("login redirect"));
}

#[test]
fn todo_watch_items() {
    let e = Env::new();
    let id = e.run_get_id(&["todo", "add", "--title", "Monitor auth latency"]);

    e.run(&["todo", "watch", &id.to_string()]);
    let detail = e.run(&["todo", "get", &id.to_string()]);
    assert!(detail.contains("[~]"));

    // Watch items appear in full list, not in open
    let open = e.run(&["todo", "list", "--status", "open"]);
    assert!(!open.contains("Monitor auth latency"));
    let all = e.run(&["todo", "list", "--status", "all"]);
    assert!(all.contains("Monitor auth latency"));
}

#[test]
fn todo_search() {
    let e = Env::new();
    e.run_get_id(&["todo", "add", "--title", "Review Kubernetes manifests"]);
    e.run_get_id(&["todo", "add", "--title", "Update Python dependencies"]);

    let out = e.run(&["todo", "search", "Kubernetes"]);
    assert!(out.contains("Kubernetes"));
    assert!(!out.contains("Python"));
}

#[test]
fn todo_json_output() {
    let e = Env::new();
    e.run(&[
        "todo",
        "add",
        "--title",
        "JSON test todo",
        "--category",
        "slack",
    ]);

    let out = e.run(&["--output", "json", "todo", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("invalid JSON");
    assert!(parsed.is_array());
    assert_eq!(parsed[0]["title"], "JSON test todo");
    assert_eq!(parsed[0]["category"], "slack");
    assert_eq!(parsed[0]["status"], "open");
}

// ─── Research ─────────────────────────────────────────────────────────────────

#[test]
fn research_full_lifecycle() {
    let e = Env::new();

    // Start
    let id = e.run_get_id(&[
        "research",
        "start",
        "--name",
        "Auth latency spike",
        "--slug",
        "auth-latency",
        "--plan",
        "Investigate why auth latency spiked to 4s on 2026-06-09",
    ]);

    let detail = e.run(&["research", "get", "auth-latency"]);
    assert!(detail.contains("Auth latency spike"));
    assert!(detail.contains("open"));
    assert!(detail.contains("Investigate why"));

    // Add sources
    e.run(&[
        "research",
        "add-source",
        &id.to_string(),
        "--url",
        "https://newrelic.com/query/123",
        "--label",
        "New Relic p99 latency",
        "--notes",
        "shows spike at 14:32",
    ]);
    let with_sources = e.run(&["research", "get", &id.to_string()]);
    assert!(with_sources.contains("New Relic p99 latency"));

    // Conclude
    e.run(&[
        "research",
        "conclude",
        &id.to_string(),
        "--findings",
        "Root cause: connection pool exhaustion. Fixed by increasing pool size to 50.",
    ]);
    let concluded = e.run(&["research", "get", &id.to_string()]);
    assert!(concluded.contains("concluded"));
    assert!(concluded.contains("connection pool exhaustion"));

    // Not in open list
    let open = e.run(&["research", "list"]);
    assert!(!open.contains("auth-latency"));

    let all = e.run(&["research", "list", "--status", "all"]);
    assert!(all.contains("auth-latency"));

    // Reopen
    e.run(&["research", "reopen", &id.to_string()]);
    let reopened = e.run(&["research", "get", &id.to_string()]);
    assert!(reopened.contains("open"));
    // concluded_at should be gone
    assert!(!reopened.contains("Concluded:"));
}

#[test]
fn command_aliases_match_canonical() {
    let e = Env::new();

    // research create/show/close alias the canonical start/get/conclude
    let id = e.run_get_id(&[
        "research",
        "create",
        "--name",
        "Alias probe",
        "--slug",
        "alias-probe",
    ]);
    let shown = e.run(&["research", "show", "alias-probe"]);
    assert!(shown.contains("Alias probe"));
    e.run(&[
        "research",
        "close",
        &id.to_string(),
        "--findings",
        "done via alias",
    ]);
    let concluded = e.run(&["research", "get", &id.to_string()]);
    assert!(concluded.contains("concluded"));

    // memory show aliases get
    e.run(&[
        "memory",
        "set",
        "alias-key",
        "alias value",
        "--type",
        "project",
    ]);
    let mem = e.run(&["memory", "show", "alias-key"]);
    assert!(mem.contains("alias value"));

    // people show aliases get
    let pid = e.run_get_id(&["people", "add", "--name", "Alias Person"]);
    let person = e.run(&["people", "show", &pid.to_string()]);
    assert!(person.contains("Alias Person"));

    // todo show aliases get
    let tid = e.run_get_id(&["todo", "add", "--title", "Alias todo"]);
    let todo = e.run(&["todo", "show", &tid.to_string()]);
    assert!(todo.contains("Alias todo"));
}

#[test]
fn research_search_plan_and_findings() {
    let e = Env::new();
    let id = e.run_get_id(&[
        "research",
        "start",
        "--name",
        "Timeout investigation",
        "--slug",
        "timeouts",
        "--plan",
        "Check Temporal workflow timeouts across services",
    ]);
    e.run(&[
        "research",
        "conclude",
        &id.to_string(),
        "--findings",
        "accounting-doc-worker had 30s limit; increased to 120s",
    ]);

    // Search by plan content
    let by_plan = e.run(&["research", "search", "Temporal"]);
    assert!(by_plan.contains("Timeout investigation"));

    // Search by findings content (hyphenated identifier)
    let by_findings = e.run(&["research", "search", "accounting-doc-worker"]);
    assert!(by_findings.contains("Timeout investigation"));
}

#[test]
fn research_json_output() {
    let e = Env::new();
    e.run(&[
        "research",
        "start",
        "--name",
        "Perf test",
        "--slug",
        "perf-test",
    ]);

    let out = e.run(&["--output", "json", "research", "list", "--status", "open"]);
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("invalid JSON");
    assert!(parsed.is_array());
    assert_eq!(parsed[0]["name"], "Perf test");
    assert_eq!(parsed[0]["status"], "open");
}

// ─── Repos ───────────────────────────────────────────────────────────────────

#[test]
fn repo_index_respects_sclerox_config_opt_out() {
    let e = Env::new();
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".sclerox")).unwrap();
    std::fs::write(dir.path().join(".sclerox/config.toml"), "index = false\n").unwrap();
    std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();

    let out = e.run(&["repo", "index", dir.path().to_str().unwrap()]);
    assert!(
        out.contains("Skipping"),
        "expected opt-out skip, got: {out}"
    );

    // The folder must not have been registered.
    let list = e.run(&["repo", "list"]);
    assert!(
        !list.contains(dir.path().to_str().unwrap()),
        "opted-out folder was registered: {list}"
    );
    // The index db must not have been created (only our config.toml is in .sclerox/).
    assert!(
        !dir.path().join(".sclerox/repo.db").exists(),
        "repo.db created despite opt-out"
    );
}

// ─── Projects ─────────────────────────────────────────────────────────────────

#[test]
fn project_lifecycle() {
    let e = Env::new();
    let person_id = e.run_get_id(&["people", "add", "--name", "Dave"]);
    let meeting_id = e.run_get_id(&[
        "meeting",
        "add",
        "--title",
        "Project Kickoff",
        "--date",
        "2026-06-09",
    ]);
    let project_id = e.run_get_id(&[
        "project",
        "add",
        "--name",
        "Auth Overhaul",
        "--description",
        "Replace legacy auth with OAuth2 PKCE",
        "--link",
        "https://jira.example.com/AUTH|JIRA",
    ]);

    let detail = e.run(&["project", "get", &project_id.to_string()]);
    assert!(detail.contains("Auth Overhaul"));
    assert!(detail.contains("OAuth2 PKCE"));
    assert!(detail.contains("JIRA"));

    e.run(&[
        "project",
        "people",
        "add",
        &project_id.to_string(),
        &person_id.to_string(),
        "--role",
        "lead",
    ]);
    e.run(&[
        "project",
        "meetings",
        "add",
        &project_id.to_string(),
        &meeting_id.to_string(),
    ]);

    let linked = e.run(&["project", "get", &project_id.to_string()]);
    assert!(linked.contains("Dave"));

    let meetings = e.run(&["project", "meetings", "list", &project_id.to_string()]);
    assert!(meetings.contains("Kickoff"));

    let search = e.run(&["project", "search", "OAuth2"]);
    assert!(search.contains("Auth Overhaul"));
}

// ─── Global search ────────────────────────────────────────────────────────────

#[test]
fn global_search_finds_all_types() {
    let e = Env::new();
    e.run(&[
        "memory",
        "set",
        "auth-note",
        "prefer JWT tokens",
        "--type",
        "project",
    ]);
    e.run(&["people", "add", "--name", "Auth Engineer"]);
    e.run(&[
        "meeting",
        "add",
        "--title",
        "Auth Review",
        "--date",
        "2026-06-09",
        "--notes",
        "auth migration plan",
    ]);
    e.run(&[
        "project",
        "add",
        "--name",
        "Auth Service",
        "--description",
        "auth refactor",
    ]);
    e.run(&["todo", "add", "--title", "Fix auth timeout"]);
    e.run(&[
        "research",
        "start",
        "--name",
        "Auth perf",
        "--slug",
        "auth-perf",
    ]);

    let out = e.run(&["search", "auth"]);

    assert!(out.contains("[memory]"), "missing memory result");
    assert!(out.contains("[person]"), "missing person result");
    assert!(out.contains("[meeting]"), "missing meeting result");
    assert!(out.contains("[project]"), "missing project result");
    assert!(out.contains("[todo]"), "missing todo result");
    assert!(out.contains("[research]"), "missing research result");
}

#[test]
fn global_search_json_output() {
    let e = Env::new();
    e.run(&["memory", "set", "widget-note", "widgets are important"]);
    e.run(&["todo", "add", "--title", "Fix widget rendering"]);

    let out = e.run(&["--output", "json", "search", "widget"]);
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("invalid JSON");
    assert!(parsed.is_array());
    assert!(parsed.as_array().unwrap().len() >= 2);

    let types: Vec<&str> = parsed
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["type"].as_str())
        .collect();
    assert!(types.contains(&"Memory"), "missing Memory type");
    assert!(types.contains(&"Todo"), "missing Todo type");
}

// ─── Migration ────────────────────────────────────────────────────────────────

#[test]
fn fresh_database_is_at_current_version() {
    let e = Env::new();
    let out = e.run(&["db", "migrate"]);
    assert!(
        out.contains("up to date"),
        "expected 'up to date', got: {out}"
    );
    assert!(!out.contains("pending"));
}

#[test]
fn migrate_repos_flag() {
    let e = Env::new();
    let out = e.run(&["db", "migrate", "--repos"]);
    assert!(out.contains("No repos") || out.contains("up to date"));
}

// ─── Shell completions ────────────────────────────────────────────────────────

#[test]
fn completions_bash() {
    let e = Env::new();
    let out = e.run(&["completions", "bash"]);
    // bash completions start with a function definition
    assert!(
        out.contains("_sclerox"),
        "expected bash completion function"
    );
}

#[test]
fn completions_zsh() {
    let e = Env::new();
    let out = e.run(&["completions", "zsh"]);
    assert!(!out.is_empty());
}

// ─── Install dry-run ─────────────────────────────────────────────────────────

#[test]
fn install_dry_run_writes_nothing() {
    let e = Env::new();
    // dry-run should succeed and print "would ..."
    e.cmd()
        .args(["install", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("would"))
        .stdout(predicate::str::contains("dry-run: nothing was written"));
}

// ─── Error handling ───────────────────────────────────────────────────────────

#[test]
fn unknown_subcommand_exits_nonzero() {
    let e = Env::new();
    e.cmd().args(["nonexistent-command"]).assert().failure();
}

#[test]
fn get_missing_person_prints_not_found() {
    let e = Env::new();
    e.cmd()
        .args(["people", "get", "9999"])
        .assert()
        .success()
        .stdout(predicate::str::contains("not found"));
}

#[test]
fn todo_done_already_done_is_idempotent() {
    let e = Env::new();
    let id = e.run_get_id(&["todo", "add", "--title", "Idempotent task"]);
    e.run(&["todo", "done", &id.to_string()]);
    // Second done should not error, just print "not found or already done"
    e.cmd()
        .args(["todo", "done", &id.to_string()])
        .assert()
        .success()
        .stdout(predicate::str::contains("already done"));
}

// ─── Performance ──────────────────────────────────────────────────────────────

#[test]
fn commands_stay_fast() {
    use std::time::{Duration, Instant};

    let e = Env::new();

    // Populate with some data
    e.run(&["memory", "set", "perf-key", "performance test value"]);
    e.run(&[
        "people",
        "add",
        "--name",
        "Perf User",
        "--email",
        "perf@test.com",
    ]);
    e.run(&["todo", "add", "--title", "Performance todo"]);
    e.run(&[
        "research",
        "start",
        "--name",
        "Perf research",
        "--slug",
        "perf-research",
    ]);

    // Best-of-N: take the FASTEST of several runs. Tests run in parallel, so a
    // single run can be inflated by a transient scheduler/CI load spike; the
    // best case still reflects true command cost, so a real regression fails it
    // while noise doesn't. (Model load can't be amortized this way — each `sclerox`
    // invocation is a fresh process — which is why `search` is budgeted below.)
    let best_of = |args: &[&str], runs: u32| -> Duration {
        (0..runs)
            .map(|_| {
                let start = Instant::now();
                e.run(args);
                start.elapsed()
            })
            .min()
            .unwrap()
    };

    // Pure SQLite-backed commands must stay fast — this is the real regression
    // guard (e.g. catching an accidental full-table scan).
    let fast: &[(&[&str], &str)] = &[
        (&["memory", "list"], "memory list"),
        (&["people", "list"], "people list"),
        (&["todo", "list"], "todo list"),
        (&["research", "list"], "research list"),
        (&["db", "migrate"], "db migrate"),
    ];
    for (args, label) in fast {
        let t = best_of(args, 3);
        assert!(
            t.as_millis() < 200,
            "{label} took {}ms (best of 3), expected < 200ms",
            t.as_millis()
        );
    }

    // `sclerox search` loads the embedding model on every invocation — a fixed
    // startup cost (~100ms+, hardware-dependent), not a data-size regression.
    // It gets a looser budget that still catches gross slowdowns (seconds).
    let t = best_of(&["search", "perf"], 3);
    assert!(
        t.as_millis() < 2000,
        "search took {}ms (best of 3), expected < 2000ms",
        t.as_millis()
    );
}

// ─── ol -> sclerox migration ─────────────────────────────────────────────────

/// `sclerox install` must adopt a pre-rename `~/.ol/config.toml` rather than
/// writing a default beside it.
///
/// Writing a default would claim the destination, so a later `sclerox migrate`
/// would decline to overwrite it and the user's real settings would stay
/// stranded at the old path with nothing reporting a failure. Adopting also
/// means install and migrate work in either order.
#[test]
fn install_adopts_a_pre_rename_config() {
    let home = TempDir::new().unwrap();
    let legacy = home.path().join(".ol").join("config.toml");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::write(&legacy, "[memory]\nmax_value_chars = 1234\n").unwrap();

    let mut c = Command::cargo_bin("sclerox").unwrap();
    // Pin every path this touches into the temp home. SCLEROX_CONFIG is
    // deliberately NOT set: this exercises real XDG resolution. USERPROFILE is
    // the home variable on Windows, where HOME alone is ignored -- without it
    // `~/.ol` and `~/.claude` would resolve to the runner's real profile.
    c.env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("XDG_DATA_HOME", home.path().join(".local/share"))
        .env("XDG_STATE_HOME", home.path().join(".local/state"))
        .env("SCLEROX_LOG", "off")
        .env_remove("SCLEROX_CONFIG")
        .args(["install", "--target", "claude"]);
    c.assert().success();

    let adopted = home.path().join(".config/sclerox/config.toml");
    let contents = std::fs::read_to_string(&adopted).expect("config at the new path");
    assert!(
        contents.contains("max_value_chars = 1234"),
        "the user's setting survived adoption, got:\n{contents}"
    );
    assert!(
        contents.contains("# cosine_threshold"),
        "template refreshed around the adopted value, got:\n{contents}"
    );
    assert!(!legacy.exists(), "legacy config consumed, not left behind");
}

/// A config already at the new path that the user has edited is never replaced
/// by adoption, and the legacy file is kept for a manual merge.
#[test]
fn install_never_overwrites_an_edited_config() {
    let home = TempDir::new().unwrap();
    let legacy = home.path().join(".ol").join("config.toml");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::write(&legacy, "[memory]\nmax_value_chars = 1234\n").unwrap();

    let current = home.path().join(".config/sclerox/config.toml");
    std::fs::create_dir_all(current.parent().unwrap()).unwrap();
    std::fs::write(&current, "[dedup]\ncosine_threshold = 0.9\n").unwrap();

    let mut c = Command::cargo_bin("sclerox").unwrap();
    c.env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("XDG_DATA_HOME", home.path().join(".local/share"))
        .env("XDG_STATE_HOME", home.path().join(".local/state"))
        .env("SCLEROX_LOG", "off")
        .env_remove("SCLEROX_CONFIG")
        .args(["install", "--target", "claude"]);
    c.assert().success();

    let contents = std::fs::read_to_string(&current).unwrap();
    assert!(
        contents.contains("cosine_threshold = 0.9"),
        "the edited value is untouched, got:\n{contents}"
    );
    assert!(legacy.exists(), "legacy config kept for a manual merge");
}
