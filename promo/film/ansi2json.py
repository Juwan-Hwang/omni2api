"""tmux capture-pane -e snapshots → rows of styled spans (JSON) for the film's terminal renderer."""
import json, re, sys
P256 = []
base = [(0,0,0),(205,49,49),(13,188,121),(229,229,16),(36,114,200),(188,63,188),(17,168,205),(229,229,229),
        (102,102,102),(241,76,76),(35,209,139),(245,245,67),(59,142,234),(214,112,214),(41,184,219),(255,255,255)]
P256 += base
for r in range(6):
    for g in range(6):
        for b in range(6):
            P256.append(tuple(0 if v == 0 else 55 + 40 * v for v in (r, g, b)))
for i in range(24): P256.append((8 + 10 * i,) * 3)
hexc = lambda c: '#%02x%02x%02x' % c
def parse(text):
    text = re.sub(r'\x1b\]8;[^\x1b]*\x1b\\', '', text)          # OSC 8 hyperlinks
    rows = []
    st = {'fg': None, 'bg': None, 'b': 0, 'd': 0, 'i': 0, 'inv': 0}
    for line in text.split('\n'):
        spans = []; cur = ''; i = 0
        def flush():
            nonlocal cur
            if cur: spans.append({'t': cur, **st}); cur = ''
        while i < len(line):
            m = re.match(r'\x1b\[([0-9;:]*)m', line[i:])
            if m:
                flush(); codes = [int(x) if x else 0 for x in re.split('[;:]', m.group(1))] or [0]; k = 0
                while k < len(codes):
                    c = codes[k]
                    if c == 0: st.update(fg=None, bg=None, b=0, d=0, i=0, inv=0)
                    elif c == 1: st['b'] = 1
                    elif c == 2: st['d'] = 1
                    elif c == 3: st['i'] = 1
                    elif c == 7: st['inv'] = 1
                    elif c == 22: st['b'] = st['d'] = 0
                    elif c == 23: st['i'] = 0
                    elif c == 27: st['inv'] = 0
                    elif c == 39: st['fg'] = None
                    elif c == 49: st['bg'] = None
                    elif 30 <= c <= 37: st['fg'] = hexc(base[c - 30])
                    elif 90 <= c <= 97: st['fg'] = hexc(base[c - 90 + 8])
                    elif 40 <= c <= 47: st['bg'] = hexc(base[c - 40])
                    elif 100 <= c <= 107: st['bg'] = hexc(base[c - 100 + 8])
                    elif c in (38, 48):
                        key = 'fg' if c == 38 else 'bg'
                        if codes[k + 1] == 5: st[key] = hexc(P256[codes[k + 2]]); k += 2
                        elif codes[k + 1] == 2: st[key] = hexc(tuple(codes[k + 2:k + 5])); k += 4
                    k += 1
                i += m.end(); continue
            m = re.match(r'\x1b\[[0-9;?]*[A-Za-z]', line[i:])
            if m: i += m.end(); continue
            cur += line[i]; i += 1
        flush(); rows.append(spans)
    return rows
if __name__ == '__main__':
    out = {}
    for arg in sys.argv[2:]:
        name, path = arg.split('=')
        out[name] = parse(open(path, encoding='utf-8').read())
    json.dump(out, open(sys.argv[1], 'w'), ensure_ascii=False)
    for k, rows in out.items():
        print('==', k)
        for j, r in enumerate(rows): print(j, ''.join(s['t'] for s in r)[:90])
