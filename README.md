# xdg-desktop-portal-atrium

`xdg-desktop-portal-atrium` is the private
[xdg-desktop-portal](https://flatpak.github.io/xdg-desktop-portal/)
backend for the Tessera desktop. It translates freedesktop portal D-Bus
requests into Portal-owned services and, only for compositor resources, a
narrow projection of Tessera IPC (protocol 29, negotiating down to 24). It
publishes ScreenCast streams
through PipeWire and hosts the encrypted, per-application Secret portal.

The repository builds a D-Bus-activated backend plus an optics (iris/lens)
UI host from a small Cargo workspace:

- `xdg-desktop-portal-atrium` assembles the backend interfaces, IPC adapters,
  and workers.
- `atrium-portal-ipc` implements the settings, capture, picking, streaming,
  and wallpaper wire contract without depending on Tessera source crates.
- `atrium-portal-prompter` runs one optics (iris/lens) interaction per
  request. It owns file browsing, consent dialogs, the application chooser,
  the launcher name editor, and Secret password input, and it hosts the
  long-lived notification daemon. It never connects to compositor IPC.
- `atrium-portal-runtime` owns the shared portal Request lifecycle.
- `atrium-portal-secret` owns the encrypted vault and native Secret backend.
- `atrium-pam` optionally forwards a verified login password for vault
  auto-unlock.

## Compatibility

Portal and Tessera releases have independent version sequences. The current
workspace speaks Tessera IPC protocol 29 and negotiates down to 24. The
protocol-24 wire schema is verified against Tessera `v0.0.11` and `v0.0.12`;
Tessera `v0.0.15` provides protocol 25, and `v0.0.16`–`v0.0.21` speak
protocol 29, which the handshake negotiates down. Wallpaper uses the
compositor's long-standing `SetWallpaper` op, available in every
supported release. This is a runtime compatibility contract, not a source
dependency; see the
[Compatibility Reference](docs/reference/compatibility.md).

## Build

Install Meson, the optics C libraries (flux/lens/iris, from the tagged
`ming2k/optics` release), PipeWire, SPA, and `pkg-config` development
packages, then run:

```bash
cargo build --locked --release --workspace
cargo test --locked --workspace
```

Build and stage the production installation with:

```bash
meson setup build --buildtype=release --prefix=/usr -Dpam=false
meson compile -C build
DESTDIR="$PWD/stage" meson install -C build
```

Meson installs both private executables under `libexecdir`, generates the
D-Bus activation file with that exact path, and installs the portal metadata
and routing configuration. The optional PAM module is enabled with
`-Dpam=true`; it requires PAM development files. A production installation
needs no other portal backend: every routed interface is served natively.
See [How to Install for Production](docs/how-to/install-production.md).

The repository's own source is MIT-licensed, including the optional
`pam_atrium.so` module.

## Protocol Development

Build and test this repository without a Tessera checkout:

```bash
cargo check --locked --workspace
cargo test --locked --workspace
```

When a compositor wire change is required, update the narrow
`atrium-portal-ipc` projection and its literal protocol fixtures, then test the
assembled daemon against the independently implemented test server. Follow
[Cross-Repository Protocol Development](docs/dev/cross-repository-development.md)
for release coordination.

## Documentation

- [Documentation index](docs/index.md)
- [Production installation](docs/how-to/install-production.md)
- [Portal support reference](docs/reference/portal-support.md)
- [Compatibility reference](docs/reference/compatibility.md)
- [Architecture decisions](docs/adr/index.md)
- [Contributor documentation](docs/dev/index.md)

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the build, test, and review
workflow. Report security issues through the process in
[SECURITY.md](SECURITY.md).
