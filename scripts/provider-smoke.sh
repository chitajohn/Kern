#!/usr/bin/env sh
# Real-provider integration tests — optional.
#
# Requires AGENTROUTER_API_KEY in the environment (never committed, never
# printed). When it is absent the tests self-skip with a notice — the script
# still exits 0.
#
# The endpoint defaults to AgentRouter; point it anywhere with
# KERN_PROVIDER_BASE_URL (e.g. https://api.openai.com/v1) — the same tests
# run against any OpenAI-compatible service.
#
# Verifies the OpenAI-compatible adapter against a live endpoint: model
# discovery, a completion, and a tool-calling round trip.
set -eu

if [ -z "${AGENTROUTER_API_KEY:-}" ]; then
  echo "AGENTROUTER_API_KEY is not set — real-provider tests will self-skip." >&2
fi
if [ -n "${KERN_PROVIDER_BASE_URL:-}" ]; then
  echo "KERN_PROVIDER_BASE_URL=$KERN_PROVIDER_BASE_URL"
fi

cargo test -p kern-model --test real_provider -- --nocapture
