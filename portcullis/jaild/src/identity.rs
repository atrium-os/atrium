//! The host's answers to the validator's identity questions
//! ([`validator::Identity`]): the launch registry portcullisd writes, read
//! fresh for each request, and the password and group databases.

use crate::validator::Identity;

/// Reads the live launch registry and account databases.
pub struct HostIdentity;

impl Identity for HostIdentity {
    fn app_binding(&self, uid: u32) -> Option<(String, String)> {
        /* An unreadable registry binds nothing, so the check denies. */
        let reg = match portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("jaild: launch registry {}: {e}", portcullis_peer::DEFAULT_REGISTRY);
                return None;
            }
        };
        reg.resolve(uid).map(|(o, a)| (o.to_string(), a.to_string()))
    }

    fn account(&self, uid: u32) -> Option<(String, u32)> {
        crate::ffi::account(uid)
    }

    fn group_name(&self, gid: u32) -> Option<String> {
        crate::ffi::group_name(gid)
    }
}
