# omni2api — launch film

Everything used to make the omni2api launch film: two monolingual cuts (EN / ZH), 59.5 s, 1920×1080, 30 fps.

- Finished cuts: `deliver/omni2api-promo-v2-en.mp4`, `deliver/omni2api-promo-v2-zh.mp4` (v1, bilingual, kept for reference)
- Storyboard, all on-screen copy in both languages, and how each beat was made: `deliver/README.md`
- Rebuild: `sh reproduce.sh` (Python 3.12 + ffmpeg 7; installs Playwright/Chromium and earshot @925ebe5)

## Layout

| path | what |
|---|---|
| `film/comp.html` | the whole film as one seekable timeline (`?lang=en|zh`) |
| `film/o3d.html`, `r3d_seq.py` | three.js `>_` → `o_` 3D shots; `film/frames/{open,end}` are the committed renders |
| `film/data/`, `session/` | real terminal captures: `omni2api up`, oh-seat, Claude Code in tmux, SDK calls, the five "anything in the seat" seats |
| `film/fonts/`, `film/lockup.svg`, `logo/` | PingFang SF, JetBrains Mono, outline wordmark, brand board |
| `render_film.py` | frame renderer with linear-light motion blur |
| `music.py`, `sound.py`, `audio/` | original score (fixed seeds) and SFX placed from the comp's events; committed mixes and stems |
| `encode.sh` | two-pass loudnorm to −14 LUFS, H.264 CRF 16 / AAC 256k |
| `v1src/` | v1 sources |

Rendered frames (`film/out/`) are not committed; the renderer regenerates them. API keys in captures are masked; the tunnel URLs shown were temporary and are dead.
