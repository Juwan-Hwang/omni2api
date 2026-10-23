"""SFX from earshot's code bank, placed from the comp's __events(), one room, mixed under the score.
Writes audio/mix.wav (48 kHz stereo). Peaks are aligned: a sound's measured peak lands on its event.
"""
import asyncio, json, sys
import numpy as np
import soundfile as sf
from scipy.signal import fftconvolve, butter, sosfilt
from earshot.forge import bank
from playwright.async_api import async_playwright

SR = 48000
LANG = sys.argv[1] if len(sys.argv) > 1 else 'en'   # v2: one mix per cut (the score is shared; keystrokes follow each cut's copy)


async def events():
    async with async_playwright() as p:
        b = await p.chromium.launch()
        pg = await b.new_page(viewport={'width': 1920, 'height': 1080})
        await pg.goto(f'http://localhost:8777/comp.html?lang={LANG}')
        await pg.wait_for_function('!!window.__ready && typeof window.__events === "function"', timeout=120000)
        await pg.evaluate('window.__ready')
        ev = await pg.evaluate('window.__events()')
        dur = await pg.evaluate('window.__meta.dur')
        await b.close()
        return ev, dur

EV, DUR = asyncio.run(events())
json.dump(EV, open(f'audio/events_{LANG}.json', 'w'), indent=1)
music, msr = sf.read('audio/music.wav', dtype='float32')
assert msr == SR
N = int((DUR + 0.2) * SR)
fx = np.zeros((N, 2), np.float32)
cache = {}


def snd(sid, **kw):
    key = (sid, tuple(sorted(kw.items())))
    if key not in cache:
        y, meta = bank.render(sid, **kw)
        y = np.asarray(y, np.float32)
        sr = meta.get('sr', 48000) if isinstance(meta, dict) else 48000
        if sr != SR:
            import soxr
            y = soxr.resample(y, sr, SR).astype(np.float32)
        if y.ndim == 1:
            y = np.stack([y, y], 1)
        pk = int(np.argmax(np.abs(y).sum(1)))
        cache[key] = (y, pk)
    return cache[key]


def place(sid, t, gain_db=0.0, pan=0.0, **kw):
    y, pk = snd(sid, **kw)
    g = 10 ** (gain_db / 20)
    l = np.cos((pan + 1) * np.pi / 4) * 1.414; r = np.sin((pan + 1) * np.pi / 4) * 1.414
    i = int(round(t * SR)) - pk
    a, b = max(0, i), min(N, i + len(y))
    if b <= a:
        return
    seg = y[a - i:b - i]
    fx[a:b, 0] += seg[:, 0] * g * l
    fx[a:b, 1] += seg[:, 1] * g * r


rk = np.random.default_rng(11)
for e in EV:
    t, k = e['t'], e['kind']
    if k == 'key':
        place('fit/key/lubed' if not e.get('soft') else 'pro_typing_key/soft', t, (-24 if e.get('soft') else -21) + rk.uniform(-2, 2), rk.uniform(-0.25, 0.25))
    elif k == 'enter':
        place('fit/key/thock', t, -16 if not e.get('soft') else -19, 0.05)
    elif k == 'morph':
        place('pro_whoosh_big/swell', t - 0.05, -14, 0.0)
    elif k == 'carry':
        place('whoosh_fast/crisp', t - 0.05, -14, 0.0)
    elif k == 'paper':
        place('pro_whoosh_big/reverse', t + 0.1, -17, 0.2)
    elif k == 'slide':
        place('swish/soft', t + 0.1, -15, -0.3)
    elif k == 'packet':
        place('pro_zip/up_hi', t + 0.2, -19, 0.0)
    elif k == 'out':
        place('pro_glitch/data', t, -26, 0.15)
    elif k == 'slot':
        place('pro_click/glass', t, -16, (-0.2 + e['k'] * 0.1))
    elif k == 'hit':
        place('pro_tap/glass', t, -14, (-0.3 if e['k'] % 2 else 0.3))
    elif k == 'feat':
        place('pro_click/glass', t, -15, 0.0)
    elif k == 'tok':
        place('pro_hover_tick/glass', t, -22, (-0.3 if e['j'] % 2 else 0.3))
    elif k == 'swell':
        place('pro_whoosh_big/swell', t + 0.4, -15, 0.0)
    elif k == 'type8':
        place('fit/key/lubed', t, -18, -0.2 + e['k'] * 0.06)
    elif k == 'home':
        place('pro_pop/chirp', t + 0.42, -20, 0.0)

# one room for every effect
n = int(1.2 * SR); tt = np.arange(n) / SR
rng = np.random.default_rng(3)
ir = rng.standard_normal((n, 2)) * np.exp(-tt * 5.0)[:, None]
ir = sosfilt(butter(2, 6000, 'low', fs=SR, output='sos'), ir, axis=0); ir[:500] = 0
ir /= np.sqrt((ir ** 2).sum(0, keepdims=True))
wet = np.stack([fftconvolve(fx[:, c], ir[:, c])[:N] for c in range(2)], 1)
fx = fx + 0.22 * wet

m = np.zeros((N, 2), np.float32)
L = min(N, len(music)); m[:L] = music[:L]
# fade the score with the picture's last fade
fade_n = int(0.9 * SR); m[N - fade_n:] *= np.linspace(1, 0, fade_n)[:, None] ** 1.5
mix = m * 1.0 + fx * 1.0
peak = np.abs(mix).max()
mix = mix / peak * 0.6
sf.write(f'audio/mix_{LANG}.wav', mix.astype(np.float32), SR, subtype='FLOAT')
print(LANG, 'events', len(EV), 'dur', round(DUR, 2), 'peak', round(float(peak), 3))
