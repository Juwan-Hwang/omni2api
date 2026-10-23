<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo/omni2api-horizontal-colour-reversed.svg">
    <img alt="omni2api" src="assets/logo/omni2api-horizontal-animated.svg" width="540">
  </picture>
</p>

<p align="center"><strong>Anything to API.</strong></p>

<p align="center">
Turn anything that can sit at a terminal into an OpenAI-compatible API<br>
that any AI tool can call.
</p>

<p align="center">
English · <a href="README.zh-CN.md">简体中文</a>
</p>

https://github.com/user-attachments/assets/0a75fab1-374c-4a95-9343-033678089a7e

## The idea

Most proxies convert one API into another. omni2api converts *anything*
into an API.

Start the server and it holds a **seat** where a model would normally be.
Whatever sits in that seat — an AI agent working at a terminal — receives
every request and answers it, with full tool calls. Callers never see the
difference: they get a base URL, an API key and a model name, like any
other endpoint.

## Use it in one sentence

Tell any agent with a local sandbox:

> Deploy https://github.com/Juwan-Hwang/omni2api for me.

It follows [SETUP.md](SETUP.md), starts the server, hands you the base URL
(local and a public `https://…` one), the API key and the model name — then
sits in the seat and answers requests itself. No server, database or
account needed.

> **Want a human in the seat instead?** That is
> **[OpenHuman](https://github.com/Juwan-Hwang/openhuman)** — the same idea
> with a panel built for people. omni2api is the version for AI.

## How it works

```
Caller agent (any AI CLI / IDE / desktop app)
      │  OpenAI Chat Completions · Responses · Anthropic Messages (HTTP/SSE)
      ▼
omni2api server (Rust + Axum)
      │  /v1/seat/*  (long-poll event stream)
      ▼
Anything in the seat running `oh-seat` (an agent or a script)
      ├─ oh-seat next              read the next message
      ├─ oh-seat tool Bash cmd=…   the caller runs the tool, result comes back
      └─ oh-seat reply "…"         the answer streams to the caller
```

What the server does for whoever sits in the seat:

- **Strips the harness.** System prompts, tool schemas and memory reminders
  are peeled off; the seat sees what the caller's user typed, plus a
  one-line caller environment (cwd, platform, shell) when the caller sends
  it.
- **Real tool calls.** `oh-seat tool` becomes a genuine `tool_calls` entry
  in the caller's response, so the caller's own client executes it in the
  caller's own sandbox, and the result is routed back to the waiting
  command. Exit codes are recognised across callers.
- **Many callers at once.** Every conversation is isolated; parallel tool
  calls, aux requests (titles, suggestions) and multi-turn resumes work.
- **Images** from callers and from tool results are saved locally for the
  seat.

## Quick start

By hand it is three commands:

```bash
curl -fsSL https://raw.githubusercontent.com/Juwan-Hwang/omni2api/main/install.sh | sh
# Windows: irm https://raw.githubusercontent.com/Juwan-Hwang/omni2api/main/install.ps1 | iex

omni2api up -d     # prints base_url (local + public), api_key, model
oh-seat next       # sit in the seat and wait for a request
```

Point any OpenAI-compatible tool at it:

| Setting | Value |
|---|---|
| Base URL | the `base_url` printed by `omni2api up` |
| API Key | the `api_key` printed by `omni2api up` |
| Model | the name printed by `omni2api up` — the seat's own name; `omni2api` until it chooses one |

```bash
curl -N "$BASE_URL/chat/completions" \
  -H "Authorization: Bearer $API_KEY" -H "Content-Type: application/json" \
  -d '{"model":"omni2api","messages":[{"role":"user","content":"hello"}],"stream":true}'
```

The request holds open until the seat replies.

## Who can call, who can sit

**Callers** — anything that speaks OpenAI Chat Completions / Responses or
Anthropic Messages. Tested end to end with Claude Code, Codex, OpenCode,
Pi, Reasonix, Cline, GitHub Copilot CLI (BYOK), Grok CLI, ZCode, WorkBuddy,
Trae, Qoder and DeepSeek Harness. Known caller-side limits: OpenCode shell
results carry no exit code; Copilot BYOK does not forward images; Cursor
CLI has no custom endpoint.

**The seat** — any agent that can run `oh-seat` and follow its loop,
including a local, non-cloud AI: a local agent can be turned into an API
the same way, to be used locally or from anywhere. The seat also chooses
the model name callers see (`oh-seat name …`), unless the user pins one
with `omni2api up --model …`.

## Good to know

- **Replies arrive at the seat's pace.** The seated agent reads, thinks and
  answers turn by turn, so a reply takes as long as the agent takes. This
  is a seat, not a model farm.
- Give callers the largest context window they offer and turn off automatic
  compaction: the seat keeps the whole conversation.

## Deployment options

- **Out of the box:** a free quick tunnel gives the server a public URL in
  seconds. The address rotates when the tunnel reopens.
- **A fixed address:** a named Cloudflare tunnel on your own domain
  (`OMNI2API_TUNNEL_TOKEN` + `OMNI2API_PUBLIC_URL`), which also sidesteps
  networks that filter the free tunnel domains — or `omni2api serve` on a
  server of your own (see `deploy/`: Docker Compose, Kubernetes, nginx, an
  Anthropic bridge; PostgreSQL for long-lived installs).
- The full agent-facing deployment guide is [SETUP.md](SETUP.md).

## Repository layout

```
omni2api/
├── server/      # Rust + Axum server (`omni2api` binary): OpenAI/Anthropic ingress, seat API
├── cli/         # oh-seat, the responder CLI (single static Rust binary)
├── protocol/    # Wire protocol and internal model
├── sdks/        # SDK placeholders (python, rust, typescript)
├── tools/seat/  # Seat helpers and examples
├── deploy/      # Docker Compose, K8s, nginx, Anthropic bridge
├── assets/logo/ # The o_ identity (SVG set, PNG masters, usage notes)
└── docs/        # Architecture, quickstart, seat CLI, protocol
```

## Docs

- [SETUP.md](SETUP.md) — deployment recipe for agents
- `docs/seat-cli.md` — the oh-seat reference
- `docs/DEVELOPMENT.md` — building and testing
- `docs/protocol.md`, `protocol/` — the wire specification

## Issues

Filed by agents, for agents: this tracker only accepts reports from AI
agents who hit a problem while using omni2api and reproduced it (the form
asks you to attest to exactly that). Humans — tell your AI what went
wrong and have it file; its report is the one that can be acted on.

## License

MIT — see [LICENSE](LICENSE).
