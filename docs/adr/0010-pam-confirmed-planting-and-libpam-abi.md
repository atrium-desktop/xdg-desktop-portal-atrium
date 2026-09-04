# ADR-0010: PAM confirmed planting, vault re-key, and the libpam C ABI

- Status: Superseded by [ADR-0020](0020-secret-vault-delegation-to-sigil.md)
- Date: 2026-08-13
- Related: [ADR-0009](0009-vault-kdf-persistence-and-password-lifecycle.md)

## Context

The PAM module wrote the vault-unlock token file inside
`pam_sm_authenticate` — before later modules in the auth stack could
still fail the login. A failed login therefore planted a plaintext
password token at `/run/user/<uid>/aegis-pam-token` that persisted for
the session, and the authenticate-time write also raced logind: on the
first login after boot, `/run/user/<uid>` may not exist yet. Separately,
a login password change had no propagation path to the vault password.

The implementation also depended on `pamsm` 0.4.3, whose `pam_module!`
macro types libpam's `flags` argument as a Rust enum that lacks
`PAM_PRELIM_CHECK` and `PAM_UPDATE_AUTHTOK` — and any combined flag
values — so every `chauthtok` call would materialize an invalid enum
discriminant (undefined behavior) exactly where the phase must be read.
The macro likewise types `argc` as `usize` while libpam passes a 32-bit
`int`, and the wrapper exposes no raw-handle escape hatch.

## Decision

1. **Confirmed planting.** `pam_sm_authenticate` only stashes the
   just-verified authtok in PAM module data behind a zeroizing cleanup
   that runs at `pam_end`. The token file is written by the first
   committing hook — `pam_sm_setcred` (any call except
   `PAM_DELETE_CRED`) or `pam_sm_open_session`, whichever fires first. A
   `planted` flag prevents double-writes, and a failed write keeps the
   stash so the later hook retries, which also covers the cold-boot first
   login where the runtime directory appears only at session
   registration. A failed login never plants a token file.
2. **Vault re-key on password change.** `pam_sm_chauthtok`, on the update
   phase only (flags bit-tested; the preliminary probe and ambiguous
   values skip), with non-empty `PAM_OLDAUTHTOK` and `PAM_AUTHTOK`,
   re-keys the target user's password-mode vault through
   `rekey_password_vault_in` (ADR-0009). Keyfile-mode and absent vaults
   skip, as do admin-initiated resets (the invoking real uid must be the
   target account); setuid-root clients such as `passwd` borrow the
   target user's filesystem identity (`setfsuid`/`setfsgid`, restored on
   drop) so the rewritten files keep the user's ownership. Every failure
   returns `PAM_SUCCESS` — the module is optional and never blocks a
   password change — and nothing sensitive is logged. Non-UTF-8 passwords
   skip.
3. **Direct libpam FFI.** `pamsm` is removed; all six entry points are
   implemented against libpam's stable C ABI with `flags` and `argc` as
   `c_int` and the constants verified against
   `/usr/include/security/`. `pam_sm_acct_mgmt` returns `PAM_IGNORE`, the
   one status that can never influence a stack.
4. **Relicensing.** `aegis-pam` moves from GPL-3.0-only to MIT, the
   workspace license: the GPL obligation came solely from `pamsm`, and
   libpam itself is BSD-licensed.

The accepted behavior change: stacks that only authenticate and never
commit credentials or open a session (some screen lockers) no longer
plant a token; those setups unlock the vault through the Portal's own
prompt.

## Alternatives

- **Keep planting at `authenticate`.** Rejected: a failed login leaves a
  session-lifetime plaintext token, and the write races logind's
  runtime-directory creation.
- **Plant at `open_session` only.** Rejected: `setcred` is the canonical
  credential-commit point and fires earlier; covering both hooks with a
  `planted` flag handles every stacking order, including session-only
  stacks.
- **Keep `pamsm` and skip the `chauthtok` hook.** Rejected: the flags-enum
  undefined behavior sits exactly in the hook that must read the phase,
  and working around it needs the raw handle the wrapper never exposes.
- **Map module failures to PAM error codes.** Rejected: the module is
  `optional` by design; it must never grant, deny, or block — at worst
  the vault stays locked or stale and the Portal prompts.

## Consequences

- The recommended stacking is three lines: `auth`, `session`, and
  `password`, each `optional pam_aegis.so`.
- `deny.toml`'s GPL license exceptions and the Meson GPL packaging
  warning are removed; the entire workspace, including a binary package
  with `pam_aegis.so`, is MIT-licensed.
- Token planting survives cold-boot first logins through the
  setcred→open_session retry, and pure-auth stacks degrade to the
  Portal's unlock prompt instead of planting early.
- The module runs `unsafe` FFI against libpam directly; the ABI constants
  are verified against the system headers, and the entry points bit-test
  flags rather than trusting enum discriminants.
