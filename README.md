# xdg-desktop-portal-atrium

`xdg-desktop-portal-atrium` is the private
[xdg-desktop-portal](https://flatpak.github.io/xdg-desktop-portal/)
backend for the Tessera desktop. It translates freedesktop portal D-Bus
requests into Portal-owned services and, only for compositor resources, a
narrow projection of Tessera IPC (protocol 29, negotiating down to 24). It
publishes ScreenCast streams
through PipeWire and routes the per-application Secret portal to the
sigil secret service.

The repository builds a D-Bus-activated headless daemon from a small Cargo workspace:

- `xdg-desktop-portal-atrium` assembles the backend interfaces, IPC adapters,
  and workers.
- `atrium-portal-ipc` implements the settings, capture, picking, streaming,
  and wallpaper wire contract without depending on Tessera source crates.
- `atrium-portal-prompter` defines the versioned, out-of-process data contract
  for companion prompts (FileChooser via Arca, PickTarget/PickApp/PickConfirm
  via Tessera).
- `atrium-portal-runtime` owns the shared portal Request lifecycle.
- `atrium-portal-secret` projects the native Secret backend onto the
  sigil daemon's IPC socket (ADR-0020); storage, unlock, and lock state
  are sigil's responsibilities.

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

Install Rust, PipeWire development libraries (`libpipewire-0.3-dev`,
`libspa-0.2-dev`), and `pkg-config`, then run:

```bash
cargo build --locked --release --workspace
cargo test --locked --workspace
```

Build and stage the production installation with:

```bash
cargo build --locked --release -p xdg-desktop-portal-atrium -p atrium-portal-prompter
DESTDIR="$PWD/stage" ./scripts/install.sh --prefix /usr --no-build
```

The install script places both private executables under `libexecdir`,
generates the D-Bus activation file with that exact path, and installs the
portal metadata and routing configuration. A production installation needs
no other portal
backend: every routed interface is served natively, and Secret retrieval
additionally requires the sigil daemon. Vault auto-unlock and the vault
password lifecycle are provided by sigil's own PAM module. See
[How to Install for Production](docs/how-to/install-production.md).

The repository's source is MIT-licensed.

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
