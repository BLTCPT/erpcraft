#!/usr/bin/env python3
"""Live-eval driver (LIVE_EVAL.md): send every captured parse request
to the live Baseten grant model through the custom.baseten connector
route (authd surrogate — the API key never appears here), save each
raw response under responses/, and assemble the replay fixture at
crates/erpcraft-cli/fixtures/recorded_live_baseten.json.

Usage: python3 drive.py [requests.json]
Re-running overwrites responses and the fixture. Token usage is
printed per call and totaled at the end.
"""
import json, os, sys
sys.path.insert(0, "/opt/muse/skills/custom-connector-setup/bin")
from dynamic_credentials import add_surrogate_to_request, read_json_response
import urllib.request

BASE = "https://inference.baseten.co/v1"
HOSTS = ["inference.baseten.co"]
PROVIDER = "custom.baseten"
MODEL = "meta-models/muse-spark-open-1026"
HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURE = os.path.join(HERE, "..", "crates", "erpcraft-cli", "fixtures",
                       "recorded_live_baseten.json")

class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args):
        raise RuntimeError("redirect refused, verify the endpoint")

def call(payload):
    data = json.dumps(payload).encode()
    req = urllib.request.Request(BASE + "/chat/completions", data=data, method="POST")
    req.add_header("Content-Type", "application/json")
    add_surrogate_to_request(req, PROVIDER, allowed_hosts=HOSTS)
    with urllib.request.build_opener(NoRedirect()).open(req, timeout=180) as resp:
        return resp.status, read_json_response(resp)

def main():
    requests_path = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "requests.json")
    scenarios = json.load(open(requests_path))
    os.makedirs(os.path.join(HERE, "responses"), exist_ok=True)

    totals = {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
    exchanges = {}   # prompt -> fixture exchange (first scenario wins)
    shared = {}      # prompt -> [scenario ids that share it]
    for sc in scenarios:
        payload = {
            "model": MODEL,
            "messages": [
                {"role": "system", "content": sc["system"]},
                {"role": "user", "content": sc["user"]},
            ],
            "max_tokens": 8192,
            "temperature": 0,
        }
        status, body = call(payload)
        record = {"id": sc["id"], "prompt": sc["prompt"], "status": status, "body": body}
        with open(os.path.join(HERE, "responses", sc["id"] + ".json"), "w") as f:
            json.dump(record, f, indent=1)
        usage = body.get("usage", {}) if isinstance(body, dict) else {}
        for k in totals:
            totals[k] += usage.get(k, 0) or 0
        content = None
        try:
            content = body["choices"][0]["message"]["content"]
        except Exception:
            pass
        print(f"{sc['id']:22s} HTTP {status}  tokens={usage.get('total_tokens')}  "
              f"content={'<none>' if content is None else str(len(content)) + ' chars'}")
        if status == 200 and content:
            shared.setdefault(sc["prompt"], []).append(sc["id"])
            if sc["prompt"] not in exchanges:
                exchanges[sc["prompt"]] = {
                    "prompt": sc["prompt"],
                    "response": content,
                    "scenario": sc["id"],
                    "usage": usage,
                }

    fixture = {
        "about": "Live-recorded agent parse responses for the full acceptance "
                 "scenario set (LIVE_EVAL.md): every scenario's genuine parse "
                 "request — captured from the real adapter — answered by the "
                 "live model, then replayed through the real binary by "
                 "tests/agent_live_replay.rs. Keyed by prompt; where two "
                 "scenarios share a prompt (T1 fresh vs. duplicate state; "
                 "paying a matched vs. a held AP invoice) the first recording "
                 "is kept and the twin lives in live-eval/responses/.",
        "recorded_from": f"baseten {MODEL} (live, via the custom.baseten connector route)",
        "recorded_on": "2026-10-31",
        "intended_route": f"baseten {MODEL}",
        "shared_prompts": {p: ids for p, ids in shared.items() if len(ids) > 1},
        "total_usage": totals,
        "exchanges": list(exchanges.values()),
    }
    with open(FIXTURE, "w") as f:
        json.dump(fixture, f, indent=1)
    print(f"\nfixture: {len(exchanges)} exchanges -> {FIXTURE}")
    print(f"TOTALS: {totals}")

if __name__ == "__main__":
    main()
