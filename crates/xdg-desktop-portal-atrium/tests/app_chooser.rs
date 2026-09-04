//! Routing regression: AppChooser is served by atrium.

const PORTAL_FILE: &str = include_str!("../../../contrib/xdg-desktop-portal/portals/atrium.portal");
const PORTALS_CONF: &str = include_str!("../../../contrib/xdg-desktop-portal/atrium-portals.conf");

#[test]
fn app_chooser_is_served_by_atrium() {
    let interfaces = PORTAL_FILE
        .lines()
        .find_map(|line| line.strip_prefix("Interfaces="))
        .expect("portal metadata must declare Interfaces");
    assert!(
        interfaces
            .split(';')
            .any(|interface| interface == "org.freedesktop.impl.portal.AppChooser"),
        "the Portal-owned AppChooser must be advertised"
    );
    assert!(
        PORTALS_CONF
            .lines()
            .any(|line| line == "org.freedesktop.impl.portal.AppChooser=atrium")
    );
}
