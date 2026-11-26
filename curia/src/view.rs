//! Drawing Curia (curia.md §2, §3) with Pergola: the sidebar of pages and
//! the Network page. Placed by hand, as Scrinium is; sizes in em so they grow
//! with the system text size.

use pergola::geom::Rect;
use pergola::layout::FlexStyle;
use pergola::node::Node;
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::{phosphor as ph, CredentialField, Glyph, ScrollState, ScrollView, VirtualList};
use pergola::{Color, Mutable, NodeId, TextStyle};

use atrium_netconf::{Link, Online};

use crate::apps::{self, AppsPage};
use crate::bluetooth::{self as bt, BtPage};
use vinculum_control::Prompt;
use crate::devices::{self, DevPage, Group, Row};
use crate::display::{self as disp, DispPage, Place};
use crate::sound::{self as snd, SoundPage};
use crate::network::{headline, manual_fields, name, security_label, Check, NetPage, EAP_METHODS, MAC_POLICIES};
use atrium_netconf::Target;
use pergola::text_edit::TextEdit;

pub(crate) fn em(pt: f32) -> f32 {
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
    Bluetooth,
    Display,
    Sound,
    Devices,
    Apps,
    Personas,
    Printers,
    Vpn,
    WorkspaceCredentials,
}

pub struct CuriaView {
    pub workspaces: Mutable<crate::workspace_access::Page>,
    pub workspace_scroll: Mutable<ScrollState>,
    pub which: Mutable<Which>,
    pub page: Mutable<NetPage>,
    pub devs: Mutable<DevPage>,
    pub dev_scroll: Mutable<ScrollState>,
    pub disp: Mutable<DispPage>,
    pub disp_scroll: Mutable<ScrollState>,
    pub sound: Mutable<SoundPage>,
    pub sound_scroll: Mutable<ScrollState>,
    pub apps: Mutable<AppsPage>,
    pub apps_scroll: Mutable<ScrollState>,
    pub bt: Mutable<BtPage>,
    pub bt_scroll: Mutable<ScrollState>,
    /// The code a device being paired shows, typed here.
    pub bt_code: Mutable<pergola::text_edit::TextEdit>,
    /// The Personas page's name field (Add a persona) and its scroll.
    pub persona_field: Mutable<TextEdit>,
    pub persona_scroll: Mutable<ScrollState>,
    /// The IP settings row's address, router and DNS fields, then the DNS
    /// servers row's field.
    /// Address, router, DNS of a fixed setting; own DNS servers; their TLS
    /// name; a work network's username, its server's domain and the
    /// anonymous identity.
    pub net_fields: [Mutable<TextEdit>; 8],
    pub net_scroll: Mutable<ScrollState>,
    /// Arca's Wi-Fi passphrase field (curia.md §4): Arca draws it and takes
    /// its keys; Curia only reserves its place and gets a handle.
    pub cred: CredentialField,
    pub size: Mutable<(f32, f32)>,
    /// The Printers page (printers.rs): its model, scroll, and the rename
    /// and add-by-address fields.
    pub printers: Mutable<crate::printers::PrintersPage>,
    pub printers_scroll: Mutable<ScrollState>,
    pub printer_fields: [Mutable<TextEdit>; 2],
    /// The VPN page (vpn.rs): its model, scroll, and the new-tunnel form.
    pub vpn: Mutable<crate::vpn::VpnPage>,
    pub vpn_scroll: Mutable<ScrollState>,
    pub vpn_fields: [Mutable<TextEdit>; 14],
    /// Arca's field for a VPN sign-in's password or code.
    pub vpn_cred: CredentialField,
}

pub fn with(page: &Mutable<NetPage>, f: impl FnOnce(&mut NetPage)) {
    let mut p = page.get_cloned();
    f(&mut p);
    page.set(p);
}

fn sans(size: f32, weight: Weight, color: Color) -> TextStyle {
    TextStyle { family: font::SANS.into(), size, weight, color }
}

pub(crate) fn text(ctx: &mut Ctx, x: f32, cy: f32, s: &str, size: f32, weight: Weight, color: Color) -> NodeId {
    let h = em(size) * 1.5;
    ctx.add(Node::Text { rect: Rect::new(x, cy - h / 2.0, 0.0, h), content: s.into(), style: sans(size, weight, color) })
}

pub(crate) fn width(s: &str, size: f32, weight: Weight) -> f32 {
    Node::measure_text(s, &sans(size, weight, Color::TRANSPARENT)).w
}

pub(crate) fn rect(ctx: &mut Ctx, x: f32, y: f32, w: f32, h: f32, fill: Color, radius: f32) -> NodeId {
    ctx.add(Node::Rect { rect: Rect::new(x, y, w, h), fill, radius })
}

pub(crate) fn icon(ctx: &mut Ctx, x: f32, y: f32, g: char, size: f32, color: Color) -> NodeId {
    Glyph::new(g).size(size).color(color).at(x, y).emit(ctx)
}

