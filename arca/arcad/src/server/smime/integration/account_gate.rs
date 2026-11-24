//! Native synthetic backend gate. Enrollment and approval are explicit test
//! inputs. The socket peer stands in for the jailed reader; no IPC handler
//! exposes account enrollment or the keys used here.

use super::*;
use arca_store::{ItemId, ItemMeta, RootKey};
use std::sync::Weak;

const PUB: [u8; 32] = [8; 32];
const PERSONA: &str = "work";
const ROOT: [u8; 32] = [7; 32];
const PREVIEW: &str = "Synthetic account preview.\r\n";

#[derive(Clone, Copy)]
struct Account {
    id: u32,
    address: &'static str,
    alias: Option<&'static str>,
}

const PRIMARY: Account = Account {
    id: 1,
    address: "primary@fixture.invalid",
    alias: Some("approved-alias@fixture.invalid"),
};
const SECONDARY: Account = Account {
    id: 2,
    address: "secondary@fixture.invalid",
    alias: None,
};

impl Account {
    fn scope(self) -> Vec<u8> {
        let mut scope = self.id.to_le_bytes().to_vec();
        scope.extend(wire::digest(self.address.as_bytes()));
        scope.extend(
            self.alias
                .map_or([0; 32], |alias| wire::digest(alias.as_bytes())),
        );
        scope
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Selection {
    account: u32,
    handle: ItemId,
    receiver: &'static str,
    digest: [u8; 32],
}

struct Approved {
    selection: Selection,
    revoked: Arc<AtomicBool>,
}

struct Gate {
    store: Store,
    pending: Option<(ItemId, Weak<AtomicBool>)>,
}

fn recipient() -> Recipient {
    Recipient::SmimeLegacy { publisher: PUB }
}

impl Gate {
    fn enrolled(&self, account: Account) -> io::Result<Option<ItemMeta>> {
        let recipient = recipient();
        let id = account.id.to_le_bytes();
        Ok(self
            .store
            .list(&Access {
                persona: PERSONA,
                recipient: &recipient,
            })
            .map_err(|_| closed())?
            .into_iter()
            .find(|row| {
                row.kind == Kind::SmimeLegacyRsaKey && row.scope.get(64..68) == Some(id.as_slice())
            }))
    }

    fn active(&self, account: Account, handle: ItemId) -> io::Result<Option<ItemMeta>> {
        let scope = account.scope();
        Ok(self
            .enrolled(account)?
            .filter(|row| row.id == handle && row.scope.get(64..) == Some(scope.as_slice())))
    }

    fn enroll(&mut self, account: Account, cert: &[u8], key: &[u8]) -> io::Result<ItemId> {
        if self.enrolled(account)?.is_some() {
            return Err(closed());
        }
        enroll_key(
            &mut self.store,
            Some(PERSONA),
            PUB,
            cert,
            key,
            &account.scope(),
        )
        .map(|(_, id)| id)
    }

    fn approve(&mut self, account: Account, selection: Selection) -> io::Result<Approved> {
        if self
            .pending
            .as_ref()
            .is_some_and(|(_, pending)| pending.upgrade().is_some())
            || selection.account != account.id
            || (selection.receiver != account.address && Some(selection.receiver) != account.alias)
            || self.active(account, selection.handle)?.is_none()
        {
            return Err(closed());
        }
        let revoked = Arc::new(AtomicBool::new(false));
        self.pending = Some((selection.handle, Arc::downgrade(&revoked)));
        Ok(Approved { selection, revoked })
    }

    fn forget(&mut self, account: Account, handle: ItemId) -> io::Result<()> {
        if let Some(row) = self.enrolled(account)?.filter(|row| row.id == handle) {
            if let Some((current, revoked)) = &self.pending {
                if *current == handle {
                    if let Some(revoked) = revoked.upgrade() {
                        revoked.store(true, Ordering::Release);
                    }
                }
            }
            let recipient = recipient();
            self.store
                .delete(
                    &row.id,
                    &Access {
                        persona: PERSONA,
                        recipient: &recipient,
                    },
                )
                .map_err(|_| closed())?;
        }
        Ok(())
    }

    fn read(
        state: &Mutex<Self>,
        account: Account,
        current: Selection,
        approved: Approved,
        (control, lifetime): (UnixStream, UnixStream),
        cms: &[u8],
        cert: &[u8],
    ) -> io::Result<Zeroizing<String>> {
        let valid = || !approved.revoked.load(Ordering::Acquire);
        let result = watch_worker(
            control,
            lifetime,
            valid,
            |control, mut lifetime, close_control| {
                let result = (|| {
                    if !valid()
                        || approved.selection != current
                        || current.account != account.id
                        || current.digest != wire::digest(cms)
                    {
                        return Err(closed());
                    }
                    let prepared = Prepared::start(control, cert, cms)?;
                    let key = {
                        let state = state.lock().unwrap();
                        let row = state.active(account, current.handle)?.ok_or_else(closed)?;
                        if !valid()
                            || row.scope[..32] != prepared.fingerprint
                            || row.scope[32..64] != prepared.cert_digest
                        {
                            return Err(closed());
                        }
                        let recipient = recipient();
                        state
                            .store
                            .release(
                                &row.id,
                                &Access {
                                    persona: PERSONA,
                                    recipient: &recipient,
                                },
                                None,
                            )
                            .map_err(|_| closed())?
                            .1
                    };
                    prepared.finish_with_key(key, valid)
                })();
                let _ = close_control.shutdown(Shutdown::Both);
                if !matches!(
                    portcullis_ipc::read_response(&mut lifetime),
                    Ok(Response::LaunchExit { code: Some(0) })
                ) {
                    return Err(closed());
                }
                result
            },
        );
        let mut state = state.lock().unwrap();
        state.pending = None;
        if !valid() || state.active(account, current.handle)?.is_none() {
            return Err(closed());
        }
        result.map_err(|_| closed())
    }
}

fn new_gate(dir: &Path) -> Gate {
    let root = RootKey::from_bytes(&ROOT).unwrap();
    Gate {
        store: Store::create(dir, SYNTHETIC_USER, &root).unwrap(),
        pending: None,
    }
}

fn second_pair() -> (Vec<u8>, Zeroizing<Vec<u8>>) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("fixture.cnf"),
        "[req]\ndistinguished_name=subject\nprompt=no\nx509_extensions=leaf\n[subject]\nCN=Synthetic secondary account\n[leaf]\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,keyEncipherment\n",
    ).unwrap();
    let run = |args: &[&str]| {
        let output = std::process::Command::new("openssl")
            .current_dir(dir.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "synthetic key generation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(&[
        "req",
        "-new",
        "-x509",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-days",
        "2",
        "-keyout",
        "key.pem",
        "-outform",
        "DER",
        "-out",
        "cert.der",
        "-config",
        "fixture.cnf",
    ]);
    run(&[
        "pkcs8", "-topk8", "-nocrypt", "-in", "key.pem", "-outform", "DER", "-out", "key.der",
    ]);
    (
        std::fs::read(dir.path().join("cert.der")).unwrap(),
        Zeroizing::new(std::fs::read(dir.path().join("key.der")).unwrap()),
    )
}

