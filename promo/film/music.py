"""Original score for the omni2api film (structure adapted from the Zephyr score), synthesised in numpy (no samples, no licences).
112 BPM, 26 bars + tail. D minor: Dm – B♭ – F – C.
Writes audio/music.wav (48 kHz stereo float) and audio/music_stems/*.wav
"""
import json, os
import numpy as np
import soundfile as sf
from scipy.signal import butter, sosfilt, fftconvolve

SR = 48000
BPM = 120
BEAT = 60 / BPM
BAR = BEAT * 4
NBARS = 28.5
DUR = 59.6   # v2: +6 s (3 bars) for the 'anything in the seat' beat at 31–37
N = int(DUR * SR)
rng = np.random.default_rng(7)
os.makedirs('audio/stems', exist_ok=True)


def tb(bar, beat=0.0):
    return bar * BAR + beat * BEAT


def buf():
    return np.zeros((N, 2), np.float32)


def add(dst, sig, t, gain=1.0, pan=0.0):
    i = int(round(t * SR))
    if sig.ndim == 1:
        l = np.cos((pan + 1) * np.pi / 4); r = np.sin((pan + 1) * np.pi / 4)
        sig = np.stack([sig * l * 1.414, sig * r * 1.414], 1)
    j = min(N, i + len(sig))
    if i < 0 or i >= N:
        return
    dst[i:j] += sig[: j - i] * gain


def lp(x, f, order=2):
    return sosfilt(butter(order, min(f, SR / 2 - 100), 'low', fs=SR, output='sos'), x, axis=0)


def hp(x, f, order=2):
    return sosfilt(butter(order, f, 'high', fs=SR, output='sos'), x, axis=0)


def bp(x, f0, f1, order=2):
    return sosfilt(butter(order, [f0, f1], 'band', fs=SR, output='sos'), x, axis=0)


def env_adsr(n, a, d, s, r, total=None):
    a, d, r = int(a * SR), int(d * SR), int(r * SR)
    total = n if total is None else total
    e = np.ones(total) * s
    e[:a] = np.linspace(0, 1, a, endpoint=False) if a else e[:a]
    e[a:a + d] = np.linspace(1, s, max(1, len(e[a:a + d])))
    rel = np.linspace(1, 0, r) ** 2 if r else np.array([])
    tail_start = max(0, total - r)
    e[tail_start:] *= rel[: total - tail_start]
    return e


def midi(m):
    return 440.0 * 2 ** ((m - 69) / 12)


def saw(f, n, phase=0.0):
    t = np.arange(n) / SR
    ph = (f * t + phase) % 1.0
    # polyBLEP-light: soften by slight lowpass later
    return 2 * ph - 1


def reverb_ir(sec=2.6, damp=3.2, pre=0.012):
    n = int(sec * SR)
    t = np.arange(n) / SR
    ir = rng.standard_normal((n, 2)) * np.exp(-damp * t)[:, None]
    ir = lp(ir, 7500)
    ir[: int(pre * SR)] = 0
    ir /= np.sqrt((ir ** 2).sum(0, keepdims=True))
    return ir.astype(np.float32)


IR_HALL = reverb_ir(3.4, 2.1)
IR_ROOM = reverb_ir(1.1, 5.5)


def verb(x, ir, wet):
    y = np.stack([fftconvolve(x[:, c], ir[:, c])[: len(x)] for c in range(2)], 1)
    return x + y * wet


# ───────────────────────── instruments ─────────────────────────

def kick(gain=1.0):
    n = int(0.55 * SR); t = np.arange(n) / SR
    f = 46 + 110 * np.exp(-t * 32)
    ph = 2 * np.pi * np.cumsum(f) / SR
    body = np.sin(ph) * np.exp(-t * 6.5)
    click = hp(rng.standard_normal(n) * np.exp(-t * 300), 2500) * 0.25
    sig = np.tanh((body + click) * 1.6) * gain
    return sig


