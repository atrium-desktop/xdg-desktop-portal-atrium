# ADR-0019: The vault lock state follows the session lock boundary

- Status: Accepted
- Date: 2026-08-23
- Related: [ADR-0009](0009-vault-kdf-persistence-and-password-lifecycle.md),
  [ADR-0010](0010-pam-confirmed-planting-and-libpam-abi.md),
  [ADR-0012](0012-derived-key-pam-tokens.md)

## Context

The vault's only automatic lock was a 15-minute inactivity watcher
(0.0.18). Its timer measured "no application read a secret" — not "the
user left" — so it locked while the user sat there working (no app had
queried the vault) and, conversely, kept the master key in memory while
the screen had been locked for 14 minutes. On a keyfile-mode vault,
which has no password, the lock then produced an unlock prompt that
could never succeed: the prompt path derives a key from a typed
password, and a keyfile vault has no KDF material for one. The result
was a dead-end "Unlock Keyring" dialog that reappeared on every
`RetrieveSecret` until the backend restarted.

Meanwhile the desktop's own lock boundary — the idle policy's secure
lock frame, the screen locker's authentication, suspend/resume — never
reached the vault at all. logind's `Session.Lock`/`Session.Unlock`
signals, the freedesktop-standard notification channel for exactly this
coupling, had no publisher and no subscriber in the Aegis stack.

## Decision

1. **The vault locks and unlocks with the session, nothing else.** The
   backend subscribes to `org.freedesktop.login1.Session.Lock` and
   `.Unlock` on the system bus, and to `Manager.PrepareForSleep`. The
   session object is resolved through `$XDG_SESSION_ID`
   (`Manager.GetSession`) with `GetSessionByPID(self)` as fallback: a
   D-Bus-activated backend lives in the user manager's cgroup, where a
   PID lookup resolves to the class=manager session — which logind
   exempts from locking and which never carries the lock signals. Lock and
   sleep-entry call `SecretService::lock_for_session` (zeroize the
   master key, dismiss queued unlock requests); Unlock and sleep-exit
   call `unlock_for_session`. The 15-minute idle watcher is removed.
2. **Keyfile-mode vaults re-unlock from `vault.key`.** A keyfile vault
   has no credential to prompt for; after a session lock the returning
   user already proved session ownership to the screen locker, so
   `unlock_for_session` (and the unlock coordinator serving a waiting
   `RetrieveSecret`) re-reads `vault.key` with the same
   `O_NOFOLLOW`/ownership/mode-0600/size validation as startup and
   proves it by authenticated decryption. A swapped or tampered keyfile
   fails closed and stays locked.
3. **Password-mode vaults stay locked on session return.** Their only
   re-unlock paths remain the PAM token (planted by a committing PAM
   hook — login, or a screen locker that establishes credentials, per
   ADR-0010) and the masked prompt. `start_pam_watcher` becomes a
   lifetime loop: it no longer exits when the vault happens to be
   unlocked at startup, so a token planted after *any* lock — session
   lock included — is consumed; a successful consumption also completes
   any requests queued behind the prompt.
4. **Locks drain the unlock queue.** A caller queued behind the unlock
   prompt when the session locks is dismissed, not answered from a vault
   the user never re-authorized after the lock.
5. **No session, no coupling.** In environments without a logind session
   (nested compositors, remote) the watcher disables itself for one
   retry interval and the vault keeps its startup state — the same
   honest fallback the inhibit integration uses for a missing system
   bus.

The `aegis-lock` side of the contract (it calls `pam_setcred` after
successful authentication, which is the committing hook ADR-0010
designates for token planting) is implemented in the Aegis repository;
this ADR covers the backend's half.

## Alternatives

- **Keep the 15-minute watcher as a belt-and-braces fallback.**
  Rejected: a timer that fires during active use and not during a real
  absence is not a fallback, it is a second, wrong policy. With the
  session boundary wired, inactivity adds no signal the boundary does
  not already carry.
- **Subscribe to the compositor's session-lock IPC instead of logind.**
  Rejected: logind's signals are the freedesktop-standard channel, they
  already reach every session component (the Secret Service daemon
  `wssp` subscribes to the same signal), and they require no new
  portal↔compositor coupling. The AGENTS boundary keeps compositor
  integration out of the secret crate.
- **Plant a PAM token for keyfile vaults too, so one re-unlock path
  serves both modes.** Rejected: ADR-0012 decision 2 — a keyfile vault
  plants no token, because that would leave secret material at rest on
  the tmpfs for no benefit. The keyfile re-read is strictly better: no
  at-rest token, same fail-closed validation.
- **Prompt for a password on a locked keyfile vault.** Rejected: the
  vault has no password; the prompt is a UI dead end (the original
  complaint).

## Consequences

- The dead-end "Unlock Keyring" prompt on keyfile vaults is gone: the
  vault unlocks silently with the session and serves waiting requests
  from the keyfile.
- A suspended or screen-locked machine no longer holds the vault master
  key in RAM — the actual security property the 15-minute timer aimed
  for, now keyed to real absence boundaries.
- Suspend now locks password-mode vaults that were unlocked by an
  earlier PAM token; they return locked and need a token (login-screen
  unlock) or the prompt. This matches how GNOME treats suspend for the
  login keyring and is deliberate.
- Users of `lock()` (the explicit API) keep the old semantics; only the
  automatic policy changed.
- Without a logind session the vault never auto-locks; deployments that
  need at-rest hygiene there should encrypt the data directory.
