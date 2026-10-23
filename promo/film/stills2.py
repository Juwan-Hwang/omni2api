"""python3 stills2.py LANG OUTPREFIX t1 t2 ... → OUTPREFIX_<lang>_<t>.png (full 1920x1080)"""
import asyncio, sys
from playwright.async_api import async_playwright
async def main():
    lang, out = sys.argv[1], sys.argv[2]; ts = [float(x) for x in sys.argv[3:]]
    async with async_playwright() as p:
        b = await p.chromium.launch(); pg = await b.new_page(viewport={'width': 1920, 'height': 1080})
        errs = []; pg.on('pageerror', lambda e: errs.append(str(e))); pg.on('console', lambda m: errs.append(m.text) if m.type == 'error' else None)
        await pg.goto(f'http://localhost:8777/comp.html?lang={lang}'); await pg.wait_for_function('window.__ready', timeout=120000); await pg.evaluate('window.__ready')
        for t in ts:
            await pg.evaluate(f'window.__seek({t})'); await pg.screenshot(path=f'{out}_{lang}_{t:05.2f}.png')
        print('errors:', errs[:5]); await b.close()
asyncio.run(main())
