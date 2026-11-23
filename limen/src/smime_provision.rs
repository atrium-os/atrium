//! Public outcomes from Arca's trusted S/MIME provisioning sheet.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Provisioned { fingerprint: [u8; 32] },
    Cancelled,
    Locked,
    Failed,
}

impl Outcome {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("S/MIME outcome encodes")
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let (result, rest) = postcard::take_from_bytes(bytes).ok()?;
        rest.is_empty().then_some(result)
    }
}
