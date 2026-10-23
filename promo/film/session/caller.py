"""A tiny OpenAI-SDK agent: it calls model 'anything' at omni2api, with one real tool (bash).
Logs every request/response with timestamps to caller.log (jsonl) so the film can replay it."""
import json, os, subprocess, sys, time
from openai import OpenAI

ep = json.load(open(os.path.expanduser('~/.omni2api/endpoint.json')))
base = sys.argv[2] if len(sys.argv) > 2 else ep['public_base_url']
client = OpenAI(base_url=base, api_key=ep['caller_key'], timeout=600)
LOG = open(sys.argv[3] if len(sys.argv) > 3 else 'caller.log', 'a')
def log(**k):
    k['t'] = time.time(); LOG.write(json.dumps(k, ensure_ascii=False) + '\n'); LOG.flush()

tools = [{"type": "function", "function": {"name": "bash", "description": "Run a shell command on this machine",
          "parameters": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}}}]
msgs = [{"role": "system", "content": "You are a coding agent. Use the bash tool."},
        {"role": "user", "content": sys.argv[1]}]
log(kind='request', base=base, model='anything', user=sys.argv[1])
while True:
    r = client.chat.completions.create(model='anything', messages=msgs, tools=tools)
    m = r.choices[0].message
    log(kind='response', finish=r.choices[0].finish_reason, content=m.content,
        tool_calls=[{"id": c.id, "name": c.function.name, "args": c.function.arguments} for c in (m.tool_calls or [])])
    msgs.append(m.model_dump(exclude_none=True))
    if not m.tool_calls:
        print('FINAL:', m.content); break
    for c in m.tool_calls:
        cmd = json.loads(c.function.arguments)['command']
        p = subprocess.run(cmd, shell=True, capture_output=True, text=True, cwd=os.path.dirname(os.path.abspath(__file__)))
        out = (p.stdout + p.stderr).strip()
        log(kind='tool_run', id=c.id, command=cmd, exit=p.returncode, output=out)
        msgs.append({"role": "tool", "tool_call_id": c.id, "content": out + f"\nexit={p.returncode}"})