fn selection(account: Account, handle: ItemId, receiver: &'static str, cms: &[u8]) -> Selection {
    Selection {
        account: account.id,
        handle,
        receiver,
        digest: wire::digest(cms),
    }
}

fn socket_pair() -> (UnixStream, UnixStream) {
    let (a, b) = UnixStream::pair().unwrap();
    for side in [&a, &b] {
        side.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        side.set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
    }
    (a, b)
}

fn worker_handshake(
    worker: &mut UnixStream,
) -> io::Result<Option<(mail_smime::envelope::EnvelopedData, usize)>> {
    worker.write_all(wire::WORKER_READY)?;
    let mut mode = [0];
    worker.read_exact(&mut mode)?;
    assert_eq!(mode, [wire::INPUT_CMS]);
    let cert = wire::read(worker, wire::MAX_CERT)?;
    let bytes = wire::read(worker, wire::MAX_CMS)?;
    let cms = mail_smime::envelope::enveloped_data(&bytes).map_err(|_| closed())?;
    let Some((fingerprint, key_bytes, wrapped)) =
        mail_smime::envelope::legacy_recipient(&cms, &cert)
    else {
        worker.write_all(&[0])?;
        return Ok(None);
    };
    worker.write_all(&[1, key_bytes as u8])?;
    worker.write_all(&fingerprint)?;
    wire::write(worker, &wrapped, 512)?;
    Ok(Some((cms, key_bytes)))
}

