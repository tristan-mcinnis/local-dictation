//! Launch at login through `SMAppService.mainAppService` (macOS 13+).
//!
//! The service is the bundle of the running executable, so these calls only
//! mean something when run from inside the installed `.app`. Unlike a System
//! Events login item, this needs no Automation grant, so it cannot stall on a
//! pending consent prompt (the old install step timed out with -1712 that way).

use objc2_foundation::{NSString, NSURL};
use objc2_service_management::{SMAppService, SMAppServiceStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginItemStatus {
    NotRegistered,
    Enabled,
    RequiresApproval,
    NotFound,
}

impl LoginItemStatus {
    fn from_raw(status: SMAppServiceStatus) -> Self {
        match status {
            SMAppServiceStatus::Enabled => Self::Enabled,
            SMAppServiceStatus::RequiresApproval => Self::RequiresApproval,
            SMAppServiceStatus::NotFound => Self::NotFound,
            _ => Self::NotRegistered,
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Self::Enabled => "enabled: Local Dictation opens at login",
            Self::NotRegistered => "not registered",
            Self::RequiresApproval => {
                "needs approval: turn Local Dictation on in System Settings > General > Login Items"
            }
            Self::NotFound => "not found: run this from the installed Local Dictation.app",
        }
    }
}

pub fn status() -> LoginItemStatus {
    LoginItemStatus::from_raw(unsafe { SMAppService::mainAppService().status() })
}

/// Idempotent: registering an enabled service again is a no-op for launchd.
pub fn register() -> Result<LoginItemStatus, String> {
    let service = unsafe { SMAppService::mainAppService() };
    unsafe { service.registerAndReturnError() }
        .map_err(|e| e.localizedDescription().to_string())?;
    Ok(LoginItemStatus::from_raw(unsafe { service.status() }))
}

pub fn unregister() -> Result<LoginItemStatus, String> {
    let service = unsafe { SMAppService::mainAppService() };
    if unsafe { service.status() } != SMAppServiceStatus::NotRegistered {
        unsafe { service.unregisterAndReturnError() }
            .map_err(|e| e.localizedDescription().to_string())?;
    }
    Ok(LoginItemStatus::from_raw(unsafe { service.status() }))
}

/// Whether an old-style login item (the one `build-app.sh` used to add through
/// System Events) still points at `app_path`. Read-only.
pub fn legacy_item_present(app_path: &str) -> bool {
    let url = NSURL::fileURLWithPath(&NSString::from_str(app_path));
    let status = unsafe { SMAppService::statusForLegacyURL(&url) };
    status == SMAppServiceStatus::Enabled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_service_status_maps_to_a_distinct_state() {
        let all = [
            SMAppServiceStatus::NotRegistered,
            SMAppServiceStatus::Enabled,
            SMAppServiceStatus::RequiresApproval,
            SMAppServiceStatus::NotFound,
        ]
        .map(LoginItemStatus::from_raw);
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert_eq!(all[1], LoginItemStatus::Enabled);
    }
}
