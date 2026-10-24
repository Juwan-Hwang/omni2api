# omni2api status JSON

The stable, machine-readable interface for tools that embed omni2api.
To deploy omni2api, follow [SETUP.md](../SETUP.md) instead.

`up --json`,
`status --json` and `down --json` print exactly one JSON object on one line
on stdout (warnings go to stderr). `endpoint.json` holds the same object
minus `running` and `pid`. (A foreground `up --json` prints one more object
each time it replaces a dead tunnel: read stdout as JSON lines.)

```sh
omni2api up -d --json      # waits until ready, then prints the object
omni2api status --json
```

| field | type | meaning |
|---|---|---|
| `schema` | int | contract version, currently `1` |
| `running` | bool | the background `up` is alive and has published its endpoint |
| `pid` | int or null | its process id |
| `model` | string | the model name callers send |
| `local_base_url` | string | `http://127.0.0.1:PORT/v1` |
| `public_base_url` | string or null | public `https://…/v1`, null without a tunnel |
| `tunnel` | string or null | `cloudflare`, `pinggy`, `token`, or null |
| `tunnel_verified` | bool | this machine reached `public_base_url` itself |
| `public_note` | string, optional | why the public URL may not work, in words |
| `caller_key` | string | API key for callers |
| `responder_key` | string | key for the seat; never hand it to callers |
| `seat.OH_SEAT_URL`, `seat.OH_SEAT_KEY` | string | what `oh-seat` needs |
| `error` | string, only when `running` is false | `not running`, `starting: endpoint not published yet`, `did not come up` (with `log`) |
| `stopped_pid` | int or null, `down` only | the process it stopped |

Exit codes: `status` 0 running, 1 not running; `up` 0 ready, 1 failed,
2 bad arguments; `down` 0 always (nothing runs afterwards).

Rules: within one `schema` value fields are only added, never renamed,
retyped or removed; readers must ignore fields they do not know. Anything
incompatible bumps `schema`. The text output without `--json` is for people
and may change any time; do not parse it.
