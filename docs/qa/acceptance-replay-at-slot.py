#!/usr/bin/env python3
"""Acceptance run for replay-at-slot: the cases the audit named.

Closed accounts, program upgrades, same-slot writes, and targets away from
the slot the transaction landed at. Each case states what must be true, and
the run reports what actually happened. Nothing here is a latency check.
"""
import json, os, sys, time, urllib.request

# Which engine to run against. `--base URL` (or SVMSCOPE_BASE) points the run
# at a local build; without one it checks the deployed engine.
E = "https://svmscope-engine.onrender.com"
if "--base" in sys.argv:
    E = sys.argv[sys.argv.index("--base") + 1]
elif os.environ.get("SVMSCOPE_BASE"):
    E = os.environ["SVMSCOPE_BASE"]
E = E.rstrip("/")
print(f"engine: {E}")

def get(path, timeout=900):
    t0 = time.time()
    try:
        with urllib.request.urlopen(E + path, timeout=timeout) as r:
            return json.load(r), time.time() - t0, None
    except urllib.error.HTTPError as e:
        return None, time.time() - t0, f"HTTP {e.code}: {e.read()[:160].decode('utf8','replace')}"
    except Exception as e:
        return None, time.time() - t0, str(e)[:160]

PHOENIX = "3VWfQkWzuG8ieTUUdigiYAFE7c1HgKgNQ9MxfnY8dfzmBFen7PqrkPBAoN6nTNNmfzYYTpwJkuvKW9CtEwGRUKLP"
FLASH   = "4FpC3L2Xt1LZqZKowrqZqWCjiHxHY7Ku7gQ1NnJwx3mi4rb7bAqJ5ttoKHAtqaBCzL9UvxuUnUFYgnBLQQF9pMiN"
SAMEBLK = "2ba656fxVaxc6FfaWSno9T6z4BoKRRhLeNLciQdGBbT6UBTdxktewzVdmE4SBoR3ydDgrp7JCRSav8RgTGS1zuBg"

# (name, path, checks) — each check is (label, fn(doc) -> bool)
CASES = [
    ("closed account, at its own slot", f"/analyze_at/{FLASH}?slot=446783231", [
        ("replay succeeds as on chain", lambda d: (d.get("replay") or {}).get("success") is True),
        ("compute matches on chain", lambda d: (d.get("replay") or {}).get("compute_units") == 32332),
        ("every certificate account is on the page", lambda d:
            not ({a["address"] for a in d["certificate"]["accounts"]} - {a["address"] for a in d["accounts"]})),
    ]),
    ("closed account, after it was closed", f"/analyze_at/{FLASH}?slot=446783300", [
        ("replay fails, as it would then", lambda d: (d.get("replay") or {}).get("success") is False),
        ("names the uninitialised account", lambda d:
            "AccountNotInitialized" in str((d.get("replay") or {}).get("error") or "") + str((d.get("replay") or {}).get("error_name") or "")),
    ]),
    ("same-slot writes", f"/replay_at/{SAMEBLK}?slot=447143947", [
        ("earlier writers in the block are labelled", lambda d:
            any(isinstance(a["source"], dict) and "SameBlock" in a["source"] for a in d["accounts"])),
        ("those accounts count as drift", lambda d:
            any(a["address"] in (d.get("drifted") or []) for a in d["accounts"]
                if isinstance(a["source"], dict) and "SameBlock" in a["source"])),
    ]),
    ("target away from the landing slot", f"/analyze_at/{PHOENIX}?slot=443942062", [
        ("fails 11 days later", lambda d: (d.get("replay") or {}).get("success") is False),
        ("the reason is the order book", lambda d:
            "NewOrderError" in str((d.get("replay") or {}).get("error_name") or (d.get("replay") or {}).get("error") or "")),
        ("estimated balances are declared", lambda d: len(d["certificate"].get("drifted") or []) > 0),
    ]),
    ("at the slot it landed", f"/analyze_at/{PHOENIX}?slot=441138938", [
        ("succeeds as on chain", lambda d: (d.get("replay") or {}).get("success") is True),
        ("compute matches on chain", lambda d: (d.get("replay") or {}).get("compute_units") == 53154),
    ]),
    ("program upgraded since the slot", f"/replay_at/{PHOENIX}?slot=441138900", [
        ("program binaries are labelled", lambda d:
            any(isinstance(a["source"], dict) and "Program" in a["source"] for a in d["accounts"])),
        ("an upgraded program would be flagged", lambda d:
            all((a["source"]["Program"].get("upgraded_since") is not True) or (a["address"] in (d.get("drifted") or []))
                for a in d["accounts"] if isinstance(a["source"], dict) and "Program" in a["source"])),
    ]),
    ("window boundary is enforced", "/replay_at/%s?slot=439600000" % FLASH, [
        ("refused as outside 30 days", lambda d: d is None),
    ]),
    ("malformed clock refused", "/slot_at?time=2026-09-01T25:00:00Z", [
        ("rejected", lambda d: d is None),
    ]),
    ("impossible date refused", "/slot_at?time=2026-02-30T00:00:00Z", [
        ("rejected", lambda d: d is None),
    ]),
    ("date lookup lands on time", "/slot_at?time=2026-09-01T06:30:00Z", [
        ("within 2 seconds", lambda d: abs(d.get("off_by_secs", 999)) <= 2),
    ]),
]

passed = failed = 0
for name, path, checks in CASES:
    doc, secs, err = get(path)
    print(f"\n{name}  ({secs:.0f}s)")
    if err and not any(l == "refused as outside 30 days" or l == "rejected" for l, _ in checks):
        print(f"   ERROR {err}")
        failed += len(checks)
        continue
    for label, fn in checks:
        try:
            ok = fn(doc)
        except Exception as e:
            ok = False
            label += f"  [{type(e).__name__}]"
        print(f"   {'PASS' if ok else 'FAIL'}  {label}")
        passed += ok
        failed += (not ok)
print(f"\n{passed} passed, {failed} failed")
sys.exit(1 if failed else 0)
