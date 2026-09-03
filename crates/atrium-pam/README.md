# atrium-pam

Optional PAM module for password-protected Tessera secret vaults.

**Unlock token.** After another PAM module verifies a password,
`pam_atrium.so`'s `authenticate` hook stashes the authtok in PAM module data
behind a zeroizing cleanup — no file yet, since later modules can still fail
the login. The first committing hook (`setcred`, or `open_session` when no
setcred hook runs) writes a short-lived mode-0600 token into the user's
runtime directory. The `atrium-portal-secret` component consumes and deletes
that token to unlock the vault without showing a second password prompt. On
a failed login `pam_end` zeroizes the stash and no token file ever exists;
stacks that only authenticate (some screen lockers) plant no token.

**Password propagation.** On the `password` update phase (not the
preliminary probe), the module re-keys the target user's password-mode
vault from `PAM_OLDAUTHTOK` to `PAM_AUTHTOK`, so the vault password tracks
the login password. All failures are silent: the module never decides
authentication and must be configured as `optional`. Admin-initiated resets
skip the vault, which then falls back to the Portal's unlock prompt.

Place `auth optional pam_atrium.so` after the module that establishes the
authentication token, `session optional pam_atrium.so` after the logind
session module, and `password optional pam_atrium.so` after the module that
sets the new authentication token. The module resolves the account with
`getpwnam_r`, writes tokens only to the kernel-owned `/run/user/<uid>`
directory, and refuses an unsafe owner or mode. It never trusts the
authenticating process's `XDG_RUNTIME_DIR` or `HOME`; the vault directory
follows an absolute `XDG_DATA_HOME` from the PAM environment when present
and the account's `pw_dir` otherwise. For setuid-root clients such as
`passwd` it borrows the target user's filesystem identity (setfsuid) for
the re-key so vault files keep the user's ownership.

The module links libpam directly and is MIT-licensed like the rest of the
workspace.
