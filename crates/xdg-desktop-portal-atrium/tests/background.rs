//! Routing regression: Background is served by Tessera.

const PORTAL_FILE: &str = include_str!("../../../contrib/xdg-desktop-portal/portals/atrium.portal");
const PORTALS_CONF: &str = include_str!("../../../contrib/xdg-desktop-portal/atrium-portals.conf");

#[test]
fn background_is_served_by_tessera() {
    let interfaces = PORTAL_FILE
        .lines()
        .find_map(|line| line.strip_prefix("Interfaces="))
        .expect("portal metadata must declare Interfaces");
    assert!(
        interfaces
            .split(';')
            .any(|interface| interface == "org.freedesktop.impl.portal.Background"),
        "the Portal-owned Background must be advertised"
    );
    assert!(
        PORTALS_CONF
            .lines()
            .any(|line| line == "org.freedesktop.impl.portal.Background=tessera")
    );
}
