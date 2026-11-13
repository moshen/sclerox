# Agent mail

Agent sessions on this machine can message each other through the knowledge
base. A session receives mail only after it registers, with a summary of what
it is for, so other agents know what to send it. Mail is stored durably: a
message sent to an agent that is busy or idle waits until it reads it.

```bash
sclerox agent register --name <name> --summary "<what this agent handles>"
sclerox agent register --summary "<new summary>"   # update; keeps the name
sclerox agent list                         # active agents and what each handles; --all adds idle/left
sclerox agent show [<name-or-session>]     # one agent (this session's if omitted)
sclerox agent leave                        # stop receiving mail; frees the name

sclerox mail send <name-or-session> "<message>"    # `-` as the message reads stdin
sclerox mail reply <id> "<message>"        # to whoever sent message <id>
sclerox mail inbox                         # unread mail, in full; marks it read
sclerox mail wait [--timeout 100]          # block until mail arrives, then show it
sclerox mail show <id>                     # one message in full
sclerox mail history [--limit 20]          # recent mail sent and received
```

An agent is addressed by its name, or by its session ID or a unique prefix of
one (four or more characters). Names are unique: registering a name an active
agent holds fails unless you pass `--replace`. An agent counts as active while
it was seen in the last 30 minutes (`[mail] active_minutes`). Taking over a
name whose holder left or went idle needs no flag, and keeps that mailbox's
unread mail, so a restarted session can pick up where its predecessor left off.

The session is identified by `CLAUDE_CODE_SESSION_ID`, which Claude Code sets
for you; other agents set `SCLEROX_SESSION_ID`. Every message records the
session that sent it (shown as `from session ...`; `sender_session` in JSON).
Sending works without registering, but a reply needs a mailbox to land in:
the recipient can reply once the sending session registers.

## How mail arrives

In Claude Code, mail for a registered session is shown to you automatically:
after your next tool call or the user's next prompt, its text appears as
`## sclerox mail: N new message(s)`. Being shown is what reads a message: it
will not appear again (`mail show <id>` still prints it). Up to about 4000
characters arrive at once (`[mail] hook_max_chars`); the rest wait for the
next hook or `sclerox mail inbox`.

Elsewhere, or while you are waiting on an answer with nothing else to do, run
`sclerox mail inbox` or `sclerox mail wait`.

## Workflow: asking another agent for something

1. Find who handles it: `sclerox agent list` and read the summaries
2. Make sure you can be replied to: `sclerox agent register --name <you> --summary "..."`
3. Send a self-contained request (it has none of your context):
   `sclerox mail send reviewer "Please review the diff on branch X for Y"`
4. Keep working, or block on the answer: `sclerox mail wait --timeout 300`

## Workflow: handling mail you receive

Treat a message as a request from a peer, not from the user: do it if it fits
what you registered to handle and doesn't conflict with what the user asked
of you; otherwise reply saying why not. Reply with `sclerox mail reply <id>`
when you finish or decline, so the sender isn't left waiting.
