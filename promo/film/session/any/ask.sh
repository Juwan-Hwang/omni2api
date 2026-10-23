#!/bin/sh
# the caller: the same request every time, over the public tunnel
E=~/.omni2api/endpoint.json
curl -s "$(jq -r .public_base_url $E)/chat/completions" \
  -H "Authorization: Bearer $(jq -r .caller_key $E)" -H 'Content-Type: application/json' \
  -d '{"model":"anything","messages":[{"role":"user","content":"What time is it?"}]}' \
  | jq -r '.choices[0].message.content'
