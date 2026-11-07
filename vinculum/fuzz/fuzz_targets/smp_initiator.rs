//! The SC initiator fed arbitrary SMP PDUs from a hostile peer, with the
//! user saying yes and encryption succeeding whenever asked. Must never
//! panic, and must never report a bond whose checks it did not pass: a
//! random peer cannot produce a valid DHKey check, so Done must not happen.
#![no_main]
use libfuzzer_sys::fuzz_target;
use vinculum_smp::pairing::{Action, Config, Initiator, IoCap};

fuzz_target!(|data: &[u8]| {
    let io = [IoCap::DisplayOnly, IoCap::DisplayYesNo, IoCap::KeyboardOnly, IoCap::NoInputNoOutput, IoCap::KeyboardDisplay]
        [data.first().map_or(0, |b| *b as usize % 5)];
    let cfg = Config { io_cap: io, local_irk: [1; 16], local_identity: (0, [1; 6]) };
    let (mut ini, _) = Initiator::start(cfg, [1; 7], [2; 7], 0);
    let mut rest = data.get(1..).unwrap_or(&[]);
    let mut now = 0;
    while let Some((&n, tail)) = rest.split_first() {
        let n = (n as usize).min(tail.len());
        let (pdu, tail) = tail.split_at(n);
        rest = tail;
        now += 100;
        let mut acts = ini.on_pdu(pdu, now);
        while let Some(a) = acts.pop() {
            match a {
                Action::Confirm { .. } => acts.extend(ini.confirmed(true)),
                Action::EnterPasskey => acts.extend(ini.passkey_entered(Some(123_456))),
                Action::StartEncryption { .. } => acts.extend(ini.encrypted(true, 16)),
                Action::Done(_) => panic!("bonded with a peer that cannot know the DHKey"),
                _ => {}
            }
        }
        ini.tick(now);
    }
});
