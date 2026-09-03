//! Routing regression: Wallpaper is served by Tessera.

const PORTAL_FILE: &str = include_str!("../../../contrib/xdg-desktop-portal/portals/atrium.portal");
const PORTALS_CONF: &str = include_str!("../../../contrib/xdg-desktop-portal/atrium-portals.conf");

#[test]
fn wallpaper_is_served_by_tessera() {
    let interface = "org.freedesktop.impl.portal.Wallpaper";
    let interfaces = PORTAL_FILE
        .lines()
        .find_map(|line| line.strip_prefix("Interfaces="))
        .expect("portal metadata must declare Interfaces");
    assert!(
        interfaces.split(';').any(|entry| entry == interface),
        "the IPC-backed Wallpaper must be advertised"
    );
    assert!(
        PORTALS_CONF
            .lines()
            .any(|line| line == format!("{interface}=tessera"))
    );
}
