import json, subprocess as sp
while True:
    out = sp.run(["oh-seat", "--json", "next"], capture_output=True, text=True).stdout
    for ev in map(json.loads, out.splitlines()):
        if ev["type"] == "message.received":
            sp.run(["oh-seat", "reply", ev["text"].upper() + "!"])
