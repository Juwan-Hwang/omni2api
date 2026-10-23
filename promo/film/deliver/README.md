# omni2api — launch film v2 (two monolingual cuts)

| file | on-screen copy | tagline |
|---|---|---|
| `omni2api-promo-v2-en.mp4` | English only | Anything to API |
| `omni2api-promo-v2-zh.mp4` | Chinese only | 万物皆可 2API |

Both: 59.5 s · 1920×1080 · 30 fps · H.264 High (CRF 16, yuv420p, faststart) · AAC 256 kb/s 48 kHz · −14.1 LUFS integrated, true peak −1.2 dBFS (two-pass loudnorm to −14 / −1.5 dBTP).

One comp drives both cuts: `film/comp.html?lang=en|zh`. Timing, camera, 3D, cursor and score are shared. Only the copy and the keystroke SFX that follow it differ. Product names, commands, code and terminal output are real data, so they stay as captured in both cuts. That includes the real `grep` result `### Anything to API — 万物皆可 2API`, which shows in the English cut too.

v1 (`omni2api-promo-v1.mp4`, bilingual, 53.5 s) and its stills are kept as they were. The v1 sources are in `../v1src/`.

## The message

omni2api turns **anything** into an API (万物皆可 2API). An AI agent in the seat is one example of "anything". The film never says that *you / me / yourself* becomes the API; that is OpenHuman's pitch. v2 removes every self-framing from v1 and adds a beat in which five different things answer the same request.

## Storyboard (120 BPM, 1 bar = 2 s; hits land on the cuts)

| time | beat | changed in v2 |
|---|---|---|
| 0.0–6.0 | Dark. A 3D `>_` drifts close and the cursor blinks three times. At **2.0** the `>` bends into the ring while the camera whips round it. It settles to `o_`, then the camera pushes into the cursor until the frame is all Signal. | — (same frames) |
| 6.0 | **Carry.** The Signal field closes down to a typing caret. | — |
| 6.5–11.5 | Thesis, typed. EN "Anything that can sit at a terminal / an AI agent. a script. a whole toolchain." → "can be an API." ZH "凡是能在终端里跑的 / AI 智能体，一段脚本，一整套工具链" → "都能变成 API". | The mixed CN line under the thesis is gone. Each cut is in one language. |
| 12.0 | **Hit.** Paper opens as a circle from the caret. | — |
| 12.1–15.5 | **The seat.** Label "The seat" / "坐席". EN "Put anything in the seat. / It answers as the model." ZH "坐席上，放什么都行 / 调用方看到的，就是模型". | **New copy.** This replaces "Tell your AI agent: Set up omni2api and make yourself the API." |
| 15.5–16.1 | ⏎. The Paper collapses into the seat terminal and the caret becomes its prompt caret. | — |
| 16.1–20.9 | Seat terminal (real): `omni2api up -d` → "Omni2API is up." with local and public base_url, masked key and model. Then `oh-seat name anything` and `oh-seat next`. Captions: EN "One command. / Local and public base_url, an api_key, a model." · "The seat names itself." ZH "一条命令 / 本地、公网地址，密钥，模型名，一次给齐" · "模型名，坐席自己起". Window title: "oh-seat — the seat" / "oh-seat — 坐席". | Captions are monolingual. |
| 21.0 | **Hit.** Claude Code (real tmux capture) slides in on a wire. Labels: CALLER / SEAT, "anything running oh-seat" (ZH 调用方 / 坐席, "任何跑着 oh-seat 的东西"). | Was "an AI agent running oh-seat". |
| 21.8–28.9 | The Claude Code ↔ seat wire sequence, as in v1: the question, the packet, the 13 tool signatures, `oh-seat tool Bash …`, the real tool_call and result. Captions: EN "Claude Code asks model “anything”. / The request lands in the seat." · "A real tool_call. / It runs on the caller’s own machine." ZH "Claude Code 呼叫模型 “anything” / 请求落到坐席上" · "真实的 tool_call / 在调用方自己的机器上执行". | The reply is cut to `oh-seat reply "Anything to API — it turns whatever sits in its seat into an OpenAI/Anthropic-compatible model. …"`. Claude Code's streamed answer is cut the same way. "Right now, that's me." is gone from both panes. The caption no longer says "请求到达坐席上的 AI". |
| 29.0–31.0 | EN "Claude Code asked. / Whatever sat in the seat answered." ZH "Claude Code 发问， / 坐席作答——坐的是什么都行". The camera dives into the caller window. | **New copy.** Was "An agent answered. / 坐席上的 AI 作答". |
| 31.0 | **Carry + hit.** The caret flies out of the dive. | — |
| **31.0–37.0** | **NEW · Anything in the seat.** Kicker EN "Same request. Anything in the seat." ZH "同一个问题，坐席换成什么都行". The caret types the caller's real request: `curl …/v1/chat/completions -d '{"model":"anything",…"What time is it?"}'`. From **32.0**, one seat per second, each on a hit: **an AI agent** (`oh-seat next` / `oh-seat reply …`) → "It's 9:11 on a Friday night in Shanghai. Go home." · **a Python script** (`shout.py`) → "WHAT TIME IS IT?!" · **a shell loop** (`loop.sh`) → "Fri Oct 23 21:05:20 CST 2026" · **a cron job** (`crontab -l`) → "Cron says 21:07." · **a cow** (`cowsay`) → the cow saying "It's 21:06. Moo.". Each seat's real code sits on the left (SEAT / 坐席). What curl actually printed sits on the right (CALLER RECEIVES / 调用方收到), and the caret types it in. A five-item index runs along the bottom. | **New beat, +6 s (3 bars).** |
| 37.0–41.0 | **Carry.** The caret leaves the cow's last line and runs the 13-name list on eighth notes, under the label "Tested end to end with" / "逐一实测通过": Claude Code, Codex, OpenCode, Pi, Reasonix, Cline, GitHub Copilot CLI, Grok CLI, ZCode, WorkBuddy, Trae, Qoder, DeepSeek Harness. Then OpenAI Chat Completions · Responses · Anthropic Messages. | Shifted +6 s (was 31–35). |
| 41–49 | Four features, one per bar, each over real data. **Strips the harness. / 剥掉外壳** · **Real tool calls. / 工具调用，是真的** · **Streams back. / 边写边回** · **Many callers at once. / 来多少调用方都行**. Sub-lines are in the table below. | Shifted +6 s. Each cut has its own sub-line. The stream is a new real capture: "Every word streams back the moment the seat sends it." replaces v1's "Hi. I'm the model now — …". |
| 49.0–50.6 | EN "Anything can be the API." ZH "万物，皆可 2API". Snare roll and riser. | **New copy.** Was "Make yourself the API. / 把自己，变成 API". |
| 50.55–51.0 | The caret swells to fill the frame. | Same timing (+6 s). |
| 51.0 | **Hit.** 3D reprise: the camera pulls back inside the cursor, the ring lands, and `o_` settles at the lockup's size and position. | Same frames (+6 s). |
| 53.5 | Pixel-matched handoff to the vector lockup. | — |
| 53.75–54.9 | The cursor types `omni2api` glyph by glyph (the outline wordmark revealed by a clip, never retyped), then slides home. | — |
| 55.0–59.5 | Tagline **Anything to API** (EN) / **万物皆可 2API** (ZH), then github.com/Juwan-Hwang/omni2api. The `o_` winks twice and the film fades. | One tagline per cut. |

