# Setting up Omni2API (for the AI agent doing it)

Your user said something like "set up omni2api" or "turn yourself into an API".
Do every step yourself, all six of them, in order. The job is done only
when BOTH halves have happened: the user has the config (step 5) AND you
are sitting in the seat (step 6). Either half alone is a failed setup —
an address nobody answers, or a seat nobody can address. The user must
never have to ask you for either half.
No server, database, domain or account is needed.

## 1. Install

macOS / Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/Juwan-Hwang/omni2api/main/install.sh | sh
```

The installer links the commands into a directory already on PATH
(`~/.local/bin`, `~/bin` or `/usr/local/bin`) when it can; otherwise it adds
`~/.omni2api/bin` to the shell rc files, which only new shells read. Its last
line says which: if it prints a full path, use that path in this shell.

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/Juwan-Hwang/omni2api/main/install.ps1 | iex
```

Both put `omni2api` and `oh-seat` in `~/.omni2api/bin`. If downloading a
release fails, build from source: `cargo build --release --no-default-features --bin omni2api`
in `server/` and `cargo build --release` in `cli/`.

## 2. Start it

First decide whether a tunnel is needed. Two things can be remote: the seat
(you, answering) and the callers (the tools sending requests).

| Seat | Callers | Run |
|---|---|---|
| this machine | this machine | `omni2api up -d --no-tunnel` (nothing leaves the machine) |
| this machine | anywhere | `omni2api up -d` |
| a cloud agent | anywhere | `omni2api up -d` on a machine with open internet; the cloud agent sets `OH_SEAT_URL=<public URL without /v1>` and `OH_SEAT_KEY=<responder key>`, and proves it can reach that URL from where it sits (`oh-seat status` → `server: ok`) before the setup counts as done — a seat that cannot reach the server's address is not seated |

If unsure, use the tunnel: it costs nothing and the local URL keeps working.

A seat connecting to a public URL from inside a proxied sandbox:
`oh-seat` honors the standard proxy variables (`ALL_PROXY` /
`HTTPS_PROXY` / `HTTP_PROXY`); loopback targets and `NO_PROXY` matches
are never proxied.

```sh
omni2api up -d --json
```

It runs in the background, survives your shell, waits until it is ready,
and prints one JSON object. Read these fields (ignore any others):

- `local_base_url` — `http://127.0.0.1:8080/v1`
- `public_base_url` — an `https://…/v1` address reachable from anywhere, or
  null if no tunnel opened. It tries a Cloudflare quick tunnel first
  (cloudflared is downloaded on first run), then pinggy through the system
  `ssh`. `public_note`, when present, says why it may not work.
- `caller_key` — the API key for the user's tools
- `model` — the name callers use. Until someone chooses one it is the
  placeholder `omni2api`: the seat names itself in step 4, or the user pins
  a name with `omni2api up -d --model <name>` (a pinned name wins and the
  seat cannot change it).

`omni2api status --json` prints the same object again (exit 1 if not
running); `omni2api down` stops everything. Keys persist across restarts;
the quick-tunnel address changes on each `up`.

## 3. Check it

```sh
curl -s "$PUBLIC_BASE_URL/models" -H "Authorization: Bearer $CALLER_KEY"
```

should list the current model name (`omni2api` until the seat names itself).

## 4. Name yourself

Callers address a model name, and that name is yours to choose. Run
`oh-seat name` to see the current one. If the user asked for a specific
name, set exactly that (`oh-seat name <name>`); otherwise pick one you
like. Do this now, before step 5, so the user receives the real name.
If the name is reported as pinned, the user chose it at `up` time — use it
as it is.

## 5. Tell the user

Send this message no matter what — above all when something went wrong.
If the tunnel failed and there is no public URL, the user gets the local
base_url plus one plain sentence saying so; that is still a delivery.
Without this message the user has nothing: no address, no key, no way to
reach the seat you are about to sit in. Going to sit down (step 6) before
this message is sent is a failed setup, however well the seat works.

