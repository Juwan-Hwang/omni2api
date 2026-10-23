# omni2api — launch film v1

`omni2api-promo-v1.mp4` · 53.5 s · 1920×1080 · 30 fps · H.264 High (CRF 16, yuv420p, faststart) · AAC 256 kb/s 48 kHz · −14 LUFS integrated, true peak ≤ −1 dBTP.

**Concept.** The `o_` is a prompt: the film opens on a 3D `>_` whose chevron bends into the ring, and from then on the Signal cursor block never leaves the screen. It types every line, rides the real terminals, runs the wire between caller and seat, and at the end types the wordmark before it goes home. Everything you see in a terminal is real output from a session where an AI agent (me) sat in the seat and answered Claude Code, the OpenAI SDK, the Anthropic SDK and curl.

## Storyboard (120 BPM, 1 bar = 2 s; hits are on the cuts)

| time | beat |
|---|---|
| 0.0–2.0 | Dark. A 3D `>_` drifts close; the cursor blinks three times (a blip on each). |
| 2.0 | **Hit.** The `>` bends into the ring (`o`), apex first, while the camera whips round it on one continuous path. |
| 2.0–4.4 | It settles square-on: `o_`, light sweeping the ring. |
| 4.6–6.0 | Push into the cursor until the frame is all Signal. |
| 6.0 | **Carry.** The Signal field closes down to a typing caret. |
| 6.5–11.5 | Kinetic type: "Anything that can sit at a terminal / an AI agent. a script. a whole toolchain." → "can be an API." + 万物皆可 2API — 都可以成为 API. |
| 12.0 | **Hit.** The caret glides off; Paper opens as a circle from it. |
| 12.1–15.5 | On Paper: "Tell your AI agent · 告诉你的 AI" / "Set up omni2api and make yourself the API." (the README quick start, word for word). |
| 15.5–16.1 | ⏎. The Paper collapses into a terminal window; the caret stays put and becomes the prompt caret. |
| 16.1–20.9 | Seat terminal: `omni2api up -d` → the real "Omni2API is up." block (local and public trycloudflare base_url, masked api_key, model). Camera pushes onto the public URL; `oh-seat name anything` → `model: anything`; `oh-seat next`. |
| 21.0 | **Hit.** The window slides right; Claude Code (real tmux capture, v2.1.309, model "anything") slides in from the left, joined by a wire. CALLER / SEAT labels. |
| 21.8–23.0 | The user types "What is ./omni2api, in one line?" in Claude Code. ⏎. A Signal packet runs the wire. |
| 23.5 | Whip to the seat: NEW CONVERSATION … client=claude_code, the message, Claude Code's 13 tool signatures, caller env. |
| 24.5–25.5 | The seat types `oh-seat tool Bash command="grep -m1 '###' omni2api/README.md" …`; a packet runs back. |
| 26.0 | Whip to Claude Code: "Reading the tagline" (the real tool_call, run on the caller's machine). |
| 26.5–27 | Result packet → `TOOL RESULT Bash status=success …` / `### Anything to API — 万物皆可 2API`. |
| 27.3–28.9 | `oh-seat reply "Anything to API — … Right now, that's me."` → `replied … (delivered live)`; packet → Claude Code streams the answer. |
| 29–31 | "Claude Code asked. An agent answered." The camera dives into the caller window. |
| 31.0 | **Carry.** The caret flies out to a 13-name rapid list on eighth notes: "Tested end to end with" Claude Code, Codex, OpenCode, Pi, Reasonix, Cline, GitHub Copilot CLI, Grok CLI, ZCode, WorkBuddy, Trae, Qoder, DeepSeek Harness. Then OpenAI Chat Completions · Responses · Anthropic Messages. |
| 35–43 | Four features, one per bar, from the README, each over real data: **Strips the harness** (Claude Code's 15,823-character system prompt and 13 schemas collapse into the one line the user typed) · **Real tool calls** (`oh-seat tool bash …` → the OpenAI SDK's `finish_reason: tool_calls` JSON, exit=0 · 19989) · **Streams back** (real SSE chunks from `curl -N`, word by word: "Hi. I'm the model now — a seat behind an API.") · **Many callers at once** (the real `oh-seat convs` list: claude_code ×2, anthropic sdk, curl, openai sdk). Chinese line under each. |
| 43–44.6 | "Make yourself the API. / 把自己，变成 API". Snare roll and riser. |
| 44.6–45.0 | The caret swells to fill the frame. |
| 45.0 | **Hit.** 3D reprise: inside the cursor, the camera pulls back; the ring flips in and lands; `o_` settles square-on at exactly the size and position of the 2D lockup symbol. |
| 47.5 | Pixel-matched handoff to the vector lockup. |
| 47.75–48.9 | The cursor types its own name: it steps glyph by glyph along the baseline revealing `omni2api` (8 keystrokes, 8 blips), then slides home beside the ring. |
| 49.0–53.5 | Tagline "Anything to API — 万物皆可 2API", URL github.com/Juwan-Hwang/omni2api. The `o_` face winks (two blinks). Fade. |

## How it was made

1. **Real usage.** I installed omni2api 0.1.11 / oh-seat 0.2.3 with `install.sh` (aarch64 musl release) and ran `omni2api up -d` (Cloudflare quick tunnel). Then I sat in the seat with `oh-seat next / tool / reply` and answered: an OpenAI Python SDK agent with a real `bash` tool (`session/caller.py`), `curl -N` streaming, the Anthropic SDK, and Claude Code 2.1.309 in tmux (`ANTHROPIC_BASE_URL` = the public tunnel, model "anything"). The tmux pane was snapshotted with colours every 0.2 s (`session/cc/snaps2/*.ans`) and converted to styled spans (`ansi2json.py` → `film/data/cc.json`). The seat transcript in the film is the real oh-seat output (`session/cc2_*.txt`), with long lines cut to the pane and the api_key masked.
2. **3D.** `film/o3d.html` (Three.js 0.170) builds the symbol from the brand master geometry (ring R74/r40, stroke 34, cursor 62×31, 15-unit gap) as a swept, chamfered solid whose centreline morphs from a filleted chevron into the circle. It renders as frames with a 3-sample 180° shutter (`r3d_seq.py`). The end shot's camera is solved so the landed symbol matches the 2D lockup in pixels.
3. **Composition.** `film/comp.html` is a seekable timeline. It has one keyframed camera with spring and whip easings over a world holding both terminals, a single Signal caret object that every section hands to the next, kinetic type in PingFang SF, and JetBrains Mono (DejaVu Sans Mono fallback) for terminals. `render_film.py` renders it frame by frame with Playwright and adds linear-light motion blur on whips.
4. **Sound.** `music.py` is an original seeded numpy score (120 BPM, A minor). Hits are at 2, 6, 12, 16, 21, 31, 35/37/39/41 and 45 s, with blips on the cursor's blinks and on the 8 typed glyphs. `sound.py` places earshot SFX (pinned at commit 925ebe56) from the comp's `__events()`: keystrokes, enters, packets, whooshes and glass taps for the caller names. `encode.sh` does a two-pass loudnorm to −14 LUFS and the encode.

Brand rules honoured: Signal is only ever the cursor/caret (never the ring or wordmark). The wordmark is the supplied outlines (`omni2api-horizontal-colour-reversed.svg`), revealed by a clip, never retyped. Ink/Paper/Graphite only.

Rebuild: `python3 -m http.server 8777 --directory film`, then `python3 r3d_seq.py open 6.0 film/frames/open`, `python3 r3d_seq.py end 3.0 film/frames/end --q '&ppu=0.75956&sx=605.43&sy=486'`, `python3 render_film.py --out film/out/frames --comp comp.html`, `python3 music.py`, `python3 sound.py`, `./encode.sh`.
