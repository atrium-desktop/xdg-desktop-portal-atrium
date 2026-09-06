# ADR-0023: sigil-wire-v2 binary protocol and zero-allocation hardening

- Status: Accepted
- Date: 2026-09-08

## Context

[ADR-0020](0020-secret-vault-delegation-to-sigil.md) delegated the secret vault to the `sigil` daemon, and [ADR-0022](0022-stateless-deterministic-secret-pipeline-and-memory-hardening.md) established a stateless, mathematically deterministic HKDF secret pipeline with memory hardening (`zeroize::Zeroizing`).

However, the native IPC wire format between `atrium-portal-secret` and `sigil` remained JSON over a length-prefixed stream. Dynamic JSON deserialization has two critical security deficiencies:
1. It allocates intermediate heap buffers (AST nodes, un-scrubbed strings) during parsing.
2. The memory allocator does not wipe deallocated heap bytes, leaving transient plaintext fragments in process memory beyond the reach of `Zeroizing`.

Sigil ADR-0004 formalizes the `sigil-wire-v2` zero-allocation binary protocol. To fulfill the zero-trace memory contract of ADR-0022, `atrium-portal-secret` must adopt this binary wire format.

## Decision

The Secret portal projection (`atrium-portal-secret/src/native.rs`) upgrades to the `sigil-wire-v2` binary protocol:

1. **Fixed Binary Framing**:
   - Header: 8 bytes `[b'S', b'I', b'G', b'L', version=2, opcode/status, u16_be payload_len]`.
   - Request: Compact length-prefixed strings `[u16_be len][bytes]` without JSON framing.
   - Response: Direct 32-byte secret payload wrapped in `Zeroizing<Vec<u8>>`.
2. **Zero JSON Heap Allocations**:
   - All `serde` and `serde_json` dependencies are removed from the native IPC dispatch path.
   - Transient buffers are scrubbed immediately upon consumption.
3. **Test Fixtures**:
   - `FakeSigil` in test harnesses speaks the `sigil-wire-v2` binary protocol verbatim.

## Consequences

- The portal daemon never parses JSON during secret retrieval, eliminating heap fragmentation and residual memory traces.
- Complete alignment with Sigil ADR-0004.
- Performance improves from microsecond JSON serialization to sub-microsecond binary streaming.