def sub_drop(f0=82, f1=31, sec=2.2):
    n = int(sec * SR); t = np.arange(n) / SR
    f = f1 + (f0 - f1) * np.exp(-t * 2.4)
    s = np.sin(2 * np.pi * np.cumsum(f) / SR)
    s = s + 0.18 * np.sin(2 * 2 * np.pi * np.cumsum(f) / SR)
    return np.tanh(s * 1.4) * np.exp(-t * 1.7) * np.minimum(1, t * 400)


def snap():
    n = int(0.32 * SR); t = np.arange(n) / SR
    x = rng.standard_normal(n)
    e = np.exp(-t * 38) + 0.5 * np.exp(-np.maximum(0, t - 0.011) * 45) * (t > 0.011)
    s = bp(x * e, 900, 5200)
    return s * 0.9


def hat(open_=False):
    n = int((0.22 if open_ else 0.06) * SR); t = np.arange(n) / SR
    x = rng.standard_normal(n)
    s = hp(x, 7800, 3) * np.exp(-t * (14 if open_ else 75))
    return s * 0.5


def pluck(m, sec=0.9, bright=1.0):
    """FM glass pluck."""
    n = int(sec * SR); t = np.arange(n) / SR
    f = midi(m)
    idx = (2.2 * bright) * np.exp(-t * 9)
    mod = np.sin(2 * np.pi * f * 3.0 * t) * idx
    car = np.sin(2 * np.pi * f * t + mod)
    car += 0.35 * np.sin(2 * np.pi * f * 2.0 * t + mod * 0.5)
    e = np.exp(-t * 5.2) * np.minimum(1, t * 900)
    return car * e * 0.5


def bell(m, sec=4.5):
    n = int(sec * SR); t = np.arange(n) / SR
    f = midi(m)
    s = (np.sin(2 * np.pi * f * t + 1.4 * np.exp(-t * 2) * np.sin(2 * np.pi * f * 3.5 * t))
         + 0.4 * np.sin(2 * np.pi * f * 2.76 * t) * np.exp(-t * 2.5)
         + 0.2 * np.sin(2 * np.pi * f * 5.4 * t) * np.exp(-t * 4))
    return s * np.exp(-t * 0.9) * np.minimum(1, t * 600) * 0.4


def bass(m, sec, cutoff=520):
    n = int(sec * SR); t = np.arange(n) / SR
    f = midi(m)
    s = 0.6 * saw(f, n) + 0.4 * saw(f * 1.004, n, 0.3) + 0.5 * np.sin(2 * np.pi * f * t)
    s = lp(s, cutoff, 3)
    e = env_adsr(n, 0.006, 0.18, 0.62, 0.08)
    return np.tanh(s * e * 1.3) * 0.55


def pad(notes, sec, cutoff=1800, attack=0.9, release=1.6):
    n = int(sec * SR)
    out = np.zeros((n, 2))
    for m in notes:
        for k, det in enumerate([-0.11, -0.05, 0.0, 0.05, 0.11]):
            f = midi(m) * 2 ** (det / 12)
            v = saw(f, n, rng.random())
            pan = (k - 2) / 2.2
            out[:, 0] += v * np.cos((pan + 1) * np.pi / 4)
            out[:, 1] += v * np.sin((pan + 1) * np.pi / 4)
    out = lp(out, cutoff, 2)
    e = env_adsr(n, attack, 0.5, 0.85, release)
    return out * e[:, None] * (0.075 / max(1, len(notes) ** 0.5))


def riser(sec, f0=300, f1=6000):
    n = int(sec * SR); t = np.arange(n) / SR
    x = rng.standard_normal(n)
    out = np.zeros(n)
    k = 24
    seglen = n // k
    for i in range(k):
        a = i * seglen; b = n if i == k - 1 else (i + 1) * seglen
        fc = f0 * (f1 / f0) ** (i / (k - 1))
        out[a:b] = bp(x[max(0, a - 2000):b], fc * 0.7, min(fc * 1.4, SR / 2 - 200))[-(b - a):]
    e = (t / sec) ** 2.2
    tone = np.sin(2 * np.pi * np.cumsum(220 * 2 ** (2.0 * t / sec)) / SR) * 0.15
    return (out * 0.8 + tone) * e


