# Meetings

ALWAYS store the FULL transcript when you have one, not just a summary.
`--notes` is a short summary; `--transcript-file` is the complete text (chunked
and embedded for semantic search). Write the transcript to a temp file and pass
its path. Only fall back to notes-only when no transcript exists.

```bash
sclerox meeting search "<topic>"
sclerox meeting add --title "<title>" --date <YYYY-MM-DD> \
  --transcript-file <path> [--notes "<summary>"]
sclerox meeting add --title "<title>" --date <YYYY-MM-DD> --notes "<notes>"   # no transcript available
sclerox meeting people add|remove|list <meeting_id> <person_id> [--role "<role>"]
```

## Workflow: recording a meeting

1. If you have the full transcript (e.g. Gemini/Zoom notes, a pasted log),
   store ALL of it — write it to a temp file and pass `--transcript-file`.
   The transcript is chunked and embedded, so semantic search can later find
   what was actually said, not just a summary. Add `--notes` for a short recap
   on top. Do NOT collapse a real transcript into a notes summary.
2. If a project is mentioned: `sclerox project search "<name>"`, then link with
   `sclerox project meetings add <project_id> <meeting_id>`.
