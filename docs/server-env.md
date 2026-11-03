# Server environment settings

The environment variables the `omni2api` server reads for its public
address and its per-request bounds. Every one is optional; the default
applies when it is unset, empty or not a positive number.

## Public address

Links the server publishes (seat-made files, `/files/<id>` URLs in API
answers) are built on one base URL:

| variable | effect |
|---|---|
| `SERVER_PUBLIC_URL` | the base URL, e.g. `https://api.example.com`; wins over everything |
| `OMNI2API_PUBLIC_URL` | used when `SERVER_PUBLIC_URL` is unset (the named-tunnel address) |
| `OMNI2API_TRUST_PROXY` | `1`/`true`/`yes`: take `X-Forwarded-Host` and `X-Forwarded-Proto` from the request |

With no address configured the request's `Host` header is used, over
`https` unless the host is loopback. `X-Forwarded-*` headers are ignored
unless `OMNI2API_TRUST_PROXY` is set, because most proxies pass a
client's own `X-Forwarded-Host` through untouched. Set it only when the
proxy in front overwrites both headers itself. A proxy that sets `Host`
to the public name (nginx `proxy_set_header Host $host;`) needs neither.

## Bounds

| variable | default | bounds |
|---|---|---|
| `OMNI2API_REALTIME_MAX_BUFFER_BYTES` | 15 MiB | uncommitted input audio per realtime connection |
| `OMNI2API_REALTIME_MAX_AUDIO_BYTES` | 64 MiB | committed and inline audio kept per realtime connection |
| `OMNI2API_REALTIME_MAX_ITEMS` | 4096 | conversation items per realtime connection |
| `OMNI2API_VECTOR_SEARCH_MAX_BYTES` | 16 MiB | file bytes one vector-store search attaches |
| `OMNI2API_BATCH_MAX_LINES` | 50000 | request lines in one batch input file |

A realtime event past a bound is refused with an `error` event
(`input_audio_buffer_too_large`, `conversation_audio_limit_exceeded`,
`conversation_items_limit_exceeded`) and changes nothing; the connection
stays open. A search whose matching files exceed its bound attaches the
most relevant files first (query words found in the file), the newest
among equals, and tells the seat how many were left out. A batch over its
line cap is refused with `400` at creation.