def swell(sec):
    """reverse-cymbal style swell into a hit."""
    n = int(sec * SR); t = np.arange(n) / SR
    x = hp(rng.standard_normal(n), 3000)
    return x * (t / sec) ** 3 * 0.5


def air(sec):
    n = int(sec * SR)
    x = rng.standard_normal((n, 2))
    x = bp(x, 400, 3200)
    t = np.arange(n) / SR
    return x * (0.5 + 0.5 * np.sin(2 * np.pi * t / 7.0))[:, None] * 0.06



# ───────────────────────── arrangement (120 BPM, bar = 2 s; film cuts land on bars) ─────────────────────────
# A minor, a darker, more "machine" palette: Am – F – C – G, with an E-pedal in the build.
CH = {'Am': [57, 64, 69, 72], 'F': [53, 60, 65, 69], 'C': [48, 55, 60, 64], 'G': [55, 62, 67, 71], 'E': [52, 59, 64, 68]}
ROOT = {'Am': 33, 'F': 29, 'C': 36, 'G': 31, 'E': 28}
ARP = {'Am': [81, 76, 84, 79, 81, 88, 84, 76], 'F': [81, 77, 84, 79, 81, 89, 84, 77],
       'C': [79, 76, 84, 79, 83, 88, 84, 76], 'G': [79, 74, 83, 79, 81, 86, 83, 74], 'E': [80, 76, 83, 80, 83, 88, 83, 76]}
PROG = ['Am', 'F', 'C', 'G']
chord_at = lambda bar: PROG[int(bar) % 4]
drums, bassb, padb, keys, fx = buf(), buf(), buf(), buf(), buf()

def blip(m, sec=0.09, g=1.0):
    """square-ish terminal blip (the cursor's voice)."""
    n = int(sec * SR); t = np.arange(n) / SR; f = midi(m)
    s = np.sign(np.sin(2 * np.pi * f * t)) * 0.5 + np.sin(2 * np.pi * f * t) * 0.5
    return lp(s * np.exp(-t * 28) * np.minimum(1, t * 3000), 5000) * 0.3 * g

def groove_bar(b, c, kick_g=0.95, hats=True, bass_on=True, arp_oct=0, arp_g=0.18, four=True):
    for q in range(4):
        if four or q in (0, 2):
            add(drums, kick(kick_g), tb(b, q))
        if q in (1, 3):
            add(drums, snap(), tb(b, q), 0.40, 0.05)
    if hats:
        for s16 in range(16):
            add(drums, hat(s16 % 8 == 6), tb(b, s16 * 0.25), 0.17 if s16 % 4 == 2 else 0.07, 0.35)
    if bass_on:
        for e8 in range(8):
            m = ROOT[c] + (12 if e8 in (3, 7) else 0)
            add(bassb, bass(m, BEAT * 0.45, 380 + 300 * (e8 % 2 == 0)), tb(b, e8 * 0.5), 0.9 if e8 % 2 == 0 else 0.6)
    for i in range(8):
        add(keys, pluck(ARP[c][i] + arp_oct, 0.45, 0.9), tb(b, i * 0.5), arp_g, (-0.45 if i % 2 else 0.45))

# 0–6  OPEN: dark air; the cursor blinks (blips on its blinks 0.25/1.0/1.75); bar 1 (2.0 s) the morph lands; push into the cursor
add(fx, air(8.0), 0.0, 1.2)
for tt in (0.25, 1.0, 1.75): add(keys, blip(81, 0.07), tt, 0.5, 0.2)
add(fx, swell(0.9), 2.0 - 0.9, 0.7)
add(drums, kick(1.25), tb(1)); add(drums, sub_drop(80, 30, 2.6), tb(1), 1.0)
add(keys, bell(69, 5.0), tb(1), 0.36, 0.15); add(keys, bell(76, 5.0), tb(1), 0.26, -0.2)
add(padb, pad(CH['Am'], BAR * 2 + 1.2, 900, 0.5, 1.4), tb(1))
for i in range(8):
    if i < 2: continue
    add(keys, pluck(ARP['Am'][i] - 12, 0.6, 0.6), tb(1, i * 0.5), 0.24, (-0.4 if i % 2 else 0.4))
