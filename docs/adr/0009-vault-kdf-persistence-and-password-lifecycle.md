# ADR-0009: Vault KDF persistence and the password lifecycle

- Status: Superseded by [ADR-0020](0020-secret-vault-delegation-to-sigil.md)
- Date: 2026-08-13

## Context

Password-mode vaults derived the master key with `Argon2::default()` and
recorded only the salt in `vault.salt`; the Argon2id parameters were
implicit in the argon2 crate version the daemon happened to build with.
An argon2-crate default change would then silently invalidate every
password-mode vault: the derivation would produce a different key and
authenticated decryption would fail on otherwise intact data.

Two lifecycle gaps compounded the risk. Nothing in the repository could
create a password-mode vault (startup only ever created keyfile-mode
vaults; password vaults were inherited from the wssp era), and nothing
could change a vault's password, so the vault password could never track
the login password the PAM module already handles.

## Decision

Password-mode KDF configuration is persisted on disk instead of implied:

1. A `vault.kdf` sidecar (JSON, schema version 1) records the exact
   Argon2id parameters and salt a vault is keyed with. The `m_cost`,
   `t_cost`, and `p_cost` fields are written from the actual
   `argon2::Params` in use, never from hardcoded literals, so the record
   stays correct if crate defaults move. `vault.kdf` is authoritative
   when present; a bare `vault.salt` marks a legacy vault keyed with the
   crate-default parameters. A malformed version, KDF name, parameter
   set, or salt fails closed.
2. A legacy salt-only vault migrates on its first successful unlock:
   `vault.kdf` is written through an atomic create (an `AlreadyExists`
   race with a concurrently migrating daemon counts as success), and
   `vault.salt` is kept as a downgrade mirror for older daemons while the
   parameters equal the crate default.
3. The password lifecycle is owned by the secret crate:
   `SecretService::create_password_vault` creates a fresh password-mode
   vault (refusing to overwrite anything), `SecretService::change_password`
   re-keys an unlocked service's vault, and the prompter-free
   `rekey_password_vault_in` re-keys a vault directory from any process —
   the entry point the PAM `password` hook uses (see
   [ADR-0010](0010-pam-confirmed-planting-and-libpam-abi.md)). Re-keying
   proves the current password by authenticated decryption before any
   file is touched, rotates to a fresh salt, and writes `vault.enc`,
   then `vault.kdf`, then `vault.salt`, each as an atomic replace.
4. The in-memory master key is heap-pinned and `mlock`ed on a best-effort
   basis: a failure (for example `RLIMIT_MEMLOCK`) logs a warning and
   never fails an unlock, and the key is zeroized and `munlock`ed on
   drop.

## Alternatives

- **Pin the argon2 crate version instead of persisting parameters.**
  Rejected because a lockfile pin still breaks on the first deliberate
  upgrade and leaves every existing vault keyed by accident; persisted
  parameters make the derivation reproducible regardless of crate
  defaults.
- **Write only `vault.kdf` and remove `vault.salt`.** Rejected because
  older daemons read `vault.salt`; keeping it as a mirror while the
  parameters equal the crate default preserves downgrade compatibility at
  no cost.
- **Re-key without proving the current password.** Rejected because
  re-encrypting under a fresh key without decrypting first would wedge
  the vault on a stale or mistyped old password; authenticated decryption
  is the proof, and a wrong password is a clean error before any write.

## Consequences

- Startup decodes `vault.kdf` when present and fails closed on malformed
  content, so a corrupt sidecar blocks the daemon rather than silently
  deriving with the wrong parameters.
- The vault password can now track the login password through the PAM
  re-key hook, and password-mode vaults no longer depend on inherited
  wssp-era files.
- The re-key's three writes are individually atomic but not
  transactional: a crash between the first (`vault.enc`) and the later
  writes can desynchronize the trio, while a failure of the first write
  leaves the previous trio fully consistent. Recovery from the crash
  window is restoring the directory from backup; the window is a
  documented code caveat.
- `mlock` never gates an unlock: sessions under a restrictive
  `RLIMIT_MEMLOCK` log a warning and run with a pageable key.