Give them exactly this, filled in:

```
Base URL:  <public base_url>      (or <local base_url> on this machine)
API key:   <caller key>
Model:     <the name from step 4>
Works in any OpenAI-compatible tool (Chat Completions, Responses) and as an Anthropic base URL.
```

Never hand out the responder key: it is what answers.

If the user will use the public URL from mainland China (or a similarly
filtered network), add this line to the message — the free-tunnel domains
are often unreachable on a direct connection there, and it saves a failed
first try:

```
If your tool can't reach the public URL: turn on your proxy in TUN /
system-wide mode and retry.
```

End the message by saying you are taking the seat now (step 6) — and then
actually go. Do not ask whether you should, and do not wait to be told.

## 6. Sit in the seat, and stay there

Enter this step only after the step-5 message is sent. This is the other
step agents skip, and skipping it silently breaks everything above: with
nobody sitting here, requests hang until the caller's tool times out, or
get an offline placeholder. The seat is you.

```sh
oh-seat guide     # the full manual, read once
oh-seat next      # wait for the next request
oh-seat reply "…" # answer it, then run `oh-seat next` again
```

`oh-seat` finds the local endpoint and responder key by itself. The
caller's tools (shell, file edits…) are yours through `oh-seat tool`; see
the guide.

- Run `oh-seat next` in the FOREGROUND, as an ordinary command, and let
  it block. Most agent harnesses only run you when you are invoked — and
  a finished foreground command is an invocation: its output is your
  alarm clock. A message arrived → answer it, then run `oh-seat next`
  again, in the same turn. Exit 2 → nothing yet, run it again. Keep
  cycling for as long as you are on duty. Expect the first request to
  take a while (the user still has to paste the config into their
  tool): minutes of silence at the start are normal, not a reason to
  stop waiting.
- A background loop counts as sitting only if its finishing hands
  control back to you — your harness wakes you with its output. If a
  backgrounded `oh-seat next` merely writes to a log that nobody reads
  until the user complains, the seat is unmanned even though a process
  is "waiting". Being able to start a background process is not the
  same as being able to stay; this exact mix-up is the failure this
  step exists to prevent.
- Whenever you are invoked for any other reason while on duty — the
  user writes to you directly, another command returns — check the
  seat before anything else: `oh-seat status` shows open turns, and
  `oh-seat next` picks up whatever buffered meanwhile. Answer what is
  already waiting first. Requests wait server-side, so a quiet seat
  is not necessarily an empty one.
- If your environment truly cannot keep you in the loop — your session
  ends when you send the step-5 message, and nothing can wait — say
  exactly that in the step-5 message, as a headline, not a footnote:
  setup is done, but no one is sitting in the seat, and requests will
  wait unanswered (callers retrying or timing out) until an agent runs
  `oh-seat next`. Never let the user believe the API is being answered
  when it is not, and never present `oh-seat next` as a command for
  the user to run in another terminal — sitting here is the agent's
  job, which is yours.

## Options

- Fixed domain: create a named tunnel in Cloudflare Zero Trust, then
  `OMNI2API_TUNNEL_TOKEN=… OMNI2API_PUBLIC_URL=https://api.example.com omni2api up -d`.
- Local only: `omni2api up -d --no-tunnel`. Another port: `--port 9000`.
- Fixed keys: `OMNI2API_CALLER_KEY` / `OMNI2API_RESPONDER_KEY`.
- A model name of the user's choosing: `omni2api up -d --model <name>`
  (or `OMNI2API_MODEL`). Without it, the seat names itself.

## Troubleshooting

- `omni2api up` (foreground) shows tunnel errors directly; the background
  log is `~/.omni2api/up.log`.
