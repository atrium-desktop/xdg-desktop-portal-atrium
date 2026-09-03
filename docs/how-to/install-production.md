# How to Install for Production

## Install Dependencies

Install Rust 1.88 or newer, Meson, Ninja, `pkg-config`, the optics C
libraries (flux, lens, and iris from the tagged `ming2k/optics` release),
PipeWire and SPA development files, `xdg-desktop-portal`,
WirePlumber, and `xdg-email`. Inhibit additionally uses logind and Print
uses the CUPS `lp` client at runtime; both are ordinary session services,
not build dependencies. Install PAM
development files only when the optional PAM module is required.

Install a Tessera runtime that implements the protocol version in the
[Compatibility Reference](../reference/compatibility.md). The Portal build
does not require a Tessera source checkout, Cargo package, or path override.

## Build and Install

Run from the repository root:

```bash
meson setup build --buildtype=release --prefix=/usr -Dpam=false
meson compile -C build
meson install -C build
```

Set `DESTDIR` for a staged distribution package:

```bash
DESTDIR="$PWD/stage" meson install -C build
```

Meson installs both private executables under the configured `libexecdir`,
generates the matching D-Bus service, and installs `atrium.portal` plus
`atrium-portals.conf` under the configured data directory.

## Enable PAM Unlock

Reconfigure and rebuild with PAM support:

```bash
meson setup build --reconfigure -Dpam=true
meson compile -C build
meson install -C build
```

Add the following lines to the PAM configuration. The `auth` line goes
after the module that establishes the authentication token, the `session`
line after the logind session module, and the `password` line after the
module that sets the new authentication token. The exact files are
display-manager specific.

```text
auth optional pam_atrium.so
session optional pam_atrium.so
password optional pam_atrium.so
```

Keep the control flag `optional`. `pam_atrium.so` never grants or denies
authentication: the unlock token is planted only once the login is
confirmed, and the `password` line lets the vault password follow login
password changes.

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
   `atrium-portals.conf` with `tessera`-only routes; remove hand-written
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

Stop the portal frontend before copying the vault, and copy the entire
`$XDG_DATA_HOME/aegis/secrets` directory as one unit. Never restore
`vault.enc` without its matching `vault.key`, or its matching `vault.kdf`
/`vault.salt` pair for a password-mode vault.

The production per-application key derivation rotates the shared value from
the pre-production `v0.0.1` implementation. Applications that encrypted data
with that value must recreate the encrypted data after upgrading. The vault
files themselves stay in place.
