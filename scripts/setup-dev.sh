#!/bin/sh
set -eu

# Points this clone's Git hooks at the repository's .githooks directory.
# Idempotent: safe to run again at any time.

if [ ! -f .githooks/pre-commit ] || ! git rev-parse --git-dir >/dev/null 2>&1; then
    printf 'error: run %s from the repository root\n' "$0" >&2
    exit 1
fi

current=$(git config --local --get core.hooksPath || true)

if [ "$current" = ".githooks" ]; then
    printf '%s\n' 'core.hooksPath is already set to .githooks; nothing changed'
else
    git config --local core.hooksPath .githooks
    printf 'set core.hooksPath to .githooks (was: %s)\n' "${current:-unset}"
fi

printf '%s\n' \
    '' \
    'The pre-commit hook now runs on every commit in this clone.' \
    'It rejects staged Cargo.toml/Cargo.lock changes that would make the' \
    'Portal depend on companion source repositories (tessera, arca, sigil' \
    'git dependencies or ../ path dependencies); use the Portal-owned' \
    'wire projections and runtime contracts instead.'
