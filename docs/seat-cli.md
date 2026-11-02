# oh-seat — the Agent Seat CLI

`oh-seat` lets an AI agent sit in the Omni2API **responder** seat from any
terminal: callers (Claude Code, Codex, opencode, pi, any OpenAI-compatible
client) send messages, and the agent answers them and runs tools on the
caller's machine. It is designed for AI agents: every choice below favours an agent driving a shell one
command at a time.

It is a single static Rust binary (like `gh` and `codex`): no runtime, no
dependencies, starts instantly. Source: `cli/`. Build: `cargo build --release`
in `cli/`; releases (with `omni2api`) build on tags `v*`, and `install.sh` /
`install.ps1` install both.

## Setup

With `omni2api up` running on the same machine nothing is needed: oh-seat
reads the URL and responder key from `~/.omni2api/endpoint.json`.

For another server:

```
export OH_SEAT_URL=https://your-server            # default: local omni2api, else http://127.0.0.1:8080
export OH_SEAT_KEY_FILE=~/.omni2api/seat_key      # a responder-scope ohk_ key
```

`OH_SEAT_KEY` works too but puts the key in shell history. Remote URLs
honor the standard proxy variables (`ALL_PROXY` / `HTTPS_PROXY` /
`HTTP_PROXY`) — a seat inside a proxied sandbox can still reach a public
server; loopback targets and `NO_PROXY` matches are never proxied. State (cursor,
active conversation, buffered events) lives in `~/.omni2api/seat-state.json`
(`--state` / `OH_SEAT_STATE`). Each state file is its own consumer with its
own server-side cursor, so several agents can read the same seat.

## The loop

```
oh-seat next                          # block until a caller message; prints it, makes it active
oh-seat tool Bash command='ls -la'    # caller runs it; prints the output
oh-seat reply 'Done.' --next          # answer, then wait for the next message
```

| command | what it does |
|---|---|
| `next [--timeout S] [--all]` | Wait for a message (default 100 s, exit 2 if none). First sight of a conversation also prints its tool names. |
| `reply TEXT \| -f FILE \| -` | Reply in the active (or `-c`) conversation. `--more` keeps the turn open, `-r` adds reasoning, `--next` waits for the next message, `-a FILE` attaches a file (API tasks). |
| `tool NAME key=value…` | Ask the caller to run a tool, wait for its result. Input: `key=value` (value parsed as JSON if valid), `key:=value` (force string), `key=@file`, `key=@-` (stdin), one JSON object, or `-` (whole input from stdin). |
| `result ID` | Keep waiting for a tool result that outlived `tool --timeout`. |
| `caps [--tool NAME [--schema]] [--full] [--diff]` | Tools as signatures (`Bash(command*: string, timeout: number)`), one tool's parameters, the whole system prompt, or what changed. |
| `history`, `convs`, `use CONV`, `save`, `status` | Context, conversation list, switch active, persist, health. |
| `name [NAME]` | Show the model name you answer to, or set your own. A name pinned by the user (`omni2api up --model …`) is locked and cannot be changed from the seat. Setting a name also updates the local `endpoint.json`. |

`--json` on any command prints one JSON object per line.

## Design rules (why it looks like this)

- **One command = one complete step.** `tool` sends and waits; `reply --next`
  answers and waits. Fewer round-trips through the agent's context.
- **The loop must stay coupled to the agent.** `next` runs in the
  foreground so its return re-invokes the agent that answers. A
  backgrounded loop only counts as sitting if the harness wakes the
  agent when it finishes; otherwise requests buffer unanswered while a
  process idles, and the seat is unmanned in every way that matters.
- **Every wait is bounded** by `--timeout` (default 100 s, below common shell
  tool limits). Nothing is lost on timeout: events are buffered in the state
  file with the cursor, and `result ID` / `next` pick them up.
- **No quoting traps.** Text and tool inputs can come from files or stdin
  (`-f -`, `key=@-`), so multi-line commands and quotes never go through the
  shell twice.
- **Auxiliary calls are labelled.** Title/suggestion requests print
  `AUX: answer with …; reply with -c CONV` and never take over the active
  conversation, so a later bare `reply` lands in the real chat. The format
  comes from the server's `expected_format`, with a local fallback.
- **API tasks are labelled too.** A request to another OpenAI endpoint
  (an image, speech, a transcript) prints `API <endpoint>: <what to send
  back>`. File tasks (images, speech): make the file with your own tools
  and answer with `reply -a FILE` (repeat `-a` for several; without `-c` it
  goes to the newest task waiting for a file); text alone is a refusal the
  caller gets as an error. Text tasks (transcriptions): a plain `reply -c`;
  when timings are asked, answer in SRT and the server converts it.
  Embeddings: the texts arrive as `inputs.json`; answer with the vectors as
  JSON (`reply -c CONV -f vectors.json`). A wrong count or length is refused
  at reply time with the reason, so fix and reply again. Moderations: the
  inputs arrive as `inputs.json` (pictures attached); answer per input with
  the categories it violates (`[[], ["violence"]]`) or scores. With no chat
  waiting for an answer, a bare `reply` answers the newest waiting task. Vector store
  search: the store's files are attached; answer with ranked passages
  (`[{"file": "a.txt", "score": 0.9, "text": "..."}]`). Realtime sessions
  are conversations you sit in (they become active, tools work as in a
  chat); a voice turn wants `reply 'transcript' -a answer.wav`. Requests answered within
  85 s keep their real HTTP status; later ones arrive as 200 with the body.
- **Long tool output is capped** (`OH_SEAT_MAX_OUTPUT`, default 30000 chars):
  head and tail are shown, the whole output is saved to
  `~/.omni2api/results/<id>.txt` for grep.
- **Exit codes mean something:** 0 ok, 2 nothing new before the timeout,
  3 network/auth/server (message says which, e.g. key lacks responder scope),
  4 usage.
- **Tool result by id** needs no server endpoint: the per-consumer cursor plus
  the persisted buffer already guarantee a result is never dropped.

## Tests

- Unit: `cargo test` in `cli/`.
- End to end with split keys (caller-scope key for a curl caller, responder
  key for oh-seat, two consumers): `tools/seat/e2e_oh_seat.sh`.
- Verified live 2026-10-21 against Claude Code, Codex and opencode callers.
