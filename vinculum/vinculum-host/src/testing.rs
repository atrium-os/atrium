//! A scripted controller for tests and fuzzing: answers bring-up with
//! canned completions so a [`Host`] reaches Ready without a real controller.
#![doc(hidden)]

use crate::Host;
use vinculum_hci_proto::h4::{Frame, Kind};
use vinculum_hci_proto::hci::{self, ErrorCode, OpCode};
use vinculum_hci_proto::Packet;

/// An event frame from any generated event packet that converts to `Event`
/// (directly or through CommandComplete/CommandStatus/LeMetaEvent).
pub fn event<E>(e: E) -> Frame
where
    E: TryInto<hci::Event>,
    E::Error: std::fmt::Debug,
{
    Frame { kind: Kind::Event, packet: e.try_into().expect("event").encode_to_vec().expect("encode") }
}

pub fn complete<C>(c: C) -> Frame
where
    C: TryInto<hci::CommandComplete>,
    C::Error: std::fmt::Debug,
{
    let cc: hci::CommandComplete = c.try_into().expect("complete");
    event(cc)
}

pub fn status(op: OpCode, s: ErrorCode) -> Frame {
    event(hci::CommandStatus { status: s, num_hci_command_packets: 1, command_op_code: op, payload: vec![] })
}

pub fn le_meta<M>(m: M) -> Frame
where
    M: TryInto<hci::LeMetaEvent>,
    M::Error: std::fmt::Debug,
{
    let me: hci::LeMetaEvent = m.try_into().expect("le meta");
    event(me)
}

/// The canned answer to one bring-up command, or None for other commands.
pub fn answer(cmd: &Frame, le_buffers: u8) -> Option<Frame> {
    let c = hci::Command::decode_full(&cmd.packet).ok()?;
    let ok = ErrorCode::Success;
    Some(match c.op_code {
        OpCode::Reset => complete(hci::ResetComplete { status: ok, num_hci_command_packets: 1 }),
        OpCode::ReadLocalVersionInformation => complete(hci::ReadLocalVersionInformationComplete {
            status: ok,
            local_version_information: hci::LocalVersionInformation {
                hci_version: hci::HciVersion::V53,
                hci_revision: 0,
                lmp_version: hci::LmpVersion::V53,
                manufacturer_name: 0xFFFF,
                lmp_subversion: 0,
            },
            num_hci_command_packets: 1,
        }),
        OpCode::ReadLocalSupportedCommands => complete(hci::ReadLocalSupportedCommandsComplete {
            status: ok,
            supported_commands: [0xFF; 64],
            num_hci_command_packets: 1,
        }),
        OpCode::ReadLocalSupportedFeatures => {
            complete(hci::ReadLocalSupportedFeaturesComplete { status: ok, lmp_features: 0, num_hci_command_packets: 1 })
        }
        OpCode::ReadBdAddr => complete(hci::ReadBdAddrComplete {
            status: ok,
            bd_addr: hci::Address::try_from(0x0000_1122_3344_5566u64).unwrap(),
            num_hci_command_packets: 1,
        }),
        OpCode::ReadBufferSize => complete(hci::ReadBufferSizeComplete {
            status: ok,
            acl_data_packet_length: 1021,
            synchronous_data_packet_length: 60,
            total_num_acl_data_packets: 4,
            total_num_synchronous_data_packets: 2,
            num_hci_command_packets: 1,
        }),
        OpCode::LeReadBufferSizeV2 => complete(hci::LeReadBufferSizeV2Complete {
            status: ok,
            le_buffer_size: hci::LeBufferSize { le_data_packet_length: 27, total_num_le_packets: le_buffers },
            iso_buffer_size: hci::LeBufferSize { le_data_packet_length: 0, total_num_le_packets: 0 },
            num_hci_command_packets: 1,
        }),
        OpCode::SetEventMask => complete(hci::SetEventMaskComplete { status: ok, num_hci_command_packets: 1 }),
        OpCode::LeSetEventMask => complete(hci::LeSetEventMaskComplete { status: ok, num_hci_command_packets: 1 }),
        OpCode::WriteScanEnable => complete(hci::WriteScanEnableComplete { status: ok, num_hci_command_packets: 1 }),
        OpCode::WriteSimplePairingMode => complete(hci::WriteSimplePairingModeComplete { status: ok, num_hci_command_packets: 1 }),
        OpCode::WriteSecureConnectionsHostSupport => {
            complete(hci::WriteSecureConnectionsHostSupportComplete { status: ok, num_hci_command_packets: 1 })
        }
        _ => return None,
    })
}

/// A Host that has finished bring-up against the canned controller.
/// Returns it with its output drained.
pub fn ready_host(le_buffers: u8) -> Host {
    let mut h = Host::new();
    h.start(0);
    for _ in 0..32 {
        let Some(cmd) = h.poll_frame() else { break };
        if let Some(a) = answer(&cmd, le_buffers) {
            h.on_frame(a, 0);
        }
    }
    assert!(h.is_ready(), "canned bring-up did not finish");
    while h.poll_event().is_some() {}
    h
}
