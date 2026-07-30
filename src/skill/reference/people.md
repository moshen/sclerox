# People

ALWAYS use the `--name` flag, never positional: `ol people add --name "Alice"`.

```bash
ol people search "<name or email or identifier>"
ol people add --name "<name>" [--email "<e>"] [--github "<u>"] [--slack "<id>"] [--atlassian "<e>"]
ol people get <id>
ol people update <id> [--name] [--notes]
ol people identifier add <person_id> <type> <value>   # add any identifier
ol people types list                                  # see valid identifier types
```
