# ADR-0013: Crash-safe two-phase vault re-key

- Status: Superseded by [ADR-0020](0020-secret-vault-delegation-to-sigil.md)
- Date: 2026-08-13
- Supersedes: the re-key write protocol of
  [ADR-0009](0009-vault-kdf-persistence-and-password-lifecycle.md)

## Context

ADR-0009's re-key wrote `vault.enc`, then `vault.kdf`, then `vault.salt`
as individually atomic replaces and documented the remaining window: a
crash between the first and the later writes leaves new ciphertext beside
old parameters — an undecryptable vault, recoverable only from backup.
ADR-0010's PAM `password` hook then made re-keys routine (every login
password change triggers one, inside arbitrary PAM client processes),
turning a theoretical crash window into a recurring one.

## Decision

The re-key — `change_password`, `rekey_password_vault_in`, and through it
the PAM chauthtok propagation — is a two-phase protocol:

1. Stage the new parameters as `vault.kdf.next` + `vault.salt.next`
   (atomic replaces).
2. Atomically replace `vault.enc` — the point of no return.
3. Rename the pending pair into `vault.kdf` / `vault.salt` and fsync the
   directory.

Unlock tries the KDF candidates in order — `vault.kdf`, `vault.kdf.next`,
legacy `vault.salt`, `vault.salt.next` — where the base files keep their
fail-closed parse behavior and a pending `.next` file is only ever
skipped, never an error. After a successful password unlock the daemon
reconciles: a winning pending pair is adopted into its final position
(together with its mirror), a legacy salt winner backfills `vault.kdf`,
and leftover pending files of an interrupted re-key are removed. A
pending file whose adoption rename fails is kept rather than deleted: it
may be the only record of the live key's parameters. Every reconcile step
is best-effort and never fails a successful unlock.

The invariant: every reachable on-disk state either decrypts under one of
the four candidates or the password is simply wrong — an interrupted
re-key never desyncs the vault. A crash before the `vault.enc` swap heals
by cleanup; a crash after it heals by adoption.

Master-key unlocks (the v2 PAM token path, ADR-0012) deliberately skip
reconciliation: without the password there is no way to tell a stale
pending pair from the live one, so `.next` files are left untouched for
the next password unlock.

## Alternatives

- **Keep the documented crash window.** Rejected: the PAM hook made
  re-keys routine, so the window's probability grew with usage, and the
  failure mode is an undecryptable vault.
- **A single transactional rewrite.** Rejected: Linux offers no
  multi-file rename atomicity; the candidate-order read achieves the same
  invariant with plain renames and an fsync.
- **Reconcile on master-key unlock too.** Rejected: without the password
  the daemon cannot distinguish a stale pending pair from the live one;
  leaving the files for the next password unlock is the safe choice.

## Consequences

- The desync-into-undecryptable-vault window recorded in ADR-0009 is
  eliminated; a total reconcile failure leaves the files in place for
  diagnosis and fails closed rather than destroying state.
- Known benign soft spot: a crash with only `vault.kdf` renamed leaves
  the `vault.salt` downgrade mirror stale — harmless because `vault.kdf`
  is authoritative — and it self-repairs on the next re-key.
- `vault.kdf.next` and `vault.salt.next` are transient vault-directory
  residents between an interrupted re-key and the next successful
  password unlock; backup tooling copies them as part of the directory
  like everything else.
