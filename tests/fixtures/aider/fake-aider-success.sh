#!/bin/sh
# Fake `aider` binary used by AiderAgent unit tests.
# Emits a representative subset of `aider --message` plain-text output for a
# successful run that touches two files and produces a commit. Aider has no
# JSON output mode, so the parser keys off line prefixes.
set -eu

cat <<'TXT'
Aider v0.71.0
Model: anthropic/sonnet-4.5
Git repo: .git with 42 files

Hello from Aider — implementing the change now.

Applied edit to src/foo.rs
Applied edit to src/bar.rs
Commit a1b2c3d feat: add foo helper

Tokens: 1.2k sent, 800 received.
Cost: $0.01 message, $0.03 session.
TXT
exit 0
