#!/usr/bin/env bash
#
# Private usage check for svmscope — how many people are actually using it.
#
# Reads the token-gated /stats endpoint on the live engine and prints a summary.
# The numbers are only visible to whoever holds SVMSCOPE_STATS_TOKEN.
#
# Setup (once):
#   export SVMSCOPE_STATS_TOKEN=<the same secret set on the server>
#   # optional, defaults to the hosted engine:
#   export SVMSCOPE_STATS_URL=https://svmscope.onrender.com
#
# Usage:
#   ./scripts/usage.sh
#
# Note: on a free-tier host the engine may be asleep — the first call can take
# 30–60s to cold-start. Just re-run if it times out.

set -euo pipefail

BASE="${SVMSCOPE_STATS_URL:-https://svmscope.onrender.com}"
TOKEN="${SVMSCOPE_STATS_TOKEN:-}"

if [[ -z "$TOKEN" ]]; then
  echo "error: set SVMSCOPE_STATS_TOKEN to the secret you configured on the server." >&2
  echo "       (Render dashboard → svmscope → Environment → SVMSCOPE_STATS_TOKEN)" >&2
  exit 1
fi

resp="$(curl -fsS --max-time 90 "${BASE}/stats?token=${TOKEN}")" || {
  echo "error: could not reach ${BASE}/stats (server asleep? wrong token? — a bad" >&2
  echo "       token returns 404 on purpose). Try again in a moment." >&2
  exit 1
}

if command -v jq >/dev/null 2>&1; then
  echo "── svmscope usage ─────────────────────────────"
  jq -r '
    "Total API calls    : \(.total_requests)",
    "Unique users (all) : \(.unique_clients)",
    "Active users 24h   : \(.active_clients_24h)",
    "Active users 7d    : \(.active_clients_7d)",
    "",
    "By endpoint:",
    (.per_endpoint | to_entries | sort_by(-.value)[] | "  \(.key): \(.value)"),
    "",
    "Recent days:",
    (.per_day_last_30 | to_entries | sort_by(.key) | reverse | .[:10][] | "  \(.key): \(.value)")
  ' <<<"$resp"
else
  # No jq — print the raw JSON.
  echo "$resp"
fi