fn synthetic_worker(mut worker: UnixStream) -> io::Result<Option<Zeroizing<String>>> {
    let Some((cms, key_bytes)) = worker_handshake(&mut worker)? else {
        return Ok(None);
    };
    let mut key = Zeroizing::new(vec![0; key_bytes]);
    worker.read_exact(key.as_mut())?;
    let plain = Zeroizing::new(mail_smime::envelope::decrypt(&cms, &key).map_err(|_| closed())?);
    Ok(wire::plain_text(&plain)
        .map(str::to_owned)
        .map(Zeroizing::new))
}

fn preview(
    state: &Mutex<Gate>,
    account: Account,
    selected: Selection,
    cms: &[u8],
    cert: &[u8],
) -> io::Result<Zeroizing<String>> {
    let approved = state.lock().unwrap().approve(account, selected)?;
    let (control, mut worker) = socket_pair();
    let (lifetime, mut launcher) = socket_pair();
    std::thread::scope(|scope| {
        let peer = scope.spawn(|| {
            if let Some(text) = synthetic_worker(worker.try_clone().unwrap()).unwrap() {
                worker.write_all(&[1]).unwrap();
                wire::write(&mut worker, text.as_bytes(), wire::MAX_TEXT).unwrap();
            }
            drop(worker);
            portcullis_ipc::write_response(&mut launcher, &Response::LaunchExit { code: Some(0) })
                .unwrap();
        });
        let result = Gate::read(
            state,
            account,
            selected,
            approved,
            (control, lifetime),
            cms,
            cert,
        );
        peer.join().unwrap();
        result
    })
}

#[test]
fn account_gate_uses_distinct_keys_and_only_the_enrolled_alias() {
    let dir = tempfile::tempdir().unwrap();
    let mut gate = new_gate(dir.path());
    let primary = gate.enroll(PRIMARY, CERT, KEY).unwrap();
    let (cert, key) = second_pair();
    let secondary = gate.enroll(SECONDARY, &cert, &key).unwrap();
    assert_ne!(primary, secondary);
    assert_ne!(
        gate.enrolled(PRIMARY).unwrap().unwrap().scope[..32],
        gate.enrolled(SECONDARY).unwrap().unwrap().scope[..32]
    );
    let other = mail_smime::envelope::encrypt(
        b"Content-Type: text/plain\r\n\r\nSynthetic account preview.\r\n",
        &[cert.clone()],
    )
    .unwrap();
    let gate = Mutex::new(gate);
    let alias = PRIMARY.alias.unwrap();
    assert_eq!(
        preview(
            &gate,
            PRIMARY,
            selection(PRIMARY, primary, alias, CMS),
            CMS,
            CERT
        )
        .unwrap()
        .as_str(),
        SYNTHETIC_PREVIEW
    );
    assert_eq!(
        preview(
            &gate,
            SECONDARY,
            selection(SECONDARY, secondary, SECONDARY.address, &other),
            &other,
            &cert
        )
        .unwrap()
        .as_str(),
        PREVIEW
    );
    assert!(preview(
        &gate,
        SECONDARY,
        selection(SECONDARY, secondary, alias, &other),
        &other,
        &cert
    )
    .is_err());
    assert!(preview(
        &gate,
        SECONDARY,
        selection(SECONDARY, primary, SECONDARY.address, CMS),
        CMS,
        CERT
    )
    .is_err());
    assert!(preview(
        &gate,
        SECONDARY,
        selection(SECONDARY, secondary, SECONDARY.address, CMS),
        CMS,
        &cert
    )
    .is_err());
}

