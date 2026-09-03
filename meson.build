project(
  'xdg-desktop-portal-atrium',
  version: '0.0.27',
  meson_version: '>=1.3.0',
)

cargo = find_program('cargo')
artifact_builder = find_program('scripts/meson-cargo-build.sh')

# Fail during configure instead of halfway through a distribution build.
dependency('libpipewire-0.3', version: '>=0.3')
dependency('libspa-0.2', version: '>=0.2')
if get_option('pam')
  dependency('pam')
endif

libexecdir = get_option('prefix') / get_option('libexecdir')
datadir = get_option('prefix') / get_option('datadir')

portal_artifacts = custom_target(
  'cargo-portal-artifacts',
  output: ['xdg-desktop-portal-atrium', 'atrium-portal-prompter'],
  command: [
    artifact_builder,
    'portal',
    meson.project_source_root(),
    meson.project_build_root(),
    cargo,
    '@OUTPUT0@',
    '@OUTPUT1@',
  ],
  build_by_default: true,
  install: true,
  install_dir: [libexecdir, libexecdir],
  install_mode: 'rwxr-xr-x',
)

service_config = configuration_data()
service_config.set('libexecdir', libexecdir)
configure_file(
  input: 'contrib/dbus-1/services/org.freedesktop.impl.portal.desktop.atrium.service.in',
  output: 'org.freedesktop.impl.portal.desktop.atrium.service',
  configuration: service_config,
  install: true,
  install_dir: datadir / 'dbus-1/services',
)

install_data(
  'contrib/xdg-desktop-portal/portals/atrium.portal',
  install_dir: datadir / 'xdg-desktop-portal/portals',
)
install_data(
  'contrib/xdg-desktop-portal/atrium-portals.conf',
  install_dir: datadir / 'xdg-desktop-portal',
)

if get_option('pam')
  custom_target(
    'cargo-pam-artifact',
    output: 'pam_atrium.so',
    command: [
      artifact_builder,
      'pam',
      meson.project_source_root(),
      meson.project_build_root(),
      cargo,
      '@OUTPUT@',
    ],
    build_by_default: true,
    install: true,
    install_dir: get_option('prefix') / get_option('libdir') / 'security',
    install_mode: 'rwxr-xr-x',
  )
endif

summary(
  {
    'Portal libexec directory': libexecdir,
    'Portal metadata directory': datadir / 'xdg-desktop-portal',
    'PAM module': get_option('pam'),
  },
  section: 'Installation',
)