/// A pressable area pushed as the parent of what is drawn in it; the
/// caller pops it.
pub(crate) fn area(ctx: &mut Ctx, r: Rect, radius: f32, fill: Color, hover: Color, on: impl Fn() + 'static) -> NodeId {
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
pub(crate) enum Look {
    Ghost,
    Secondary,
    Primary,
    Danger,
}

/// A text button, 30 high at 100% text; returns its width.
pub(crate) fn button(ctx: &mut Ctx, x: f32, y: f32, label: &str, look: Look, enabled: bool, on: impl Fn() + 'static) -> f32 {
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
        ctx.min_size(pergola::geom::Size::new(side_w() + em(560.0), em(440.0)));
        rect(ctx, 0.0, 0.0, w, h, t.bg_canvas(), 0.0);
        // The sidebar.
        rect(ctx, 0.0, 0.0, side_w(), h, t.bg_surface(), 0.0);
        text(ctx, em(20.0), em(30.0), "Settings", type_size::LG, Weight::Semibold, t.text_primary());
        let now = self.which.get();
        for (i, (which, glyph, label)) in [(Which::Network, ph::WIFI_HIGH, "Network"), (Which::Bluetooth, ph::BLUETOOTH, "Bluetooth"), (Which::Display, ph::MONITOR, "Display"), (Which::Sound, ph::SPEAKER_HIGH, "Sound"), (Which::Devices, ph::USB, "Devices"), (Which::Apps, ph::SQUARES_FOUR, "Apps"), (Which::Personas, ph::USERS, "Personas"), (Which::Printers, ph::FILES, "Printers"), (Which::Vpn, ph::LOCK_SIMPLE, "VPN"), (Which::WorkspaceCredentials, ph::LOCK_SIMPLE, "Workspace access")].into_iter().enumerate() {
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
            Which::Bluetooth => self.bluetooth(ctx, side_w() + em(32.0), w - side_w() - em(64.0), h),
            Which::Display => self.display(ctx, side_w() + em(32.0), w - side_w() - em(64.0), h),
            Which::Sound => self.sound(ctx, side_w() + em(32.0), w - side_w() - em(64.0), h),
            Which::Devices => self.devices(ctx, side_w() + em(32.0), w - side_w() - em(64.0), h),
            Which::Apps => self.apps(ctx, side_w() + em(32.0), w - side_w() - em(64.0), h),
            Which::Personas => self.personas(ctx, side_w() + em(32.0), w - side_w() - em(64.0), h),
            Which::Printers => self.printers(ctx, side_w() + em(32.0), w - side_w() - em(64.0), h),
            Which::WorkspaceCredentials => self.workspace_credentials(ctx, side_w() + em(32.0), w - side_w() - em(64.0), h),
            Which::Vpn => self.vpn(ctx, side_w() + em(32.0), w - side_w() - em(64.0), h),
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
        let mut bx = x0 + pw - em(16.0);
        if s.is_some_and(|s| s.link == Link::Wifi) {
            let label = "Disconnect";
            let bw = width(label, type_size::SM, Weight::Medium) + em(24.0);
            bx -= bw;
            let pg = self.page.clone();
            button(ctx, bx, y + (ch - em(30.0)) / 2.0, label, Look::Secondary, true, move || with(&pg, NetPage::leave));
            bx -= em(8.0);
        }
        // The network's sign-in page (atrium-net.md §8.1): the thing to do
        // when the probe saw a portal, else offered quietly.
        if let Some(needed) = p.sign_in_offer() {
            let label = "Sign in to network";
            let bw = width(label, type_size::SM, Weight::Medium) + em(24.0);
            bx -= bw;
            let pg = self.page.clone();
            button(ctx, bx, y + (ch - em(30.0)) / 2.0, label, if needed { Look::Primary } else { Look::Ghost }, true, move || {
                let token = pergola::clipboard::current_activation();
                with(&pg, |p| p.sign_in(token))
            });
        }
        y += ch + em(24.0);
        if !p.reachable {
            return;
        }

        if p.trusting.is_some() {
            y = self.trust_prompt(ctx, &p, x0, pw, y);
        }

        // Wired 802.1X (wifi.md §7.5): a port whose switch asks to sign in.
        if let Some(i) = p.wired_port() {
            text(ctx, x0, y + em(10.0), "Wired network", type_size::SM, Weight::Semibold, t.text_secondary());
            y += em(28.0);
            let rh = em(44.0);
            icon(ctx, x0 + em(12.0), y + em(13.0), ph::PLUGS_CONNECTED, em(18.0), t.text_secondary());
            let port = format!("Ethernet ({})", i.name);
            text(ctx, x0 + em(44.0), y + rh / 2.0, &port, type_size::SM, Weight::Medium, t.text_primary());
            let lx = x0 + em(44.0) + width(&port, type_size::SM, Weight::Medium) + em(16.0);
            text(ctx, lx, y + rh / 2.0, &crate::network::wired_label(i).unwrap_or_default(), type_size::SM, Weight::Regular, t.text_secondary());
            let state = i.dot1x.as_deref().unwrap_or("");
            let open = p.asking.as_deref() == Some(crate::network::WIRED);
            let mut bx = x0 + pw - em(8.0);
            let by = y + (rh - em(30.0)) / 2.0;
            if state == "authorized" || state == "failed" || state == "running" {
                let label = "Forget sign-in";
                bx -= width(label, type_size::SM, Weight::Medium) + em(24.0);
                let pg = self.page.clone();
                button(ctx, bx, by, label, Look::Ghost, true, move || with(&pg, NetPage::forget_wired));
                bx -= em(8.0);
            }
            let label = if state == "needed" { "Sign in…" } else { "Change sign-in…" };
            bx -= width(label, type_size::SM, Weight::Medium) + em(24.0);
            let pg = self.page.clone();
            let look = if state == "needed" || state == "failed" { Look::Primary } else { Look::Secondary };
            button(ctx, bx, by, label, look, !open, move || with(&pg, NetPage::sign_in_wired));
            y += rh;
            if open && p.ent.is_some() {
                y = self.enterprise_sheet(ctx, &p, x0, pw, y);
            }
            y += em(20.0);
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
            if p.asking.as_deref() == Some(n.ssid.as_slice()) && p.ent.is_some() {
                y = self.enterprise_sheet(ctx, &p, x0, pw, y);
            } else if p.asking.as_deref() == Some(n.ssid.as_slice()) {
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
            // Interfaces, IP settings and DNS servers scroll: the rows that
            // open to type in can run past the window's foot.
            let top = y;
            let foot = if p.notice.is_some() { em(52.0) } else { em(8.0) };
            let content = pergola::view::Draw(|ctx: &mut Ctx| {
                let t = ctx.theme;
                let g = ctx.push(Node::stack(pergola::geom::Axis::Vertical, Rect::new(0.0, 0.0, pw, 0.0), 0.0));
                ctx.set_flex(g, FlexStyle { free: true, ..FlexStyle::default() });
                let mut y = em(4.0);
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
                    text(ctx, em(12.0), y + em(10.0), &line, type_size::SM, Weight::Regular, t.text_secondary());
                    y += em(22.0);
                    if !i.addrs6.is_empty() {
                        text(ctx, em(24.0), y + em(10.0), &format!("IPv6 {}", i.addrs6.join(", ")), type_size::SM, Weight::Regular, t.text_tertiary());
                        y += em(22.0);
                    }
                }
                y += em(12.0);
                y = self.ip_settings(ctx, &p, 0.0, pw, y) + em(8.0);
                if let Some(Node::Stack { rect, .. }) = ctx.tree.get_mut(g) {
                    rect.size.h = y;
                }
                ctx.pop();
            });
            ScrollView::new(self.net_scroll.clone(), Rect::new(x0, top, pw, (h - top - foot).max(em(60.0))), content)
                .key(96)
                .backdrop(t.bg_canvas())
                .emit(ctx);
        }

        // The system picker, over the window, while the sheet asks for a file.
        if p.pick.is_some() {
            let (w, h) = self.size.get();
            let id = ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: Color::TRANSPARENT, radius: 0.0 });
            ctx.slot(id, crate::vpn::PICK_SLOT);
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
    /// IP settings (curia.md §3): a fixed address for each saved network
    /// and wired port, and the machine's own DNS servers. Under "Show
    /// details", as most people never need them.
    ///
    /// A work or campus network (802.1X, wifi.md §7.4): username, Arca's
    /// password field, the sign-in method, and how its server is checked.
    fn enterprise_sheet(&self, ctx: &mut Ctx, p: &NetPage, x0: f32, pw: f32, mut y: f32) -> f32 {
        let t = ctx.theme;
        let Some(f) = &p.ent else { return y };
        let fx = x0 + em(44.0);
        let fw = ((pw - em(120.0)) / 2.0).clamp(em(150.0), em(260.0));
        let fy = y + em(4.0);
        let fields = (self.net_fields[5].clone(), self.net_fields[6].clone(), self.net_fields[7].clone());
        let join = {
            let (pg, fields) = (self.page.clone(), fields.clone());
            move || {
                let v = |m: &Mutable<TextEdit>| m.get_cloned().text().to_string();
                let (u, d, a) = (v(&fields.0), v(&fields.1), v(&fields.2));
                with(&pg, |p| p.ent_join_pressed(&u, &d, &a, std::time::Instant::now()));
            }
        };
        let submit = join.clone();
        pergola::widgets::TextField::new(self.net_fields[5].clone())
            .placeholder("Username (e.g. alice@corp.example)")
            .autofocus(true)
            .at(fx, fy)
            .width(fw)
            .on_submit(move |_| submit())
            .render(ctx);
        let rx = fx + fw + em(32.0);
        if f.tls() {
            // No password: a key made in Arca, certified by the network.
            let (pg, user) = (self.page.clone(), self.net_fields[5].clone());
            let label = if f.request.is_some() { "Make a new request" } else { "Make a certificate request" };
            button(ctx, rx, fy + em(1.0), label, Look::Secondary, p.joining.is_none(), move || {
                let u = user.get_cloned().text().to_string();
                with(&pg, |p| p.request_certificate(&u))
            });
        } else {
            // Arca draws its key badge left of the field's rect.
            self.cred.clone().at(rx, fy).size(fw, em(32.0)).render(ctx);
        }
        y = fy + em(40.0);
        let pg = self.page.clone();
        let mut bx = fx;
        bx += button(ctx, bx, y, &format!("Sign-in: {}", EAP_METHODS[f.method].1), Look::Secondary, true, move || with(&pg, NetPage::next_method)) + em(8.0);
        let check = match (f.check, &f.ca) {
            (Check::FirstJoin, _) => "Check server: ask on first join".to_string(),
            (Check::System, _) => "Check server: system certificates".to_string(),
            (Check::CaFile, Some((n, _))) => format!("Check server: CA {n}"),
            (Check::CaFile, None) => "Check server: CA file…".to_string(),
        };
        let pg = self.page.clone();
        bx += button(ctx, bx, y, &check, Look::Secondary, p.pick.is_none(), move || with(&pg, NetPage::next_check)) + em(8.0);
        pergola::widgets::TextField::new(self.net_fields[6].clone())
            .placeholder(if f.check == Check::FirstJoin { "Server's domain (optional)" } else { "Server's domain (needed)" })
            .at(bx, y)
            .width((rx + fw - bx).max(em(140.0)))
            .render(ctx);
        y += em(40.0);
        // Only the tunnelled methods have an outer identity of their own.
        if !f.tls() {
            pergola::widgets::TextField::new(self.net_fields[7].clone())
                .placeholder("Anonymous identity (optional, e.g. anonymous@corp.example)")
                .at(fx, y)
                .width(rx + fw - fx)
                .render(ctx);
            y += em(40.0);
        }
        if let (true, Some((_, pem))) = (f.tls(), &f.request) {
            let lines: Vec<&str> = pem.lines().collect();
            let mh = em(40.0) + em(13.0) * lines.len() as f32 + em(44.0);
            rect(ctx, fx, y, rx + fw - fx, mh, t.bg_surface(), 8.0);
            text(ctx, fx + em(12.0), y + em(18.0), "Give your IT team this certificate request, then import what they sign. The key stays in Arca.", type_size::SM, Weight::Regular, t.text_secondary());
            for (i, l) in lines.iter().enumerate() {
                text(ctx, fx + em(12.0), y + em(40.0) + em(13.0) * i as f32, l, type_size::XS, Weight::Regular, t.text_primary());
            }
            let by = y + mh - em(40.0);
            let pem2 = pem.clone();
            let mut bx = fx + em(12.0);
            bx += button(ctx, bx, by, "Copy the request", Look::Secondary, true, move || {
                let _ = pergola::clipboard::copy_text(&pem2, false);
            }) + em(8.0);
            let pg = self.page.clone();
            let label = match &f.cert {
                Some((n, _)) => format!("Signed certificate: {n}"),
                None => "Import the signed certificate…".to_string(),
            };
            button(ctx, bx, by, &label, if f.cert.is_some() { Look::Secondary } else { Look::Primary }, p.pick.is_none(), move || with(&pg, NetPage::import_certificate));
            y += mh + em(8.0);
        }
        let ready = p.joining.is_none() && p.armed.is_none() && p.pick.is_none() && if f.tls() { f.cert.is_some() } else { p.field_up };
        let verb = if p.asking.as_deref() == Some(crate::network::WIRED) { "Sign in" } else { "Join" };
        let jw = button(ctx, fx, y, verb, Look::Primary, ready, join);
        let pg = self.page.clone();
        button(ctx, fx + jw + em(8.0), y, "Cancel", Look::Ghost, true, move || with(&pg, NetPage::cancel_ask));
        y += em(40.0);
        if !p.field_up && !f.tls() {
            text(ctx, fx, y - em(2.0), "The password field is not available right now; trying again…", type_size::XS, Weight::Regular, t.text_tertiary());
            y += em(20.0);
        }
        y
    }

    /// A first join stopped at the server's certificate: what it is, and
    /// "Trust this server" (pinned for this network only).
    fn trust_prompt(&self, ctx: &mut Ctx, p: &NetPage, x0: f32, pw: f32, mut y: f32) -> f32 {
        let t = ctx.theme;
        let Some((ssid, seen)) = &p.trusting else { return y };
        let h = em(150.0);
        rect(ctx, x0, y, pw, h, t.bg_surface(), 8.0);
        let tx = x0 + em(16.0);
        text(ctx, tx, y + em(20.0), &format!("Trust the server of {}?", name(ssid)), type_size::MD, Weight::Semibold, t.text_primary());
        let names = if seen.names.is_empty() { seen.subject.clone() } else { seen.names.join(", ") };
        let lines = [
            format!("Server: {names}"),
            format!("Issued by {} · valid until {}", if seen.issuer.is_empty() { "?" } else { &seen.issuer }, if seen.not_after.is_empty() { "?" } else { &seen.not_after }),
            format!("SHA-256 {}", seen.sha256.iter().take(16).map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")),
            "Ask your network's IT team if this is their server. Your password is sent only once you trust it.".to_string(),
        ];
        for (i, l) in lines.iter().enumerate() {
            let c = if i == 3 { t.text_tertiary() } else { t.text_secondary() };
            text(ctx, tx, y + em(44.0 + 18.0 * i as f32), l, type_size::SM, Weight::Regular, c);
        }
        let by = y + h - em(40.0);
        let pg = self.page.clone();
        let w = button(ctx, tx, by, "Trust this server", Look::Primary, p.joining.is_none(), move || with(&pg, NetPage::trust));
        let pg = self.page.clone();
        button(ctx, tx + w + em(8.0), by, "Don't trust", Look::Ghost, true, move || with(&pg, NetPage::dont_trust));
        y += h + em(16.0);
        y
    }

    fn ip_settings(&self, ctx: &mut Ctx, p: &NetPage, x0: f32, pw: f32, mut y: f32) -> f32 {
        let t = ctx.theme;
        text(ctx, x0, y + em(10.0), "IP settings", type_size::SM, Weight::Semibold, t.text_secondary());
        y += em(28.0);
        let mut targets: Vec<(Target, String)> = p.profiles.iter().take(6).map(|pr| (Target::Wifi(pr.ssid.clone()), format!("{} (Wi-Fi)", name(&pr.ssid)))).collect();
        targets.extend(p.ifaces.iter().filter(|i| !i.wifi).map(|i| (Target::Wired(i.name.clone()), format!("{} (Ethernet)", i.name))));
        let rh = em(36.0);
        for (tg, label) in targets {
            let m = p.manual_of(&tg).cloned();
            text(ctx, x0 + em(12.0), y + rh / 2.0, &label, type_size::SM, Weight::Medium, t.text_primary());
            let now = match &m {
                Some(m) => format!("Fixed: {}/{}", m.addr, m.prefix),
                None => "Automatic (DHCP)".to_string(),
            };
            text(ctx, x0 + em(220.0), y + rh / 2.0, &now, type_size::SM, Weight::Regular, t.text_secondary());
            let open = p.ip_editing.as_ref() == Some(&tg);
            let label = if open { "Close" } else { "Change" };
            let bw = width(label, type_size::SM, Weight::Medium) + em(24.0);
            let (pg, f, vals, tg2) = (self.page.clone(), self.net_fields.clone(), manual_fields(m.as_ref()), tg.clone());
            button(ctx, x0 + pw - bw, y + (rh - em(30.0)) / 2.0, label, Look::Ghost, true, move || {
                for (i, v) in vals.iter().enumerate() {
                    f[i].set(TextEdit::new(v));
                }
                with(&pg, |p| p.edit_ip(tg2.clone()));
            });
            y += rh;
            if open {
                let fx = x0 + em(24.0);
                let fy = y + em(2.0);
                let widths = [em(170.0), em(130.0), em(190.0)];
                let holders = ["e.g. 192.168.1.20/24", "Router", "DNS servers (optional)"];
                let mut x = fx;
                for i in 0..3 {
                    let (pg, f) = (self.page.clone(), self.net_fields.clone());
                    pergola::widgets::TextField::new(self.net_fields[i].clone())
                        .placeholder(holders[i])
                        .autofocus(i == 0)
                        .at(x, fy)
                        .width(widths[i])
                        .on_submit(move |_| {
                            let v: Vec<String> = f.iter().take(3).map(|s| s.get_cloned().text().to_string()).collect();
                            with(&pg, |p| p.save_ip(false, &v[0], &v[1], &v[2]));
                        })
                        .render(ctx);
                    x += widths[i] + em(8.0);
                }
                let by = fy + em(40.0);
                let (pg, f) = (self.page.clone(), self.net_fields.clone());
                let mut bx = fx;
                bx += button(ctx, bx, by, "Use this address", Look::Primary, true, move || {
                    let v: Vec<String> = f.iter().take(3).map(|s| s.get_cloned().text().to_string()).collect();
                    with(&pg, |p| p.save_ip(false, &v[0], &v[1], &v[2]));
                }) + em(8.0);
                let pg = self.page.clone();
                bx += button(ctx, bx, by, "Automatic", Look::Secondary, m.is_some(), move || with(&pg, |p| p.save_ip(true, "", "", ""))) + em(8.0);
                let pg = self.page.clone();
                button(ctx, bx, by, "Cancel", Look::Ghost, true, move || with(&pg, NetPage::cancel_edit));
                y = by + em(38.0);
                if let Some(e) = &p.edit_error {
                    text(ctx, fx, y + em(6.0), e, type_size::XS, Weight::Regular, t.danger_fg());
                    y += em(22.0);
                }
            }
        }
        // The machine's own DNS servers.
        let now = match (&p.dns_tls, p.own_dns.is_empty()) {
            (_, true) => "Each network's own".to_string(),
            (Some(n), false) => format!("{}, encrypted ({n})", p.own_dns.join(", ")),
            (None, false) => p.own_dns.join(", "),
        };
        text(ctx, x0 + em(12.0), y + rh / 2.0, "DNS servers", type_size::SM, Weight::Medium, t.text_primary());
        text(ctx, x0 + em(220.0), y + rh / 2.0, &now, type_size::SM, Weight::Regular, t.text_secondary());
        let label = if p.dns_editing { "Close" } else { "Change" };
        let bw = width(label, type_size::SM, Weight::Medium) + em(24.0);
        let (pg, f, ft, cur, curt) = (self.page.clone(), self.net_fields[3].clone(), self.net_fields[4].clone(), p.own_dns.join(", "), p.dns_tls.clone().unwrap_or_default());
        button(ctx, x0 + pw - bw, y + (rh - em(30.0)) / 2.0, label, Look::Ghost, true, move || {
            f.set(TextEdit::new(&cur));
            ft.set(TextEdit::new(&curt));
            with(&pg, NetPage::edit_dns);
        });
        y += rh;
        if p.dns_editing {
            let fx = x0 + em(24.0);
            let fy = y + em(2.0);
            let fw = em(300.0);
            let pg = self.page.clone();
            pergola::widgets::TextField::new(self.net_fields[3].clone())
                .placeholder("e.g. 9.9.9.9, 2620:fe::fe (empty: each network's)")
                .autofocus(true)
                .at(fx, fy)
                .width(fw)
                .on_submit({
                    let ft = self.net_fields[4].clone();
                    move |v| {
                        let n = ft.get_cloned().text().to_string();
                        with(&pg, |p| p.save_dns(v, &n))
                    }
                })
                .render(ctx);
            // Encrypted DNS (DNS over TLS): the name on the servers'
            // certificate. Strict: no plain DNS if it doesn't match.
            let ty = fy + em(40.0);
            text(ctx, fx, ty + em(8.0), "Encrypted (DNS over TLS)", type_size::SM, Weight::Medium, t.text_primary());
            let ty = ty + em(22.0);
            let (pg, f) = (self.page.clone(), self.net_fields[3].clone());
            pergola::widgets::TextField::new(self.net_fields[4].clone())
                .placeholder("server name, e.g. dns.quad9.net (empty: off)")
                .at(fx, ty)
                .width(fw)
                .on_submit(move |n| {
                    let v = f.get_cloned().text().to_string();
                    with(&pg, |p| p.save_dns(&v, n))
                })
                .render(ctx);
            let (pg, f, ft) = (self.page.clone(), self.net_fields[3].clone(), self.net_fields[4].clone());
            let bx = fx + fw + em(8.0);
            let sw = button(ctx, bx, fy + em(1.0), "Save", Look::Primary, true, move || {
                let v = f.get_cloned().text().to_string();
                let n = ft.get_cloned().text().to_string();
                with(&pg, |p| p.save_dns(&v, &n));
            });
            let pg = self.page.clone();
            button(ctx, bx + sw + em(8.0), fy + em(1.0), "Cancel", Look::Ghost, true, move || with(&pg, NetPage::cancel_edit));
            y = ty + em(40.0);
            text(ctx, fx, y + em(6.0), "With a name, lookups go only to these servers over TLS. If their certificate doesn't match, nothing resolves.", type_size::XS, Weight::Regular, t.text_secondary());
            y += em(22.0);
            if let Some(e) = &p.edit_error {
                text(ctx, fx, y + em(6.0), e, type_size::XS, Weight::Regular, t.danger_fg());
                y += em(22.0);
            }
        }
        y
    }

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

        // The rest scrolls: at a large text or UI size, or on a small mode,
        // the monitors' rows run past the window.
        let foot = if d.notice.is_some() { em(52.0) } else { em(8.0) };
        let top = y;
        let content = pergola::view::Draw(|ctx: &mut Ctx| {
            let g = ctx.push(Node::stack(pergola::geom::Axis::Vertical, Rect::new(0.0, 0.0, pw, 0.0), 0.0));
            ctx.set_flex(g, FlexStyle { free: true, ..FlexStyle::default() });
            let x0 = 0.0;
            let mut y = em(4.0);
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
                            let on = d.current_mode().is_some_and(|c| (c.width, c.height, c.refresh_mhz) == (m.width, m.height, m.refresh_mhz));
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


            if let Some(Node::Stack { rect, .. }) = ctx.tree.get_mut(g) {
                rect.size.h = y;
            }
            ctx.pop();
        });
        ScrollView::new(self.disp_scroll.clone(), Rect::new(x0, top, pw, (h - top - foot).max(em(60.0))), content)
            .key(92)
            .backdrop(t.bg_canvas())
            .emit(ctx);

        if let Some(n) = &d.notice {
            let ny = h - em(44.0);
            rect(ctx, x0, ny, pw, em(32.0), t.state_hover(), 6.0);
            text(ctx, x0 + em(12.0), ny + em(16.0), n, type_size::SM, Weight::Medium, t.danger_fg());
        }
    }
}

pub fn with_sound(page: &Mutable<SoundPage>, f: impl FnOnce(&mut SoundPage)) {
    let mut p = page.get_cloned();
    f(&mut p);
    page.set(p);
}

/// A level slider (0 to 1) that tells `f` each move.
fn level(ctx: &mut Ctx, x: f32, y: f32, w: f32, v: f32, enabled: bool, f: impl Fn(f32) + 'static) {
    pergola::widgets::Slider::new(Mutable::new(v as f64), 0.0, 1.0)
        .step(0.01)
        .at(x, y)
        .width(w)
        .enabled(enabled)
        .on_change(move |v| f(v as f32))
        .render(ctx);
}

impl CuriaView {
    /// The Sound page (curia.md §12): the volume, each app playing, and the
    /// outputs, over choragusd's volume-control socket.
    fn sound(&self, ctx: &mut Ctx, x0: f32, pw: f32, h: f32) {
        let p = self.sound.get_cloned();
        let t = ctx.theme;
        let mut y = em(20.0);
        text(ctx, x0, y + em(16.0), "Sound", type_size::XL, Weight::Semibold, t.text_primary());
        y += em(48.0);
        match &p.link {
            Some(Err(why)) => {
                let ch = em(64.0);
                rect(ctx, x0, y, pw, ch, t.bg_surface(), 8.0);
                icon(ctx, x0 + em(16.0), y + em(20.0), ph::SPEAKER_X, em(24.0), t.text_secondary());
                text(ctx, x0 + em(56.0), y + em(24.0), "Sound settings are not available", type_size::MD, Weight::Medium, t.text_primary());
                text(ctx, x0 + em(56.0), y + em(44.0), &format!("{why}; trying again"), type_size::SM, Weight::Regular, t.text_secondary());
                return;
            }
            _ if !p.ready() => {
                text(ctx, x0, y + em(10.0), "Asking choragusd for the sound settings…", type_size::SM, Weight::Regular, t.text_tertiary());
                return;
            }
            _ => {}
        }
        let top = y;
        let sw = (pw - em(300.0)).clamp(em(120.0), em(360.0));
        let content = pergola::view::Draw(|ctx: &mut Ctx| {
            let t = ctx.theme;
            let g = ctx.push(Node::stack(pergola::geom::Axis::Vertical, Rect::new(0.0, 0.0, pw, 0.0), 0.0));
            ctx.set_flex(g, FlexStyle { free: true, ..FlexStyle::default() });
            let x0 = 0.0;
            let lx = x0 + em(140.0);
            let mut y = em(4.0);
            let label = |ctx: &mut Ctx, y: f32, l: &str| {
                text(ctx, x0, y + em(15.0), l, type_size::SM, Weight::Medium, ctx.theme.text_secondary());
            };
            fn choices(ctx: &mut Ctx, x: f32, y: f32, items: Vec<(String, bool, Box<dyn Fn()>)>) {
                let mut x = x;
                for (l, on, f) in items {
                    x += button(ctx, x, y, &l, if on { Look::Primary } else { Look::Secondary }, true, f) + em(6.0);
                }
            }

            // The volume.
            label(ctx, y, "Volume");
            let fixed = p.master_fixed();
            let pg = self.sound.clone();
            level(ctx, lx, y + em(7.0), sw, p.master, !fixed, move |v| with_sound(&pg, |s| s.set_master(v)));
            let pct = if fixed { "Fixed".to_string() } else { snd::percent(p.master) };
            text(ctx, lx + sw + em(12.0), y + em(15.0), &pct, type_size::SM, Weight::Regular, t.text_secondary());
            let pg = self.sound.clone();
            button(ctx, lx + sw + em(110.0), y, if p.master_muted { "Unmute" } else { "Mute" }, if p.master_muted { Look::Primary } else { Look::Secondary }, true, move || with_sound(&pg, SoundPage::toggle_master_mute));
            y += em(48.0);

            // The outputs.
            label(ctx, y, "Output");
            if p.outputs.is_empty() {
                text(ctx, lx, y + em(15.0), "No output device", type_size::SM, Weight::Regular, t.text_tertiary());
                y += em(42.0);
            } else {
                choices(ctx, lx, y, p.outputs.iter().map(|o| {
                    let (pg, id) = (self.sound.clone(), o.id);
                    (o.name.clone(), o.id == p.current, Box::new(move || with_sound(&pg, |s| s.pick_output(id))) as Box<dyn Fn()>)
                }).collect());
                y += em(42.0);
            }
            if let Some(o) = p.current_output() {
                label(ctx, y, "Balance");
                text(ctx, lx, y + em(15.0), "Left", type_size::XS, Weight::Regular, t.text_tertiary());
                let bx = lx + em(36.0);
                let pg = self.sound.clone();
                pergola::widgets::Slider::new(Mutable::new(o.balance as f64), -1.0, 1.0)
                    .step(0.05)
                    .at(bx, y + em(7.0))
                    .width(sw - em(72.0))
                    .on_change(move |v| with_sound(&pg, |s| s.set_balance(v as f32)))
                    .render(ctx);
                text(ctx, bx + sw - em(72.0) + em(8.0), y + em(15.0), "Right", type_size::XS, Weight::Regular, t.text_tertiary());
                if o.balance != 0.0 {
                    let pg = self.sound.clone();
                    button(ctx, lx + sw + em(12.0), y, "Centre", Look::Ghost, true, move || with_sound(&pg, |s| s.set_balance(0.0)));
                }
                y += em(42.0);
                label(ctx, y, "Speakers");
                choices(ctx, lx, y, snd::LAYOUTS.iter().map(|&(c, l)| {
                    let pg = self.sound.clone();
                    (l.to_string(), o.channels == c, Box::new(move || with_sound(&pg, |s| s.set_layout(c))) as Box<dyn Fn()>)
                }).collect());
                y += em(42.0);
                if o.channels == 2 {
                    label(ctx, y, "Headphone surround");
                    let pg = self.sound.clone();
                    choices(ctx, lx, y, vec![
                        ("Off".to_string(), !o.spatial, Box::new({ let pg = pg.clone(); move || with_sound(&pg, |s| if s.current_output().is_some_and(|o| o.spatial) { s.toggle_spatial() }) }) as Box<dyn Fn()>),
                        ("On".to_string(), o.spatial, Box::new(move || with_sound(&pg, |s| if s.current_output().is_some_and(|o| !o.spatial) { s.toggle_spatial() })) as Box<dyn Fn()>),
                    ]);
                    y += em(42.0);
                }
                label(ctx, y, "Volume control");
                choices(ctx, lx, y, SoundPage::volume_modes(o).into_iter().map(|m| {
                    let pg = self.sound.clone();
                    (snd::volume_mode_label(m).to_string(), o.volume_setting == m, Box::new(move || with_sound(&pg, |s| s.set_volume_mode(m))) as Box<dyn Fn()>)
                }).collect());
                y += em(52.0);
            }

            // The apps playing now.
            rect(ctx, x0, y, pw, 1.0, t.border_default(), 0.0);
            y += em(12.0);
            text(ctx, x0, y + em(10.0), "Apps", type_size::SM, Weight::Semibold, t.text_secondary());
            y += em(30.0);
            if p.apps.is_empty() {
                text(ctx, x0, y + em(12.0), "No app is playing sound. Apps show here while they play.", type_size::SM, Weight::Regular, t.text_tertiary());
                y += em(32.0);
            }
            let surround = p.current_output().is_some_and(|o| o.channels == 2 && o.spatial);
            for a in &p.apps {
                text(ctx, x0, y + em(15.0), &snd::app_name(a), type_size::SM, Weight::Medium, t.text_primary());
                let id = a.app_id.clone();
                let pg = self.sound.clone();
                level(ctx, lx + em(60.0), y + em(7.0), sw - em(60.0), a.volume, true, move |v| with_sound(&pg, |s| s.set_app(&id, v)));
                text(ctx, lx + sw + em(12.0), y + em(15.0), &snd::percent(a.volume), type_size::SM, Weight::Regular, t.text_secondary());
                let (id, pg) = (a.app_id.clone(), self.sound.clone());
                let mut bx = lx + sw + em(60.0);
                bx += button(ctx, bx, y, if a.muted { "Unmute" } else { "Mute" }, if a.muted { Look::Primary } else { Look::Ghost }, true, move || with_sound(&pg, |s| s.toggle_app_mute(&id))) + em(6.0);
                if surround {
                    let (id, pg) = (a.app_id.clone(), self.sound.clone());
                    button(ctx, bx, y, if a.spatial { "Surround: on" } else { "Surround: off" }, Look::Ghost, true, move || with_sound(&pg, |s| s.toggle_app_spatial(&id)));
                }
                y += em(40.0);
            }
            if let Some(Node::Stack { rect, .. }) = ctx.tree.get_mut(g) {
                rect.size.h = y;
            }
            ctx.pop();
        });
        ScrollView::new(self.sound_scroll.clone(), Rect::new(x0, top, pw, (h - top - em(8.0)).max(em(60.0))), content)
            .key(93)
            .backdrop(t.bg_canvas())
            .emit(ctx);
    }
}

pub fn with_apps(page: &Mutable<AppsPage>, f: impl FnOnce(&mut AppsPage)) {
    let mut p = page.get_cloned();
    f(&mut p);
    page.set(p);
}

impl CuriaView {
    /// The Apps page (curia.md §13): each installed app, who allowed it,
    /// and what it asks for when opened up.
    fn apps(&self, ctx: &mut Ctx, x0: f32, pw: f32, h: f32) {
        let p = self.apps.get_cloned();
        let t = ctx.theme;
        let mut y = em(20.0);
        // An app's own settings (curia.md §15): its pane, in the page.
        if let Some(pane) = &p.pane {
            let pg = self.apps.clone();
            button(ctx, x0, y + em(2.0), "Back to Apps", Look::Ghost, true, move || with_apps(&pg, AppsPage::close_settings));
            y += em(44.0);
            text(ctx, x0, y + em(16.0), &pane.name, type_size::XL, Weight::Semibold, t.text_primary());
            y += em(40.0);
            text(ctx, x0, y + em(8.0), &format!("Its own settings, for {}", apps::persona_name(&pane.persona)), type_size::SM, Weight::Regular, t.text_secondary());
            y += em(28.0);
            if !pane.up {
                text(ctx, x0, y + em(10.0), "Opening…", type_size::SM, Weight::Regular, t.text_tertiary());
            }
            let id = ctx.add(Node::Rect { rect: Rect::new(x0, y, apps::PANE_W.min(pw), apps::PANE_H.min((h - y).max(0.0))), fill: Color::TRANSPARENT, radius: 0.0 });
            ctx.slot(id, apps::PANE_SLOT);
            return;
        }
        text(ctx, x0, y + em(16.0), "Apps", type_size::XL, Weight::Semibold, t.text_primary());
        y += em(48.0);
        match &p.list {
            None => {
                text(ctx, x0, y + em(10.0), "Asking which apps are installed…", type_size::SM, Weight::Regular, t.text_tertiary());
                return;
            }
            Some(Err(why)) => {
                let ch = em(64.0);
                rect(ctx, x0, y, pw, ch, t.bg_surface(), 8.0);
                icon(ctx, x0 + em(16.0), y + em(20.0), ph::SQUARES_FOUR, em(24.0), t.text_secondary());
                text(ctx, x0 + em(56.0), y + em(24.0), "The apps' settings are not available", type_size::MD, Weight::Medium, t.text_primary());
                text(ctx, x0 + em(56.0), y + em(44.0), &format!("{why}; trying again"), type_size::SM, Weight::Regular, t.text_secondary());
                return;
            }
            Some(Ok(_)) => {}
        }
        text(ctx, x0, y + em(8.0), &p.summary(), type_size::SM, Weight::Regular, t.text_secondary());
        y += em(28.0);
        let top = y;
        let content = pergola::view::Draw(|ctx: &mut Ctx| {
            let t = ctx.theme;
            let g = ctx.push(Node::stack(pergola::geom::Axis::Vertical, Rect::new(0.0, 0.0, pw, 0.0), 0.0));
            ctx.set_flex(g, FlexStyle { free: true, ..FlexStyle::default() });
            let mut y = 0.0;
            for e in p.apps() {
                let open = p.open.as_deref() == Some(e.id.as_str());
                let busy = p.busy.as_deref() == Some(e.id.as_str());
                let rh = em(56.0);
                // The row: name and status; pressing it opens the details.
                let (pg, id) = (self.apps.clone(), e.id.clone());
                area(ctx, Rect::new(0.0, y, pw, rh), 8.0, t.bg_surface(), t.state_hover(), move || with_apps(&pg, |a| a.toggle_open(&id)));
                icon(ctx, em(14.0), y + em(19.0), if open { ph::CARET_DOWN } else { ph::CARET_RIGHT }, em(16.0), t.text_secondary());
                text(ctx, em(40.0), y + em(20.0), &e.name, type_size::MD, Weight::Medium, t.text_primary());
                text(ctx, em(40.0), y + em(40.0), &apps::status(e), type_size::SM, Weight::Regular, t.text_secondary());
                ctx.pop();
                // Its buttons, from the right.
                let mut bx = pw - em(12.0);
                let by = y + (rh - em(30.0)) / 2.0;
                let confirming = p.confirm.as_deref() == Some(e.id.as_str());
                if apps::can_remove(e) && !confirming {
                    let l = "Remove access";
                    bx -= width(l, type_size::SM, Weight::Medium) + em(24.0);
                    let (pg, id) = (self.apps.clone(), e.id.clone());
                    button(ctx, bx, by, l, Look::Secondary, !busy && p.busy.is_none(), move || with_apps(&pg, |a| a.remove(&id)));
                    bx -= em(8.0);
                }
                if e.settings {
                    let l = "Settings";
                    bx -= width(l, type_size::SM, Weight::Medium) + em(24.0);
                    let (pg, id) = (self.apps.clone(), e.id.clone());
                    button(ctx, bx, by, l, Look::Secondary, true, move || with_apps(&pg, |a| a.open_settings(&id)));
                    bx -= em(8.0);
                }
                if apps::can_allow(e) {
                    let l = if busy { "Allowing…" } else { "Allow" };
                    bx -= width(l, type_size::SM, Weight::Medium) + em(24.0);
                    let (pg, id) = (self.apps.clone(), e.id.clone());
                    // The press itself goes with the ask: portcullisd spends it
                    // with frescod, so only a real click or Enter here allows.
                    button(ctx, bx, by, l, Look::Primary, p.busy.is_none(), move || {
                        let token = pergola::clipboard::current_activation();
                        with_apps(&pg, |a| a.allow(&id, token))
                    });
                }
                y += rh + em(4.0);
                if confirming {
                    let msg = format!("Remove what you allowed {}? It asks again when it next opens.", e.name);
                    text(ctx, em(40.0), y + em(15.0), &msg, type_size::SM, Weight::Regular, t.text_primary());
                    let mut bx = pw - em(12.0);
                    let l = "Cancel";
                    bx -= width(l, type_size::SM, Weight::Medium) + em(24.0);
                    let pg = self.apps.clone();
                    button(ctx, bx, y, l, Look::Ghost, true, move || with_apps(&pg, AppsPage::cancel_remove));
                    let l = "Remove";
                    bx -= width(l, type_size::SM, Weight::Medium) + em(24.0) + em(6.0);
                    let (pg, id) = (self.apps.clone(), e.id.clone());
                    button(ctx, bx, y, l, Look::Danger, true, move || with_apps(&pg, |a| a.remove(&id)));
                    y += em(38.0);
                }
                // Settings asks which persona's first.
                if p.pane_pick.as_deref() == Some(e.id.as_str()) {
                    text(ctx, em(40.0), y + em(15.0), "Open its settings for", type_size::SM, Weight::Regular, t.text_primary());
                    let mut bx = em(40.0) + width("Open its settings for", type_size::SM, Weight::Regular) + em(12.0);
                    for persona in std::iter::once("personal".to_string()).chain(p.named().iter().map(|(n, _)| n.clone())) {
                        let l = apps::persona_name(&persona);
                        let (pg, id) = (self.apps.clone(), e.id.clone());
                        button(ctx, bx, y, &l, Look::Secondary, true, move || with_apps(&pg, |a| a.settings_in(&id, &persona)));
                        bx += width(&l, type_size::SM, Weight::Medium) + em(24.0) + em(8.0);
                    }
                    y += em(38.0);
                }
                if let Some((_, why)) = p.pane_note.as_ref().filter(|(a, _)| *a == e.id) {
                    text(ctx, em(40.0), y + em(12.0), why, type_size::SM, Weight::Regular, t.danger_fg());
                    y += em(28.0);
                }
                if let Some((_, why)) = p.note.as_ref().filter(|(a, _)| *a == e.id) {
                    text(ctx, em(40.0), y + em(12.0), why, type_size::SM, Weight::Regular, t.danger_fg());
                    y += em(28.0);
                }
                if open {
                    let line = |ctx: &mut Ctx, y: &mut f32, s: &str, c: Color| {
                        text(ctx, em(40.0), *y + em(11.0), s, type_size::SM, Weight::Regular, c);
                        *y += em(24.0);
                    };
                    y += em(4.0);
                    if e.asks.is_empty() {
                        line(ctx, &mut y, "It asks for nothing beyond its own window.", t.text_secondary());
                    } else {
                        text(ctx, em(40.0), y + em(11.0), "It asks to", type_size::SM, Weight::Semibold, t.text_secondary());
                        y += em(24.0);
                        for a in &e.asks {
                            let waiting = e.by.is_none() && e.missing.contains(a);
                            line(ctx, &mut y, &format!("•  {a}{}", if waiting { " (not allowed yet)" } else { "" }), t.text_primary());
                        }
                    }
                    if e.missing.first().is_some_and(|m| m.starts_with("Manifest has changed")) {
                        line(ctx, &mut y, "It changed since it was allowed, so it asks again.", t.text_secondary());
                    }
                    if !e.restricted.is_empty() {
                        line(ctx, &mut y, &format!("Only the system can allow {}.", e.restricted.join(", ")), t.text_secondary());
                    }
                    // Running in the background (background-apps.md §3.2):
                    // a switch per persona, off until the person turns it on.
                    if e.background.is_some() {
                        y += em(8.0);
                        let named = p.named();
                        for persona in std::iter::once("personal".to_string()).chain(named.iter().map(|(n, _)| n.clone())) {
                            let Some(words) = apps::background_words(e, &persona, !named.is_empty()) else { break };
                            let on = apps::background_on(e, &persona);
                            text(ctx, em(40.0), y + em(16.0), &words, type_size::SM, Weight::Regular, t.text_primary());
                            let state = match (busy, on) {
                                (true, _) => "Changing…",
                                (false, true) => "On",
                                (false, false) if e.background.as_ref().is_some_and(|b| b.stale) => "Off: it changed since you allowed it",
                                (false, false) => "Off",
                            };
                            text(ctx, em(40.0), y + em(32.0), state, type_size::XS, Weight::Regular, t.text_secondary());
                            let lab = if on { "Turn off" } else { "Turn on" };
                            let bx = pw - em(12.0) - width(lab, type_size::SM, Weight::Medium) - em(24.0);
                            let (pg, id) = (self.apps.clone(), e.id.clone());
                            button(ctx, bx, y + em(5.0), lab, Look::Secondary, p.busy.is_none(), move || {
                                let token = pergola::clipboard::current_activation();
                                with_apps(&pg, |a| a.set_background(&id, &persona, !on, token))
                            });
                            y += em(40.0);
                        }
                    }
                    line(ctx, &mut y, &e.id, t.text_tertiary());
                    y += em(8.0);
                }
            }
            // Links (opening-links.md §4.4): which app opens each scheme.
            y += em(20.0);
            text(ctx, 0.0, y + em(14.0), "Links", type_size::LG, Weight::Semibold, t.text_primary());
            y += em(32.0);
            match &p.links {
                None => {
                    text(ctx, 0.0, y + em(10.0), "Asking which apps open links…", type_size::SM, Weight::Regular, t.text_tertiary());
                    y += em(28.0);
                }
                Some(Err(why)) => {
                    text(ctx, 0.0, y + em(10.0), &format!("Links' apps are not available: {why}"), type_size::SM, Weight::Regular, t.text_secondary());
                    y += em(28.0);
                }
                Some(Ok(v)) if v.is_empty() => {
                    text(ctx, 0.0, y + em(10.0), "No app here opens links.", type_size::SM, Weight::Regular, t.text_tertiary());
                    y += em(28.0);
                }
                Some(Ok(_)) => {}
            }
            for l in p.schemes() {
                let open = p.link_open.as_deref() == Some(l.scheme.as_str());
                let busy = p.link_busy.as_deref() == Some(l.scheme.as_str());
                let rh = em(56.0);
                let (pg, sc) = (self.apps.clone(), l.scheme.clone());
                area(ctx, Rect::new(0.0, y, pw, rh), 8.0, t.bg_surface(), t.state_hover(), move || with_apps(&pg, |a| a.toggle_link(&sc)));
                icon(ctx, em(14.0), y + em(19.0), if open { ph::CARET_DOWN } else { ph::CARET_RIGHT }, em(16.0), t.text_secondary());
                text(ctx, em(40.0), y + em(20.0), &format!("{} links", l.scheme), type_size::MD, Weight::Medium, t.text_primary());
                text(ctx, em(40.0), y + em(40.0), &if busy { "Changing…".to_string() } else { apps::link_status(l) }, type_size::SM, Weight::Regular, t.text_secondary());
                ctx.pop();
                y += rh + em(4.0);
                if let Some((_, why)) = p.link_note.as_ref().filter(|(s, _)| *s == l.scheme) {
                    text(ctx, em(40.0), y + em(12.0), why, type_size::SM, Weight::Regular, t.danger_fg());
                    y += em(28.0);
                }
                if open {
                    // Each app that takes it, and Ask each time; the press
                    // goes with the ask, as Allow's does.
                    let choices: Vec<(Option<String>, String, bool)> = l.apps.iter()
                        .map(|c| (Some(c.id.clone()), c.name.clone(), l.yours && c.default))
                        .chain([(None, "Ask each time".to_string(), !l.yours && !l.apps.iter().any(|c| c.default))])
                        .collect();
                    for (app, name, now) in choices {
                        let bh = em(36.0);
                        text(ctx, em(40.0), y + em(18.0), &name, type_size::SM, Weight::Regular, t.text_primary());
                        if now {
                            let l2 = "In use";
                            text(ctx, pw - em(12.0) - width(l2, type_size::SM, Weight::Medium) - em(12.0), y + em(18.0), l2, type_size::SM, Weight::Medium, t.text_secondary());
                        } else {
                            let lab = if app.is_some() { "Use" } else { "Ask" };
                            let bx = pw - em(12.0) - width(lab, type_size::SM, Weight::Medium) - em(24.0);
                            let (pg, sc) = (self.apps.clone(), l.scheme.clone());
                            button(ctx, bx, y + em(3.0), lab, Look::Secondary, p.link_busy.is_none(), move || {
                                let token = pergola::clipboard::current_activation();
                                with_apps(&pg, |a| a.set_link(&sc, app.as_deref(), token))
                            });
                        }
                        y += bh;
                    }
                    y += em(8.0);
                }
            }
            // Share (share-sheet.md §4.4): the apps the share sheet
            // offers, in its order; Hide and Move up, each on a press.
            y += em(20.0);
            text(ctx, 0.0, y + em(14.0), "Share", type_size::LG, Weight::Semibold, t.text_primary());
            y += em(32.0);
            match &p.share {
                None => {
                    text(ctx, 0.0, y + em(10.0), "Asking which apps take shares…", type_size::SM, Weight::Regular, t.text_tertiary());
                    y += em(28.0);
                }
                Some(Err(why)) => {
                    text(ctx, 0.0, y + em(10.0), &format!("The share sheet's apps are not available: {why}"), type_size::SM, Weight::Regular, t.text_secondary());
                    y += em(28.0);
                }
                Some(Ok(v)) if v.is_empty() => {
                    text(ctx, 0.0, y + em(10.0), "No app here takes shares.", type_size::SM, Weight::Regular, t.text_tertiary());
                    y += em(28.0);
                }
                Some(Ok(_)) => {
                    text(ctx, 0.0, y + em(10.0), "The share sheet lists these apps in this order.", type_size::SM, Weight::Regular, t.text_secondary());
                    y += em(28.0);
                }
            }
            if let Some(why) = &p.share_note {
                text(ctx, 0.0, y + em(10.0), why, type_size::SM, Weight::Regular, t.danger_fg());
                y += em(28.0);
            }
            for (i, r) in p.share_targets().iter().enumerate() {
                let rh = em(48.0);
                rect(ctx, 0.0, y, pw, rh, t.bg_surface(), 8.0);
                let fg = if r.hidden { t.text_tertiary() } else { t.text_primary() };
                text(ctx, em(16.0), y + em(18.0), &r.name, type_size::MD, Weight::Medium, fg);
                text(ctx, em(16.0), y + em(36.0), &if r.hidden { "Not on the share sheet".to_string() } else { r.id.clone() }, type_size::XS, Weight::Regular, t.text_tertiary());
                let lab = if r.hidden { "Show" } else { "Hide" };
                let bx = pw - em(12.0) - width(lab, type_size::SM, Weight::Medium) - em(24.0);
                let (pg, id, hide) = (self.apps.clone(), r.id.clone(), !r.hidden);
                button(ctx, bx, y + em(9.0), lab, Look::Secondary, !p.share_busy, move || {
                    let token = pergola::clipboard::current_activation();
                    with_apps(&pg, |a| a.share_hide(&id, hide, token))
                });
                if i > 0 {
                    let up = "Move up";
                    let ux = bx - em(8.0) - width(up, type_size::SM, Weight::Medium) - em(24.0);
                    let (pg, id) = (self.apps.clone(), r.id.clone());
                    button(ctx, ux, y + em(9.0), up, Look::Secondary, !p.share_busy, move || {
                        let token = pergola::clipboard::current_activation();
                        with_apps(&pg, |a| a.share_up(&id, token))
                    });
                }
                y += rh + em(4.0);
            }
            if let Some(Node::Stack { rect, .. }) = ctx.tree.get_mut(g) {
                rect.size.h = y;
            }
            ctx.pop();
        });
        ScrollView::new(self.apps_scroll.clone(), Rect::new(x0, top, pw, (h - top - em(8.0)).max(em(60.0))), content)
            .key(94)
            .backdrop(t.bg_canvas())
            .emit(ctx);
    }

    /// The Personas page (personas.md §2, §6): Personal and the named
    /// personas with their colours, Add a persona, Remove (which asks
    /// twice, then Sigillum's sheet), and the switches between personas.
    fn personas(&self, ctx: &mut Ctx, x0: f32, pw: f32, h: f32) {
        let p = self.apps.get_cloned();
        let t = ctx.theme;
        let mut y = em(20.0);
        text(ctx, x0, y + em(16.0), "Personas", type_size::XL, Weight::Semibold, t.text_primary());
        y += em(48.0);
        match &p.crossings {
            None => {
                text(ctx, x0, y + em(10.0), "Asking for your personas…", type_size::SM, Weight::Regular, t.text_tertiary());
                return;
            }
            Some(Err(why)) => {
                let ch = em(64.0);
                rect(ctx, x0, y, pw, ch, t.bg_surface(), 8.0);
                icon(ctx, x0 + em(16.0), y + em(20.0), ph::USERS, em(24.0), t.text_secondary());
                text(ctx, x0 + em(56.0), y + em(24.0), "Personas are not available", type_size::MD, Weight::Medium, t.text_primary());
                text(ctx, x0 + em(56.0), y + em(44.0), &format!("{why}; trying again"), type_size::SM, Weight::Regular, t.text_secondary());
                return;
            }
            Some(Ok(_)) => {}
        }
        text(ctx, x0, y + em(8.0), "Each persona's apps keep their own data, accounts and network.", type_size::SM, Weight::Regular, t.text_secondary());
        y += em(28.0);
        let top = y;
        let swatch = |ctx: &mut Ctx, x: f32, y: f32, d: f32, colour: Option<&str>| {
            match colour.and_then(fresco_protocol::persona::rgb) {
                Some([r, g, b]) => { rect(ctx, x, y, d, d, Color::from_hex(&format!("#{r:02x}{g:02x}{b:02x}")), d / 2.0); }
                None => {
                    rect(ctx, x, y, d, d, ctx.theme.border_strong(), d / 2.0);
                    let k = em(2.0);
                    rect(ctx, x + k, y + k, d - 2.0 * k, d - 2.0 * k, ctx.theme.bg_surface(), d / 2.0);
                }
            }
        };
        let content = pergola::view::Draw(|ctx: &mut Ctx| {
            let t = ctx.theme;
            let g = ctx.push(Node::stack(pergola::geom::Axis::Vertical, Rect::new(0.0, 0.0, pw, 0.0), 0.0));
            ctx.set_flex(g, FlexStyle { free: true, ..FlexStyle::default() });
            let mut y = 0.0;
            // Personal: always there.
            let rh = em(56.0);
            rect(ctx, 0.0, y, pw, rh, t.bg_surface(), 8.0);
            swatch(ctx, em(16.0), y + (rh - em(16.0)) / 2.0, em(16.0), None);
            text(ctx, em(44.0), y + em(20.0), "Personal", type_size::MD, Weight::Medium, t.text_primary());
            text(ctx, em(44.0), y + em(40.0), "Always here; your home folders", type_size::SM, Weight::Regular, t.text_secondary());
            y += rh + em(8.0);
            let free = p.free_colours();
            for (name, colour) in p.named() {
                let busy = p.persona_busy.as_deref() == Some(name.as_str());
                let confirm = p.persona_confirm.as_deref() == Some(name.as_str());
                let rh = em(if confirm { 132.0 } else { 92.0 });
                rect(ctx, 0.0, y, pw, rh, t.bg_surface(), 8.0);
                swatch(ctx, em(16.0), y + em(20.0), em(16.0), Some(colour));
                text(ctx, em(44.0), y + em(28.0), &apps::persona_name(name), type_size::MD, Weight::Medium, t.text_primary());
                let lab = "Remove…";
                let bx = pw - em(12.0) - width(lab, type_size::SM, Weight::Medium) - em(24.0);
                if !confirm {
                    let (pg, n) = (self.apps.clone(), name.clone());
                    button(ctx, bx, y + em(13.0), lab, Look::Secondary, p.persona_busy.is_none(), move || {
                        let token = pergola::clipboard::current_activation();
                        with_apps(&pg, |a| a.remove_persona(&n, token))
                    });
                }
                // Its colour: the other free colours, a press each.
                text(ctx, em(44.0), y + em(60.0), "Colour", type_size::SM, Weight::Regular, t.text_secondary());
                let mut cx = em(100.0);
                let d = em(20.0);
                for c in std::iter::once(colour.as_str()).chain(free.iter().copied()) {
                    let here = c == colour;
                    if here {
                        rect(ctx, cx - em(3.0), y + em(50.0) - em(3.0), d + em(6.0), d + em(6.0), t.focus_ring(), (d + em(6.0)) / 2.0);
                    }
                    let (pg, n, cc) = (self.apps.clone(), name.clone(), c.to_string());
                    area(ctx, Rect::new(cx, y + em(50.0), d, d), d / 2.0, Color::TRANSPARENT, Color::TRANSPARENT, move || {
                        let token = pergola::clipboard::current_activation();
                        with_apps(&pg, |a| a.set_colour(&n, &cc, token))
                    });
                    swatch(ctx, cx, y + em(50.0), d, Some(c));
                    ctx.pop();
                    cx += d + em(10.0);
                }
                if busy {
                    text(ctx, cx + em(8.0), y + em(60.0), "Changing…", type_size::SM, Weight::Regular, t.text_tertiary());
                }
                if confirm {
                    let w1 = format!("Removing {} deletes its apps\u{2019} data on this computer.", apps::persona_name(name));
                    text(ctx, em(44.0), y + em(90.0), &w1, type_size::SM, Weight::Regular, t.danger_fg());
                    let l2 = format!("Remove {}", apps::persona_name(name));
                    let (pg, n) = (self.apps.clone(), name.clone());
                    let bw = button(ctx, em(44.0), y + em(102.0) - em(4.0) + em(6.0), &l2, Look::Danger, true, move || {
                        let token = pergola::clipboard::current_activation();
                        with_apps(&pg, |a| a.remove_persona(&n, token))
                    });
                    let pg = self.apps.clone();
                    button(ctx, em(52.0) + bw, y + em(104.0), "Cancel", Look::Ghost, true, move || with_apps(&pg, |a| a.cancel_remove_persona()));
                }
                y += rh;
                if let Some((_, why)) = p.persona_note.as_ref().filter(|(n, _)| n == name) {
                    text(ctx, em(16.0), y + em(12.0), why, type_size::SM, Weight::Regular, t.danger_fg());
                    y += em(24.0);
                }
                y += em(8.0);
            }
            // Add a persona: a name, a free colour, Add.
            y += em(12.0);
            text(ctx, 0.0, y + em(14.0), "Add a persona", type_size::LG, Weight::Semibold, t.text_primary());
            y += em(36.0);
            match p.new_colour() {
                None => {
                    text(ctx, 0.0, y + em(10.0), "Eight personas at most: every colour is taken.", type_size::SM, Weight::Regular, t.text_secondary());
                    y += em(28.0);
                }
                Some(chosen) => {
                    let (pg, field) = (self.apps.clone(), self.persona_field.clone());
                    pergola::widgets::TextField::new(self.persona_field.clone())
                        .placeholder("Name, like Work or School")
                        .at(0.0, y)
                        .width(em(220.0))
                        .on_submit(move |v| {
                            let token = pergola::clipboard::current_activation();
                            let v = v.to_string();
                            with_apps(&pg, |a| a.add_persona(&v, token));
                            if pg.get_cloned().persona_busy.is_some() { field.set(Default::default()); }
                        })
                        .render(ctx);
                    let (pg, field) = (self.apps.clone(), self.persona_field.clone());
                    let lab = if p.persona_busy.as_deref() == Some("") { "Adding…" } else { "Add" };
                    button(ctx, em(232.0), y + em(2.0), lab, Look::Primary, p.persona_busy.is_none(), move || {
                        let token = pergola::clipboard::current_activation();
                        let v = field.get_cloned().text().to_string();
                        with_apps(&pg, |a| a.add_persona(&v, token));
                        if pg.get_cloned().persona_busy.is_some() { field.set(Default::default()); }
                    });
                    y += em(44.0);
                    text(ctx, 0.0, y + em(10.0), "Colour", type_size::SM, Weight::Regular, t.text_secondary());
                    let mut cx = em(56.0);
                    let d = em(20.0);
                    for c in free.iter().copied() {
                        if c == chosen {
                            rect(ctx, cx - em(3.0), y - em(3.0), d + em(6.0), d + em(6.0), t.focus_ring(), (d + em(6.0)) / 2.0);
                        }
                        let (pg, cc) = (self.apps.clone(), c.to_string());
                        area(ctx, Rect::new(cx, y, d, d), d / 2.0, Color::TRANSPARENT, Color::TRANSPARENT, move || with_apps(&pg, |a| a.choose_new_colour(&cc)));
                        swatch(ctx, cx, y, d, Some(c));
                        ctx.pop();
                        cx += d + em(10.0);
                    }
                    y += em(32.0);
                    if let Some((_, why)) = p.persona_note.as_ref().filter(|(n, _)| n.is_empty()) {
                        text(ctx, 0.0, y + em(8.0), why, type_size::SM, Weight::Regular, t.danger_fg());
                        y += em(24.0);
                    }
                }
            }
            // Between personas (personas.md §6): a switch per direction
            // and kind, off by default; on asks Sigillum's sheet too.
            let dirs = p.directions();
            if !dirs.is_empty() {
                y += em(20.0);
                text(ctx, 0.0, y + em(14.0), "Between personas", type_size::LG, Weight::Semibold, t.text_primary());
                y += em(32.0);
            }
            if let Some(Err(why)) = &p.crossings {
                text(ctx, 0.0, y + em(10.0), &format!("The switches are not available: {why}"), type_size::SM, Weight::Regular, t.text_secondary());
                y += em(28.0);
            }
            for (from, to) in dirs {
                text(ctx, 0.0, y + em(14.0), &format!("{} to {}", apps::persona_name(&from), apps::persona_name(&to)), type_size::MD, Weight::Medium, t.text_primary());
                y += em(28.0);
                for kind in portcullis_ipc::CROSSING_KINDS {
                    let sw = portcullis_ipc::CrossingSwitch { from: from.clone(), to: to.clone(), kind: kind.to_string() };
                    let on = p.crossing_on(&sw);
                    let busy = p.crossing_busy.as_ref().filter(|(s, _)| *s == sw).map(|(_, to_on)| *to_on);
                    let bh = em(40.0);
                    text(ctx, em(16.0), y + em(16.0), &apps::crossing_words(&sw), type_size::SM, Weight::Regular, t.text_primary());
                    let state = match busy {
                        Some(true) => "Confirm it's you…",
                        Some(false) => "Turning off…",
                        None if on => "On",
                        None => "Off",
                    };
                    text(ctx, em(16.0), y + em(32.0), state, type_size::XS, Weight::Regular, t.text_secondary());
                    let lab = if on { "Turn off" } else { "Turn on" };
                    let bx = pw - em(12.0) - width(lab, type_size::SM, Weight::Medium) - em(24.0);
                    let pg = self.apps.clone();
                    button(ctx, bx, y + em(5.0), lab, Look::Secondary, p.crossing_busy.is_none(), move || {
                        let token = pergola::clipboard::current_activation();
                        with_apps(&pg, |a| a.set_crossing(sw.clone(), !on, token))
                    });
                    y += bh;
                    if let Some((_, why)) = p.crossing_note.as_ref().filter(|(s, _)| s.from == from && s.to == to && s.kind == *kind) {
                        text(ctx, em(16.0), y + em(6.0), why, type_size::SM, Weight::Regular, t.danger_fg());
                        y += em(24.0);
                    }
                }
                y += em(8.0);
            }
            if let Some(Node::Stack { rect, .. }) = ctx.tree.get_mut(g) {
                rect.size.h = y;
            }
            ctx.pop();
        });
        ScrollView::new(self.persona_scroll.clone(), Rect::new(x0, top, pw, (h - top - em(8.0)).max(em(60.0))), content)
            .key(95)
            .backdrop(t.bg_canvas())
            .emit(ctx);
    }
}

pub fn with_bt(page: &Mutable<BtPage>, f: impl FnOnce(&mut BtPage)) {
    let mut p = page.get_cloned();
    f(&mut p);
    page.set(p);
}

impl CuriaView {
    /// The Bluetooth page (curia.md §14).
    fn bluetooth(&self, ctx: &mut Ctx, x0: f32, pw: f32, h: f32) {
        let p = self.bt.get_cloned();
        let t = ctx.theme;
        let mut y = em(20.0);
        text(ctx, x0, y + em(16.0), "Bluetooth", type_size::XL, Weight::Semibold, t.text_primary());
        y += em(48.0);
        let card = |ctx: &mut Ctx, y: f32, g: char, head: &str, sub: &str| {
            let t = ctx.theme;
            rect(ctx, x0, y, pw, em(64.0), t.bg_surface(), 8.0);
            icon(ctx, x0 + em(16.0), y + em(20.0), g, em(24.0), t.text_secondary());
            text(ctx, x0 + em(56.0), y + em(24.0), head, type_size::MD, Weight::Medium, t.text_primary());
            text(ctx, x0 + em(56.0), y + em(44.0), sub, type_size::SM, Weight::Regular, t.text_secondary());
        };
        match (&p.link, &p.status) {
            (Some(Err(why)), _) => {
                card(ctx, y, ph::BLUETOOTH_SLASH, "Bluetooth is not available", &format!("{why}; trying again"));
                return;
            }
            (_, None) => {
                text(ctx, x0, y + em(10.0), "Asking vinculumd for Bluetooth…", type_size::SM, Weight::Regular, t.text_tertiary());
                return;
            }
            _ => {}
        }
        let st = p.status.clone().unwrap_or(bt::Status { ready: false, powered: false, address: None, scanning: false, pairable: false });
        let (head, sub) = match (&st.address, st.ready, st.powered) {
            (_, false, _) => ("Bluetooth is not ready", "The Bluetooth adapter is starting or not plugged in".to_string()),
            (_, true, false) => ("Bluetooth is off", "Paired devices can't connect until it is on".to_string()),
            (Some(a), true, true) => ("Bluetooth is on", format!("This computer is {a}")),
            (None, true, true) => ("Bluetooth is on", "Ready".to_string()),
        };
        let on = st.ready && st.powered;
        card(ctx, y, if on { ph::BLUETOOTH } else { ph::BLUETOOTH_SLASH }, head, &sub);
        if st.ready {
            // Right to left: Turn off/on, then Find devices while on.
            let mut bx = x0 + pw - em(16.0);
            let pl = if st.powered { "Turn off" } else { "Turn on" };
            bx -= width(pl, type_size::SM, Weight::Medium) + em(24.0);
            let (pg, want) = (self.bt.clone(), !st.powered);
            button(ctx, bx, y + em(17.0), pl, if st.powered { Look::Ghost } else { Look::Primary }, true, move || with_bt(&pg, |b| b.power(want)));
            if on {
                let l = if st.scanning { "Stop looking" } else { "Find devices" };
                bx -= width(l, type_size::SM, Weight::Medium) + em(32.0);
                let pg = self.bt.clone();
                button(ctx, bx, y + em(17.0), l, if st.scanning { Look::Secondary } else { Look::Primary }, true, move || with_bt(&pg, BtPage::toggle_scan));
            }
        }
        y += em(76.0);
        let name_of = |a: &vinculum_control::Addr| {
            p.found.iter().find(|f| f.address == *a).map(|f| bt::label(f.name.as_deref(), a))
                .or_else(|| p.devices.as_ref().and_then(|v| v.iter().find(|d| d.address == *a)).map(|d| bt::label(Some(&d.name), a)))
                .unwrap_or_else(|| a.to_string())
        };
        // A pairing prompt: drawn here, by the trusted settings app only.
        if let Some((a, prompt)) = p.prompt {
            let name = name_of(&a);
            let ch = em(if matches!(prompt, Prompt::EnterPasskey) { 108.0 } else { 76.0 });
            rect(ctx, x0, y, pw, ch, t.accent_bg(), 8.0);
            let (head, sub) = match prompt {
                Prompt::Confirm { number: Some(n) } => (format!("Does {name} show {:03} {:03}?", n / 1000, n % 1000), "Pair only if the numbers match.".to_string()),
                Prompt::Confirm { number: None } => (format!("Pair with {name}?"), "It shows no code to compare.".to_string()),
                Prompt::Display { passkey } => (format!("Type {:06} on {name}", passkey), "Then press Enter on it.".to_string()),
                Prompt::EnterPasskey => (format!("Type the code {name} shows"), "Six digits.".to_string()),
            };
            text(ctx, x0 + em(16.0), y + em(24.0), &head, type_size::MD, Weight::Semibold, t.text_primary());
            text(ctx, x0 + em(16.0), y + em(48.0), &sub, type_size::SM, Weight::Regular, t.text_secondary());
            let mut bx = x0 + pw - em(16.0);
            let by = y + em(23.0);
            let mut right = |ctx: &mut Ctx, l: &str, look: Look, f: Box<dyn Fn()>| {
                bx -= width(l, type_size::SM, Weight::Medium) + em(24.0);
                button(ctx, bx, by, l, look, true, f);
                bx -= em(8.0);
            };
            match prompt {
                Prompt::Confirm { .. } => {
                    let pg = self.bt.clone();
                    right(ctx, "Cancel", Look::Ghost, Box::new(move || with_bt(&pg, |b| b.confirm(false))));
                    let pg = self.bt.clone();
                    right(ctx, "Pair", Look::Primary, Box::new(move || with_bt(&pg, |b| b.confirm(true))));
                }
                Prompt::Display { .. } => {
                    let pg = self.bt.clone();
                    right(ctx, "Done", Look::Secondary, Box::new(move || with_bt(&pg, BtPage::dismiss_display)));
                }
                Prompt::EnterPasskey => {
                    let (pg, code) = (self.bt.clone(), self.bt_code.clone());
                    right(ctx, "Cancel", Look::Ghost, Box::new(move || { code.set(Default::default()); with_bt(&pg, |b| b.passkey(None)) }));
                    let (pg, code) = (self.bt.clone(), self.bt_code.clone());
                    right(ctx, "Pair", Look::Primary, Box::new(move || { let c = code.get_cloned().text().to_string(); code.set(Default::default()); with_bt(&pg, |b| b.passkey(Some(&c))) }));
                    let (pg, code) = (self.bt.clone(), self.bt_code.clone());
                    pergola::widgets::TextField::new(self.bt_code.clone())
                        .placeholder("123456")
                        .autofocus(true)
                        .at(x0 + em(16.0), y + em(64.0))
                        .width(em(160.0))
                        .on_submit(move |v| { code.set(Default::default()); with_bt(&pg, |b| b.passkey(Some(v))) })
                        .render(ctx);
                }
            }
            y += ch + em(12.0);
        }
        if let Some(n) = &p.note {
            text(ctx, x0, y + em(10.0), n, type_size::SM, Weight::Regular, t.text_secondary());
            y += em(28.0);
        }
        let top = y;
        let content = pergola::view::Draw(|ctx: &mut Ctx| {
            let t = ctx.theme;
            let g = ctx.push(Node::stack(pergola::geom::Axis::Vertical, Rect::new(0.0, 0.0, pw, 0.0), 0.0));
            ctx.set_flex(g, FlexStyle { free: true, ..FlexStyle::default() });
            let mut y = em(4.0);
            let row = |ctx: &mut Ctx, y: f32, head: &str, sub: &str| {
                let t = ctx.theme;
                rect(ctx, 0.0, y, pw, em(56.0), t.bg_surface(), 8.0);
                text(ctx, em(16.0), y + em(20.0), head, type_size::MD, Weight::Medium, t.text_primary());
                text(ctx, em(16.0), y + em(40.0), sub, type_size::SM, Weight::Regular, t.text_secondary());
            };
            let buttons = |ctx: &mut Ctx, y: f32, items: Vec<(String, Look, bool, Box<dyn Fn()>)>| {
                let mut bx = pw - em(12.0);
                for (l, look, on, f) in items {
                    bx -= width(&l, type_size::SM, Weight::Medium) + em(24.0);
                    button(ctx, bx, y + em(13.0), &l, look, on, f);
                    bx -= em(8.0);
                }
            };
            text(ctx, 0.0, y + em(10.0), "Paired devices", type_size::SM, Weight::Semibold, t.text_secondary());
            y += em(30.0);
            match &p.devices {
                Some(v) if v.is_empty() => {
                    text(ctx, 0.0, y + em(10.0), "None yet. Find devices to pair one.", type_size::SM, Weight::Regular, t.text_tertiary());
                    y += em(32.0);
                }
                None => {
                    y += em(8.0);
                }
                Some(v) => {
                    for d in v {
                        let a = d.address;
                        row(ctx, y, &bt::label(Some(&d.name), &a), &bt::device_line(d));
                        let pg = self.bt.clone();
                        let mut items: Vec<(String, Look, bool, Box<dyn Fn()>)> = Vec::new();
                        if p.confirm_forget != Some(a) {
                            let pg2 = pg.clone();
                            items.push(("Forget".into(), Look::Ghost, true, Box::new(move || with_bt(&pg2, |b| b.forget(a)))));
                            if d.connected {
                                items.push(("Disconnect".into(), Look::Secondary, true, Box::new(move || with_bt(&pg, |b| b.disconnect(a)))));
                            } else {
                                items.push(("Connect".into(), Look::Secondary, on, Box::new(move || with_bt(&pg, |b| b.connect(a)))));
                            }
                        }
                        buttons(ctx, y, items);
                        y += em(60.0);
                        if p.confirm_forget == Some(a) {
                            text(ctx, em(16.0), y + em(15.0), &format!("Forget {}? To use it again, pair it again.", bt::label(Some(&d.name), &a)), type_size::SM, Weight::Regular, t.text_primary());
                            let pg = self.bt.clone();
                            let pg2 = pg.clone();
                            buttons(ctx, y - em(13.0), vec![
                                ("Cancel".into(), Look::Ghost, true, Box::new(move || with_bt(&pg, BtPage::cancel_forget))),
                                ("Forget".into(), Look::Danger, true, Box::new(move || with_bt(&pg2, |b| b.forget(a)))),
                            ]);
                            y += em(38.0);
                        }
                    }
                }
            }
            if st.scanning || !p.found.is_empty() {
                y += em(8.0);
                text(ctx, 0.0, y + em(10.0), "Devices nearby", type_size::SM, Weight::Semibold, t.text_secondary());
                y += em(30.0);
                if p.found.is_empty() {
                    text(ctx, 0.0, y + em(10.0), "Looking… Put the device in pairing mode.", type_size::SM, Weight::Regular, t.text_tertiary());
                    y += em(32.0);
                }
                for f in &p.found {
                    let a = f.address;
                    row(ctx, y, &bt::label(f.name.as_deref(), &a), &bt::found_line(f));
                    let pg = self.bt.clone();
                    let busy = p.pairing == Some(a);
                    buttons(ctx, y, vec![(if busy { "Pairing…".into() } else { "Pair".into() }, Look::Primary, !busy && p.pairing.is_none() && on, Box::new(move || with_bt(&pg, |b| b.pair(a))))]);
                    y += em(60.0);
                }
            }
            if let Some(Node::Stack { rect, .. }) = ctx.tree.get_mut(g) {
                rect.size.h = y;
            }
            ctx.pop();
        });
        ScrollView::new(self.bt_scroll.clone(), Rect::new(x0, top, pw, (h - top - em(8.0)).max(em(60.0))), content)
            .key(95)
            .backdrop(t.bg_canvas())
            .emit(ctx);
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
        let v = CuriaView { workspaces: Mutable::new(Default::default()), workspace_scroll: Mutable::new(Default::default()), which: Mutable::new(Which::Network), page: Mutable::new(p.clone()), devs: Mutable::new(DevPage::default()), dev_scroll: Mutable::new(Default::default()), disp: Mutable::new(DispPage::default()), disp_scroll: Mutable::new(Default::default()), sound: Mutable::new(SoundPage::default()), sound_scroll: Mutable::new(Default::default()), apps: Mutable::new(AppsPage::default()), apps_scroll: Mutable::new(Default::default()), bt: Mutable::new(BtPage::default()), bt_scroll: Mutable::new(Default::default()), bt_code: Mutable::new(Default::default()), persona_field: Mutable::new(Default::default()), persona_scroll: Mutable::new(Default::default()), net_fields: std::array::from_fn(|_| Mutable::new(Default::default())), net_scroll: Mutable::new(Default::default()), cred, size: Mutable::new((960.0, 720.0)), printers: Mutable::new(Default::default()), printers_scroll: Mutable::new(Default::default()), printer_fields: std::array::from_fn(|_| Mutable::new(Default::default())), vpn: Mutable::new(Default::default()), vpn_scroll: Mutable::new(Default::default()), vpn_fields: std::array::from_fn(|_| Mutable::new(Default::default())), vpn_cred: CredentialField::vpn_secret(Mutable::new(Default::default())) };
        assert!(texts(&v).iter().any(|s| s == "Network settings are not available"));
        p.apply(Got::Status(Some(Status { link: Link::Wifi, online: Online::Unknown, metered: false, bars: Some(3), ssid: Some(b"home".to_vec()), portal: None })));
        p.apply(Got::Profiles(Ok(vec![Profile { ssid: b"home".to_vec(), open: false, wpa3_only: false, mac: "network".into(), auto: true, prio: 2, metered: false, manual: None, enterprise: false }])));
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

    /// What frescod holds after each frame, as the deltas leave it: one
    /// "details" label, never the one before as well.
    #[test]
    fn the_details_label_is_drawn_once_as_the_page_changes() {
        use std::collections::HashMap;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        #[derive(Default)]
        struct Model(HashMap<u32, (Option<String>, Option<u32>)>);
        impl Model {
            fn gone(&mut self, id: u32) {
                self.0.remove(&id);
                let kids: Vec<u32> = self.0.iter().filter(|(_, v)| v.1 == Some(id)).map(|(k, _)| *k).collect();
                for k in kids {
                    self.gone(k);
                }
            }
        }
        impl pergola::Surface for Model {
            fn begin_frame(&mut self) -> std::io::Result<()> { Ok(()) }
            fn end_frame(&mut self) -> std::io::Result<()> { Ok(()) }
            fn set_node(&mut self, id: NodeId, node: &Node, group: Option<NodeId>, _: u32) -> std::io::Result<()> {
                let t = if let Node::Text { content, .. } = node { Some(content.clone()) } else { None };
                self.0.insert(id.0, (t, group.map(|g| g.0)));
                Ok(())
            }
            fn set_group(&mut self, id: NodeId, _: &Node, group: Option<NodeId>, _: u32) -> std::io::Result<()> {
                let kept = self.0.get(&id.0).map(|v| v.0.clone()).unwrap_or(None);
                self.0.insert(id.0, (kept, group.map(|g| g.0)));
                Ok(())
            }
            fn clear_node(&mut self, id: NodeId) -> std::io::Result<()> {
                self.gone(id.0);
                Ok(())
            }
            fn present(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        let dirty = Arc::new(AtomicBool::new(true));
        macro_rules! m { ($v:expr) => { Mutable::with_dirty($v, dirty.clone()) }; }
        let mut p = NetPage::new();
        p.take_wants();
        p.apply(Got::Status(Some(Status { link: Link::Wifi, online: Online::Unknown, metered: false, bars: Some(3), ssid: Some(b"home".to_vec()), portal: None })));
        let page = m!(p.clone());
        let cred = CredentialField::wifi_passphrase(Mutable::new(Default::default()));
        let v = CuriaView { workspaces: m!(Default::default()), workspace_scroll: m!(Default::default()), which: m!(Which::Network), page: page.clone(), devs: m!(DevPage::default()), dev_scroll: m!(Default::default()), disp: m!(DispPage::default()), disp_scroll: m!(Default::default()), sound: m!(SoundPage::default()), sound_scroll: m!(Default::default()), apps: m!(AppsPage::default()), apps_scroll: m!(Default::default()), bt: m!(BtPage::default()), bt_scroll: m!(Default::default()), bt_code: m!(Default::default()), persona_field: m!(Default::default()), persona_scroll: m!(Default::default()), net_fields: std::array::from_fn(|_| m!(Default::default())), net_scroll: m!(Default::default()), cred, size: m!((960.0, 720.0)), printers: m!(Default::default()), printers_scroll: m!(Default::default()), printer_fields: std::array::from_fn(|_| m!(Default::default())), vpn: m!(Default::default()), vpn_scroll: m!(Default::default()), vpn_fields: std::array::from_fn(|_| m!(Default::default())), vpn_cred: CredentialField::vpn_secret(Mutable::new(Default::default())) };
        let mut app = pergola::App::new_with_flag(v, dirty.clone());
        let mut s = Model::default();
        let labels = |s: &Model| s.0.values().filter_map(|v| v.0.clone()).filter(|t| t.ends_with(" details")).collect::<Vec<_>>();
        pergola::commit(&mut s, &app.tick()).unwrap();
        assert_eq!(labels(&s), vec!["Show details".to_string()]);
        // The saved networks arrive: the label moves down.
        p.apply(Got::Profiles(Ok(vec![Profile { ssid: b"home".to_vec(), open: false, wpa3_only: false, mac: "network".into(), auto: true, prio: 0, metered: false, manual: None, enterprise: false }])));
        page.set(p.clone());
        pergola::commit(&mut s, &app.tick()).unwrap();
        assert_eq!(labels(&s), vec!["Show details".to_string()], "after the list came");
        p.toggle_details();
        page.set(p.clone());
        pergola::commit(&mut s, &app.tick()).unwrap();
        assert_eq!(labels(&s), vec!["Hide details".to_string()], "after the click");
    }

    #[test]
    fn details_show_ip_settings_and_dns_servers() {
        use atrium_netconf::{Iface, Manual};
        let mut p = NetPage::new();
        p.take_wants();
        p.apply(Got::Status(Some(Status { link: Link::Wired, online: Online::Yes, metered: false, bars: None, ssid: None, portal: None })));
        let m = Manual { addr: "10.0.0.5".parse().unwrap(), prefix: 24, router: Some("10.0.0.1".parse().unwrap()), dns: vec![] };
        p.apply(Got::Profiles(Ok(vec![Profile { ssid: b"home".to_vec(), open: false, wpa3_only: false, mac: "network".into(), auto: true, prio: 0, metered: false, manual: Some(m), enterprise: false }])));
        let em0 = Iface { name: "em0".into(), wifi: false, link: "up".into(), addr: Some("192.168.1.9/24".into()), router: None, dns: vec![], mac: String::new(), addrs6: vec![], dns6: vec![], online: Online::Yes, metered: false, egress: true, manual: None, dot1x: None, dot1x_why: None };
        p.apply(Got::Details(Ok(vec![em0])));
        p.apply(Got::OwnDns(Ok((vec!["9.9.9.9".into()], Some("dns.quad9.net".into())))));
        p.toggle_details();
        p.edit_ip(atrium_netconf::Target::Wired("em0".into()));
        p.edit_error = Some("\"10.0.0.300\" is not an IPv4 address".into());
        let cred = CredentialField::wifi_passphrase(Mutable::new(Default::default()));
        let v = CuriaView { workspaces: Mutable::new(Default::default()), workspace_scroll: Mutable::new(Default::default()), which: Mutable::new(Which::Network), page: Mutable::new(p), devs: Mutable::new(DevPage::default()), dev_scroll: Mutable::new(Default::default()), disp: Mutable::new(DispPage::default()), disp_scroll: Mutable::new(Default::default()), sound: Mutable::new(SoundPage::default()), sound_scroll: Mutable::new(Default::default()), apps: Mutable::new(AppsPage::default()), apps_scroll: Mutable::new(Default::default()), bt: Mutable::new(BtPage::default()), bt_scroll: Mutable::new(Default::default()), bt_code: Mutable::new(Default::default()), persona_field: Mutable::new(Default::default()), persona_scroll: Mutable::new(Default::default()), net_fields: std::array::from_fn(|_| Mutable::new(Default::default())), net_scroll: Mutable::new(Default::default()), cred, size: Mutable::new((960.0, 720.0)), printers: Mutable::new(Default::default()), printers_scroll: Mutable::new(Default::default()), printer_fields: std::array::from_fn(|_| Mutable::new(Default::default())), vpn: Mutable::new(Default::default()), vpn_scroll: Mutable::new(Default::default()), vpn_fields: std::array::from_fn(|_| Mutable::new(Default::default())), vpn_cred: CredentialField::vpn_secret(Mutable::new(Default::default())) };
        let all = texts(&v);
        for want in ["IP settings", "home (Wi-Fi)", "Fixed: 10.0.0.5/24", "em0 (Ethernet)", "Automatic (DHCP)", "Use this address", "Automatic", "Close", "DNS servers", "9.9.9.9, encrypted (dns.quad9.net)", "\"10.0.0.300\" is not an IPv4 address"] {
            assert!(all.iter().any(|s| s == want), "{want} missing from {all:?}");
        }
        // The DNS servers row open: the encrypted DNS field and what it does.
        let mut p = v.page.get_cloned();
        p.edit_dns();
        v.page.set(p);
        let all = texts(&v);
        for want in ["Encrypted (DNS over TLS)", "Save", "With a name, lookups go only to these servers over TLS. If their certificate doesn't match, nothing resolves."] {
            assert!(all.iter().any(|s| s == want), "{want} missing from {all:?}");
        }
    }

    #[test]
    fn the_display_page_shows_appearance_text_size_and_monitors() {
        use crate::display::{Got, Snapshot};
        use fresco_protocol::{WmDisplayMode, WmOutput, WmRect};
        let cred = CredentialField::wifi_passphrase(Mutable::new(Default::default()));
        let v = CuriaView { workspaces: Mutable::new(Default::default()), workspace_scroll: Mutable::new(Default::default()), which: Mutable::new(Which::Display), page: Mutable::new(NetPage::new()), devs: Mutable::new(DevPage::default()), dev_scroll: Mutable::new(Default::default()), disp: Mutable::new(DispPage::new()), disp_scroll: Mutable::new(Default::default()), sound: Mutable::new(SoundPage::default()), sound_scroll: Mutable::new(Default::default()), apps: Mutable::new(AppsPage::default()), apps_scroll: Mutable::new(Default::default()), bt: Mutable::new(BtPage::default()), bt_scroll: Mutable::new(Default::default()), bt_code: Mutable::new(Default::default()), persona_field: Mutable::new(Default::default()), persona_scroll: Mutable::new(Default::default()), net_fields: std::array::from_fn(|_| Mutable::new(Default::default())), net_scroll: Mutable::new(Default::default()), cred, size: Mutable::new((960.0, 720.0)), printers: Mutable::new(Default::default()), printers_scroll: Mutable::new(Default::default()), printer_fields: std::array::from_fn(|_| Mutable::new(Default::default())), vpn: Mutable::new(Default::default()), vpn_scroll: Mutable::new(Default::default()), vpn_fields: std::array::from_fn(|_| Mutable::new(Default::default())), vpn_cred: CredentialField::vpn_secret(Mutable::new(Default::default())) };
        assert!(texts(&v).iter().any(|s| s == "Asking Forum for the display settings…"));
        with_disp(&v.disp, |p| p.apply(Got::State(Err("forum-wm is not answering".into()))));
        assert!(texts(&v).iter().any(|s| s == "forum-wm is not answering; trying again"));
        let o = |n: u32, x: i32| WmOutput { output: n, identity: 9 + n as u64, rect: WmRect { x, y: 0, w: 1920, h: 1080 }, refresh_mhz: 60000, present: true, scale_milli: 1000, auto_scale_milli: 1000, ui_size_milli: 1000, ui_size_set: false, internal: false, mirror_of: 0, width_mm: 0, height_mm: 0, flapping: false };
        with_disp(&v.disp, |p| p.apply(Got::State(Ok(Snapshot { dark: true, text_size: 1300, outputs: vec![o(0, 0), o(1, 1920)], modes: vec![WmDisplayMode { width: 1920, height: 1080, refresh_mhz: 60000, preferred: true }], current: 0 }))));
        with_disp(&v.disp, DispPage::toggle_modes);
        let all = texts(&v);
        let (tree, _) = pergola::view::render(&v, Semantic::LIGHT);
        assert!(tree.iter().any(|(_, n)| matches!(n, Node::Scroll { .. })), "the settings scroll");
        for want in ["Display", "Appearance", "Light", "Dark", "Text size", "130%", "Display 1", "Display 2", "UI size", "Smaller", "Default", "150%", "Resolution", "1920 × 1080 · 60 Hz", "1920 × 1080 · 60 Hz (native)", "Place", "Right of", "Mirror Display 1"] {
            assert!(all.iter().any(|s| s == want), "{want} missing from {all:?}");
        }
    }

    #[test]
    fn the_sound_page_shows_volume_outputs_and_apps() {
        use lyra_protocol::wire::{AppEntry, FromControl, Output, Role, VolumeMode};
        let cred = CredentialField::wifi_passphrase(Mutable::new(Default::default()));
        let v = CuriaView { workspaces: Mutable::new(Default::default()), workspace_scroll: Mutable::new(Default::default()), which: Mutable::new(Which::Sound), page: Mutable::new(NetPage::new()), devs: Mutable::new(DevPage::default()), dev_scroll: Mutable::new(Default::default()), disp: Mutable::new(DispPage::default()), disp_scroll: Mutable::new(Default::default()), sound: Mutable::new(SoundPage::default()), sound_scroll: Mutable::new(Default::default()), apps: Mutable::new(AppsPage::default()), apps_scroll: Mutable::new(Default::default()), bt: Mutable::new(BtPage::default()), bt_scroll: Mutable::new(Default::default()), bt_code: Mutable::new(Default::default()), persona_field: Mutable::new(Default::default()), persona_scroll: Mutable::new(Default::default()), net_fields: std::array::from_fn(|_| Mutable::new(Default::default())), net_scroll: Mutable::new(Default::default()), cred, size: Mutable::new((960.0, 720.0)), printers: Mutable::new(Default::default()), printers_scroll: Mutable::new(Default::default()), printer_fields: std::array::from_fn(|_| Mutable::new(Default::default())), vpn: Mutable::new(Default::default()), vpn_scroll: Mutable::new(Default::default()), vpn_fields: std::array::from_fn(|_| Mutable::new(Default::default())), vpn_cred: CredentialField::vpn_secret(Mutable::new(Default::default())) };
        assert!(texts(&v).iter().any(|s| s == "Asking choragusd for the sound settings…"));
        with_sound(&v.sound, |p| p.lost("choragusd is not running".into()));
        assert!(texts(&v).iter().any(|s| s == "choragusd is not running; trying again"));
        with_sound(&v.sound, |p| {
            p.connected();
            p.apply(FromControl::Apps { master: 0.42, master_muted: false, apps: vec![AppEntry { app_id: "org.atrium.player".into(), label: "Video".into(), role: Role::Media, volume: 0.8, muted: false, streams: 1, spatial: true }] });
            p.apply(FromControl::Outputs { current: 1, outputs: vec![Output { id: 1, name: "Speakers".into(), volume: VolumeMode::Digital, volume_setting: None, hw_volume: false, balance: 0.0, channels: 2, spatial: true }] });
        });
        let all = texts(&v);
        for want in ["Sound", "Volume", "42%", "Mute", "Output", "Speakers", "Balance", "Stereo", "5.1", "Headphone surround", "On", "Volume control", "Automatic", "Fixed (line out)", "Apps", "Player · Video", "80%", "Surround: on"] {
            assert!(all.iter().any(|s| s == want), "{want} missing from {all:?}");
        }
    }

    #[test]
    fn the_apps_page_shows_who_allowed_each_app() {
        use crate::apps::Got;
        use portcullis_ipc::{AccessBy, AccessEntry};
        let cred = CredentialField::wifi_passphrase(Mutable::new(Default::default()));
        let v = CuriaView { workspaces: Mutable::new(Default::default()), workspace_scroll: Mutable::new(Default::default()), which: Mutable::new(Which::Apps), page: Mutable::new(NetPage::new()), devs: Mutable::new(DevPage::default()), dev_scroll: Mutable::new(Default::default()), disp: Mutable::new(DispPage::default()), disp_scroll: Mutable::new(Default::default()), sound: Mutable::new(SoundPage::default()), sound_scroll: Mutable::new(Default::default()), apps: Mutable::new(AppsPage::default()), apps_scroll: Mutable::new(Default::default()), bt: Mutable::new(BtPage::default()), bt_scroll: Mutable::new(Default::default()), bt_code: Mutable::new(Default::default()), persona_field: Mutable::new(Default::default()), persona_scroll: Mutable::new(Default::default()), net_fields: std::array::from_fn(|_| Mutable::new(Default::default())), net_scroll: Mutable::new(Default::default()), cred, size: Mutable::new((960.0, 720.0)), printers: Mutable::new(Default::default()), printers_scroll: Mutable::new(Default::default()), printer_fields: std::array::from_fn(|_| Mutable::new(Default::default())), vpn: Mutable::new(Default::default()), vpn_scroll: Mutable::new(Default::default()), vpn_fields: std::array::from_fn(|_| Mutable::new(Default::default())), vpn_cred: CredentialField::vpn_secret(Mutable::new(Default::default())) };
        assert!(texts(&v).iter().any(|s| s == "Asking which apps are installed…"));
        with_apps(&v.apps, |p| p.apply(Got::List(Err("portcullisd is not answering".into()))));
        assert!(texts(&v).iter().any(|s| s == "portcullisd is not answering; trying again"));
        let e = |id: &str, name: &str, by, missing: Vec<String>, yours: Option<String>, restricted: Vec<String>| AccessEntry { id: id.into(), name: name.into(), by, asks: vec!["Play audio".into(), "Access the microphone".into()], missing, yours, restricted, settings: false, background: None };
        with_apps(&v.apps, |p| {
            p.apply(Got::List(Ok(vec![
                e("org.atrium.player", "Player", Some(AccessBy::You), vec![], Some("2026-11-15T09:00:00Z".into()), vec![]),
                e("org.example.rec", "Recorder", None, vec!["Access the microphone".into()], None, vec![]),
                e("org.atrium.curia", "Settings", Some(AccessBy::System), vec![], None, vec!["app-grants".into()]),
            ])));
            p.toggle_open("org.example.rec");
            p.remove("org.atrium.player");
        });
        let all = texts(&v);
        for want in ["Apps", "3 apps, 1 not allowed yet", "Player", "Allowed by you on 2026-11-15", "Recorder", "Asks when it opens", "Allow", "Settings", "Part of the system: allowed by the system", "It asks to", "•  Access the microphone (not allowed yet)", "•  Play audio", "Remove what you allowed Player? It asks again when it next opens.", "Remove", "Cancel"] {
            assert!(all.iter().any(|s| s == want), "{want} missing from {all:?}");
        }
        assert_eq!(all.iter().filter(|s| *s == "Allow").count(), 1, "Settings itself is the system's: no Allow");
    }

    #[test]
    fn the_bluetooth_page_shows_the_prompt_paired_and_nearby() {
        use vinculum_control::{Addr, DeviceInfo, Event};
        let cred = CredentialField::wifi_passphrase(Mutable::new(Default::default()));
        let v = CuriaView { workspaces: Mutable::new(Default::default()), workspace_scroll: Mutable::new(Default::default()), which: Mutable::new(Which::Bluetooth), page: Mutable::new(NetPage::new()), devs: Mutable::new(DevPage::default()), dev_scroll: Mutable::new(Default::default()), disp: Mutable::new(DispPage::default()), disp_scroll: Mutable::new(Default::default()), sound: Mutable::new(SoundPage::default()), sound_scroll: Mutable::new(Default::default()), apps: Mutable::new(AppsPage::default()), apps_scroll: Mutable::new(Default::default()), bt: Mutable::new(BtPage::default()), bt_scroll: Mutable::new(Default::default()), bt_code: Mutable::new(Default::default()), persona_field: Mutable::new(Default::default()), persona_scroll: Mutable::new(Default::default()), net_fields: std::array::from_fn(|_| Mutable::new(Default::default())), net_scroll: Mutable::new(Default::default()), cred, size: Mutable::new((960.0, 720.0)), printers: Mutable::new(Default::default()), printers_scroll: Mutable::new(Default::default()), printer_fields: std::array::from_fn(|_| Mutable::new(Default::default())), vpn: Mutable::new(Default::default()), vpn_scroll: Mutable::new(Default::default()), vpn_fields: std::array::from_fn(|_| Mutable::new(Default::default())), vpn_cred: CredentialField::vpn_secret(Mutable::new(Default::default())) };
        assert!(texts(&v).iter().any(|s| s == "Asking vinculumd for Bluetooth…"));
        with_bt(&v.bt, |p| p.lost("vinculumd is not running".into()));
        assert!(texts(&v).iter().any(|s| s == "vinculumd is not running; trying again"));
        let a = |n: u8| Addr { bytes: [0xC0, 1, 2, 3, 4, n], random: true };
        with_bt(&v.bt, |p| {
            p.connected();
            p.apply(Event::Ok);
            p.apply(Event::Status { ready: true, powered: true, address: Some(Addr { bytes: [0, 0x1B, 0xDC, 1, 2, 3], random: false }), scanning: true, pairable: true, agents: 1, connections: 1 });
            p.apply(Event::Devices { devices: vec![DeviceInfo { address: a(1), identity: None, name: "Keys".into(), authenticated: true, secure_connections: true, keyboard_allowed: true, connected: true }] });
            p.apply(Event::Found { address: a(2), name: Some("Mouse".into()), rssi: -70, class_of_device: None });
            p.pair(a(2));
            p.apply(Event::Prompt { address: a(2), prompt: vinculum_control::Prompt::Confirm { number: Some(42) } });
        });
        let all = texts(&v);
        for want in ["Bluetooth", "Bluetooth is on", "This computer is 00:1B:DC:01:02:03", "Stop looking", "Turn off", "Does Mouse show 000 042?", "Pair only if the numbers match.", "Pair", "Cancel", "Paired devices", "Keys", "Connected", "Disconnect", "Forget", "Devices nearby", "Mouse", "Good signal", "Pairing…"] {
            assert!(all.iter().any(|s| s == want), "{want} missing from {all:?}");
        }
    }

    #[test]
    fn the_devices_page_shows_nobodys_first() {
        use atrium_devices::{Device, Owner};
        let cred = CredentialField::wifi_passphrase(Mutable::new(Default::default()));
        let v = CuriaView { workspaces: Mutable::new(Default::default()), workspace_scroll: Mutable::new(Default::default()), which: Mutable::new(Which::Devices), page: Mutable::new(NetPage::new()), devs: Mutable::new(DevPage::default()), dev_scroll: Mutable::new(Default::default()), disp: Mutable::new(DispPage::default()), disp_scroll: Mutable::new(Default::default()), sound: Mutable::new(SoundPage::default()), sound_scroll: Mutable::new(Default::default()), apps: Mutable::new(AppsPage::default()), apps_scroll: Mutable::new(Default::default()), bt: Mutable::new(BtPage::default()), bt_scroll: Mutable::new(Default::default()), bt_code: Mutable::new(Default::default()), persona_field: Mutable::new(Default::default()), persona_scroll: Mutable::new(Default::default()), net_fields: std::array::from_fn(|_| Mutable::new(Default::default())), net_scroll: Mutable::new(Default::default()), cred, size: Mutable::new((960.0, 720.0)), printers: Mutable::new(Default::default()), printers_scroll: Mutable::new(Default::default()), printer_fields: std::array::from_fn(|_| Mutable::new(Default::default())), vpn: Mutable::new(Default::default()), vpn_scroll: Mutable::new(Default::default()), vpn_fields: std::array::from_fn(|_| Mutable::new(Default::default())), vpn_cred: CredentialField::vpn_secret(Mutable::new(Default::default())) };
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
