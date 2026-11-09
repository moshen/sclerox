//! Running git against a chosen repository.

use std::path::Path;
use std::process::Command;

/// What `git rev-parse --local-env-vars` lists: variables that tie a git
/// command to one repository. Git sets them while it runs hooks, where they
/// would otherwise name that repo instead of the one being asked about.
const GIT_REPO_ENV: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CONFIG",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_DIR",
    "GIT_GRAFT_FILE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
    "GIT_REPLACE_REF_BASE",
    "GIT_SHALLOW_FILE",
    "GIT_WORK_TREE",
];

/// A git command that runs on the repo at `dir`, whatever the environment
/// says.
pub fn git_command(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir);
    // A repo's own config can name this as a program to run, and a copied
    // folder brings its `.git/config` along.
    command.args(["-c", "core.fsmonitor=false"]);
    for var in GIT_REPO_ENV {
        command.env_remove(var);
    }
    // Sclerox only reads repos, and may do so from a hook while the user runs
    // git: an optional index refresh could make their command fail on the lock.
    command.env("GIT_OPTIONAL_LOCKS", "0");
    command.stdin(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());
    command
}
