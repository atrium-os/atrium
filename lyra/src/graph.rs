//! lyrad graph engine — admission + topological-deadline assignment.
//!
//! Production transcription of the deterministic model proven in the gpusim
//! engine (`engine/src/lyra.rs`, phase L0): a DAG of CBS-reserved nodes whose
//! data dependency maps to deadline order. The sink carries the hard DMA
//! deadline; upstream nodes get **earlier** cascading sub-deadlines by
//! topological depth; serial EDF over those runs the graph in dependency order.
//! `admit` topo-sorts, assigns the cascade, enforces `Σ Q ≤ U_lane · T`, and
//! emits the per-node lane reservations the broker shim sponsors. Times are in
//! **microseconds** — the unit of the kernel lane ABI (`lam_lane_req_for`).
//!
//! This module is host-portable and host-tested; the FreeBSD lane wiring lives
//! in [`crate::lane`]. Keeping the planner separate from the ioctl layer mirrors
//! frescod (engine vs the `/dev/laminar` shim).

pub type NodeId = usize;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeKind {
    /// A client stream, asset player, or capture tap.
    Source,
    /// An effect/resampler — in its own Portcullis jail (overrun-isolated).
    Process,
    /// Sum N inputs to M outputs.
    Mix,
    /// Device output (the hard deadline), capture sink, or monitor tap.
    Sink,
}

/// A graph node: a CBS reservation with upstream dependencies.
#[derive(Clone)]
pub struct Node {
    pub kind: NodeKind,
    /// CBS budget `Q` in microseconds — the reserved worst-case process time.
    pub budget_us: u64,
    /// Upstream nodes this one pulls from (the edges).
    pub inputs: Vec<NodeId>,
}

impl Node {
    pub fn new(kind: NodeKind, budget_us: u64, inputs: &[NodeId]) -> Self {
        Node { kind, budget_us, inputs: inputs.to_vec() }
    }
}

/// A processing graph for one clock domain.
pub struct Graph {
    pub nodes: Vec<Node>,
    /// Device buffer period `T` in microseconds (the sink frame grid).
    pub period_us: u64,
    /// Admission cap in per-mille (750 = `U_lane` 0.75).
    pub u_lane_permille: u64,
}

/// A lane reservation for one node — what the broker sponsors via `SPONSOR_FOR`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reservation {
    pub node: NodeId,
    /// CBS budget `Q` (µs).
    pub q_us: u64,
    /// Period `T` (µs) — the device period; shared by every node in the graph.
    pub t_us: u64,
    /// Cascading sub-deadline within the period (µs from period start) — the
    /// node's cumulative finish time under serial EDF. Deeper = earlier.
    pub deadline_offset_us: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AdmitError {
    OverSubscribed { needed_us: u64, available_us: u64 },
    Cyclic,
    NoSink,
}

impl Graph {
    fn sink(&self) -> Option<NodeId> {
        self.nodes.iter().position(|n| n.kind == NodeKind::Sink)
    }

    /// Kahn topological sort over the dependency edges (deterministic order);
    /// `None` if cyclic.
    fn toposort(&self) -> Option<Vec<NodeId>> {
        let n = self.nodes.len();
        let mut indeg: Vec<usize> = (0..n).map(|v| self.nodes[v].inputs.len()).collect();
        let mut ready: Vec<NodeId> = (0..n).filter(|&v| indeg[v] == 0).collect();
        ready.sort_unstable();
        let mut order = Vec::with_capacity(n);
        let mut i = 0;
        while i < ready.len() {
            let u = ready[i];
            i += 1;
            order.push(u);
            for v in 0..n {
                if self.nodes[v].inputs.contains(&u) {
                    indeg[v] -= 1;
                    if indeg[v] == 0 {
                        ready.push(v);
                    }
                }
            }
        }
        (order.len() == n).then_some(order)
    }