## All on-screen copy

| where | EN | ZH |
|---|---|---|
| thesis 1 | Anything that can sit at a terminal | 凡是能在终端里跑的 |
| thesis 1b | an AI agent. a script. a whole toolchain. | AI 智能体，一段脚本，一整套工具链 |
| thesis 2 | can be an API. | 都能变成 API |
| seat label | The seat | 坐席 |
| seat beat | Put anything in the seat. / It answers as the model. | 坐席上，放什么都行 / 调用方看到的，就是模型 |
| window titles | claude — caller · oh-seat — the seat | claude — 调用方 · oh-seat — 坐席 |
| wire labels | CALLER Claude Code · SEAT anything running oh-seat | 调用方 Claude Code · 坐席 任何跑着 oh-seat 的东西 |
| caption 17.5 | One command. / Local and public base_url, an api_key, a model. | 一条命令 / 本地、公网地址，密钥，模型名，一次给齐 |
| caption 19.6 | The seat names itself. | 模型名，坐席自己起 |
| caption 23.6 | Claude Code asks model “anything”. / The request lands in the seat. | Claude Code 呼叫模型 “anything” / 请求落到坐席上 |
| caption 25.6 | A real tool_call. / It runs on the caller’s own machine. | 真实的 tool_call / 在调用方自己的机器上执行 |
| caption 29.0 | Claude Code asked. / Whatever sat in the seat answered. | Claude Code 发问， / 坐席作答——坐的是什么都行 |
| anything kicker | Same request. Anything in the seat. | 同一个问题，坐席换成什么都行 |
| anything boxes | Seat · Caller receives | 坐席 · 调用方收到 |
| anything names | An AI agent. · A Python script. · A shell loop. · A cron job. · A cow. | 一个 AI 智能体 · 一段 Python 脚本 · 一个 shell 循环 · 一个定时任务 · 一头牛 |
| anything index | an AI agent · a Python script · a shell loop · a cron job · cowsay | AI 智能体 · Python 脚本 · shell 循环 · 定时任务 · cowsay |
| callers label | Tested end to end with | 逐一实测通过 |
| feature 1 | Strips the harness. / The seat sees only what the user typed. | 剥掉外壳 / 坐席只看到用户亲手打的那句话 |
| feature 1 data label | system prompt · 15,823 chars · 13 tool schemas | 系统提示词 · 15,823 字符 · 13 个工具定义 |
| feature 2 | Real tool calls. / oh-seat tool becomes genuine tool_calls in the caller’s response. | 工具调用，是真的 / oh-seat tool 直接变成调用方收到的 tool_calls |
| feature 2 data label | OpenAI Python SDK → the tool ran in the caller’s own shell · exit=0 · 19989 | OpenAI Python SDK → 工具在调用方自己的 shell 里执行 · exit=0 · 19989 |
| feature 3 | Streams back. / Every word goes back as SSE, as it is sent. | 边写边回 / 每个词一出手，就以 SSE 流回调用方 |
| feature 4 | Many callers at once. / Every caller gets its own isolated conversation. | 来多少调用方都行 / 每个调用方一条会话，互不串线 |
| finale | Anything can be the API. | 万物，皆可 2API |
| end tagline | Anything to API | 万物皆可 2API |

