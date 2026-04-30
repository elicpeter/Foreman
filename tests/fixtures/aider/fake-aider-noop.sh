#!/bin/sh
# Aider ran but made no edits and produced no commit. Token report still emitted.
# Tests the "no-op" stop_reason path: exit 0, no ToolUse events, but TokenDelta
# still produced.
set -eu

cat <<'TXT'
Aider v0.71.0
Model: anthropic/sonnet-4.5

Nothing to change — the file already implements what you asked.

Tokens: 320 sent, 45 received.
Cost: $0.00 message, $0.01 session.
TXT
exit 0
