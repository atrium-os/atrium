//! A-MSDU deaggregation with the FragAttacks rules (wifi.md §3.1, §8.5).
//!
//! Each subframe: DA (6), SA (6), length (2, big-endian), MSDU, padding to a
//! multiple of 4 (none after the last). The whole A-MSDU is checked before
//! any subframe is delivered: one bad subframe drops all of it.

use crate::frame::{is_group, Drop, RFC1042};
use aura_proto::Mac;

pub const MAX_SUBFRAMES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sub<'a> {
    pub da: Mac,
    pub sa: Mac,
    pub msdu: &'a [u8],
}

/// A validated A-MSDU body; iteration is infallible.
#[derive(Clone, Copy, Debug)]
pub struct Amsdu<'a> {
    body: &'a [u8],
}

fn next(b: &[u8]) -> Result<(Sub<'_>, &[u8]), Drop> {
    if b.len() < 14 {
        return Err(Drop::AmsduInjection);
    }
    let len = u16::from_be_bytes([b[12], b[13]]) as usize;
    let end = 14 + len;
    if b.len() < end || len == 0 {
        return Err(Drop::AmsduInjection);
    }
    let mut da = [0; 6];
    let mut sa = [0; 6];
    da.copy_from_slice(&b[..6]);
    sa.copy_from_slice(&b[6..12]);
    let sub = Sub { da, sa, msdu: &b[14..end] };
    let rest = &b[end..];
    if rest.is_empty() {
        return Ok((sub, rest));
    }
    let pad = (4 - end % 4) % 4;
    if rest.len() < pad || rest[..pad].iter().any(|&p| p != 0) {
        return Err(Drop::AmsduInjection);
    }
    Ok((sub, &rest[pad..]))
}

impl<'a> Amsdu<'a> {
    /// Checks every subframe: the first DA must not look like an RFC 1042
    /// header (the injection signature), every DA must be `own` or a group
    /// address, no SA may be `own`.
    pub fn parse(body: &'a [u8], own: &Mac) -> Result<Amsdu<'a>, Drop> {
        if body.len() >= 6 && body[..6] == RFC1042 {
            return Err(Drop::AmsduInjection);
        }
        let mut b = body;
        let mut n = 0;
        while !b.is_empty() {
            let (s, rest) = next(b)?;
            if (s.da != *own && !is_group(&s.da)) || is_group(&s.sa) {
                return Err(Drop::AmsduInjection);
            }
            if s.sa == *own {
                return Err(Drop::FromSelf);
            }
            n += 1;
            if n > MAX_SUBFRAMES {
                return Err(Drop::AmsduInjection);
            }
            b = rest;
        }
        if n == 0 {
            return Err(Drop::AmsduInjection);
        }
        Ok(Amsdu { body })
    }

    pub fn iter(&self) -> impl Iterator<Item = Sub<'a>> + 'a {
        let mut b = self.body;
        core::iter::from_fn(move || {
            if b.is_empty() {
                return None;
            }
            // Validated in `parse`.
            let (s, rest) = next(b).ok()?;
            b = rest;
            Some(s)
        })
    }
}
