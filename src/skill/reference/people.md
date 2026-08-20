# People

ALWAYS use the `--name` flag, never positional: `sclerox people add --name "Alice"`.

```bash
sclerox people search "<name or email or identifier>"
sclerox people add --name "<name>" [--email "<e>"] [--github "<u>"] [--slack "<id>"] [--atlassian "<e>"]
sclerox people get <id>
sclerox people update <id> [--name] [--notes]
sclerox people identifier add <person_id> <type> <value>   # add any identifier
sclerox people types list                                  # see valid identifier types
```
