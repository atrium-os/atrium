//! Relay routing core — transport-independent.
//!
//! Every connection (device or publisher) is a
//! [`ConnId`] with an outbound [`mpsc::Sender`]; the
//! transport layer's writer thread drains the matching
//! `Receiver` onto the socket. The core never touches a
//! socket directly, which keeps it unit-testable
//! without spinning up TCP.
//!
//! Roles are implicit: a connection that sends
//! `Subscribe` accumulates routes and receives `Push`;
//! a connection that sends `Publish` triggers fan-out
//! and receives `PublishAccepted`. The same connection
//! could do both — the core doesn't care.

use crate::proto::{ClientMsg, PushKey, RelayMsg};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Opaque per-connection identifier.
pub type ConnId = u64;

struct State {
    /// pubkey -> the connections that subscribed to it.
    routes: HashMap<PushKey, Vec<ConnId>>,
    /// connection id -> its outbound channel.
    conns: HashMap<ConnId, Sender<RelayMsg>>,
    next_conn: ConnId,
}

/// The relay's routing table. Cheap to share behind an
/// `Arc`; all methods take `&self`.
pub struct Relay {
    state: Mutex<State>,
    next_push_id: AtomicU64,
}

impl Default for Relay {
    fn default() -> Self {
        Self::new()
    }
}

impl Relay {
    pub fn new() -> Self {
        Relay {
            state: Mutex::new(State {
                routes: HashMap::new(),
                conns: HashMap::new(),
                next_conn: 1,
            }),
            next_push_id: AtomicU64::new(1),
        }
    }

    /// Register a new connection. Returns its id and
    /// the receiver the transport's writer thread
    /// drains onto the socket.
    pub fn add_connection(&self) -> (ConnId, Receiver<RelayMsg>) {
        let (tx, rx) = mpsc::channel();
        let mut s = self.state.lock().unwrap();
        let id = s.next_conn;
        s.next_conn += 1;
        s.conns.insert(id, tx);
        (id, rx)
    }

    /// Drop a connection: remove it from every route +
    /// the connection table. Idempotent.
    pub fn remove_connection(&self, conn: ConnId) {
        let mut s = self.state.lock().unwrap();
        s.conns.remove(&conn);
        for subs in s.routes.values_mut() {
            subs.retain(|c| *c != conn);
        }
        s.routes.retain(|_, subs| !subs.is_empty());
    }

    /// Process one client message for connection
    /// `conn`. Any reply is delivered via `conn`'s
    /// channel (Pong, PublishAccepted) or the
    /// subscribers' channels (Push).
    pub fn on_msg(&self, conn: ConnId, msg: ClientMsg) {
        match msg {
            ClientMsg::Subscribe { keys } => {
                let mut s = self.state.lock().unwrap();
                for k in keys {
                    let subs = s.routes.entry(k).or_default();
                    if !subs.contains(&conn) {
                        subs.push(conn);
                    }
                }
            }
            ClientMsg::Unsubscribe { keys } => {
                let mut s = self.state.lock().unwrap();
                for k in &keys {
                    if let Some(subs) = s.routes.get_mut(k) {
                        subs.retain(|c| *c != conn);
                    }
                }
                s.routes.retain(|_, subs| !subs.is_empty());
            }
            ClientMsg::Ack { id: _ } => {
                // v0: at-most-once, acks are a no-op.
                // Spec §3.2's retry-until-ack is a
                // follow-up that needs a pending-push
                // table here.
            }
            ClientMsg::Ping => {
                let s = self.state.lock().unwrap();
                if let Some(tx) = s.conns.get(&conn) {
                    let _ = tx.send(RelayMsg::Pong);
                }
            }
            ClientMsg::Publish { to_key, blob, ttl_secs: _ } => {
                let delivered = self.fan_out(to_key, blob);
                let id = self.next_push_id.load(Ordering::SeqCst);
                let s = self.state.lock().unwrap();
                if let Some(tx) = s.conns.get(&conn) {
                    let _ = tx.send(RelayMsg::PublishAccepted {
                        id, delivered,
                    });
                }
            }
        }
    }

    /// Route a blob to every connection subscribed to
    /// `to_key`. Returns the delivered count. Public so
    /// a publisher-side HTTP shim (future) can call it
    /// without constructing a `ClientMsg`.
    pub fn fan_out(&self, to_key: PushKey, blob: Vec<u8>) -> u32 {
        let id = self.next_push_id.fetch_add(1, Ordering::SeqCst);
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let s = self.state.lock().unwrap();
        let Some(subs) = s.routes.get(&to_key) else {
            return 0;
        };
        let mut delivered = 0u32;
        for conn in subs {
            if let Some(tx) = s.conns.get(conn) {
                if tx.send(RelayMsg::Push {
                    id, to_key, ts, blob: blob.clone(),
                }).is_ok() {
                    delivered += 1;
                }
            }
        }
        delivered
    }

