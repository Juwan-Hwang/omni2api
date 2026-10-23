#!/bin/sh
E=~/.omni2api/endpoint.json
curl -sN "$(jq -r .public_base_url $E)/chat/completions" -H "Authorization: Bearer $(jq -r .caller_key $E)" -H 'Content-Type: application/json' \
  -d '{"model":"anything","stream":true,"messages":[{"role":"user","content":"stream me one line"}]}'
