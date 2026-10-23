import asyncio, sys
from playwright.async_api import async_playwright
async def main():
    out = sys.argv[1]; ts = [float(x) for x in sys.argv[2:]]
    async with async_playwright() as p:
        b = await p.chromium.launch(); pg = await b.new_page(viewport={'width': 1920, 'height': 1080})
        errs = []; pg.on('pageerror', lambda e: errs.append(str(e))); pg.on('console', lambda m: errs.append(m.text) if m.type == 'error' else None)
        await pg.goto('http://localhost:8777/comp.html?hud=1'); await pg.wait_for_function('window.__ready', timeout=120000); await pg.evaluate('window.__ready')
        for t in ts:
            await pg.evaluate(f'window.__seek({t})'); await pg.screenshot(path=f'{out}_{t:05.2f}.png')
        print('errors:', errs[:5]); await b.close()
asyncio.run(main())
