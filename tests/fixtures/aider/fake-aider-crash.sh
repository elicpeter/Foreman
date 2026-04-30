#!/bin/sh
# Aider exits non-zero with an error on stderr. Aider has no structured error
# event, so AiderAgent surfaces the exit code plus stderr tail.
set -eu

echo "Error: missing ANTHROPIC_API_KEY environment variable" 1>&2
exit 1
