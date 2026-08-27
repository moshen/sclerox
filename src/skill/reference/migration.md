# Migrating from `ol`

`ol` was renamed to `sclerox` and moved to XDG paths. Subcommands and flags
are unchanged: `ol todo list` becomes `sclerox todo list`.

One command does the whole move:

```bash
sclerox migrate --dry-run          # show every action, change nothing
sclerox migrate                    # do it
sclerox install                    # either order works
```

`sclerox migrate` is hidden from `sclerox --help`. `sclerox install` prints a
pointer to it when it finds an old install.

## Order does not matter

Install and migrate can run in either order, and either one moves you onto the
new config.

If `~/.ol/config.toml` is still there when `sclerox install` runs, install
adopts it: the file moves to `~/.config/sclerox/config.toml`, your values are
kept, and the surrounding template is refreshed with any keys added since. It
prints what it did. Writing a fresh default instead would claim the destination
and make a later migrate decline to overwrite it, stranding your settings at
the old path with nothing reporting a problem.

A config already at the new path is never overwritten by either command. When
both exist and the current one has settings of its own, both files are kept and
the conflict is reported:

```
kept ~/.config/sclerox/config.toml — it has settings of its own,
so ~/.ol/config.toml was left in place.
    Merge the two by hand, then delete the old file.
```

The one exception is a generated template with nothing set in it. That carries
no intent, so migration replaces it with your real config rather than treating
it as a conflict.

## Back up first

`sclerox migrate` **moves** files rather than copying them. The database is the
only irreplaceable thing; indexes and logs can be rebuilt.

```bash
cp -Rp ~/.ol ~/.ol.backup-$(date +%F)
shasum -a256 ~/.ol/ol.db ~/.ol.backup-*/ol.db     # confirm the copy matches
```

Back up the files migration rewrites too, since they are edited in place:

```bash
cp -p ~/.claude/settings.json ~/.claude/settings.json.bak
cp -p ~/.claude/CLAUDE.md ~/.claude/CLAUDE.md.bak
```

Check nothing is mid-write before starting. The old session hooks spawn
background distillation processes, and migration is deliberately not automatic
so it cannot race one:

```bash
pgrep -fl 'ol hook' || echo "no writers"
```

## What migrate does

**Global paths**

| Old | New |
| --- | --- |
| `~/.ol/config.toml` | `~/.config/sclerox/config.toml` |
| `~/.ol/ol.db` | `~/.local/share/sclerox/sclerox.db` |
| `~/.ol/logs/ol-DATE.log` | `~/.local/state/sclerox/logs/sclerox-DATE.log` |
| `~/.ol/distilled/` | `~/.local/state/sclerox/distilled/` |

**Per-repo indexes.** Every registered repo's `<repo>/.ol/` becomes
`<repo>/.sclerox/`, and the registry row is repointed at it. The index survives
the rename, so this is never a re-index.

**Per-folder opt-out markers.** A folder that set `index = false` is never in
the registry, so those are found by walking up from registered repos.

**Stale integrations.** The `ol-kb` skill directory, `# ol-kb-hook` entries in
`settings.json`, and `<!-- ol-kb -->` doc sections. Every marker that gated "is
this already installed?" changed name, so without this `sclerox install` writes
new artifacts alongside the old ones: duplicate hooks, two competing skill
directories, an orphaned doc section.

## What migrate does NOT do

**Remove the old `ol` binary.** This is the one that bites, because it fails
silently. With its database moved, `ol` does not error: it creates a fresh
empty `~/.ol/ol.db` and returns no results with exit 0. Anything still calling
`ol ...` reads an empty knowledge base and reports "not found", which is
indistinguishable from a real miss. Migrate warns when it finds one:

```bash
cargo uninstall ol                 # or remove it from PATH
which ol || echo "gone"
```

**Update your own docs and skills.** Any `ol ...` command in your CLAUDE.md
files, skills, scripts, or aliases needs rewriting to `sclerox`. Find them:

```bash
grep -rnE '(^|[^a-zA-Z0-9_/.-])ol ' ~/.claude/skills/ ~/.claude/CLAUDE.md
```

Rewrite command invocations only. A blind `ol` to `sclerox` substitution
corrupts any word containing those letters, and historical notes that mention
`ol` are a record of what the tool was called at the time, not instructions to
fix.

**Delete `~/.ol`.** The directory is removed only if migration emptied it.
Anything migration does not claim, such as an `ol.db.pre-v12-backup`, keeps it
alive. That is deliberate: those are your files.

## Verify it worked

Compare row counts against the backup and confirm the database is sound:

```bash
DB=~/.local/share/sclerox/sclerox.db
for t in memory todos people meetings projects repos; do
  printf '%-10s ' "$t"; sqlite3 "$DB" "SELECT COUNT(*) FROM $t;"
done
sqlite3 "$DB" 'PRAGMA integrity_check;'
```

The database checksum WILL differ from your backup even on a clean migration:
SQLite folds the write-ahead log into the main file when it closes. Row counts
and `integrity_check` are the real signals, not the hash.

Confirm exactly one set of hooks, no leftover `ol` markers, and a working
index:

```bash
jq '.hooks' ~/.claude/settings.json
grep -c 'ol-kb-hook' ~/.claude/settings.json          # expect 0
ls -d ~/.claude/skills/*-kb                           # expect sclerox-kb only
sclerox search "<something you know is stored>"
sclerox code search "<a symbol in an indexed repo>"
```

Re-running `sclerox migrate` is safe. It reports "Nothing to migrate" once
everything is on the new layout.

## Troubleshooting

**`install` keeps telling me to migrate.** Fixed in current versions, which
report pending work only when something migration would actually move is still
in place. On an older build this fires whenever `~/.ol` exists at all, even
holding nothing but a backup file. Cross-check with `sclerox migrate`: if it
says "Nothing to migrate", believe it.

**A repo says "directory no longer exists".** A registry row outlived its
folder. `sclerox repo sync` prunes those. Note that sync also consolidates
nested registrations, so it can remove more entries than you expect; read its
output.

**A repo says "no `.sclerox/repo.db` after rename".** The folder is there but
its index is not. Rebuild it with `sclerox repo index <path>`.

**Searches return nothing after migrating.** Almost always the old binary still
on PATH, silently reading an empty database. Check `which ol` first.
