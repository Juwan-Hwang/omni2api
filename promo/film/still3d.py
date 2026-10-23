import asyncio, sys
from playwright.async_api import async_playwright
async def main():
    shot, q, out = sys.argv[1], sys.argv[2], sys.argv[3]; ts = [float(x) for x in sys.argv[4:]]
    async with async_playwright() as p:
        b = await p.chromium.launch(args=['--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader', '--ignore-gpu-blocklist'])
        pg = await b.new_page(viewport={'width': 1920, 'height': 1080})
        errs=[]; pg.on('pageerror', lambda e: errs.append(str(e))); pg.on('console', lambda m: errs.append(m.text) if m.type=='error' else None)
        await pg.goto(f'http://localhost:8777/o3d.html?shot={shot}{q}')
        await pg.wait_for_function('window.__ready===true', timeout=60000)
        for t in ts:
            await pg.evaluate(f'window.__seek({t})'); await pg.screenshot(path=f'{out}_{t:05.2f}.png')
        print(errs[:3]); await b.close()
asyncio.run(main())
