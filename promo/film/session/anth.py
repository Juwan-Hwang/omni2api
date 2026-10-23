import json,os,anthropic
ep=json.load(open(os.path.expanduser('~/.omni2api/endpoint.json')))
c=anthropic.Anthropic(base_url=ep['public_base_url'].removesuffix('/v1'),api_key=ep['caller_key'])
try:
    ms=c.models.list(); print('models.list ok', [(m.id, m.created_at) for m in ms.data])
except Exception as e: print('models.list ERROR', type(e).__name__, str(e)[:300])
r=c.messages.create(model='anything',max_tokens=200,messages=[{"role":"user","content":"Say hi in Chinese."}])
print('messages ok', r.content[0].text)
