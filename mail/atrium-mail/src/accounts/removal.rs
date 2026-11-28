use arcad::proto::{op, status};
use mail_store::{AccountId, Store};
use std::os::unix::net::UnixStream;
use std::time::Duration;

#[cfg(feature = "legacy-smime-integration")]
pub const FIXTURE_ACCOUNT_ID: &str = "linked-synthetic-work-account";

pub(super) const ARCA_UID: u32 = 65020;

/// The dedicated daemon UID in 42-atrium-arcad.toml. A successful reply
/// from a socket substituted by the app cannot authorize local deletion.
fn fixture_instance_request(
    path: &std::path::Path,
    uid: u32,
    operation: u16,
    body: &[u8],
) -> Result<[u8; 16], String> {
    use std::os::fd::AsRawFd;
    let call = || -> std::io::Result<[u8; 16]> {
        let mut stream = UnixStream::connect(path)?;
        if portcullis_peer::uid_gid(stream.as_raw_fd())?.0 != uid {
            return Err(std::io::Error::other("unexpected Arca peer"));
        }
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        arcad::proto::write_request(&mut stream, operation, body)?;
        let (status, data) =
            arcad::proto::read_app_reply(&mut stream, arcad::proto::AppReply::Ticket)?;
        if status != status::OK {
            return Err(std::io::Error::other(format!(
                "Arca refused the account request ({status})"
            )));
        }
        data.try_into()
            .map_err(|_| std::io::Error::other("invalid account instance"))
    };
    call().map_err(|e| e.to_string())
}

#[cfg(feature = "legacy-smime-integration")]
pub(crate) fn bind_fixture(
    store: &Store,
    path: &std::path::Path,
    account: AccountId,
) -> Result<(), String> {
    if store
        .account_lifecycle(account)
        .map_err(|e| e.to_string())?
        .is_some()
    {
        return Err("This account is already linked or being removed.".into());
    }
    let instance = fixture_instance_request(path, ARCA_UID, op::MAIL_FIXTURE_INSTANCE, &[])?;
    store
        .write(|w| {
            w.bind_legacy_account(account, instance)?;
            w.set_setting(0, FIXTURE_ACCOUNT_ID, Some(u64::from(account)))
        })
        .map_err(|e| e.to_string())
}

pub(super) fn resume_removal_with(
    store: &Store,
    path: &std::path::Path,
    account: AccountId,
    uid: u32,
    forget: impl FnOnce(
        &mail_store::AccountConfig,
        Option<&(mail_store::Server, [u8; 16])>,
    ) -> Result<(), String>,
) -> Result<(), String> {
    use mail_store::removal::Lifecycle;
    let phase = || store.account_lifecycle(account).map_err(|e| e.to_string());
    if let Some(Lifecycle::Removing(instance)) = phase()? {
        let acknowledgment =
            fixture_instance_request(path, uid, op::MAIL_FIXTURE_REVOKE, &instance)?;
        store
            .write(|w| w.finish_legacy_removal(account, acknowledgment))
            .map_err(|e| e.to_string())?;
    }
    if let Some(Lifecycle::Credentials { config, sieve }) = phase()? {
        forget(&config, sieve.as_ref())?;
        store
            .write(|w| w.finish_credential_cleanup(account))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcad::proto;
    use mail_store::removal::Lifecycle;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixListener;

    #[test]
    fn removal_replays_the_saved_instance_after_an_unavailable_or_wrong_peer() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("mail");
        let path = dir.path().join("arcad");
        let old = [2; 16];
        let store = Store::open(&db).unwrap();
        let config = mail_store::AccountConfig {
            address: "login@fixture.invalid".into(),
            name: "fixture".into(),
            protocol: mail_store::Protocol::Jmap {
                session: "https://jmap.fixture.invalid".into(),
            },
            incoming_item: Some([3; 16]),
            outgoing_item: None,
            oauth: None,
            reached: mail_store::Reached::Never,
        };
        let id = store.add_configured(&config).unwrap();
        store.write(|w| w.bind_legacy_account(id, old)).unwrap();
        store.write(|w| w.begin_legacy_removal(id)).unwrap();
        assert!(
            resume_removal_with(&store, &path, id, ARCA_UID, |_, _| panic!("early cleanup"))
                .is_err()
        );
        drop(store);

        let store = Store::open(&db).unwrap();
        let listener = UnixListener::bind(&path).unwrap();
        let (peer, _) = UnixStream::pair().unwrap();
        let uid = portcullis_peer::uid_gid(peer.as_raw_fd()).unwrap().0;
        let server = std::thread::spawn(move || {
            // The unauthorized peer gets no request.
            use std::io::Read;
            let (mut s, _) = listener.accept().unwrap();
            assert_eq!(s.read(&mut [0]).unwrap(), 0);
            for ack in [[8; 16], old] {
                let (mut s, _) = listener.accept().unwrap();
                let request = proto::read_frame(&mut s).unwrap().unwrap();
                assert_eq!(
                    request.as_ref(),
                    [op::MAIL_FIXTURE_REVOKE.to_le_bytes().as_slice(), &old].concat()
                );
                proto::write_reply(&mut s, status::OK, &ack).unwrap();
            }
        });
        let attempt = |peer_uid| {
            resume_removal_with(&store, &path, id, peer_uid, |cfg, _| {
                assert!(store.accounts().unwrap().is_empty());
                assert_eq!(cfg, &config);
                Err("credential cleanup interrupted".into())
            })
        };
        assert!(attempt(uid.wrapping_add(1)).is_err());
        assert!(attempt(uid).is_err());
        assert_eq!(
            store.account_lifecycle(id).unwrap(),
            Some(Lifecycle::Removing(old))
        );
        assert_eq!(store.accounts().unwrap().len(), 1);
        assert_eq!(attempt(uid), Err("credential cleanup interrupted".into()));
        drop(store);
        let store = Store::open(&db).unwrap();
        resume_removal_with(&store, &path, id, uid, |cfg, _| {
            assert_eq!(cfg.incoming_item, Some([3; 16]));
            Ok(())
        })
        .unwrap();
        assert!(store.accounts().unwrap().is_empty());
        assert_eq!(store.account_lifecycle(id).unwrap(), None);
        server.join().unwrap();
    }
}
