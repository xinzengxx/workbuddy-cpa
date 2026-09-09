#!/usr/bin/env bash
# e2e-check.sh: end-to-end verification of the Rust workbuddy plugin against
# an isolated CPA instance (port 8399, test management key). Run after the
# instance is up: see Task 10 of the implementation plan.
set -e
B="http://127.0.0.1:8399"
K="Authorization: Bearer sk-Cj9KaR4VJoG5nqCzk"
MK="Authorization: Bearer testkey123"

echo "== 1) models list =="
curl -s "$B/v1/models" -H "$K" | python3 -c "
import json,sys
d=[m['id'] for m in json.load(sys.stdin)['data'] if m.get('owned_by')=='workbuddy']
assert 'glm-5.3-flash' in d, d
assert len(d)>=11, d
print('models OK', len(d), 'incl glm-5.3-flash')
"

echo "== 2) management credits =="
curl -s "$B/v0/management/plugins/workbuddy/accounts" -H "$MK" | python3 -c "
import json,sys
d=json.load(sys.stdin)
c=d['accounts'][0]['credits']
assert not c.get('error'), c
assert c['pack_count']>=4, c
print('credits OK', c['total_remain'], '/', c['total_size'], 'packs:', c['pack_count'])
"

echo "== 3) panel page =="
curl -sf "$B/v0/resource/plugins/workbuddy/panel" | grep -q "总积分额度" && echo "panel OK"

echo "== 4) non-streaming chat =="
curl -s "$B/v1/chat/completions" -H "$K" -H 'Content-Type: application/json' \
  -d '{"model":"glm-5.3-flash","messages":[{"role":"user","content":"只回复两个字：收到"}],"max_tokens":2048,"stream":false}' \
  | python3 -c "
import json,sys
r=json.load(sys.stdin)
assert r['choices'][0]['message']['content'].strip(), r
print('chat OK:', r['choices'][0]['message']['content'].strip()[:20])
"

echo "== 5) streaming chat (cross-format path) =="
curl -s "$B/v1/messages" -H "$K" -H 'Content-Type: application/json' -H 'anthropic-version: 2023-06-01' \
  -d '{"model":"glm-5.3-flash","max_tokens":2048,"stream":true,"messages":[{"role":"user","content":"只回复两个字：收到"}]}' \
  | head -c 300 | grep -q "event\|data" && echo "stream OK"

echo "== ALL E2E CHECKS PASSED =="
