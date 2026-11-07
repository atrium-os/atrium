//! HCI command flow control (Core 6.0 Vol 4 Part E §4.4).
//!
//! The controller says how many commands it will accept
//! (`Num_HCI_Command_Packets` in every Command Complete / Command Status);
//! Vinculum keeps it simpler and stricter: **one command in flight**, the
//! rest queued, each with a deadline. A completion for an opcode that is not
//! in flight is a protocol violation (counted, ignored). A command with no
//! completion by its deadline means the controller is wedged: the host
//! reports it and the transport resets the controller.

use std::collections::VecDeque;
use vinculum_hci_proto::hci::{Command, OpCode};

/// How long a command may wait for its completion.
pub const COMMAND_TIMEOUT_MS: u64 = 2000;
/// Queue bound. Vinculum issues commands itself, never on a peer's behalf
/// one-for-one, so a deep queue means a bug, not load.
pub const MAX_QUEUED: usize = 64;

#[derive(Debug)]
pub struct InFlight<T> {
    pub op: OpCode,
    pub tag: T,
    pub deadline_ms: u64,
}

#[derive(Debug)]
pub struct CommandQueue<T> {
    queue: VecDeque<(Command, T)>,
    in_flight: Option<InFlight<T>>,
    /// The controller's latest Num_HCI_Command_Packets. Starts at 1 (the
    /// spec's assumption before the first event).
    credits: u8,
    pub violations: u32,
}

impl<T> Default for CommandQueue<T> {
    fn default() -> Self {
        CommandQueue { queue: VecDeque::new(), in_flight: None, credits: 1, violations: 0 }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum QueueError {
    Full,
}

impl<T> CommandQueue<T> {
    pub fn push(&mut self, cmd: Command, tag: T) -> Result<(), QueueError> {
        if self.queue.len() >= MAX_QUEUED {
            return Err(QueueError::Full);
        }
        self.queue.push_back((cmd, tag));
        Ok(())
    }

    /// The next command to send now, if the controller has room and nothing
    /// is in flight.
    pub fn next_to_send(&mut self, now_ms: u64) -> Option<Command> {
        if self.in_flight.is_some() || self.credits == 0 {
            return None;
        }
        let (cmd, tag) = self.queue.pop_front()?;
        self.in_flight = Some(InFlight { op: cmd.op_code, tag, deadline_ms: now_ms + COMMAND_TIMEOUT_MS });
        Some(cmd)
    }

    /// A Command Complete or Command Status arrived. `op` may be `None`
    /// (opcode 0x0000, the controller only updating its credits).
    /// Returns the tag of the command it completes.
    pub fn complete(&mut self, op: OpCode, credits: u8) -> Option<T> {
        self.credits = credits;
        if op == OpCode::None {
            return None;
        }
        match &self.in_flight {
            Some(f) if f.op == op => self.in_flight.take().map(|f| f.tag),
            _ => {
                self.violations += 1;
                None
            }
        }
    }

    /// The in-flight command whose deadline passed, if any.
    pub fn expired(&self, now_ms: u64) -> Option<OpCode> {
        self.in_flight.as_ref().filter(|f| now_ms >= f.deadline_ms).map(|f| f.op)
    }

    pub fn deadline(&self) -> Option<u64> {
        self.in_flight.as_ref().map(|f| f.deadline_ms)
    }

    pub fn idle(&self) -> bool {
        self.in_flight.is_none() && self.queue.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vinculum_hci_proto::hci;

    fn cmd<C: TryInto<Command>>(c: C) -> Command
    where
        C::Error: std::fmt::Debug,
    {
        c.try_into().unwrap()
    }

    #[test]
    fn one_in_flight_in_order() {
        let mut q = CommandQueue::default();
        q.push(cmd(hci::Reset {}), 1).unwrap();
        q.push(cmd(hci::ReadBdAddr {}), 2).unwrap();
        assert_eq!(q.next_to_send(0).unwrap().op_code, OpCode::Reset);
        assert!(q.next_to_send(0).is_none(), "second waits for the first");
        assert_eq!(q.complete(OpCode::Reset, 1), Some(1));
        assert_eq!(q.next_to_send(1).unwrap().op_code, OpCode::ReadBdAddr);
        assert_eq!(q.complete(OpCode::ReadBdAddr, 1), Some(2));
        assert!(q.idle());
    }

    #[test]
    fn stray_completion_is_a_violation() {
        let mut q: CommandQueue<u8> = CommandQueue::default();
        assert_eq!(q.complete(OpCode::Reset, 1), None);
        assert_eq!(q.violations, 1);
        assert_eq!(q.complete(OpCode::None, 1), None);
        assert_eq!(q.violations, 1, "a NOP credit update is legal");
    }

    #[test]
    fn zero_credits_hold_the_queue_and_timeouts_fire() {
        let mut q = CommandQueue::default();
        q.push(cmd(hci::Reset {}), ()).unwrap();
        q.complete(OpCode::None, 0);
        assert!(q.next_to_send(0).is_none());
        q.complete(OpCode::None, 1);
        q.next_to_send(10).unwrap();
        assert_eq!(q.expired(10 + COMMAND_TIMEOUT_MS - 1), None);
        assert_eq!(q.expired(10 + COMMAND_TIMEOUT_MS), Some(OpCode::Reset));
    }

    #[test]
    fn bounded() {
        let mut q = CommandQueue::default();
        for _ in 0..MAX_QUEUED {
            q.push(cmd(hci::Reset {}), ()).unwrap();
        }
        assert_eq!(q.push(cmd(hci::Reset {}), ()), Err(QueueError::Full));
    }
}
