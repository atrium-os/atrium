//! Drawing Curia (curia.md §2, §3) with Pergola: the sidebar of pages and
//! the Network page. Placed by hand, as Scrinium is; sizes in em so they grow
//! with the system text size.

use pergola::geom::Rect;
use pergola::node::Node;
use pergola::text_edit::TextEdit;
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::{phosphor as ph, Glyph, TextField};
use pergola::{Color, Mutable, NodeId, TextStyle};

use atrium_netconf::{Link, Online};

use crate::network::{headline, name, security_label, NetPage, MAC_POLICIES};

fn em(pt: f32) -> f32 {
    pergola::theme::size::em(pt)
}

pub fn side_w() -> f32 {
    em(200.0)
}

pub struct CuriaView {
    pub page: Mutable<NetPage>,
    /// The passphrase being typed (interim, curia.md §4).
    pub pass: Mutable<TextEdit>,
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
        // The sidebar: one page so far.
        rect(ctx, 0.0, 0.0, side_w(), h, t.bg_surface(), 0.0);
        text(ctx, em(20.0), em(30.0), "Settings", type_size::LG, Weight::Semibold, t.text_primary());
        let sel = Rect::new(em(10.0), em(56.0), side_w() - em(20.0), em(34.0));
        rect(ctx, sel.origin.x, sel.origin.y, sel.size.w, sel.size.h, t.state_hover(), 6.0);
        icon(ctx, em(20.0), em(63.0), ph::WIFI_HIGH, em(18.0), t.accent_fg());
        text(ctx, em(48.0), em(73.0), "Network", type_size::SM, Weight::Medium, t.text_primary());
        self.network(ctx, side_w() + em(32.0), w - side_w() - em(64.0), h);
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
                let pg = self.page.clone();
                let pass = self.pass.clone();
                let holder = ctx.push(Node::Rect { rect: Rect::new(fx, fy, fw, em(32.0)), fill: Color::TRANSPARENT, radius: 0.0 });
                TextField::new(self.pass.clone())
                    .placeholder("Passphrase")
                    .at(fx, fy)
                    .width(fw)
                    .on_submit(move |v| {
                        with(&pg, |p| p.join_typed(v));
                        pass.set(TextEdit::new("").secret(true));
                    })
                    .render(ctx);
                ctx.pop();
                if let Some(&bg) = ctx.tree.children_of(holder).get(2) {
                    ctx.autofocus(bg);
                }
                let (pg, pass) = (self.page.clone(), self.pass.clone());
                let typed = !self.pass.get_cloned().text().is_empty();
                let bx = fx + fw + em(12.0);
                let jw = button(ctx, bx, fy + em(1.0), "Join", Look::Primary, typed && p.joining.is_none(), move || {
                    let v = pass.get_cloned().text().to_string();
                    with(&pg, |p| p.join_typed(&v));
                    pass.set(TextEdit::new("").secret(true));
                });
                let (pg, pass) = (self.page.clone(), self.pass.clone());
                button(ctx, bx + jw + em(8.0), fy + em(1.0), "Cancel", Look::Ghost, true, move || {
                    with(&pg, NetPage::cancel_ask);
                    pass.set(TextEdit::new("").secret(true));
                });
                y += em(44.0);
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
        let v = CuriaView { page: Mutable::new(p.clone()), pass: Mutable::new(TextEdit::new("").secret(true)), size: Mutable::new((960.0, 720.0)) };
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
        assert!(all.iter().any(|s| s == "Passphrase"), "the passphrase row is open under cafe");
    }
}