    /// Number of distinct pubkeys with at least one
    /// subscriber. Test / observability helper.
    pub fn route_count(&self) -> usize {
        self.state.lock().unwrap().routes.len()
    }

    /// Number of live connections. Test / observability
    /// helper.
    pub fn connection_count(&self) -> usize {
        self.state.lock().unwrap().conns.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribe_then_publish_delivers_to_the_device() {
        let relay = Relay::new();
        let (device, device_rx) = relay.add_connection();
        let (publisher, pub_rx) = relay.add_connection();

        let key = [0x11u8; 32];
        relay.on_msg(device, ClientMsg::Subscribe { keys: vec![key] });
        assert_eq!(relay.route_count(), 1);

        relay.on_msg(publisher, ClientMsg::Publish {
            to_key: key,
            blob: vec![0xca, 0xfe],
            ttl_secs: 60,
        });

        // Device got the push.
        match device_rx.try_recv().expect("device should get a push") {
            RelayMsg::Push { to_key, blob, .. } => {
                assert_eq!(to_key, key);
                assert_eq!(blob, vec![0xca, 0xfe]);
            }
            other => panic!("expected Push, got {:?}", other),
        }
        // Publisher got the accept with delivered=1.
        match pub_rx.try_recv().expect("publisher should get an accept") {
            RelayMsg::PublishAccepted { delivered, .. } => {
                assert_eq!(delivered, 1);
            }
            other => panic!("expected PublishAccepted, got {:?}", other),
        }
    }

    #[test]
    fn publish_to_unsubscribed_key_delivers_to_nobody() {
        let relay = Relay::new();
        let (publisher, pub_rx) = relay.add_connection();
        relay.on_msg(publisher, ClientMsg::Publish {
            to_key: [0x99u8; 32], blob: vec![1], ttl_secs: 1,
        });
        match pub_rx.try_recv().unwrap() {
            RelayMsg::PublishAccepted { delivered, .. } => {
                assert_eq!(delivered, 0);
            }
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn fan_out_reaches_every_subscriber() {
        let relay = Relay::new();
        let key = [0x22u8; 32];
        let (d1, rx1) = relay.add_connection();
        let (d2, rx2) = relay.add_connection();
        let (d3, rx3) = relay.add_connection();
        relay.on_msg(d1, ClientMsg::Subscribe { keys: vec![key] });
        relay.on_msg(d2, ClientMsg::Subscribe { keys: vec![key] });
        // d3 subscribes to a different key.
        relay.on_msg(d3, ClientMsg::Subscribe { keys: vec![[0x33u8; 32]] });

        let delivered = relay.fan_out(key, vec![7, 7, 7]);
        assert_eq!(delivered, 2);
        assert!(matches!(rx1.try_recv(), Ok(RelayMsg::Push { .. })));
        assert!(matches!(rx2.try_recv(), Ok(RelayMsg::Push { .. })));
        assert!(rx3.try_recv().is_err(), "d3 must not receive it");
    }

    #[test]
    fn unsubscribe_stops_delivery() {
        let relay = Relay::new();
        let key = [0x44u8; 32];
        let (device, device_rx) = relay.add_connection();
        relay.on_msg(device, ClientMsg::Subscribe { keys: vec![key] });
        relay.on_msg(device, ClientMsg::Unsubscribe { keys: vec![key] });
        assert_eq!(relay.route_count(), 0);

        let delivered = relay.fan_out(key, vec![1]);
        assert_eq!(delivered, 0);
        assert!(device_rx.try_recv().is_err());
    }

    #[test]
    fn remove_connection_clears_its_routes() {
        let relay = Relay::new();
        let key = [0x55u8; 32];
        let (device, _rx) = relay.add_connection();
        relay.on_msg(device, ClientMsg::Subscribe { keys: vec![key] });
        assert_eq!(relay.connection_count(), 1);
        assert_eq!(relay.route_count(), 1);

        relay.remove_connection(device);
        assert_eq!(relay.connection_count(), 0);
        assert_eq!(relay.route_count(), 0);
    }

    #[test]
    fn ping_gets_a_pong() {
        let relay = Relay::new();
        let (conn, rx) = relay.add_connection();
        relay.on_msg(conn, ClientMsg::Ping);
        assert!(matches!(rx.try_recv(), Ok(RelayMsg::Pong)));
    }

    #[test]
    fn duplicate_subscribe_does_not_double_deliver() {
        let relay = Relay::new();
        let key = [0x66u8; 32];
        let (device, device_rx) = relay.add_connection();
        relay.on_msg(device, ClientMsg::Subscribe { keys: vec![key] });
        relay.on_msg(device, ClientMsg::Subscribe { keys: vec![key] });

        let delivered = relay.fan_out(key, vec![1]);
        assert_eq!(delivered, 1, "duplicate subscribe must not double-count");
        assert!(matches!(device_rx.try_recv(), Ok(RelayMsg::Push { .. })));
        assert!(device_rx.try_recv().is_err(), "exactly one push");
    }
}
