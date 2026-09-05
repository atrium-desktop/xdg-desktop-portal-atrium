# How to Install for Production

## Install Dependencies

Install Rust 1.88 or newer, `pkg-config`, the optics C
libraries (flux, lens, and iris from the tagged `ming2k/optics` release),
`xdg-desktop-portal`,
WirePlumber, and `xdg-email`. Inhibit additionally uses logind and Print
uses the CUPS `lp` client at runtime; both are ordinary session services,
not build dependencies.

Install a Tessera runtime that implements the protocol version in the
[Compatibility Reference](../reference/compatibility.md). The Portal build
does not require a Tessera source checkout, Cargo package, or path override.

## Build and Install

Run from the repository root:

```bash
./scripts/install.sh --prefix /usr
```

Set `DESTDIR` for a staged distribution package (build once, then install
from the existing `target/release` artifacts):

```bash
cargo build --locked --release -p xdg-desktop-portal-atrium -p atrium-portal-prompter
DESTDIR="$PWD/stage" ./scripts/install.sh --prefix /usr --no-build
```

The install script places both private executables under the configured
`libexecdir`, generates the matching D-Bus service, and installs
`atrium.portal` plus `tessera-portals.conf` (with `atrium-portals.conf` compatibility)
under the configured data directory.

## Enable Secret Storage and Vault Unlock

Secret retrieval delegates to the sigil daemon (ADR-0020). Install and
enable the sigil service, which owns the at-rest vault, unlock prompting,
and the logind session-lock binding; its PAM module (`pam_sigil`) provides
login-time auto-unlock and vault password propagation. Configure and
secure that module per the sigil installation guide.

## Restart the Portal Frontend

Restart the user portal after installing or upgrading:

```bash
systemctl --user restart xdg-desktop-portal.service
```

Log out and back in when the session does not use the systemd user service.
Confirm that `XDG_CURRENT_DESKTOP` contains `tessera` before starting the
session portal.

## Validate the Installation

Confirm the installed files and activate the backend:

```bash
busctl --user introspect \
  org.freedesktop.impl.portal.desktop.atrium \
  /org/freedesktop/portal/desktop
```

The output must list exactly the native interfaces in the
[Portal Support Reference](../reference/portal-support.md). Check recent
activation errors with:

```bash
journalctl --user \
  -u xdg-desktop-portal.service \
  --since today
```

ScreenCast additionally requires a running PipeWire server and WirePlumber.
Verify both before diagnosing the backend:

```bash
pw-dump
wpctl status
```

## Migrate From the GTK Fallback

Releases before [ADR-0007](../adr/0007-full-stack-interface-ownership.md)
delegated uncovered interfaces to `xdg-desktop-portal-gtk` through an
`tessera;gtk` route. Migrate such a deployment before starting the new
backend:

1. Remove the `xdg-desktop-portal-gtk` package. Every interface the routing
   configuration names is now served natively, so the fallback serves
   nothing.
2. Delete any portals configuration that still routes to `gtk`. Check for
   `portals.conf` or `*-portals.conf` files naming `tessera;gtk` under
   `/usr/share/xdg-desktop-portal/`, `/etc/xdg/xdg-desktop-portal/`, and
   `~/.config/xdg-desktop-portal/`. The new package installs
   `tessera-portals.conf` (and `atrium-portals.conf`) with `atrium`-only routes; remove hand-written
   overrides rather than editing the packaged file.
3. Install the new package as in [Build and Install](#build-and-install).
4. Restart the frontend as in
   [Restart the Portal Frontend](#restart-the-portal-frontend).
5. Re-validate the interface list as in
   [Validate the Installation](#validate-the-installation). The output must
   list the native interfaces only; interfaces that used to fall back
   (Inhibit, AppChooser, Notification, DynamicLauncher, Wallpaper, Access,
   OpenURI, Background, Print) are now served by the Tessera backend itself.

## Back Up or Migrate Secrets

The vault is owned by the sigil daemon; back up and migrate it with the
sigil tooling while the sigil service is stopped. Never restore vault
files without their matching key material — see the sigil documentation
for the authoritative procedures.
