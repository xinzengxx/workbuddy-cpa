#!/usr/bin/env bash
# e2e-check.sh: end-to-end verification of the Rust workbuddy plugin against an
# isolated CPA instance. Defaults target the CN plugin on the local test instance
# on 8399; override with BASE / AK / MK_KEY for another instance:
#
#   BASE=http://127.0.0.1:8400 AK=<api-key> MK_KEY=<mgmt-key> bash scripts/e2e-check.sh
#
# International plugin (Global realm, single deepseek model):
#
#   PLUGIN_ID=workbuddy-intl-cpa PROVIDER=workbuddy-intl MODEL=deepseek-v4.1-flash \
#   MIN_MODELS=1 MIN_PACKS=1 bash scripts/e2e-check.sh
set -e
B="${BASE:-http://127.0.0.1:8399}"
K="Authorization: Bearer ${AK:-sk-Cj9KaR4VJoG5nqCzk}"
MK="Authorization: Bearer ${MK_KEY:-testkey123}"
PLUGIN_ID="${PLUGIN_ID:-workbuddy-cpa}"
PROVIDER="${PROVIDER:-workbuddy}"
MODEL="${MODEL:-glm-5.3-flash}"
MIN_MODELS="${MIN_MODELS:-12}"
MIN_PACKS="${MIN_PACKS:-4}"
P="${PLUGIN_BASE:-/v0/management/plugins/$PROVIDER}"
PY=python3
export PROVIDER MODEL MIN_MODELS MIN_PACKS

echo "== 1) models list (provider=$PROVIDER, model=$MODEL) =="
curl -s "$B/v1/models" -H "$K" | $PY -c "
import json,os,sys
p=os.environ['PROVIDER']; want=os.environ['MODEL']; least=int(os.environ['MIN_MODELS'])
d=[m['id'] for m in json.load(sys.stdin)['data'] if m.get('owned_by')==p]
assert want in d, (want, d)
assert len(d)>=least, d
print('models OK', len(d), 'incl', want)
"

echo "== 2) management accounts (multi-account) =="
curl -s "$B$P/accounts" -H "$MK" | $PY -c "
import json,os,sys
least=int(os.environ['MIN_PACKS'])
d=json.load(sys.stdin)
assert 'error' not in d, d
assert '_dbg_files' not in d, 'debug field leaked into the response'
accs=d['accounts']
assert accs, 'no accounts listed'
need={'auth_index','file_name','name','nickname','enabled','disabled','status','credits'}
for a in accs:
    missing=need-set(a)
    assert not missing, (a.get('file_name'), missing)
enabled=[a for a in accs if a['enabled']]
assert enabled, 'no enabled account'
c=enabled[0]['credits']
assert not c.get('error'), c
assert c['pack_count']>=least, c
print('accounts OK', len(accs), 'listed /', len(enabled), 'enabled; first credits',
      c['total_remain'], '/', c['total_size'], 'packs:', c['pack_count'])
"

echo "== 3) add-account login flow (start + poll) =="
START=$(curl -s "$B$P/login/start" -H "$MK")
STATE=$(printf '%s' "$START" | $PY -c "
import json,sys
d=json.load(sys.stdin)
assert d.get('url') and d.get('state'), d
print(d['state'])
")
curl -s -X POST "$B$P/login/poll" -H "$MK" -H 'Content-Type: application/json' \
  -d "{\"state\":\"$STATE\"}" | $PY -c "
import json,sys
d=json.load(sys.stdin)
assert d.get('status') in ('pending','success','error'), d
assert d['status']!='error', d
print('login OK: start returned auth url, poll ->', d['status'])
"

echo "== 4) credential lifecycle delegated to the host =="
curl -s "$B$P/accounts" -H "$MK" | $PY -c "
import json,sys
d=json.load(sys.stdin)
accs=d['accounts']
assert any(a.get('file_name') for a in accs), 'no file_name to key the host API on'
assert all('disabled' in a for a in accs), 'host-owned disabled flag missing'
print('delegation OK: host-owned disabled flag surfaced for', len(accs), 'accounts')
"

echo "== 5) panel page (plugin=$PLUGIN_ID) =="
PANEL=$(curl -sf "$B/v0/resource/plugins/$PLUGIN_ID/panel")
for needle in "总积分额度" "账号配额总览" "账号额度明细" addAccount deleteAccount toggleAccount "auth-files"; do
  printf '%s' "$PANEL" | grep -q -- "$needle" || { echo "panel missing: $needle"; exit 1; }
done
printf '%s' "$PANEL" | grep -q -- "/v0/management/plugins/$PROVIDER" \
  || { echo "panel points at the wrong provider"; exit 1; }
echo "panel OK (two zones + add/delete/toggle wired to the host API)"

echo "== 6) non-streaming chat =="
curl -s "$B/v1/chat/completions" -H "$K" -H 'Content-Type: application/json' \
  -d "{\"model\":\"$MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"只回复两个字：收到\"}],\"max_tokens\":2048,\"stream\":false}" \
  | $PY -c "
import json,sys
r=json.load(sys.stdin)
assert r['choices'][0]['message']['content'].strip(), r
print('chat OK:', r['choices'][0]['message']['content'].strip()[:20])
"

echo "== 7) streaming chat (cross-format path) =="
curl -s "$B/v1/messages" -H "$K" -H 'Content-Type: application/json' -H 'anthropic-version: 2023-06-01' \
  -d "{\"model\":\"$MODEL\",\"max_tokens\":2048,\"stream\":true,\"messages\":[{\"role\":\"user\",\"content\":\"只回复两个字：收到\"}]}" \
  | head -c 300 | grep -q "event\|data" && echo "stream OK"

echo "== ALL E2E CHECKS PASSED =="
