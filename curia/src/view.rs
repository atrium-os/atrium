//! Drawing Curia (curia.md §2, §3) with Pergola: the sidebar of pages and
//! the Network page. Placed by hand, as Scrinium is; sizes in em so they grow
//! with the system text size.

use pergola::geom::Rect;
use pergola::layout::FlexStyle;
use pergola::node::Node;
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::{phosphor as ph, CredentialField, Glyph, ScrollState, VirtualList};
use pergola::{Color, Mutable, NodeId, TextStyle};

use atrium_netconf::{Link, Online};

use crate::devices::{self, DevPage, Group, Row};
use crate::display::{self as disp, DispPage, Place};
use crate::network::{headline, name, security_label, NetPage, MAC_POLICIES};

fn em(pt: f32) -> f32 {
    pergola::theme::size::em(pt)
}

pub fn side_w() -> f32 {
    em(200.0)
}

/// The sidebar's pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Which {
    #[default]
    Network,
    Display,
    Devices,
}

pub struct CuriaView {
    pub which: Mutable<Which>,
    pub page: Mutable<NetPage>,
    pub devs: Mutable<DevPage>,
    pub dev_scroll: Mutable<ScrollState>,
    pub disp: Mutable<DispPage>,
    /// Arca's Wi-Fi passphrase field (curia.md §4): Arca draws it and takes
    /// its keys; Curia only reserves its place and gets a handle.
    pub cred: CredentialField,
    pub size: Mutable<(f32, f32)>,
}

pub fn with(page: &Mutable<NetPage>, f: impl FnOnce(&mut NetPage)) {
    let mut p = page.get_cloned();
    f(&mut p);
    page.set(p);
}

fn sans(size: f32, weight: Weight, color: Color) -> TextStyle {
    TextStyle { family: font::SANS.into(), size, weight, color }
}

fn text(ctx: &mut Ctx, x: f32, cy: f32, s: &str, size: f32, weight: Weight, color: Color) -> NodeId {
    let h = em(size) * 1.5;
    ctx.add(Node::Text { rect: Rect::new(x, cy - h / 2.0, 0.0, h), content: s.into(), style: sans(size, weight, color) })
}

fn width(s: &str, size: f32, weight: Weight) -> f32 {
    Node::measure_text(s, &sans(size, weight, Color::TRANSPARENT)).w
}

fn rect(ctx: &mut Ctx, x: f32, y: f32, w: f32, h: f32, fill: Color, radius: f32) -> NodeId {
    ctx.add(Node::Rect { rect: Rect::new(x, y, w, h), fill, radius })
}

fn icon(ctx: &mut Ctx, x: f32, y: f32, g: char, size: f32, color: Color) -> NodeId {
    Glyph::new(g).size(size).color(color).at(x, y).emit(ctx)
}

