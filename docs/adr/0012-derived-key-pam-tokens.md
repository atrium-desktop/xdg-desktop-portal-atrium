# ADR-0012: Derived-key PAM unlock tokens

- Status: Superseded by [ADR-0020](0020-secret-vault-delegation-to-sigil.md)
- Date: 2026-08-13
- Related: [ADR-0009](0009-vault-kdf-persistence-and-password-lifecycle.md),
  [ADR-0010](0010-pam-confirmed-planting-and-libpam-abi.md)

## Context

The token format established by ADR-0010 plants the raw login password at
`/run/user/<uid>/aegis-pam-token` (mode 0600, session tmpfs) until the
portal consumes it. For the common case — an account with a password-mode
vault — the daemon needs only the vault master key, which is derivable
from the password plus the persisted KDF parameters (ADR-0009). Planting
the reusable login password where a single-purpose vault key suffices
widens the at-rest secret for no benefit: a same-uid reader of the file,
or a token the portal never consumes, holds the user's login password for
the rest of the session.

## Decision

1. When the account holds a password-mode vault, the PAM module derives
   the Argon2id vault master key — the authoritative `vault.kdf`
   parameters, or the legacy crate defaults for a salt-only vault — and
   plants the token as ASCII `aegis-key-v1:` plus 64 lowercase hex chars
   (77 bytes). Anything else in the file parses as a legacy raw-password
   token, which the daemon keeps accepting.
2. A keyfile-mode vault plants no token at all: the daemon unlocks from
   `vault.key` at startup, so a token would only leave secret material at
   rest.
3. A non-UTF-8 password or an unresolvable vault layout falls back to the
   legacy raw-password plant.
4. The `aegis-key-v1:` prefix commits to the key format: malformed hex
   after the prefix makes the whole token invalid — it fails closed and
   never falls through to the password path.
5. The derivation lives in the new public
   `aegis_portal_secret::derive_token_key_in`. It reads the KDF files
   with `O_NOFOLLOW` and size caps but *without* the real-uid ownership
   check, because the PAM module legitimately runs as root in the login
   stack and resolves the vault directory from the target account's
   passwd entry; this exception is documented at the API. Symlinks,
   non-regular files, group/world-writable modes, and oversize content
   are still refused.
6. The daemon unlocks a v2 token by direct master-key decryption
   (`unlock_with_master_key`): authenticated decryption is the validity
   check, so a wrong or garbage key fails closed. No KDF reconciliation
   happens on this path (see
   [ADR-0013](0013-two-phase-vault-rekey.md)). The single-shot,
   unlink-before-read consumption is unchanged.

## Alternatives

- **Keep planting the raw password.** Rejected: the at-rest tmpfs secret
  stays the reusable login password when a single-purpose key suffices.
- **Encrypt the token to a daemon-held key.** Rejected: it adds a key
  distribution problem inside the same-uid trust model the stack already
  assumes, and protects the password no better than the file mode does.
- **Derive a separate token-specific key.** Rejected: the vault master
  key is exactly what the daemon consumes; a second derivation adds a
  second secret to protect without narrowing anything.

## Consequences

- On password-mode systems the at-rest tmpfs secret narrows from the
  reusable login password to the vault key: a stolen v2 token still
  unlocks that vault — authenticated decryption, not token secrecy, is
  the integrity boundary — but it cannot authenticate as the user
  anywhere else.
- The token file remains readable by same-uid processes while it exists,
  and an unconsumed token persists for the session; the single-shot
  consumption and the mode-0600 hardening are unchanged.
- Legacy raw-password tokens keep working, so mixed older/newer module
  and daemon pairs degrade gracefully; only the prefixed format is
  interpreted strictly.
- Keyfile-mode accounts no longer leave any token on the tmpfs after a
  confirmed login.
