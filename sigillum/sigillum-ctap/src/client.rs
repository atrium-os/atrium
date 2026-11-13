//! A blocking CTAP 2.1 client over any [`HidDevice`]: the platform side of
//! one authenticator, as `sigillum-fido` runs it.

use zeroize::Zeroizing;

use crate::cmd::{self, Assertion, Attestation, ClientPin, ClientPinResponse, GetAssertion, Info, MakeCredential, PinAuth};
use crate::hid::{self, Event, HidDevice, InitResponse, Reassembler};
use crate::pin::{self, Shared};
use crate::{status, Error};

/// How long one CTAPHID transaction may run. A user-presence wait sends
/// keepalives every 100 ms or so, which restart the clock; the ceremony's
/// own 120 s limit is the broker's (sigillum.md §7.2).
pub const READ_TIMEOUT_MS: u32 = 1000;

pub struct Client<D: HidDevice> {
    dev: D,
    cid: u32,
    pub init: InitResponse,
    /// Called with each keepalive status (`UP_NEEDED`: tell the sheet to
    /// say "touch your key").
    pub on_keepalive: Option<Box<dyn FnMut(u8)>>,
}

impl<D: HidDevice> Client<D> {
    /// `INIT` on the broadcast channel; the answer must echo our nonce.
    pub fn open(mut dev: D) -> Result<Client<D>, Error> {
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
                if init.caps & hid::caps::CBOR == 0 {
                    return Err(Error::Protocol("authenticator has no CTAP2 (U2F-only keys arrive with S2)"));
                }
                return Ok(Client { dev, cid: init.cid, init, on_keepalive: None });
            }
        }
        Err(Error::Timeout)
    }

    /// Send one CTAP2 command, return the response body after a success
    /// status.
    pub fn cbor(&mut self, command: u8, params: &[u8]) -> Result<Vec<u8>, Error> {
        let mut msg = Vec::with_capacity(1 + params.len());
        msg.push(command);
        msg.extend_from_slice(params);
        for p in hid::frame(self.cid, hid::cmd::CBOR, &msg)? {
            self.dev.write_report(&p)?;
        }
        let mut r = Reassembler::new(self.cid);
        loop {
            let p = self.dev.read_report(READ_TIMEOUT_MS)?;
            match r.feed(&p)? {
                Event::Ignored | Event::Partial => {}
                Event::Keepalive(s) => {
                    if let Some(f) = self.on_keepalive.as_mut() {
                        f(s);
                    }
                }
                Event::Message(c, body) => {
                    if c != hid::cmd::CBOR || body.is_empty() {
                        return Err(Error::Protocol("unexpected CTAPHID response"));
                    }
                    if body[0] != status::OK {
                        return Err(Error::Status(body[0]));
                    }
                    return Ok(body[1..].to_vec());
                }
            }
        }
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
        let req = ClientPin { protocol: pin::PROTOCOL, sub: cmd::pin_sub::GET_PIN_RETRIES, ..ClientPin::default() };
        ClientPinResponse::parse(&self.cbor(cmd::CLIENT_PIN, &req.encode())?)?.retries.ok_or(Error::Protocol("no pinRetries"))
    }

    fn shared(&mut self) -> Result<(Shared, crate::cose::CoseKey), Error> {
        let req = ClientPin { protocol: pin::PROTOCOL, sub: cmd::pin_sub::GET_KEY_AGREEMENT, ..ClientPin::default() };
        let resp = ClientPinResponse::parse(&self.cbor(cmd::CLIENT_PIN, &req.encode())?)?;
        let peer = resp.key_agreement.ok_or(Error::Protocol("no keyAgreement"))?;
        let (sk, ours) = pin::platform_key();
        Ok((Shared::agree(&sk, &peer)?, ours))
    }

    /// `getPinUvAuthTokenUsingPinWithPermissions` (protocol 2). The token is
    /// good for `permissions` on `rp_id` until the authenticator expires it.
    pub fn pin_token(&mut self, pin_utf8: &[u8], permissions: u8, rp_id: &str) -> Result<PinToken, Error> {
        if pin_utf8.len() < 4 || pin_utf8.len() > 63 {
            return Err(Error::Request("PIN length"));
        }
        let (shared, ours) = self.shared()?;
        let hash = pin::pin_hash(pin_utf8);
        let req = ClientPin {
            protocol: pin::PROTOCOL,
            sub: cmd::pin_sub::GET_PIN_UV_AUTH_TOKEN_USING_PIN_WITH_PERMISSIONS,
            key_agreement: Some(ours),
            pin_hash_enc: Some(shared.encrypt(&*hash)?),
            permissions: Some(permissions),
            rp_id: Some(rp_id.to_owned()),
            ..ClientPin::default()
        };
        let resp = ClientPinResponse::parse(&self.cbor(cmd::CLIENT_PIN, &req.encode())?)?;
        let token = shared.decrypt(&resp.token_enc.ok_or(Error::Protocol("no pinUvAuthToken"))?)?;
        if token.len() != 32 {
            return Err(Error::Protocol("pinUvAuthToken length"));
        }
        Ok(PinToken(token))
    }
}

/// A pinUvAuthToken, zeroized on drop.
pub struct PinToken(Zeroizing<Vec<u8>>);

impl PinToken {
    /// `pinUvAuthParam` over a clientDataHash.
    pub fn auth(&self, client_data_hash: &[u8; 32]) -> PinAuth {
        PinAuth { param: pin::authenticate(&self.0, client_data_hash).to_vec(), protocol: pin::PROTOCOL }
    }
}
