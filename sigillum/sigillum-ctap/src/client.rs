//! A blocking CTAP 2.1 client over any [`HidDevice`]: the platform side of
//! one authenticator, as `sigillum-fido` runs it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use zeroize::Zeroizing;

use crate::cmd::{self, Assertion, Attestation, ClientPin, ClientPinResponse, GetAssertion, Info, MakeCredential, PinAuth};
use crate::hid::{self, Event, HidDevice, InitResponse, Reassembler};
use crate::pin::{self, Shared};
use crate::{status, Error};

/// How long a transaction may go without hearing from the device. A CTAP2
/// user-presence wait sends keepalives every 100 ms or so, which restart
/// the clock, but a U2F-only key sends none and may take a while over a
/// slow bus (QEMU's emulated key answers a registration after more than a
/// second); the ceremony's own 120 s limit is the broker's (sigillum.md
/// §7.2).
pub const READ_TIMEOUT_MS: u32 = 5000;
/// One read's wait, so a cancel is noticed promptly.
pub const POLL_MS: u32 = 100;

pub struct Client<D: HidDevice> {
    dev: D,
    cid: u32,
    pub init: InitResponse,
    /// Called with each keepalive status (`UP_NEEDED`: tell the sheet to
    /// say "touch your key").
    pub on_keepalive: Option<Box<dyn FnMut(u8)>>,
    /// Set by the driver's socket reader when the broker cancels: the
    /// pending command gets `CTAPHID_CANCEL` and ends.
    pub cancel: Arc<AtomicBool>,
    /// The PIN/UV auth protocol to speak: 2 by default; the driver sets 1
    /// for a key whose `GetInfo` lists only 1 (CTAP 2.0 keys).
    pub pin_protocol: u8,
}

impl<D: HidDevice> Client<D> {
    /// `INIT` on the broadcast channel; the answer must echo our nonce.
    pub fn open(mut dev: D) -> Result<Client<D>, Error> {
        // Drop what an earlier client left unread (an answer that came after
        // it gave up): reports queue in the node across opens.
        for _ in 0..256 {
            match dev.read_report(0) {
                Ok(_) => {}
                Err(Error::Timeout) => break,
                Err(e) => return Err(e),
            }
        }
        let mut nonce = [0u8; 8];
        getrandom::getrandom(&mut nonce).map_err(|e| Error::Io(e.to_string()))?;
        for p in hid::frame(hid::BROADCAST, hid::cmd::INIT, &nonce)? {
            dev.write_report(&p)?;
        }
        let mut r = Reassembler::new(hid::BROADCAST);
        // Other platforms' INIT answers on the broadcast channel are skipped
        // by nonce; give up after a bounded number of reports.
        for _ in 0..64 {
            let p = dev.read_report(READ_TIMEOUT_MS)?;
            if let Event::Message(c, body) = r.feed(&p)? {
                if c != hid::cmd::INIT {
                    return Err(Error::Protocol("unexpected answer to INIT"));
                }
                let init = InitResponse::parse(&body)?;
                if init.nonce != nonce {
                    r = Reassembler::new(hid::BROADCAST);
                    continue;
                }
                return Ok(Client { dev, cid: init.cid, init, on_keepalive: None, cancel: Arc::new(AtomicBool::new(false)), pin_protocol: pin::PROTOCOL });
            }
        }
        Err(Error::Timeout)
    }

    pub fn ctap2(&self) -> bool {
        self.init.caps & hid::caps::CBOR != 0
    }

    /// One CTAPHID transaction: send, then read until a message on our
    /// channel, passing keepalives on and honouring a cancel.
    fn transact(&mut self, command: u8, payload: &[u8]) -> Result<(u8, Vec<u8>), Error> {
        for p in hid::frame(self.cid, command, payload)? {
            self.dev.write_report(&p)?;
        }
        let mut r = Reassembler::new(self.cid);
        let mut idle = 0;
        let mut cancel_sent = false;
        loop {
            if !cancel_sent && self.cancel.load(Ordering::SeqCst) {
                self.cancel()?;
                cancel_sent = true;
            }
            let p = match self.dev.read_report(POLL_MS) {
                Ok(p) => p,
                Err(Error::Timeout) => {
                    idle += POLL_MS;
                    if idle >= READ_TIMEOUT_MS {
                        return Err(if cancel_sent { Error::Cancelled } else { Error::Timeout });
                    }
                    continue;
                }
                Err(e) => return Err(e),
            };
            match r.feed(&p)? {
                Event::Ignored | Event::Partial => {}
                Event::Keepalive(s) => {
                    idle = 0;
                    if let Some(f) = self.on_keepalive.as_mut() {
                        f(s);
                    }
                }
                Event::Message(c, body) => return Ok((c, body)),
            }
        }
    }

    /// Send one CTAP2 command, return the response body after a success
    /// status.
    pub fn cbor(&mut self, command: u8, params: &[u8]) -> Result<Vec<u8>, Error> {
        if !self.ctap2() {
            return Err(Error::NoCtap2);
        }
        let mut msg = Vec::with_capacity(1 + params.len());
        msg.push(command);
        msg.extend_from_slice(params);
        let (c, body) = self.transact(hid::cmd::CBOR, &msg)?;
        if c != hid::cmd::CBOR || body.is_empty() {
            return Err(Error::Protocol("unexpected CTAPHID response"));
        }
        if body[0] == status::KEEPALIVE_CANCEL && self.cancel.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        if body[0] != status::OK {
            return Err(Error::Status(body[0]));
        }
        Ok(body[1..].to_vec())
    }