add(fx, riser(BAR * 0.85, 300, 9000), tb(3) - BAR * 0.85, 0.7)   # into the cursor (6.0)
# 6–12 THESIS: thin pulse, typing ticks carry the rhythm; 9.5 the second line; 12 the Paper opens
add(drums, kick(1.15), tb(3)); add(drums, sub_drop(70, 33, 1.6), tb(3), 0.7)
for b in (3, 4, 5):
    c = ['Am', 'F', 'C'][b - 3]
    for q in range(4): add(drums, kick(0.55), tb(b, q), 0.55)
    for e8 in range(8): add(bassb, bass(ROOT[c], BEAT * 0.4, 300), tb(b, e8 * 0.5), 0.55)
    add(padb, pad(CH[c], BAR + 0.7, 1100, 0.3, 0.8), tb(b), 0.8)
add(keys, bell(76, 3.0), 9.5, 0.2, 0.0)
add(fx, swell(0.8), tb(6) - 0.8, 0.6)
# 12–16 QUOTE (Paper): bright, open; 15.5 enter → 16 the terminal
add(drums, kick(1.1), tb(6)); add(keys, bell(81, 4.0), tb(6), 0.3, 0.2); add(keys, bell(72, 4.0), tb(6), 0.25, -0.2)
add(padb, pad(CH['F'], BAR * 2 + 0.4, 2600, 0.15, 0.6), tb(6), 1.0)
for b in (6, 7):
    c = ['F', 'C'][b - 6]
    for i in range(8): add(keys, pluck(ARP[c][i], 0.5, 1.1), tb(b, i * 0.5), 0.2, (-0.45 if i % 2 else 0.45))
    for q in (0, 2): add(drums, kick(0.6), tb(b, q))
add(fx, riser(0.5, 800, 10000), 15.5, 0.45)
# 16–31 TERMINAL + SPLIT: the groove
for b in range(8, 15):
    c = chord_at(b)
    groove_bar(b, c, arp_g=0.16 if b < 10 else 0.19, arp_oct=0 if b < 12 else 12, four=b >= 10)
    if b % 2 == 0: add(padb, pad(CH[c], BAR * 2 + 0.6, 1500 if b < 12 else 2200, 0.25, 0.7), tb(b), 0.85)
add(drums, kick(1.15), tb(8)); add(drums, sub_drop(72, 34, 1.3), tb(8), 0.6)
add(drums, kick(1.1), tb(10.5)); add(fx, swell(0.6), 21.0 - 0.6, 0.5)      # split at 21
# 31–37 ANY (v2): same request, five seats, one per 1 s from 32; a hit on each seat, a tick when its answer lands (+0.42)
add(fx, riser(0.6, 500, 9000), 30.4, 0.6)
add(drums, kick(1.15), 31.0); add(drums, sub_drop(76, 32, 1.2), 31.0, 0.7)
for b, c in ((15.5, 'Am'), (16.5, 'F'), (17.5, 'C')):
    groove_bar(b, c, arp_g=0.15, arp_oct=0, four=b > 15.5, hats=b > 15.5)
    add(padb, pad(CH[c], BAR + 0.6, 2000, 0.2, 0.6), tb(b), 0.8)
for k, m in enumerate([76, 79, 81, 84, 88]):
    tt = 32.0 + k
    add(drums, kick(1.0), tt); add(keys, bell(m, 2.0), tt, 0.14, (-0.2 + 0.1 * k))
    add(keys, blip(88 + (k % 2) * 3, 0.06), tt + 0.42, 0.32, 0.25)
