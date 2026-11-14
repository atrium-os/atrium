//! From a checked lease to host changes, and back. Pure: `main` executes
//! the actions and reports failures.

use net_proto::{LeaseV4, Route};
use std::net::Ipv4Addr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    AddAddr { addr: Ipv4Addr, prefix: u8 },
    DelAddr { addr: Ipv4Addr },
    /// `gw` unspecified: on-link through our own address.
    AddRoute(Route),
    DelRoute(Route),
    SetMtu(u16),
    SetDns(Vec<Ipv4Addr>),
}

/// What this interface currently has applied.
#[derive(Clone, Debug, Default)]
pub struct Applied {
    pub lease: Option<LeaseV4>,
}

fn routes_of(l: &LeaseV4) -> Vec<Route> {
    let mut v = l.routes.clone();
    if let Some(r) = l.router {
        v.push(Route { dst: Ipv4Addr::UNSPECIFIED, prefix: 0, gw: r });
    }
    v
}

impl Applied {
    /// Changes to go from what is applied to `new`. Unchanged parts are
    /// left alone, so a renewal with the same values does nothing.
    pub fn bind(&mut self, new: LeaseV4) -> Vec<Action> {
        let mut a = Vec::new();
        let old = self.lease.take();
        let same_addr = old.as_ref().is_some_and(|o| o.addr == new.addr && o.prefix == new.prefix);
        let old_routes = old.as_ref().map(routes_of).unwrap_or_default();
        let new_routes = routes_of(&new);
        for r in old_routes.iter().filter(|r| !same_addr || !new_routes.contains(r)) {
            a.push(Action::DelRoute(*r));
        }
        if let Some(o) = &old {
            if !same_addr {
                a.push(Action::DelAddr { addr: o.addr });
            }
        }
        if !same_addr {
            a.push(Action::AddAddr { addr: new.addr, prefix: new.prefix });
        }
        // Specific routes before the default, so nothing leaks through the
        // default while they are missing.
        let mut add: Vec<Route> = new_routes.into_iter().filter(|r| !same_addr || !old_routes.contains(r)).collect();
        add.sort_by_key(|r| std::cmp::Reverse(r.prefix));
        a.extend(add.into_iter().map(Action::AddRoute));
        if new.mtu.is_some() && old.as_ref().map(|o| o.mtu) != Some(new.mtu) {
            a.push(Action::SetMtu(new.mtu.unwrap()));
        }
        if old.as_ref().map(|o| &o.dns) != Some(&new.dns) && !new.dns.is_empty() {
            a.push(Action::SetDns(new.dns.clone()));
        }
        self.lease = Some(new);
        a
    }

    pub fn lose(&mut self) -> Vec<Action> {
        let Some(o) = self.lease.take() else { return vec![] };
        let mut a: Vec<Action> = routes_of(&o).into_iter().map(Action::DelRoute).collect();
        a.push(Action::DelAddr { addr: o.addr });
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(addr: [u8; 4], dns: u8) -> LeaseV4 {
        LeaseV4 {
            addr: addr.into(),
            prefix: 24,
            router: Some(Ipv4Addr::new(addr[0], addr[1], addr[2], 1)),
            routes: vec![Route { dst: Ipv4Addr::new(10, 9, 0, 0), prefix: 16, gw: Ipv4Addr::new(addr[0], addr[1], addr[2], 2) }],
            dns: vec![Ipv4Addr::new(addr[0], addr[1], addr[2], dns)],
            mtu: None,
            lease_s: 600,
            server: Ipv4Addr::new(addr[0], addr[1], addr[2], 1),
        }
    }

    #[test]
    fn first_bind_renewal_change_loss() {
        let mut a = Applied::default();
        let first = a.bind(l([10, 77, 0, 5], 1));
        assert_eq!(first[0], Action::AddAddr { addr: Ipv4Addr::new(10, 77, 0, 5), prefix: 24 });
        assert!(matches!(first[1], Action::AddRoute(Route { prefix: 16, .. })));
        assert!(matches!(first[2], Action::AddRoute(Route { prefix: 0, .. })));
        assert!(matches!(first[3], Action::SetDns(_)));
        // Renewal with identical values: nothing.
        assert!(a.bind(l([10, 77, 0, 5], 1)).is_empty());
        // New DNS only.
        assert_eq!(a.bind(l([10, 77, 0, 5], 3)), vec![Action::SetDns(vec![Ipv4Addr::new(10, 77, 0, 3)])]);
        // New network: routes out, address swapped, routes in.
        let moved = a.bind(l([192, 168, 7, 9], 1));
        assert!(matches!(moved[0], Action::DelRoute(_)) && matches!(moved[1], Action::DelRoute(_)));
        assert_eq!(moved[2], Action::DelAddr { addr: Ipv4Addr::new(10, 77, 0, 5) });
        assert_eq!(moved[3], Action::AddAddr { addr: Ipv4Addr::new(192, 168, 7, 9), prefix: 24 });
        let lost = a.lose();
        assert_eq!(lost.last(), Some(&Action::DelAddr { addr: Ipv4Addr::new(192, 168, 7, 9) }));
        assert!(a.lose().is_empty());
    }
}