**CJK notes.** Every typed line has a fixed on-screen duration, not a fixed character rate, so a 9-character Chinese line and a 35-character English line finish on the same beat (`typed(text, cls, t0, dur)` in the comp). Chinese headlines have no trailing 。 because the full-width stop left a gap before the cursor. Chinese gets +1 px tracking instead of the Latin negative tracking, and line-height 1.32 on the big two-line beats. In the Chinese cut, keystroke SFX fire on every glyph; in the English cut they fire on every second letter.

## What is real in the new beat

Everything in it was captured on 2026-10-23 at 21:05–21:12 CST. I reinstalled omni2api 0.1.11 / oh-seat 0.2.3 with `install.sh`, ran `omni2api up -d` (public trycloudflare tunnel) and `oh-seat name anything`. The caller is `session/any/ask.sh`, a curl call over the public URL asking model "anything" "What time is it?". Each seat ran alone while the caller asked once. Its code and curl's output are in `session/any/`:

- an AI agent: me, using `oh-seat next` / `oh-seat reply` (`agent.*`)
- `shout.py`: a 6-line Python loop (`shout.*`)
- `loop.sh`: a 4-line bash loop (`loop.*`)
- a real crontab entry run by cron (`crontab.txt`, `cron.caller.txt`)
- `cow.sh`: a cowsay one-liner (`cow.*`)

`stream.sh` / `stream.sse` hold the new SSE capture used in "Streams back". The server, cron and crontab were all shut down afterwards. Keys in `up.txt` are masked.

## How it was made

1. **Real usage** is as in v1 (Claude Code 2.1.309 in tmux, the OpenAI/Anthropic SDKs, curl), plus the v2 captures above.
2. **3D.** `film/o3d.html` and `r3d_seq.py` are unchanged. The open and the reprise frames are reused: v2 frames 0–179 and 1530–1661 are byte-identical copies of the v1 frames.
3. **Composition.** One seekable timeline, `film/comp.html`, with a `?lang=en|zh` switch (`COPY` table). The new beat is `buildAny`/`drawAny`. Its caret geometry is analytic, so it works when seeking in any order. Programming ligatures are switched off on every monospace layer, because JetBrains Mono had turned the cow's `||-` into a glyph. `render_film.py --comp "comp.html?lang=…" --out film/out/frames_<lang>`.
4. **Sound.** `music.py` (shared) has 3 bars inserted at 31 s. It has a hit on each seat at 32/33/34/35/36 and a tick as each answer lands (+0.42 s). Everything after it moves +6 s, so the hits sit at 2, 6, 12, 16, 21, 31, 32–36, 37, 41/43/45/47 and 51 s; I checked them against the onsets. `sound.py <lang>` places earshot SFX (commit 925ebe5) from that cut's `__events()` and writes `audio/mix_<lang>.wav`. `encode.sh <lang>` runs the two-pass loudnorm and the encode.

Brand rules: Signal appears only on the cursor and caret. The wordmark comes from the outline file. Colours are Ink, Paper and Graphite.

Rebuild: `python3 -m http.server 8777 --directory film`, then `python3 music.py`, then `python3 sound.py en` and `python3 sound.py zh`. For each lang: `python3 render_film.py --out film/out/frames_<lang> --comp "comp.html?lang=<lang>"` (the renderer skips frames that already exist), then `./encode.sh <lang>`.

Stills (fresh, per cut): `still-v2-{en,zh}-01-open-3d … 12-end-lockup.png`.