# 37–41 CALLERS: a hit on every name (16ths at 120 = 0.125; names on 8ths = 0.25)
add(fx, riser(0.6, 500, 9000), 36.4, 0.6)
for k in range(13):
    tt = 37.25 + k * 0.25
    c = 'Am' if k < 4 else 'F' if k < 8 else 'G'
    for m in CH[c]: add(keys, pluck(m + 12, 0.25, 1.4), tt, 0.10, 0.0)
    add(drums, kick(0.6), tt, 0.6); add(drums, hat(), tt + 0.125, 0.12, 0.4)
add(drums, kick(1.15), 37.0); add(drums, sub_drop(76, 32, 1.2), 37.0, 0.7)
add(padb, pad(CH['G'], 4.4, 2400, 0.2, 0.6), 37.0, 0.7)
# 41–49 FEATURES: four bars, a hit per feature, full groove

for i, b in enumerate((20.5, 21.5, 22.5, 23.5)):
    c = PROG[i]
    groove_bar(b, c, arp_g=0.2, arp_oct=12, four=True)
    add(padb, pad(CH[c], BAR + 0.6, 2600, 0.15, 0.6), tb(b), 0.9)
    add(drums, kick(1.05), tb(b)); add(keys, bell(CH[c][-1] + 12, 2.0), tb(b), 0.12, 0.0)
for j in range(10): add(keys, blip(84 + (j % 3) * 2, 0.06), 45.45 + j * 0.105, 0.35, (-0.3 if j % 2 else 0.3))
# 49–51 TAG: drop to a pedal, riser into the reprise
add(padb, pad(CH['E'], 2.3, 1800, 0.1, 0.4), 49.0, 0.9)
for e8 in range(4): add(bassb, bass(ROOT['E'], BEAT * 0.45, 500), 49.0 + e8 * 0.5, 0.7)
add(fx, riser(1.5, 250, 11000), 49.5, 0.8)
for i in range(12): add(drums, snap(), 49.5 + i * 0.125, 0.04 + 0.26 * (i / 11) ** 2, 0.0)
# 51– END: the hit as the ring lands (51 + 1.1), then the name is typed (8 blips) and the cursor goes home
HIT = 51.0
add(drums, kick(1.35), HIT); add(drums, sub_drop(84, 28, 3.4), HIT, 1.05)
add(padb, pad([33, 45, 52, 57, 60, 64, 69], 9.0, 2000, 0.05, 3.0), HIT, 1.25)
for m, g, p in [(69, 0.32, 0.2), (76, 0.22, -0.25), (57, 0.28, 0.0)]: add(keys, bell(m, 7.0), HIT, g, p)
add(keys, bell(88, 4.0), HIT + 1.1, 0.12, 0.3)
for k in range(8): add(keys, blip([69, 72, 76, 74, 72, 76, 79, 81][k], 0.07), 53.75 + k * 0.125, 0.45, (-0.25 + k * 0.07))
add(keys, bell(81, 4.5), 53.75 + 1.18, 0.2, 0.0)

# ───────────────────────── mix ─────────────────────────
drums = verb(drums, IR_ROOM, 0.18)
keys = verb(keys, IR_HALL, 0.42)
padb = verb(padb, IR_HALL, 0.35)
fx = verb(fx, IR_HALL, 0.3)
bassb = lp(bassb, 3000)
drums = hp(drums, 38)
stems = {'drums': drums * 0.55, 'bass': hp(bassb, 45) * 0.42, 'pad': padb * 2.2, 'keys': keys * 2.3, 'fx': fx * 1.3}
mix = sum(stems.values())
mix = hp(mix, 28)
peak = np.abs(mix).max()
mix = mix / peak * 0.7
for k, v in stems.items():
    sf.write(f'audio/stems/{k}.wav', (v / peak * 0.7).astype(np.float32), SR, subtype='FLOAT')
sf.write('audio/music.wav', mix.astype(np.float32), SR, subtype='FLOAT')
json.dump({'bpm': BPM, 'beat': BEAT, 'bar': BAR, 'nbars': NBARS, 'dur': DUR}, open('audio/music.json', 'w'))
print('music', round(DUR, 2), 's peak', round(float(peak), 2))
