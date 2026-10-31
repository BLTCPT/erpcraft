#!/usr/bin/env python3
"""Refill one scenario whose live call returned no content (the
model occasionally spends the whole completion budget in a
reasoning loop — see LIVE_EVAL.md). Retries that single scenario's
captured request until the model answers, saves the response over
responses/<id>.json, and rebuilds the replay fixture from all saved
responses. Usage: python3 refill.py <scenario-id> [max-attempts]
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

def content_of(body):
    try:
        return body["choices"][0]["message"]["content"]
    except Exception:
        return None

def main():
    scenario_id = sys.argv[1]
    max_attempts = int(sys.argv[2]) if len(sys.argv) > 2 else 6
    scenarios = {s["id"]: s for s in json.load(open(os.path.join(HERE, "requests.json")))}
    sc = scenarios[scenario_id]
    payload = {
        "model": MODEL,
        "messages": [
            {"role": "system", "content": sc["system"]},
            {"role": "user", "content": sc["user"]},
        ],
        "max_tokens": 8192,
        "temperature": 0,
    }
    spent = 0
    for attempt in range(1, max_attempts + 1):
        status, body = call(payload)
        usage = body.get("usage", {}) if isinstance(body, dict) else {}
        spent += usage.get("total_tokens", 0) or 0
        content = content_of(body)
        print(f"attempt {attempt}: HTTP {status} tokens={usage.get('total_tokens')} "
              f"content={'<none>' if content is None else str(len(content)) + ' chars'}")
        if status == 200 and content:
            with open(os.path.join(HERE, "responses", scenario_id + ".json"), "w") as f:
                json.dump({"id": scenario_id, "prompt": sc["prompt"],
                           "status": status, "body": body}, f, indent=1)
            break
    else:
        print(f"no answer after {max_attempts} attempts ({spent} tokens spent)")
        sys.exit(1)

    # Rebuild the fixture from every saved response, in request order.
    totals = {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
    exchanges, shared = {}, {}
    for s in scenarios.values():
        path = os.path.join(HERE, "responses", s["id"] + ".json")
        if not os.path.exists(path):
            continue
        record = json.load(open(path))
        body = record["body"]
        usage = body.get("usage", {}) if isinstance(body, dict) else {}
        for k in totals:
            totals[k] += usage.get(k, 0) or 0
        content = content_of(body)
        if record["status"] == 200 and content:
            shared.setdefault(s["prompt"], []).append(s["id"])
            if s["prompt"] not in exchanges:
                exchanges[s["prompt"]] = {
                    "prompt": s["prompt"], "response": content,
                    "scenario": s["id"], "usage": usage,
                }
    fixture = json.load(open(FIXTURE))
    fixture["exchanges"] = list(exchanges.values())
    fixture["shared_prompts"] = {p: ids for p, ids in shared.items() if len(ids) > 1}
    fixture["total_usage"] = totals
    with open(FIXTURE, "w") as f:
        json.dump(fixture, f, indent=1)
    print(f"fixture rebuilt: {len(exchanges)} exchanges, sweep totals {totals}")

if __name__ == "__main__":
    main()