/// A pressable area pushed as the parent of what is drawn in it; the
/// caller pops it.
fn area(ctx: &mut Ctx, r: Rect, radius: f32, fill: Color, hover: Color, on: impl Fn() + 'static) -> NodeId {
    let id = ctx.push(Node::Rect { rect: r, fill, radius });
    if ctx.is_hovered(id) || ctx.is_pressed(id) {
        let c = if ctx.is_pressed(id) { ctx.theme.state_pressed() } else { hover };
        if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(id) {
            *fill = c;
        }
    }
    ctx.on_click(id, on);
    id
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Look {
    Ghost,
    Secondary,
    Primary,
    Danger,
}

/// A text button, 30 high at 100% text; returns its width.
fn button(ctx: &mut Ctx, x: f32, y: f32, label: &str, look: Look, enabled: bool, on: impl Fn() + 'static) -> f32 {
    let t = ctx.theme;
    let (w, h) = (width(label, type_size::SM, Weight::Medium) + em(24.0), em(30.0));
    let (fill, hover, fg) = match look {
        Look::Ghost => (Color::TRANSPARENT, t.state_hover(), t.text_primary()),
        Look::Secondary => (t.bg_surface(), t.state_hover(), t.text_primary()),
        Look::Primary => (t.accent_fill(), t.accent_fill_hover(), t.text_on_accent()),
        Look::Danger => (t.danger_fg(), t.danger_fg(), t.text_on_accent()),
    };
    if look == Look::Secondary {
        rect(ctx, x, y, w, h, t.border_strong(), 6.0);
    }
    let inner = if look == Look::Secondary { Rect::new(x + 1.0, y + 1.0, w - 2.0, h - 2.0) } else { Rect::new(x, y, w, h) };
    if enabled {
        area(ctx, inner, 6.0, fill, hover, on);
    } else {
        ctx.push(Node::Rect { rect: inner, fill: if matches!(look, Look::Primary | Look::Danger) { t.state_hover() } else { fill }, radius: 6.0 });
    }
    text(ctx, x + em(12.0), y + h / 2.0, label, type_size::SM, Weight::Medium, if enabled { fg } else { t.text_disabled() });
    ctx.pop();
    w
}

fn bars_glyph(bars: u8) -> char {
    match bars {
        4 | 3 => ph::WIFI_HIGH,
        2 => ph::WIFI_MEDIUM,
        1 => ph::WIFI_LOW,
        _ => ph::WIFI_NONE,
    }
}

impl View for CuriaView {
    fn render(&self, ctx: &mut Ctx) {
        let (w, h) = self.size.get();
        let t = ctx.theme;
        ctx.min_size(pergola::geom::Size::new(side_w() + em(560.0), em(420.0)));
        rect(ctx, 0.0, 0.0, w, h, t.bg_canvas(), 0.0);
        // The sidebar.
        rect(ctx, 0.0, 0.0, side_w(), h, t.bg_surface(), 0.0);
        text(ctx, em(20.0), em(30.0), "Settings", type_size::LG, Weight::Semibold, t.text_primary());
        let now = self.which.get();
        for (i, (which, glyph, label)) in [(Which::Network, ph::WIFI_HIGH, "Network"), (Which::Display, ph::MONITOR, "Display"), (Which::Devices, ph::USB, "Devices")].into_iter().enumerate() {
            let y = em(56.0) + i as f32 * em(38.0);
            let on = now == which;
            let sel = self.which.clone();
            area(ctx, Rect::new(em(10.0), y, side_w() - em(20.0), em(34.0)), 6.0, if on { t.state_hover() } else { Color::TRANSPARENT }, t.state_hover(), move || sel.set(which));
            icon(ctx, em(20.0), y + em(7.0), glyph, em(18.0), if on { t.accent_fg() } else { t.text_secondary() });
            text(ctx, em(48.0), y + em(17.0), label, type_size::SM, Weight::Medium, t.text_primary());
            ctx.pop();
        }
        match now {
            Which::Network => self.network(ctx, side_w() + em(32.0), w - side_w() - em(64.0), h),
            Which::Display => self.display(ctx, side_w() + em(32.0), w - side_w() - em(64.0), h),
            Which::Devices => self.devices(ctx, side_w() + em(32.0), w - side_w() - em(64.0), h),
        }
    }
}

impl CuriaView {
    fn network(&self, ctx: &mut Ctx, x0: f32, pw: f32, h: f32) {
        let p = self.page.get_cloned();
        let t = ctx.theme;
        let mut y = em(20.0);
        text(ctx, x0, y + em(16.0), "Network", type_size::XL, Weight::Semibold, t.text_primary());
        y += em(48.0);

        // Status.
        let ch = em(64.0);
        rect(ctx, x0, y, pw, ch, t.bg_surface(), 8.0);
        let s = p.status.as_ref().filter(|_| p.reachable);
        let g = match s {
            None => ph::WIFI_SLASH,
            Some(s) if matches!(s.online, Online::No | Online::Captive) && s.link != Link::None => ph::WIFI_X,
            Some(s) => match s.link {
                Link::None => ph::WIFI_SLASH,
                Link::Wired => ph::PLUGS_CONNECTED,
                Link::Wifi => bars_glyph(s.bars.unwrap_or(4)),
            },
        };
        icon(ctx, x0 + em(16.0), y + em(20.0), g, em(24.0), t.text_secondary());
        let (head, sub) = headline(p.status.as_ref(), p.reachable);
        match &sub {
            Some(sub) => {
                text(ctx, x0 + em(56.0), y + em(24.0), &head, type_size::MD, Weight::Medium, t.text_primary());
                text(ctx, x0 + em(56.0), y + em(44.0), sub, type_size::SM, Weight::Regular, t.text_secondary());
            }
            None => {
                text(ctx, x0 + em(56.0), y + ch / 2.0, &head, type_size::MD, Weight::Medium, t.text_primary());
            }
        }
        if s.is_some_and(|s| s.link == Link::Wifi) {
            let label = "Disconnect";
            let bw = width(label, type_size::SM, Weight::Medium) + em(24.0);
            let pg = self.page.clone();
            button(ctx, x0 + pw - bw - em(16.0), y + (ch - em(30.0)) / 2.0, label, Look::Secondary, true, move || with(&pg, NetPage::leave));
        }
        y += ch + em(24.0);
        if !p.reachable {
            return;
        }

        // Networks around.
        text(ctx, x0, y + em(10.0), "Wi-Fi networks", type_size::SM, Weight::Semibold, t.text_secondary());
        {
            let pg = self.page.clone();
            let label = "Scan again";
            let bw = width(label, type_size::SM, Weight::Medium) + em(24.0);
            button(ctx, x0 + pw - bw, y - em(5.0), label, Look::Ghost, true, move || with(&pg, |p| p.wants.push(crate::network::Want::Scan)));
        }
        y += em(28.0);
        let rh = em(40.0);
        let listed = p.listed();
        if listed.is_empty() {
            let msg = if p.scanned { "No networks found" } else { "Looking for networks…" };
            text(ctx, x0 + em(12.0), y + rh / 2.0, msg, type_size::SM, Weight::Regular, t.text_tertiary());
            y += rh;
        }
        for n in listed.into_iter().take(8) {
            let ssid = n.ssid.clone();
            let pg = self.page.clone();
            let on = p.is_connected(&n.ssid);
            area(ctx, Rect::new(x0, y, pw, rh), 6.0, Color::TRANSPARENT, t.state_hover(), move || with(&pg, |p| p.pick(&ssid)));
            icon(ctx, x0 + em(12.0), y + em(11.0), bars_glyph(n.bars), em(18.0), t.text_secondary());
            text(ctx, x0 + em(44.0), y + rh / 2.0, &name(&n.ssid), type_size::SM, if on { Weight::Semibold } else { Weight::Regular }, t.text_primary());
            let mut right = security_label(&n.security).to_string();
            if on {
                right = format!("Connected · {right}");
            } else if p.joining.as_deref() == Some(n.ssid.as_slice()) {
                right = format!("Joining… · {right}");
            } else if n.known {
                right = format!("Saved · {right}");
            }
            let rw = width(&right, type_size::SM, Weight::Regular);
            text(ctx, x0 + pw - rw - em(12.0), y + rh / 2.0, &right, type_size::SM, Weight::Regular, t.text_secondary());
            ctx.pop();
            y += rh;
            if p.asking.as_deref() == Some(n.ssid.as_slice()) {
                let fy = y + em(4.0);
                let fx = x0 + em(44.0);
                let fw = (pw - em(260.0)).clamp(em(160.0), em(320.0));
                self.cred.clone().at(fx, fy).size(fw, em(32.0)).render(ctx);
                let pg = self.page.clone();
                let bx = fx + fw + em(12.0);
                let ready = p.joining.is_none() && p.armed.is_none() && p.field_up;
                let jw = button(ctx, bx, fy + em(1.0), "Join", Look::Primary, ready, move || {
                    with(&pg, |p| p.join_pressed(std::time::Instant::now()));
                });
                let pg = self.page.clone();
                button(ctx, bx + jw + em(8.0), fy + em(1.0), "Cancel", Look::Ghost, true, move || {
                    with(&pg, NetPage::cancel_ask);
                });
                y += em(44.0);
                if !p.field_up {
                    // Arca's side (limend, arca-ui) is away: say so rather than
                    // show a field that silently takes nothing.
                    text(ctx, fx, y - em(2.0), "The passphrase field is not available right now; trying again…", type_size::XS, Weight::Regular, t.text_tertiary());
                    y += em(20.0);
                }
            }
        }
        y += em(20.0);

        // Saved networks.
        text(ctx, x0, y + em(10.0), "Saved networks", type_size::SM, Weight::Semibold, t.text_secondary());
        y += em(28.0);
        if p.profiles.is_empty() {
            text(ctx, x0 + em(12.0), y + em(20.0), "None yet: join one above", type_size::SM, Weight::Regular, t.text_tertiary());
            y += em(40.0);
        }
        let sh = em(44.0);
        for pr in p.profiles.iter().take(6) {
            rect(ctx, x0, y + sh - 1.0, pw, 1.0, t.border_default(), 0.0);
            text(ctx, x0 + em(12.0), y + sh / 2.0, &name(&pr.ssid), type_size::SM, Weight::Medium, t.text_primary());
            let by = y + (sh - em(30.0)) / 2.0;
            let mut bx = x0 + em(170.0);
            let ssid = pr.ssid.clone();
            let pg = self.page.clone();
            let on = pr.auto;
            bx += button(ctx, bx, by, if on { "Auto-join: on" } else { "Auto-join: off" }, Look::Secondary, true, move || with(&pg, |p| p.set_auto(&ssid, !on))) + em(6.0);
            let (ssid, pg, m) = (pr.ssid.clone(), self.page.clone(), pr.metered);
            bx += button(ctx, bx, by, if m { "Metered" } else { "Not metered" }, Look::Secondary, true, move || with(&pg, |p| p.set_metered(&ssid, !m))) + em(6.0);
            let mi = MAC_POLICIES.iter().position(|(k, _)| *k == pr.mac).unwrap_or(0);
            let (ssid, pg) = (pr.ssid.clone(), self.page.clone());
            let short = ["Address: per network", "Address: new each time", "Address: device's own"][mi];
            bx += button(ctx, bx, by, short, Look::Secondary, true, move || with(&pg, |p| p.set_mac(&ssid, (mi + 1) % MAC_POLICIES.len()))) + em(6.0);
            let (ssid, pg) = (pr.ssid.clone(), self.page.clone());
            bx += button(ctx, bx, by, "−", Look::Ghost, true, move || with(&pg, |p| p.bump_prio(&ssid, -1)));
            let pt = format!("Priority {}", pr.prio);
            text(ctx, bx + em(4.0), y + sh / 2.0, &pt, type_size::SM, Weight::Regular, t.text_secondary());
            bx += width(&pt, type_size::SM, Weight::Regular) + em(8.0);
            let (ssid, pg) = (pr.ssid.clone(), self.page.clone());
            button(ctx, bx, by, "+", Look::Ghost, true, move || with(&pg, |p| p.bump_prio(&ssid, 1)));
            let asking = p.confirm_forget.as_deref() == Some(pr.ssid.as_slice());
            let label = if asking { "Forget it" } else { "Forget" };
            let fw = width(label, type_size::SM, Weight::Medium) + em(24.0);
            let (ssid, pg) = (pr.ssid.clone(), self.page.clone());
            button(ctx, x0 + pw - fw, by, label, if asking { Look::Danger } else { Look::Ghost }, true, move || with(&pg, |p| p.forget(&ssid)));
            y += sh;
        }
        y += em(20.0);

        // Details.
        {
            let pg = self.page.clone();
            let label = if p.show_details { "Hide details" } else { "Show details" };
            button(ctx, x0, y, label, Look::Ghost, true, move || with(&pg, NetPage::toggle_details));
            y += em(36.0);
        }
        if p.show_details {
            for i in &p.ifaces {
                let line = format!(
                    "{}{}: {} · {} · router {} · DNS {}{}",
                    i.name,
                    if i.wifi { " (Wi-Fi)" } else { "" },
                    i.link,
                    i.addr.clone().unwrap_or_else(|| "no address".into()),
                    i.router.clone().unwrap_or_else(|| "none".into()),
                    if i.dns.is_empty() { "none".to_string() } else { i.dns.join(", ") },
                    if i.egress { " · default route" } else { "" }
                );
                text(ctx, x0 + em(12.0), y + em(10.0), &line, type_size::SM, Weight::Regular, t.text_secondary());
                y += em(22.0);
                if !i.addrs6.is_empty() {
                    text(ctx, x0 + em(24.0), y + em(10.0), &format!("IPv6 {}", i.addrs6.join(", ")), type_size::SM, Weight::Regular, t.text_tertiary());
                    y += em(22.0);
                }
            }
        }

        // The last outcome, at the foot.
        if let Some(n) = &p.notice {
            let ny = h - em(44.0);
            rect(ctx, x0, ny, pw, em(32.0), if n.error { t.state_hover() } else { t.bg_surface() }, 6.0);
            text(ctx, x0 + em(12.0), ny + em(16.0), &n.text, type_size::SM, Weight::Medium, if n.error { t.danger_fg() } else { t.text_primary() });
        }
    }
}

impl CuriaView {
    /// The Devices page (curia.md §3a): what dispensatord decided for each
    /// device. Read-only.
    fn devices(&self, ctx: &mut Ctx, x0: f32, pw: f32, h: f32) {
        let d = self.devs.get_cloned();
        let t = ctx.theme;
        let mut y = em(20.0);
        text(ctx, x0, y + em(16.0), "Devices", type_size::XL, Weight::Semibold, t.text_primary());
        y += em(40.0);
        text(ctx, x0, y + em(10.0), &d.summary(), type_size::SM, Weight::Regular, t.text_secondary());
        y += em(32.0);
        let rows = d.rows();
        if rows.is_empty() {
            return;
        }
        let rh = em(36.0);
        let devs = self.devs.clone();
        let list = d.devices().to_vec();
        VirtualList::new(self.dev_scroll.clone(), Rect::new(x0, y, pw, (h - y - em(16.0)).max(rh)), rows.len(), rh, move |i, ctx| {
            let t = ctx.theme;
            let row = ctx.push(Node::stack(pergola::geom::Axis::Vertical, Rect::new(0.0, 0.0, pw, rh), 0.0));
            ctx.set_flex(row, FlexStyle { free: true, ..FlexStyle::default() });
            match &rows[i] {
                Row::Header { group, count } => {
                    text(ctx, 0.0, rh * 0.6, &format!("{} · {count}", group.title()), type_size::SM, Weight::Semibold, t.text_secondary());
                }
                Row::Device(k) => {
                    let dev = &list[*k];
                    let (dv, k2) = (devs.clone(), *k);
                    let open = d.is_open(dev);
                    area(ctx, Rect::new(0.0, 1.0, pw, rh - 2.0), 6.0, if open { t.state_hover() } else { Color::TRANSPARENT }, t.state_hover(), move || with_devs(&dv, |p| p.toggle(k2)));
                    let warn = Group::of(dev) == Group::Nobody;
                    icon(ctx, em(12.0), (rh - em(16.0)) / 2.0, if open { ph::CARET_DOWN } else { ph::CARET_RIGHT }, em(14.0), t.text_tertiary());
                    let mut x = em(34.0);
                    if let Some(k) = devices::kind(dev) {
                        text(ctx, x, rh / 2.0, k, type_size::SM, Weight::Medium, t.text_primary());
                        x += width(k, type_size::SM, Weight::Medium) + em(8.0);
                    }
                    text(ctx, x, rh / 2.0, &dev.name, type_size::SM, Weight::Regular, t.text_secondary());
                    let right = devices::owner_line(&dev.owner);
                    let rw = width(&right, type_size::SM, Weight::Regular);
                    text(ctx, pw - rw - em(12.0), rh / 2.0, &right, type_size::SM, Weight::Regular, if warn { t.warning_fg() } else { t.text_secondary() });
                    ctx.pop();
                }
                Row::Detail(s) => {
                    text(ctx, em(34.0), rh / 2.0, s, type_size::XS, Weight::Regular, t.text_tertiary());
                }
                Row::Toggle { group, count, shown } => {
                    let label = match (group, *shown, *count) {
                        (Group::Kernel, true, _) => "Hide FreeBSD's devices".to_string(),
                        (Group::Kernel, false, 1) => "Show the device FreeBSD drives".to_string(),
                        (Group::Kernel, false, n) => format!("Show the {n} devices FreeBSD drives"),
                        (_, true, _) => "Hide them".to_string(),
                        (_, false, 1) => "Show it".to_string(),
                        (_, false, n) => format!("Show all {n}"),
                    };
                    let (dv, g) = (devs.clone(), *group);
                    let w = width(&label, type_size::SM, Weight::Medium) + em(24.0);
                    area(ctx, Rect::new(0.0, 3.0, w, rh - 6.0), 6.0, Color::TRANSPARENT, t.state_hover(), move || with_devs(&dv, |p| p.toggle_group(g)));
                    text(ctx, em(12.0), rh / 2.0, &label, type_size::SM, Weight::Medium, t.accent_fg());
                    ctx.pop();
                }
                Row::Message(m) => {
                    text(ctx, em(12.0), rh / 2.0, m, type_size::SM, Weight::Regular, t.text_tertiary());
                }
            }
            ctx.pop();
        })
        .key(91)
        .backdrop(t.bg_canvas())
        .emit(ctx);
    }
}

pub fn with_disp(page: &Mutable<DispPage>, f: impl FnOnce(&mut DispPage)) {
    let mut p = page.get_cloned();
    f(&mut p);
    page.set(p);
}

impl CuriaView {
    /// The Display page (curia.md §11): light or dark, text size, and each
    /// monitor's UI size, resolution and place, over forum-wm's settings
    /// socket.
    fn display(&self, ctx: &mut Ctx, x0: f32, pw: f32, h: f32) {
        let d = self.disp.get_cloned();
        let t = ctx.theme;
        let mut y = em(20.0);
        text(ctx, x0, y + em(16.0), "Display", type_size::XL, Weight::Semibold, t.text_primary());
        y += em(48.0);
        let s = match &d.state {
            None => {
                text(ctx, x0, y + em(10.0), "Asking Forum for the display settings…", type_size::SM, Weight::Regular, t.text_tertiary());
                return;
            }
            Some(Err(why)) => {
                let ch = em(64.0);
                rect(ctx, x0, y, pw, ch, t.bg_surface(), 8.0);
                icon(ctx, x0 + em(16.0), y + em(20.0), ph::MONITOR, em(24.0), t.text_secondary());
                text(ctx, x0 + em(56.0), y + em(24.0), "Display settings are not available", type_size::MD, Weight::Medium, t.text_primary());
                text(ctx, x0 + em(56.0), y + em(44.0), &format!("{why}; trying again"), type_size::SM, Weight::Regular, t.text_secondary());
                return;
            }
            Some(Ok(s)) => s.clone(),
        };
        let lx = x0 + em(140.0);
        let label = |ctx: &mut Ctx, y: f32, l: &str| {
            text(ctx, x0, y + em(15.0), l, type_size::SM, Weight::Medium, ctx.theme.text_secondary());
        };
        // A row of choices; the chosen one is drawn as the primary button.
        fn choices(ctx: &mut Ctx, x: f32, y: f32, items: Vec<(String, bool, Box<dyn Fn()>)>) {
            let mut x = x;
            for (l, on, f) in items {
                x += button(ctx, x, y, &l, if on { Look::Primary } else { Look::Secondary }, true, f) + em(6.0);
            }
        }

        label(ctx, y, "Appearance");
        choices(ctx, lx, y, [(false, "Light"), (true, "Dark")].into_iter().map(|(dark, l)| {
            let pg = self.disp.clone();
            (l.to_string(), s.dark == dark, Box::new(move || with_disp(&pg, |p| p.set_dark(dark))) as Box<dyn Fn()>)
        }).collect());
        y += em(42.0);
        label(ctx, y, "Text size");
        choices(ctx, lx, y, fresco_protocol::TEXT_SIZES.iter().map(|&m| {
            let pg = self.disp.clone();
            (format!("{}%", m / 10), s.text_size == m, Box::new(move || with_disp(&pg, |p| p.set_text_size(m))) as Box<dyn Fn()>)
        }).collect());
        y += em(52.0);

        // The resolution waiting to be kept.
        if let Some(left) = d.trial_left(std::time::Instant::now()) {
            let ch = em(48.0);
            rect(ctx, x0, y, pw, ch, t.bg_surface(), 8.0);
            text(ctx, x0 + em(16.0), y + ch / 2.0, &format!("Keep this resolution? Going back in {left} s"), type_size::SM, Weight::Medium, t.text_primary());
            let (pg, pg2) = (self.disp.clone(), self.disp.clone());
            let bw = width("Go back", type_size::SM, Weight::Medium) + em(24.0);
            let kw = width("Keep", type_size::SM, Weight::Medium) + em(24.0);
            button(ctx, x0 + pw - bw - em(12.0), y + (ch - em(30.0)) / 2.0, "Go back", Look::Secondary, true, move || with_disp(&pg2, DispPage::revert_mode));
            button(ctx, x0 + pw - bw - kw - em(18.0), y + (ch - em(30.0)) / 2.0, "Keep", Look::Primary, true, move || with_disp(&pg, DispPage::keep_mode));
            y += ch + em(16.0);
        }

        if s.outputs.is_empty() {
            rect(ctx, x0, y, pw, 1.0, t.border_default(), 0.0);
            text(ctx, x0, y + em(24.0), "No monitor is connected (this session draws off-screen)", type_size::SM, Weight::Regular, t.text_tertiary());
        }
        let first = s.outputs.first().map(|o| o.output);
        for (i, o) in s.outputs.iter().enumerate() {
            rect(ctx, x0, y, pw, 1.0, t.border_default(), 0.0);
            y += em(12.0);
            icon(ctx, x0, y + em(4.0), ph::MONITOR, em(18.0), t.text_secondary());
            text(ctx, x0 + em(28.0), y + em(13.0), &disp::output_name(i, o), type_size::MD, Weight::Semibold, t.text_primary());
            let right = if !o.present {
                "Not connected".to_string()
            } else if let Some(m) = d.mirrors(o) {
                format!("Shows Display {}", s.outputs.iter().position(|x| x.output == m).map_or(0, |k| k + 1))
            } else {
                format!("{} × {} · scale {:.2}", o.rect.w, o.rect.h, o.scale_milli as f32 / 1000.0)
            };
            let rw = width(&right, type_size::SM, Weight::Regular);
            text(ctx, x0 + pw - rw, y + em(13.0), &right, type_size::SM, Weight::Regular, t.text_secondary());
            y += em(36.0);

            label(ctx, y, "UI size");
            let out = o.output;
            let def = disp::default_ui_size(o);
            choices(ctx, lx, y, fresco_protocol::UI_SIZES.iter().map(|&m| {
                let pg = self.disp.clone();
                let mut l = disp::ui_size_label(m);
                if m == def && m != 1000 {
                    l.push_str(" (default)");
                }
                (l, o.ui_size_milli == m, Box::new(move || with_disp(&pg, |p| p.set_ui_size(out, m))) as Box<dyn Fn()>)
            }).collect());
            y += em(42.0);

            // The resolution: forum-wm switches the main monitor's mode only.
            if i == 0 {
                label(ctx, y, "Resolution");
                let cur = d.current_mode().map(|m| disp::mode_label(&m)).unwrap_or_else(|| "unknown".into());
                text(ctx, lx, y + em(15.0), &cur, type_size::SM, Weight::Regular, t.text_primary());
                let cx = lx + width(&cur, type_size::SM, Weight::Regular) + em(16.0);
                let pg = self.disp.clone();
                button(ctx, cx, y, if d.picking_mode { "Close" } else { "Change…" }, Look::Ghost, d.trial.is_none(), move || with_disp(&pg, DispPage::toggle_modes));
                y += em(38.0);
                if d.picking_mode {
                    for m in d.mode_choices().into_iter().take(12) {
                        let pg = self.disp.clone();
                        let on = d.current_mode().is_some_and(|c| (c.width, c.height) == (m.width, m.height));
                        let mut l = disp::mode_label(&m);
                        if m.preferred {
                            l.push_str(" (native)");
                        }
                        area(ctx, Rect::new(lx, y, em(320.0), em(30.0)), 6.0, if on { t.state_hover() } else { Color::TRANSPARENT }, t.state_hover(), move || {
                            with_disp(&pg, |p| p.pick_mode(m, std::time::Instant::now()))
                        });
                        text(ctx, lx + em(12.0), y + em(15.0), &l, type_size::SM, if on { Weight::Semibold } else { Weight::Regular }, t.text_primary());
                        ctx.pop();
                        y += em(30.0);
                    }
                    y += em(8.0);
                }
            }

            // Where it sits, and mirroring, next to the first monitor.
            if let (Some(f), true) = (first, i > 0) {
                let main = &s.outputs[0];
                let mirroring = d.mirrors(o).is_some();
                label(ctx, y, "Place");
                let now = DispPage::placed(o, main);
                let mut items: Vec<(String, bool, Box<dyn Fn()>)> = Vec::new();
                if !mirroring {
                    for pl in Place::ALL {
                        let pg = self.disp.clone();
                        items.push((pl.label().to_string(), now == Some(pl), Box::new(move || with_disp(&pg, |p| p.place(out, pl, f)))));
                    }
                }
                let pg = self.disp.clone();
                if mirroring {
                    items.push(("Stop mirroring".to_string(), false, Box::new(move || with_disp(&pg, |p| p.mirror(out, None)))));
                } else {
                    items.push(("Mirror Display 1".to_string(), false, Box::new(move || with_disp(&pg, |p| p.mirror(out, Some(f))))));
                }
                choices(ctx, lx, y, items);
                if !mirroring {
                    text(ctx, lx, y + em(44.0), "of Display 1", type_size::XS, Weight::Regular, t.text_tertiary());
                    y += em(16.0);
                }
                y += em(42.0);
            }
            y += em(8.0);
        }

        if let Some(n) = &d.notice {
            let ny = h - em(44.0);
            rect(ctx, x0, ny, pw, em(32.0), t.state_hover(), 6.0);
            text(ctx, x0 + em(12.0), ny + em(16.0), n, type_size::SM, Weight::Medium, t.danger_fg());
        }
    }
}

pub fn with_devs(page: &Mutable<DevPage>, f: impl FnOnce(&mut DevPage)) {
    let mut p = page.get_cloned();
    f(&mut p);
    page.set(p);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::Got;
    use atrium_netconf::{Nearby, Profile, Status};
    use pergola::theme::Semantic;

    fn texts(v: &CuriaView) -> Vec<String> {
        let (tree, _) = pergola::view::render(v, Semantic::LIGHT);
        tree.iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.clone()) } else { None }).collect()
    }

    #[test]
    fn the_network_page_shows_status_networks_and_saved_ones() {
        let mut p = NetPage::new();
        p.take_wants();
        let cred = CredentialField::wifi_passphrase(Mutable::new(Default::default()));
        let v = CuriaView { which: Mutable::new(Which::Network), page: Mutable::new(p.clone()), devs: Mutable::new(DevPage::default()), dev_scroll: Mutable::new(Default::default()), disp: Mutable::new(DispPage::default()), cred, size: Mutable::new((960.0, 720.0)) };
        assert!(texts(&v).iter().any(|s| s == "Network settings are not available"));
        p.apply(Got::Status(Some(Status { link: Link::Wifi, online: Online::Unknown, metered: false, bars: Some(3), ssid: Some(b"home".to_vec()), portal: None })));
        p.apply(Got::Profiles(Ok(vec![Profile { ssid: b"home".to_vec(), open: false, wpa3_only: false, mac: "network".into(), auto: true, prio: 2, metered: false }])));
        p.apply(Got::Nearby(Ok(vec![Nearby { ssid: b"home".to_vec(), security: "wpa2".into(), bars: 3, known: true }, Nearby { ssid: b"cafe".to_vec(), security: "wpa3".into(), bars: 2, known: false }])));
        p.pick(b"cafe");
        v.page.set(p);
        let all = texts(&v);
        for want in ["Wi-Fi: home", "Disconnect", "Wi-Fi networks", "Connected · WPA2", "cafe", "WPA3", "Join", "Saved networks", "Auto-join: on", "Priority 2", "Forget"] {
            assert!(all.iter().any(|s| s == want), "{want} missing from {all:?}");
        }
        // The passphrase row is open under cafe: Arca's slot, never a field of ours.
        let (tree, _) = pergola::view::render(&v, Semantic::LIGHT);
        assert!(!tree.iter().any(|(_, n)| matches!(n, Node::Text { content, .. } if content == "Passphrase")));
        assert!(all.iter().any(|s| s == "Cancel"));
    }

    #[test]
    fn the_display_page_shows_appearance_text_size_and_monitors() {
        use crate::display::{Got, Snapshot};
        use fresco_protocol::{WmDisplayMode, WmOutput, WmRect};
        let cred = CredentialField::wifi_passphrase(Mutable::new(Default::default()));
        let v = CuriaView { which: Mutable::new(Which::Display), page: Mutable::new(NetPage::new()), devs: Mutable::new(DevPage::default()), dev_scroll: Mutable::new(Default::default()), disp: Mutable::new(DispPage::new()), cred, size: Mutable::new((960.0, 720.0)) };
        assert!(texts(&v).iter().any(|s| s == "Asking Forum for the display settings…"));
        with_disp(&v.disp, |p| p.apply(Got::State(Err("forum-wm is not answering".into()))));
        assert!(texts(&v).iter().any(|s| s == "forum-wm is not answering; trying again"));
        let o = |n: u32, x: i32| WmOutput { output: n, identity: 9 + n as u64, rect: WmRect { x, y: 0, w: 1920, h: 1080 }, refresh_mhz: 60000, present: true, scale_milli: 1000, auto_scale_milli: 1000, ui_size_milli: 1000, ui_size_set: false, internal: false, mirror_of: 0, width_mm: 0, height_mm: 0, flapping: false };
        with_disp(&v.disp, |p| p.apply(Got::State(Ok(Snapshot { dark: true, text_size: 1300, outputs: vec![o(0, 0), o(1, 1920)], modes: vec![WmDisplayMode { width: 1920, height: 1080, refresh_mhz: 60000, preferred: true }], current: 0 }))));
        with_disp(&v.disp, DispPage::toggle_modes);
        let all = texts(&v);
        for want in ["Display", "Appearance", "Light", "Dark", "Text size", "130%", "Display 1", "Display 2", "UI size", "Smaller", "Default", "150%", "Resolution", "1920 × 1080 · 60 Hz", "1920 × 1080 · 60 Hz (native)", "Place", "Right of", "Mirror Display 1"] {
            assert!(all.iter().any(|s| s == want), "{want} missing from {all:?}");
        }
    }

    #[test]
    fn the_devices_page_shows_nobodys_first() {
        use atrium_devices::{Device, Owner};
        let cred = CredentialField::wifi_passphrase(Mutable::new(Default::default()));
        let v = CuriaView { which: Mutable::new(Which::Devices), page: Mutable::new(NetPage::new()), devs: Mutable::new(DevPage::default()), dev_scroll: Mutable::new(Default::default()), disp: Mutable::new(DispPage::default()), cred, size: Mutable::new((960.0, 720.0)) };
        assert!(texts(&v).iter().any(|s| s == "Asking which devices this machine has…"));
        let d = |n: &str, ids: &str, owner| Device { name: n.into(), id: 1, bus: "usb".into(), parent: "uhub0".into(), ids: ids.into(), owner };
        with_devs(&v.devs, |p| p.apply(Ok(vec![
            d("uhub0", "", Owner::Kernel { driver: "uhub".into(), note: None }),
            d("ugen0.3:0", "intclass=0x06", Owner::Nobody { why: "no driver and no rule".into() }),
        ])));
        let all = texts(&v);
        for want in ["Devices", "Network", "2 devices, 1 not in use", "Not in use · 1", "Camera or phone (MTP/PTP)", "ugen0.3:0", "no driver and no rule", "FreeBSD drivers · 1", "Show the device FreeBSD drives"] {
            assert!(all.iter().any(|s| s == want), "{want} missing from {all:?}");
        }
    }
}
