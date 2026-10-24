# Sitting in the seat over MCP

`oh-seat mcp` is the seat as a stdio MCP server, for agents whose host speaks
MCP better than it runs shell commands. Every tool runs the matching
`oh-seat` command, so output, state and behaviour are the CLI's. The terminal
CLI stays the primary way; use MCP when it is the easier one for your host.

Tools: `next`, `call_tool`, `result`, `reply`, `caps`, `history`, `convs`,
`use_conv`, `name`, `status`, `guide`. The loop is `next` → optionally
`call_tool` → `reply` → `next`.

## Register it

The server reads `~/.omni2api/endpoint.json` written by `omni2api up` (or
`$OMNI2API_HOME/endpoint.json`). Pass `OMNI2API_HOME` when the host runs it
with a different `HOME`.

| Host | Register | Tools appear as |
| --- | --- | --- |
| Claude Code | `claude mcp add seat -- oh-seat mcp` | `mcp__seat__next` (may sit behind ToolSearch) |
| Codex | `codex mcp add seat -- oh-seat mcp` | `mcp__seat__next` |
| OpenCode | `"mcp": {"seat": {"type": "local", "command": ["oh-seat", "mcp"]}}` in `opencode.json` | `seat_next` |
| GitHub Copilot CLI | `copilot mcp add seat -- oh-seat mcp` | `seat-next` |
| Cline CLI | `cline mcp add seat --yes -- oh-seat mcp` | `seat__next` |
| Grok CLI | `grok mcp add seat -- oh-seat mcp` | via `search_tool`, then `use_tool` with `seat__next` |
| Reasonix | `reasonix mcp add seat -- oh-seat mcp` | via `use_capability`: `mcp-tool:seat/next` |
| ZCode CLI | `mcp.servers.seat = {"type": "stdio", "command": "oh-seat", "args": ["mcp"]}` in `~/.zcode/cli/setting.json` | `mcp__seat__next` |

All eight were tested end to end (caller message → `next` → `call_tool` with a
failing command → `reply` → caller gets the answer). Pi has no MCP; use the CLI.

## Waiting

Hosts abort tool calls on their own clock (ZCode 30 s, Cline 60 s), so a wait
lasts at most 25 s and then says "nothing new, call `next` again". That is
normal: keep calling `next` while on duty. A call the host cancels stops its
wait without losing the message. `OH_SEAT_MCP_WAIT` raises the default and the
cap together, only for a host you know waits longer.

## Upgrading

Reasonix pins tool definitions and holds back changed ones: after upgrading
`oh-seat`, run `reasonix mcp trust seat` if `next` or `call_tool` goes missing.