    /// Admit the graph and emit per-node lane reservations. The sink, running
    /// last, finishes at `Σ Q ≤ U_lane·T < T` — comfortably before the DMA
    /// deadline, leaving `(1 − U_lane)·T` for the WFQ tier.
    pub fn admit(&self) -> Result<Vec<Reservation>, AdmitError> {
        if self.sink().is_none() {
            return Err(AdmitError::NoSink);
        }
        let order = self.toposort().ok_or(AdmitError::Cyclic)?;
        let needed: u64 = self.nodes.iter().map(|n| n.budget_us).sum();
        let available = self.period_us * self.u_lane_permille / 1000;
        if needed > available {
            return Err(AdmitError::OverSubscribed { needed_us: needed, available_us: available });
        }
        let mut acc = 0u64;
        let mut res = Vec::with_capacity(self.nodes.len());
        for &id in &order {
            acc += self.nodes[id].budget_us;
            res.push(Reservation {
                node: id,
                q_us: self.nodes[id].budget_us,
                t_us: self.period_us,
                deadline_offset_us: acc,
            });
        }
        // emit in topological (= deadline) order.
        Ok(res)
    }
}

/// Build the canonical single-stream consumer graph: `source → mix → sink`. The
/// simplest case of the full substrate. `client_q_us` is the client stream's
/// budget; the mix and sink are lyrad's own.
pub fn consumer_graph(period_us: u64, client_q_us: u64) -> Graph {
    mixer_graph(period_us, &[client_q_us])
}

/// Build the **passthrough** graph: `source → sink`, no mix node — the
/// degenerate exclusive graph an untouchable format (DSD/bitstream, §12 gap 1 /
/// `passthrough.rs`) runs as. The source's bytes reach the device bit-exactly;
/// the device is claimed exclusively (admission of any second stream is the
/// session layer's job to refuse while this is live).
pub fn passthrough_graph(period_us: u64, client_q_us: u64) -> Graph {
    Graph {
        nodes: vec![
            Node::new(NodeKind::Source, client_q_us, &[]),
            Node::new(NodeKind::Sink, period_us / 20, &[0]),
        ],
        period_us,
        u_lane_permille: 750,
    }
}

