#!/bin/sh
# Version-consistency check: every user-visible version/protocol number
# must agree with its source of truth in code. Run manually or from CI;
# exits non-zero with the specific mismatch on the first failure.
#
# Sources of truth:
#   Cargo.toml workspace.package.version
#   atrium-portal-ipc PROTOCOL_VERSION / MIN_PROTOCOL_VERSION
#   atrium-portal-prompter PROCESS_CONTRACT_VERSION
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
status=0

fail() {
    printf 'error: %s\n' "$1" >&2
    status=1
}

# ---- workspace version <-> changelog -------------------------------------
cargo_version=$(
    sed -n 's/^version = "\([^"]*\)"$/\1/p' "$repo/Cargo.toml" | head -n 1
)
if [ -z "$cargo_version" ]; then
    fail "could not read workspace.package.version from Cargo.toml"
fi

latest_heading=$(sed -n 's/^## \[\([0-9][0-9.]*\)\] - .*/\1/p' \
    "$repo/CHANGELOG.md" | head -n 1)
if [ "$cargo_version" != "$latest_heading" ]; then
    fail "CHANGELOG.md newest release $latest_heading != version $cargo_version"
fi
if ! grep -q "^\[$cargo_version\]: " "$repo/CHANGELOG.md"; then
    fail "CHANGELOG.md is missing the [$cargo_version] link-ref definition"
fi
unreleased_base=$(sed -n 's/^\[Unreleased\]: .*compare\/v\([0-9.]*\)\.\.\.HEAD$/\1/p' \
    "$repo/CHANGELOG.md" | head -n 1)
if [ "$unreleased_base" != "$cargo_version" ]; then
    fail "CHANGELOG.md [Unreleased] compare base v$unreleased_base != v$cargo_version"
fi

# ---- IPC protocol numbers <-> prose --------------------------------------
schema="$repo/crates/atrium-portal-ipc/src/schema.rs"
proto=$(sed -n 's/^pub const PROTOCOL_VERSION: u32 = \([0-9]*\);$/\1/p' "$schema")
proto_min=$(sed -n 's/^pub const MIN_PROTOCOL_VERSION: u32 = \([0-9]*\);$/\1/p' "$schema")
if [ -z "$proto" ] || [ -z "$proto_min" ]; then
    fail "could not read PROTOCOL_VERSION/MIN_PROTOCOL_VERSION from $schema"
    proto=${proto:-unknown}
    proto_min=${proto_min:-unknown}
fi

check_doc_protocol() {
    file=$1
    if grep -q "protocol $proto, negotiating down to $proto_min" "$file"; then
        return 0
    fi
    if grep -q "$proto, negotiates down to $proto_min" "$file"; then
        return 0
    fi
    fail "$file does not state 'protocol $proto, negotiating down to $proto_min'"
}

check_doc_protocol "$repo/README.md"
check_doc_protocol "$repo/docs/reference/compatibility.md"

# Any *older* protocol offered as the current one is stale prose.
for f in "$repo/README.md" "$repo/docs/reference/compatibility.md"; do
    if grep -q 'narrow projection of Tessera IPC (protocol [0-9]*' "$f"; then
        stated=$(sed -n 's/.*narrow projection of Tessera IPC (protocol \([0-9]*\).*/\1/p' \
            "$f" | head -n 1)
        if [ "$stated" != "$proto" ]; then
            fail "$f states IPC protocol $stated in the summary, code says $proto"
        fi
    fi
    if grep -qE 'workspace (speaks|offers) protocol [0-9]+' "$f"; then
        stated=$(sed -n 's/.*workspace \(speaks\|offers\) protocol \([0-9]*\).*/\2/p' \
            "$f" | head -n 1)
        if [ "$stated" != "$proto" ]; then
            fail "$f says the workspace speaks protocol $stated, code says $proto"
        fi
    fi
done

# ---- prompter contract <-> smoke-test payloads ---------------------------
contract_lib="$repo/crates/atrium-portal-prompter/src/lib.rs"
contract=$(sed -n 's/^pub const PROCESS_CONTRACT_VERSION: u32 = \([0-9]*\);$/\1/p' \
    "$contract_lib")
if [ -z "$contract" ]; then
    fail "could not read PROCESS_CONTRACT_VERSION from $contract_lib"
else
    testing="$repo/docs/dev/portal-ui-testing.md"
    stale=$(grep -c "{\"version\":$contract,\"prompt\":" "$testing" || true)
    total=$(grep -c '{"version":[0-9]*,"prompt":' "$testing" || true)
    if [ "$stale" != "$total" ] || [ "$total" -eq 0 ]; then
        fail "$testing: $((total - stale)) payload(s) do not use contract version $contract"
    fi
fi

if [ "$status" -eq 0 ]; then
    printf 'version consistency: ok (v%s, ipc %s>=%s, contract %s)\n' \
        "$cargo_version" "$proto" "$proto_min" "${contract:-unknown}"
fi
exit "$status"