#[test]
fn account_gate_readd_cannot_reuse_old_handle_after_store_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let mut gate = new_gate(dir.path());
    let old = gate.enroll(PRIMARY, CERT, KEY).unwrap();
    let selected = selection(PRIMARY, old, PRIMARY.address, CMS);
    drop(gate.approve(PRIMARY, selected).unwrap());
    drop(gate.approve(PRIMARY, selected).unwrap());
    gate.forget(PRIMARY, old).unwrap();
    gate.forget(PRIMARY, old).unwrap();
    drop(gate);
    let mut store = Store::open(dir.path(), SYNTHETIC_USER).unwrap();
    store.unlock(&RootKey::from_bytes(&ROOT).unwrap()).unwrap();
    let mut gate = Gate {
        store,
        pending: None,
    };
    assert!(gate
        .approve(PRIMARY, selection(PRIMARY, old, PRIMARY.address, CMS))
        .is_err());
    let new = gate.enroll(PRIMARY, CERT, KEY).unwrap();
    assert_ne!(old, new);
    gate.forget(PRIMARY, old).unwrap();
    let gate = Mutex::new(gate);
    assert!(preview(
        &gate,
        PRIMARY,
        selection(PRIMARY, old, PRIMARY.address, CMS),
        CMS,
        CERT
    )
    .is_err());
    assert_eq!(
        preview(
            &gate,
            PRIMARY,
            selection(PRIMARY, new, PRIMARY.address, CMS),
            CMS,
            CERT
        )
        .unwrap()
        .as_str(),
        SYNTHETIC_PREVIEW
    );
}

#[test]
fn account_gate_rechecks_the_account_binding_before_key_use() {
    let dir = tempfile::tempdir().unwrap();
    let mut gate = new_gate(dir.path());
    let handle = gate.enroll(PRIMARY, CERT, KEY).unwrap();
    let selected = selection(PRIMARY, handle, PRIMARY.alias.unwrap(), CMS);
    let approved = gate.approve(PRIMARY, selected).unwrap();
    let changed = Account {
        alias: None,
        ..PRIMARY
    };
    assert!(gate.enroll(changed, CERT, KEY).is_err());
    let state = Mutex::new(gate);
    let (control, mut worker) = socket_pair();
    let (lifetime, mut launcher) = socket_pair();
    std::thread::scope(|scope| {
        let read = scope.spawn(|| {
            Gate::read(
                &state,
                changed,
                selected,
                approved,
                (control, lifetime),
                CMS,
                CERT,
            )
        });
        assert!(worker_handshake(&mut worker).unwrap().is_some());
        assert_eq!(worker.read(&mut [0]).unwrap(), 0);
        portcullis_ipc::write_response(&mut launcher, &Response::LaunchExit { code: Some(0) })
            .unwrap();
        assert!(read.join().unwrap().is_err());
    });
}

#[test]
fn account_gate_removal_revokes_pending_worker_read() {
    let dir = tempfile::tempdir().unwrap();
    let mut gate = new_gate(dir.path());
    let handle = gate.enroll(PRIMARY, CERT, KEY).unwrap();
    let selected = selection(PRIMARY, handle, PRIMARY.address, CMS);
    let approved = gate.approve(PRIMARY, selected).unwrap();
    let state = Mutex::new(gate);
    let (control, mut worker) = socket_pair();
    let (lifetime, mut launcher) = socket_pair();
    std::thread::scope(|scope| {
        let read = scope.spawn(|| {
            Gate::read(
                &state,
                PRIMARY,
                selected,
                approved,
                (control, lifetime),
                CMS,
                CERT,
            )
        });
        assert_eq!(
            synthetic_worker(worker.try_clone().unwrap())
                .unwrap()
                .unwrap()
                .as_str(),
            SYNTHETIC_PREVIEW
        );
        // The worker has derived the fixture view and is holding its reply.
        state.try_lock().unwrap().forget(PRIMARY, handle).unwrap();
        assert_eq!(worker.read(&mut [0]).unwrap(), 0);
        assert_eq!(launcher.read(&mut [0]).unwrap(), 0);
        assert!(read.join().unwrap().is_err());
        assert!(state.lock().unwrap().pending.is_none());
    });
}