/// Build the real consumer baseline: **N client streams → one mix → sink** — the
/// foundation-app case where several apps play at once. Each source's budget is
/// its declared per-period work; the mix fans them in (its budget grows a little
/// with the stream count, the per-stream sum cost); the sink feeds the device.
/// Admission across *all* streams is the cap that bounds how many can play at the
/// chosen latency — `U_lane` is the guarantee, surfaced as `OverSubscribed`.
pub fn mixer_graph(period_us: u64, client_q_us: &[u64]) -> Graph {
    let n = client_q_us.len();
    let mut nodes: Vec<Node> = client_q_us
        .iter()
        .map(|&q| Node::new(NodeKind::Source, q, &[]))
        .collect();
    let sources: Vec<NodeId> = (0..n).collect();
    // mix budget: a small fixed cost plus a per-stream summing cost.
    let mix_us = period_us / 40 + (n as u64) * (period_us / 200);
    nodes.push(Node::new(NodeKind::Mix, mix_us, &sources)); // node n
    nodes.push(Node::new(NodeKind::Sink, period_us / 20, &[n])); // node n+1
    Graph { nodes, period_us, u_lane_permille: 750 }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 48 kHz, 128-frame buffer ≈ 2667 µs.
    const PERIOD: u64 = 2667;

    #[test]
    fn consumer_graph_admits_with_cascading_deadlines() {
        let g = consumer_graph(PERIOD, 1000);
        let res = g.admit().expect("admissible");
        assert_eq!(res.len(), 3);
        // topological order: source(0) → mix(1) → sink(2); deadlines cascade up.
        assert_eq!(res[0].node, 0);
        assert!(res[0].deadline_offset_us < res[1].deadline_offset_us);
        assert!(res[1].deadline_offset_us < res[2].deadline_offset_us);
        // the sink (last) finishes within the period with WFQ headroom.
        assert!(res[2].deadline_offset_us < PERIOD);
        assert_eq!(res.iter().map(|r| r.t_us).collect::<Vec<_>>(), vec![PERIOD; 3]);
    }

    #[test]
    fn deep_chain_assigns_earlier_deadlines_upstream() {
        // source → eq → reverb → mix → sink.
        let g = Graph {
            nodes: vec![
                Node::new(NodeKind::Source, 200, &[]),
                Node::new(NodeKind::Process, 300, &[0]),
                Node::new(NodeKind::Process, 300, &[1]),
                Node::new(NodeKind::Mix, 100, &[2]),
                Node::new(NodeKind::Sink, 100, &[3]),
            ],
            period_us: PERIOD,
            u_lane_permille: 750,
        };
        let res = g.admit().unwrap();
        // reservations are emitted in deadline order; cumulative = 200,500,800,900,1000.
        let offs: Vec<u64> = res.iter().map(|r| r.deadline_offset_us).collect();
        assert_eq!(offs, vec![200, 500, 800, 900, 1000]);
    }

    #[test]
    fn oversubscription_is_rejected() {
        let mut g = consumer_graph(PERIOD, 1000);
        g.nodes[0].budget_us = 3000; // > U·T (= 2000)
        match g.admit() {
            Err(AdmitError::OverSubscribed { needed_us, available_us }) => {
                assert!(needed_us > available_us);
                assert_eq!(available_us, PERIOD * 750 / 1000);
            }
            other => panic!("expected oversubscription: {other:?}"),
        }
    }

    #[test]
    fn many_streams_mix_into_one_sink() {
        // four apps playing at once → one mix → sink. All four sources feed the
        // mix; the mix feeds the sink; cascading deadlines order them.
        let g = mixer_graph(PERIOD, &[400, 400, 400, 400]);
        let res = g.admit().expect("four 0.4ms streams fit");
        assert_eq!(res.len(), 6); // 4 sources + mix + sink
        // the mix node depends on all four sources; it is scheduled after them.
        let mix = res.iter().find(|r| r.node == 4).unwrap();
        let sink = res.iter().find(|r| r.node == 5).unwrap();
        let last_source = res.iter().filter(|r| r.node < 4).map(|r| r.deadline_offset_us).max().unwrap();
        assert!(mix.deadline_offset_us > last_source, "mix runs after every source");
        assert!(sink.deadline_offset_us > mix.deadline_offset_us, "sink runs last");
        assert!(sink.deadline_offset_us < PERIOD);
    }

    #[test]
    fn too_many_streams_exceed_the_admission_cap() {
        // enough concurrent streams to blow U·T (= 2000 µs): the cap bounds how
        // many can play at this latency — the guarantee, surfaced honestly.
        let many: Vec<u64> = vec![400; 8]; // 8 × 0.4 ms = 3.2 ms of sources alone
        match mixer_graph(PERIOD, &many).admit() {
            Err(AdmitError::OverSubscribed { needed_us, available_us }) => {
                assert!(needed_us > available_us);
            }
            other => panic!("expected oversubscription: {other:?}"),
        }
    }

    #[test]
    fn passthrough_is_a_two_node_source_to_sink_graph() {
        // DSD/bitstream: no mix node — bytes reach the device untouched.
        let g = passthrough_graph(PERIOD, 1000);
        let res = g.admit().expect("passthrough admits");
        assert_eq!(res.len(), 2, "source -> sink only, no mix/DSP");
        assert!(g.nodes.iter().all(|n| n.kind != NodeKind::Mix), "no mixing");
        assert_eq!(res[0].node, 0); // source first
        assert!(res[1].deadline_offset_us < PERIOD);
    }

    #[test]
    fn cycle_and_no_sink_are_rejected() {
        let mut g = consumer_graph(PERIOD, 500);
        g.nodes[0].inputs = vec![2]; // source depends on sink: a cycle
        assert_eq!(g.admit().err(), Some(AdmitError::Cyclic));

        let g2 = Graph {
            nodes: vec![Node::new(NodeKind::Source, 100, &[])],
            period_us: PERIOD,
            u_lane_permille: 750,
        };
        assert_eq!(g2.admit().err(), Some(AdmitError::NoSink));
    }
}
