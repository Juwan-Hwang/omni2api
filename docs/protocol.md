# Omni2API Protocol Specification

## Version

| Component | Version |
|---|---|
| Wire Protocol | `1.0.0` |
| Minimum Server | `0.1.0` |

## Compatibility Strategy

The protocol follows [Semantic Versioning](https://semver.org):

| Client → Server | Compatible? | Behavior |
|---|---|---|
| Same major, any minor/patch | ✅ Fully compatible | All events processed normally |
| Same major, client has higher minor | ✅ Forward-compatible | Unknown server events forwarded to `rawEvents` stream |
| Same major, server has higher minor | ✅ Forward-compatible | New optional fields ignored by older client |
| Different major | ❌ Not compatible | Connection may be rejected |

### Forward Compatibility Implementation

The SDK uses `EventTypeX.fromWireName()` which returns `null` for unknown event types. Unknown events are forwarded to the `rawEvents` stream and ignored by typed sub-streams, ensuring the client never crashes on new event types.

## Transport

- **Human ↔ Server**: WebSocket (`/ws` endpoint)
- **Agent ↔ Server**: HTTP (`POST /v1/chat/completions` with SSE streaming)
- **Serialization**: JSON text frames

## Authentication

The WebSocket connection authenticates via API key as a query parameter:

```
GET /ws?api_key=ohk_xxx
```

The server also accepts `?token=...` and `?authorization=...` (with or without `Bearer ` prefix), but `api_key` is the canonical form.

On success, the server sends a `session.connected` event. On failure, the connection is closed without an event.

## Event Envelope (Server → Client)

All server events share the `Omni2APIEvent` envelope:

```json
{
  "event_id": "evt_0189abc123",
  "event_type": "message.received",
  "conversation_id": "cnv_0189abc123def456",
  "timestamp": "2025-01-15T09:28:00.123Z",
  "data": { ... }
}
```

| Field | Type | Description |
|---|---|---|
| `event_id` | string | Unique event ID (UUIDv7, `evt_` prefixed). Used for deduplication. |
| `event_type` | string | Event type discriminator (see below) |
| `conversation_id` | string | Conversation this event belongs to |
| `timestamp` | ISO 8601 | Server-side event timestamp |
| `data` | object | Event-specific payload |

## Event Types (Server → Client)

| `event_type` | Trigger | Payload |
|---|---|---|
| `session.connected` | WS auth succeeded | `SessionConnectedData` |
| `message.received` | Agent sent a message | `MessageReceivedData` |
| `reply.delivered` | Human's reply delivered to SSE | `ReplyDeliveredData` |
| `capability.updated` | Agent published capabilities | `CapabilityUpdatedData` |
| `tool.result` | Agent returned tool result | `ToolResultData` |
| `async.invocation.created` | A non-streaming request went async | `AsyncInvocationCreatedData` |
| `error` | Protocol or server error | `ErrorData` |

### Payload Schemas

#### `session.connected`
```json
{
  "human_id": "hum_demo_alice",
  "display_name": "Alice Chen"
}
```

#### `message.received`
```json
{
  "message_id": "msg_0189def456",
  "sender_id": "agt_demo_codex",
  "sender_type": "agent",
  "text": "Review the auth module."
}
```

#### `reply.delivered`
```json
{
  "delivered": true
}
```

#### `capability.updated`
```json
{
  "agent_id": "agt_demo_codex",
  "capabilities": [ { "id": "...", "name": "...", ... } ],
  "context": { "agent_id": "...", "system_prompt": "..." }
}
```

#### `tool.result`
```json
{
  "invocation_id": "inv_0189abc",
  "tool_id": "github_create_issue",
  "status": "success",
  "output": { ... },
  "error": null
}
```

#### `async.invocation.created`
```json
{
  "invocation_id": "inv_0189abc",
  "conversation_id": "cnv_0189abc123def456",
  "poll_url": "/v1/replies/inv_0189abc"
}
```

Sent when a non-streaming `POST /v1/chat/completions` returns `202 Accepted` because no reply arrived within the reply window. The client should attach this `invocation_id` to the eventual `reply.send` so the pending HTTP caller is woken.

#### `error`
```json
{
  "code": "tool_not_found",
  "message": "Tool not found in manifest."
}
```

## Client Messages (Client → Server)

Client messages use a tagged union with `event_type` as the discriminator:

| `event_type` | Method | Required |
|---|---|---|
| `reply.send` | `client.sendReply()` | Yes — core chat flow |
| `presence.update` | `client.updatePresence()` | Optional |
| `capability.manifest` | `client.publishCapabilityManifest()` | Optional native extension |
| `tool.invoke` | `client.invokeTool()` | Optional native extension |

### Message Schemas

#### `reply.send`
```json
{
  "event_type": "reply.send",
  "conversation_id": "cnv_xxx",
  "text": "the reply",
  "invocation_id": "inv_0189abc"
}
```

`invocation_id` is optional. Include it when replying to an `async.invocation.created` event so the parked HTTP request is completed; the server falls back to the live reply channel when it is absent.

#### `presence.update`
```json
{
  "event_type": "presence.update",
  "status": "online"
}
```

#### `capability.manifest`
```json
{
  "event_type": "capability.manifest",
  "agent_id": "agt_xxx",
  "capabilities": [ { "id": "...", "name": "...", ... } ],
  "context": { ... }
}
```

#### `tool.invoke`
```json
{
  "event_type": "tool.invoke",
  "conversation_id": "cnv_xxx",
  "invocation_id": "inv_xxx",
  "tool_id": "github_create_issue",
  "input": { "repo": "...", "title": "..." }
}
```

When the human's client sends `tool.invoke` as its reply to a turn, the server does **not** treat it as text. It is parsed into a first-class OpenAI tool call:

| `tool.invoke` field | OpenAI response field |
|---|---|
| `tool_id` | `choices[].message.tool_calls[].function.name` |
| `input` (object) | `choices[].message.tool_calls[].function.arguments` (serialized JSON string) |
| `invocation_id` | `choices[].message.tool_calls[].id` |

`finish_reason` becomes `tool_calls`, so compliant clients execute the tool and post the result back as a `role: tool` message. If the client omits `invocation_id`, the server generates one — the agent's result message will still match, because the id is echoed on both sides of the round trip.

## HTTP Endpoints

| Route | Purpose |
|---|---|
| `POST /v1/chat/completions` | OpenAI-compatible ingress; SSE when `stream: true` |
| `POST /v1/files`, `GET /v1/files`, `GET`/`DELETE /v1/files/{file_id}`, `GET /v1/files/{file_id}/content` | OpenAI-style file store (caller key, multipart `file` + `purpose`); `file_id` resolves in OpenAI `file`/`input_file` and Anthropic `document` (`source.type: file`). Kept in memory, oldest dropped past 512 MB |
| `GET /v1/replies/{invocation_id}` | Long-poll an async reply. `?wait=<secs>` (default 30, max 300) holds the connection and returns as soon as the human replies, otherwise `202` while still pending |

Request headers that change ingress behaviour:

| Header | Effect |
|---|---|
| `X-Omni2api-Mode: async` | Return `202` immediately instead of waiting for a reply |
| `X-Omni2api-Conversation-Id` | Session key for conversation isolation |
| `X-Omni2api-Session-Id` | Lower-priority session key |

## ContentBlock Union

Messages use a tagged union with `"type"` discriminator:

```json
{ "type": "text", "text": "..." }
{ "type": "structured", "schema": {...}, "data": {...} }
{ "type": "multipart", "parts": [...] }
```

## Enum Wire Values

All enums serialize to lowercase strings:

| Enum | Values |
|---|---|
| `ParticipantType` | `"human"`, `"agent"` |
| `MessageStatus` | `"pending"`, `"sent"`, `"delivered"`, `"read"`, `"failed"` |
| `InvocationStatus` | `"pending"`, `"success"`, `"error"`, `"cancelled"`, `"timeout"` |
| `HumanStatus` | `"online"`, `"away"`, `"offline"` |

## Field Naming Convention

- **JSON wire format**: `snake_case` (e.g., `event_id`, `conversation_id`, `human_id`)

## Delivery Semantics

- **At-least-once**: Events carry `event_id` for client-side deduplication.
- **Offline persistence**: Messages sent while the human is offline are stored and drained on reconnect.
- **Multi-device fanout**: All connected devices of the same human receive events.

## Auto-Reconnect

The SDK automatically reconnects with exponential backoff:

| Attempt | Delay |
|---|---|
| 1 | 1s |
| 2 | 2s |
| 3 | 4s |
| 4 | 8s |
| 5 | 16s |
| 6+ | 30s (cap) |

User-initiated `disconnect()` suppresses auto-reconnect.
