#!/usr/bin/env python3
"""Extract candidate Pump reserve preimages from saved getBlock JSON.

Research tool, not an exact-state certificate. Only the four legacy SOL
reserve fields are reconstructed; account layouts, other bytes, programs,
sysvars and arbitrary target-slot history are NOT reconstructed.

python3 scripts/probe_pump_history.py BLOCK.json SLOT --out REPORT.json
Input must be a finalized, full, JSON-encoded getBlock response. Keep the
original response: its SHA-256 is included for reproducibility, not as a
consensus proof. No network requests or writes to the production recorder.
"""

import argparse
import base64
import hashlib
import json
import re
import struct
from datetime import datetime, timezone
from pathlib import Path

PROGRAM = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P"
DISCRIMINATOR = hashlib.sha256(b"event:TradeEvent").digest()[:8]
ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
FIELDS = ("virtual_sol", "virtual_token", "real_sol", "real_token")


def base58(raw):
    n = int.from_bytes(raw, "big")
    out = ""
    while n:
        n, digit = divmod(n, 58)
        out = ALPHABET[digit] + out
    return "1" * (len(raw) - len(raw.lstrip(b"\0"))) + out


def decode_event(raw):
    if len(raw) < 129 or raw[:8] != DISCRIMINATOR:
        return None
    sol, tokens = struct.unpack_from("<QQ", raw, 40)
    buy = raw[56]
    if buy not in (0, 1):
        raise ValueError("invalid Borsh boolean")
    post = struct.unpack_from("<QQQQ", raw, 97)
    # Official legacy buy/sell reserve transitions, inverted. These are
    # candidate field values, not evidence that all inputs are recovered.
    delta = (sol, -tokens, sol, -tokens)
    pre = tuple(p - d if buy else p + d for p, d in zip(post, delta))
    if any(not 0 <= value < 2**64 for value in pre):
        raise ValueError("reserve inversion overflow")
    return {
        "mint": base58(raw[8:40]),
        "is_buy": bool(buy),
        "sol_amount": sol,
        "token_amount": tokens,
        "event_timestamp": struct.unpack_from("<q", raw, 89)[0],
        "event_bytes": len(raw),
        "candidate_pre": dict(zip(FIELDS, pre)),
        "reported_post": dict(zip(FIELDS, post)),
    }


def transaction_events(tx):
    meta = tx.get("meta")
    if not meta or "err" not in meta or meta["err"] is not None:
        return []
    stack, events = [], []
    for index, line in enumerate(meta.get("logMessages") or []):
        invocation = re.fullmatch(r"Program (\w+) invoke \[(\d+)\]", line)
        finish = re.fullmatch(r"Program (\w+) (?:success|failed:.*)", line)
        if invocation:
            if int(invocation[2]) != len(stack) + 1:
                return []  # incomplete/inconsistent log stack
            stack.append(invocation[1])
        elif finish:
            if not stack or stack.pop() != finish[1]:
                return []
        elif line.startswith("Program data: ") and stack and stack[-1] == PROGRAM:
            try:
                raw = base64.b64decode(line[14:], validate=True)
                event = decode_event(raw)
            except ValueError:
                continue
            if event:
                events.append(dict(event, log_index=index))
    # A truncated log may omit later events/writes. Reject the transaction.
    return events if not stack else []


def probe(response, slot):
    if response.get("error"):
        raise ValueError(f"RPC error: {response['error']}")
    block = response["result"]
    rows, previous, pairs = [], {}, []
    for index, tx in enumerate(block["transactions"]):
        for event in transaction_events(tx):
            row = dict(event, tx_index=index, signature=tx["transaction"]["signatures"][0])
            earlier = previous.get(row["mint"])
            if earlier:
                pairs.append({
                    "mint": row["mint"],
                    "earlier_signature": earlier["signature"],
                    "later_signature": row["signature"],
                    "fields_match": earlier["reported_post"] == row["candidate_pre"],
                })
            previous[row["mint"]] = row
            rows.append(row)
    return {
        "schema_version": 1,
        "status": "experimental_partial_fields_only",
        "slot": slot,
        "block_time": block["blockTime"],
        "block_date_utc": datetime.fromtimestamp(block["blockTime"], timezone.utc).isoformat(),
        "program": PROGRAM,
        "transactions_in_block": len(block["transactions"]),
        "trade_events": len(rows),
        "consecutive_pairs": len(pairs),
        "matching_pairs": sum(p["fields_match"] for p in pairs),
        "limitations": [
            "Consecutive event agreement checks four fields, not complete account state.",
            "Legacy SOL reserve inversion; newer quote-asset semantics need separate support.",
            "No historical program binaries or other execution inputs are supplied.",
            "No arbitrary-slot or complete 30-day coverage is established.",
        ],
        "events": rows,
        "pairs": pairs,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("block", type=Path)
    parser.add_argument("slot", type=int)
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    raw = args.block.read_bytes()
    report = probe(json.loads(raw), args.slot)
    report["source_sha256"] = hashlib.sha256(raw).hexdigest()
    args.out.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({k: report[k] for k in (
        "status", "slot", "block_date_utc", "trade_events", "consecutive_pairs", "matching_pairs"
    )}))


if __name__ == "__main__":
    main()
