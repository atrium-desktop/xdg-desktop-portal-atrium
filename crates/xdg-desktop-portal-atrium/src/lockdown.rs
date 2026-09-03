//! `org.freedesktop.impl.portal.Lockdown`: high-level portal restrictions.
//!
//! The backend ABI defines seven read-write properties so portal frontends
//! can share one in-session lockdown state. Values start permissive and zbus
//! emits standard property-change notifications when a value is updated.

use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
pub(crate) struct LockdownIface {
    disable_printing: AtomicBool,
    disable_save_to_disk: AtomicBool,
    disable_application_handlers: AtomicBool,
    disable_location: AtomicBool,
    disable_camera: AtomicBool,
    disable_microphone: AtomicBool,
    disable_sound_output: AtomicBool,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Lockdown")]
impl LockdownIface {
    #[zbus(property, name = "disable-printing")]
    fn disable_printing(&self) -> bool {
        self.disable_printing.load(Ordering::Acquire)
    }

    #[zbus(property, name = "disable-printing")]
    fn set_disable_printing(&self, value: bool) {
        self.disable_printing.store(value, Ordering::Release);
    }

    #[zbus(property, name = "disable-save-to-disk")]
    fn disable_save_to_disk(&self) -> bool {
        self.disable_save_to_disk.load(Ordering::Acquire)
    }

    #[zbus(property, name = "disable-save-to-disk")]
    fn set_disable_save_to_disk(&self, value: bool) {
        self.disable_save_to_disk.store(value, Ordering::Release);
    }

    #[zbus(property, name = "disable-application-handlers")]
    fn disable_application_handlers(&self) -> bool {
        self.disable_application_handlers.load(Ordering::Acquire)
    }

    #[zbus(property, name = "disable-application-handlers")]
    fn set_disable_application_handlers(&self, value: bool) {
        self.disable_application_handlers
            .store(value, Ordering::Release);
    }

    #[zbus(property, name = "disable-location")]
    fn disable_location(&self) -> bool {
        self.disable_location.load(Ordering::Acquire)
    }

    #[zbus(property, name = "disable-location")]
    fn set_disable_location(&self, value: bool) {
        self.disable_location.store(value, Ordering::Release);
    }

    #[zbus(property, name = "disable-camera")]
    fn disable_camera(&self) -> bool {
        self.disable_camera.load(Ordering::Acquire)
    }

    #[zbus(property, name = "disable-camera")]
    fn set_disable_camera(&self, value: bool) {
        self.disable_camera.store(value, Ordering::Release);
    }

    #[zbus(property, name = "disable-microphone")]
    fn disable_microphone(&self) -> bool {
        self.disable_microphone.load(Ordering::Acquire)
    }

    #[zbus(property, name = "disable-microphone")]
    fn set_disable_microphone(&self, value: bool) {
        self.disable_microphone.store(value, Ordering::Release);
    }

    #[zbus(property, name = "disable-sound-output")]
    fn disable_sound_output(&self) -> bool {
        self.disable_sound_output.load(Ordering::Acquire)
    }

    #[zbus(property, name = "disable-sound-output")]
    fn set_disable_sound_output(&self, value: bool) {
        self.disable_sound_output.store(value, Ordering::Release);
    }
}