    /// One U2F APDU over `CTAPHID_MSG`: (body, status word).
    pub fn msg(&mut self, apdu: &[u8]) -> Result<(Vec<u8>, u16), Error> {
        let (c, body) = self.transact(hid::cmd::MSG, apdu)?;
        if c != hid::cmd::MSG {
            return Err(Error::Protocol("unexpected CTAPHID response"));
        }
        let (b, sw) = crate::u2f::split(&body)?;
        Ok((b.to_vec(), sw))
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// `CTAPHID_CANCEL`: no response is expected; the pending command ends
    /// with `KEEPALIVE_CANCEL`.
    pub fn cancel(&mut self) -> Result<(), Error> {
        for p in hid::frame(self.cid, hid::cmd::CANCEL, &[])? {
            self.dev.write_report(&p)?;
        }
        Ok(())
    }

    pub fn get_info(&mut self) -> Result<Info, Error> {
        Info::parse(&self.cbor(cmd::GET_INFO, &[])?)
    }

    pub fn make_credential(&mut self, req: &MakeCredential) -> Result<Attestation, Error> {
        let a = Attestation::parse(&self.cbor(cmd::MAKE_CREDENTIAL, &req.encode()?)?)?;
        Ok(a)
    }

    pub fn get_assertion(&mut self, req: &GetAssertion) -> Result<Assertion, Error> {
        Assertion::parse(&self.cbor(cmd::GET_ASSERTION, &req.encode()?)?, &req.allow)
    }

    pub fn pin_retries(&mut self) -> Result<u64, Error> {
        let req = ClientPin { protocol: self.pin_protocol, sub: cmd::pin_sub::GET_PIN_RETRIES, ..ClientPin::default() };
        ClientPinResponse::parse(&self.cbor(cmd::CLIENT_PIN, &req.encode())?)?.retries.ok_or(Error::Protocol("no pinRetries"))
    }

    fn shared(&mut self) -> Result<(Shared, crate::cose::CoseKey), Error> {
        let req = ClientPin { protocol: self.pin_protocol, sub: cmd::pin_sub::GET_KEY_AGREEMENT, ..ClientPin::default() };
        let resp = ClientPinResponse::parse(&self.cbor(cmd::CLIENT_PIN, &req.encode())?)?;
        let peer = resp.key_agreement.ok_or(Error::Protocol("no keyAgreement"))?;
        let (sk, ours) = pin::platform_key();
        Ok((Shared::agree_with(self.pin_protocol, &sk, &peer)?, ours))
    }

    /// `getPinUvAuthTokenUsingPinWithPermissions` (protocol 2). The token is
    /// good for `permissions` on `rp_id` until the authenticator expires it.
    /// With protocol 1, CTAP 2.0's `getPinToken`: no permissions, no RP.
    pub fn pin_token(&mut self, pin_utf8: &[u8], permissions: u8, rp_id: &str) -> Result<PinToken, Error> {
        if pin_utf8.len() < 4 || pin_utf8.len() > 63 {
            return Err(Error::Request("PIN length"));
        }
        let (shared, ours) = self.shared()?;
        let hash = pin::pin_hash(pin_utf8);
        let one = self.pin_protocol == pin::PROTOCOL_ONE;
        let req = ClientPin {
            protocol: self.pin_protocol,
            sub: if one { cmd::pin_sub::GET_PIN_TOKEN } else { cmd::pin_sub::GET_PIN_UV_AUTH_TOKEN_USING_PIN_WITH_PERMISSIONS },
            key_agreement: Some(ours),
            pin_hash_enc: Some(shared.encrypt(&*hash)?),
            permissions: if one { None } else { Some(permissions) },
            rp_id: if one { None } else { Some(rp_id.to_owned()) },
            ..ClientPin::default()
        };
        let resp = ClientPinResponse::parse(&self.cbor(cmd::CLIENT_PIN, &req.encode())?)?;
        let token = shared.decrypt(&resp.token_enc.ok_or(Error::Protocol("no pinUvAuthToken"))?)?;
        // Protocol 2's token is 32 bytes; protocol 1 allows any multiple of
        // 16 (16 or 32 in practice).
        if (one && (token.len() != 16 && token.len() != 32)) || (!one && token.len() != 32) {
            return Err(Error::Protocol("pinUvAuthToken length"));
        }
        Ok(PinToken(token, self.pin_protocol))
    }
}

/// A pinUvAuthToken, zeroized on drop.
pub struct PinToken(Zeroizing<Vec<u8>>, u8);

impl PinToken {
    /// `pinUvAuthParam` over a clientDataHash.
    pub fn auth(&self, client_data_hash: &[u8; 32]) -> PinAuth {
        PinAuth { param: pin::authenticate_with(self.1, &self.0, client_data_hash), protocol: self.1 }
    }
}