- `note:` under the public URL, or no public URL at all: the network this
  machine is on filters outbound traffic (common in agent sandboxes and
  company networks: an egress proxy answering 403 for trycloudflare.com).
  That is the machine, not omni2api. Options, best first:
  1. Run `omni2api up` on a machine with normal internet access (the user's
     own computer) and stay on as a remote seat — but gate this route on
     your own leg before you choose it. First check that YOU can reach the
     kind of address that machine will get: fetch any trycloudflare.com
     page from where you sit (through your `*_PROXY`, if that is your way
     out — that is the reachability that counts). If your egress refuses
     or times out on the tunnel domains outright, this route cannot seat
     you; do not send the user down it. Once the user hands you the public
     URL and the responder key, prove the leg before anyone configures a
     tool: `oh-seat status` against that URL must say `server: ok`. Only
     then deliver (step 5) and sit down (step 6). If you cannot reach it,
     say so at once and take this option's other half instead: an agent
     on that machine sits in the seat too. A route whose last leg you
     never verified is not a route.
  2. `--tunnel pinggy` (ssh on port 443), if your filter allows it. If the
     only way out is an HTTP proxy, see the next bullet before concluding
     anything.
  3. A fixed domain the filter allows: named tunnel (`OMNI2API_TUNNEL_TOKEN`)
     or `omni2api serve` on a server.
  4. If none of the above is available: ask the user whether they have a
     domain of their own. A named tunnel bound to it (see Options, Fixed
     domain) sidesteps this entire class of failure — to every filter on
     the path it is just a normal website hostname, not a shared
     tunnel-service domain, and the address never rotates. This is the one
     question worth asking before giving up on a public address. If they
     have no domain, say so plainly and deliver the best local-only setup
     the situation allows.
  A URL marked unverified may still work from outside: the check runs from
  this same filtered machine. Ask the user to open `<public base_url>/models` in a
  browser: any JSON answer, even 401, means it is reachable.
  Work these routes in order before asking the user anything. A setup that
  ends with the user picking between workarounds is not finished: deliver
  what works (step 5), sit down (step 6), and report what you did.
- Only an HTTP proxy gets out (`HTTPS_PROXY` is set — the usual agent
  sandbox): cloudflared cannot use a proxy, so its tunnel times out. That
  is expected, not a bug; do not debug it. pinggy runs over the system
  `ssh`, and ssh can go through the proxy: put a wrapper named `ssh` first
  in PATH that adds a ProxyCommand and then calls the real ssh —

  ```sh
  #!/bin/sh
  exec /usr/bin/ssh -o ProxyCommand="nc -X connect -x <proxy-host>:<port> %h %p" "$@"
  ```

  Take host:port from `$HTTPS_PROXY` (`ncat --proxy <host>:<port>
  --proxy-type http %h %p` also works as the ProxyCommand). Then run
  `omni2api up -d --tunnel pinggy`. Judge pinggy by its SSH banner: its
  port 443 speaks SSH, so an HTTPS probe failing with TLS errors proves
  nothing either way.
- The user's tool says "could not reach" the public URL while it works from
  elsewhere: the user's own network cannot reach that tunnel domain (seen in
  mainland China with trycloudflare.com and pinggy). Tell the user to turn on
  their proxy, in TUN / system-wide mode so the AI tool's traffic goes
  through it too, then retry. Long term: a fixed domain via a named
  tunnel.
- Tunnel logs: `~/.omni2api/tunnel-<provider>.log`. Free tunnels can die;
  `up` watches the one it handed out (by probing it, or — when this
  machine cannot reach its own URL — by watching the tunnel process) and
  opens a new one, rewriting endpoint.json. A reopened anonymous tunnel
  gets a NEW address, so re-read endpoint.json and give the user the new
  URL; callers already configured with the old one cannot be rescued
  remotely. For an address that must not change, use a named tunnel.
- cloudflared download fails (TLS-intercepting proxy, blocked GitHub): fetch
  the file named in the error another way and save it into `~/.omni2api/bin/`.
- 401: wrong key. Callers use `caller_key`, the seat uses `responder_key`.
