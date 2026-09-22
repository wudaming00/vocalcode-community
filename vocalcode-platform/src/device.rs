//! Stable per-machine fingerprint for license device-binding. Uses the OS
//! machine id (Windows MachineGuid, macOS IOPlatformUUID, Linux
//! /etc/machine-id) via the cross-platform `machine-uid` crate.

/// Opaque, stable device id for this machine.
///
/// Failure is represented by an empty string, which the activation layer
/// rejects with a useful error. Never substitute a shared constant: doing so
/// would make every machine whose OS fingerprint lookup failed indistinguishable
/// to the licence server and let one receipt validate on all of them.
pub fn device_id() -> String {
    match machine_uid::get() {
        Ok(id) if !id.trim().is_empty() => id,
        Ok(_) => {
            log::warn!("OS machine fingerprint was empty; activation is unavailable");
            String::new()
        }
        Err(error) => {
            log::warn!("could not read OS machine fingerprint: {error}");
            String::new()
        }
    }
}
