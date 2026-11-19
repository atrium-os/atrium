//! Workspace credential grants (dev-tools.md §8). Portcullis scopes requests
//! to the Settings launch's owner and verifies the activation for mutations.

use pergola::geom::{Axis, Rect};
use pergola::layout::FlexStyle;
use pergola::node::Node;
use pergola::theme::{type_size, Weight};
use pergola::view::{Ctx, Draw};
use pergola::widgets::ScrollView;
use pergola::Mutable;
use portcullis_ipc::{Request, Response, WorkspaceCredential, WorkspaceCredentialKind, WorkspaceInfo};

use crate::view::{button, em, text, width, CuriaView, Look};

#[derive(Clone, Default)]
pub struct Page {
    workspaces: Vec<WorkspaceInfo>,
    selected: Option<(String, String)>,
    credentials: Vec<WorkspaceCredential>,
    busy: bool,
    request: Option<Request>,
    error: Option<String>,
}

impl Page {
    fn queue(&mut self, request: Request) {
        self.error = None;
        self.busy = true;
        self.request = Some(request);
    }

    pub fn open(&mut self) {
        if self.busy {
            return;
        }
        self.selected = None;
        self.credentials.clear();
        self.queue(Request::Workspaces);
    }

    fn select(&mut self, workspace: String, persona: String) {
        if self.busy {
            return;
        }
        self.credentials.clear();
        self.selected = Some((workspace.clone(), persona.clone()));
        self.queue(Request::WorkspaceCredentials { workspace, persona });
    }

    fn set_grant(&mut self, credential: [u8; 16], granted: bool) {
        if self.busy {
            return;
        }
        let Some((workspace, persona)) = self.selected.clone() else { return };
        let Some(activation) = pergola::clipboard::current_activation() else {
            self.error = Some("Press Grant or Revoke to change access".into());
            return;
        };
        self.queue(Request::SetWorkspaceCredential { workspace, persona, credential, granted, activation });
    }

    pub fn has_request(&self) -> bool { self.request.is_some() }

    pub fn take_request(&mut self) -> Option<Request> { self.request.take() }

    pub fn apply(&mut self, reply: Result<Response, String>) {
        self.busy = false;
        match reply {
            Ok(Response::WorkspaceList { workspaces, .. }) => self.workspaces = workspaces,
            Ok(Response::WorkspaceCredentials { mut credentials }) => {
                // Keep button positions stable across grant refreshes.
                credentials.sort_unstable_by(|a, b| {
                    let token = |item: &WorkspaceCredential| item.kind == WorkspaceCredentialKind::HttpsToken;
                    (token(a), &a.label, a.id).cmp(&(token(b), &b.label, b.id))
                });
                self.credentials = credentials;
            }
            Ok(Response::Ok) => {
                if let Some((workspace, persona)) = self.selected.clone() {
                    // Keep the rows visible and their buttons disabled while
                    // fetching the authoritative grant state after the write.
                    self.queue(Request::WorkspaceCredentials { workspace, persona });
                }
            }
            Ok(_) => self.error = Some("Unexpected workspace access response".into()),
            Err(message) => self.error = Some(message),
        }
    }
}

pub fn update(page: &Mutable<Page>, edit: impl FnOnce(&mut Page)) {
    let mut state = page.get_cloned();
    edit(&mut state);
    page.set(state);
}

// Fit a service-provided label into its column, preserving character boundaries.
fn fit(value: &str, available: f32) -> String {
    let measure = |s: &str| width(s, type_size::MD, Weight::Medium);
    if measure(value) <= available {
        return value.into();
    }
    let mut label = String::new();
    for ch in value.chars() {
        label.push(ch);
        if measure(&label) + measure("…") > available {
            label.pop();
            break;
        }
    }
    label.push('…');
    label
}

impl CuriaView {
    pub(crate) fn workspace_credentials(&self, ctx: &mut Ctx, x: f32, w: f32, h: f32) {
        let t = ctx.theme;
        let state = self.workspaces.get_cloned();
        text(ctx, x, em(36.0), "Workspace access", type_size::XL, Weight::Semibold, t.text_primary());
        let mut top = em(68.0);
        if let Some((workspace, persona)) = &state.selected {
            let page = self.workspaces.clone();
            let scroll = self.workspace_scroll.clone();
            let back = button(ctx, x, top, "Workspaces", Look::Secondary, !state.busy, move || {
                update(&page, Page::open);
                scroll.set(Default::default());
            });
            let scope = fit(&format!("{workspace} ({persona})"), w - back - em(16.0));
            text(ctx, x + back + em(16.0), top + em(15.0), &scope, type_size::MD, Weight::Medium, t.text_primary());
            top += em(42.0);
        }
        let has_rows = if state.selected.is_some() {
            !state.credentials.is_empty()
        } else {
            !state.workspaces.is_empty()
        };
        let status = if state.busy {
            (!has_rows).then(|| "Loading…".to_string())
        } else if let Some(error) = &state.error {
            Some(fit(error, w))
        } else if state.selected.is_some() && state.credentials.is_empty() {
            Some("No SSH keys or HTTPS tokens in this persona".into())
        } else if state.selected.is_none() && state.workspaces.is_empty() {
            Some("No workspaces".into())
        } else {
            None
        };
        if let Some(status) = status {
            let color = if state.error.is_some() { t.danger_fg() } else { t.text_secondary() };
            text(ctx, x, top + em(12.0), &status, type_size::SM, Weight::Regular, color);
            top += em(30.0);
        }
        if state.error.is_some() {
            let page = self.workspaces.clone();
            let selected = state.selected.clone();
            button(ctx, x, top, "Refresh", Look::Secondary, true, move || {
                update(&page, |p| match &selected {
                    Some((workspace, persona)) => p.select(workspace.clone(), persona.clone()),
                    None => p.open(),
                });
            });
            top += em(42.0);
        }
        let page = self.workspaces.clone();
        let scroll = self.workspace_scroll.clone();
        let show_credentials = state.selected.is_some();
        let rows = if show_credentials { state.credentials.len() } else { state.workspaces.len() };
        let content = Draw(move |ctx: &mut Ctx| {
            let t = ctx.theme;
            let height = rows as f32 * em(56.0);
            let group = ctx.push(Node::stack(Axis::Vertical, Rect::new(0.0, 0.0, w, height), 0.0));
            ctx.set_flex(group, FlexStyle { free: true, ..FlexStyle::default() });
            if show_credentials {
                for (index, credential) in state.credentials.iter().enumerate() {
                    let y = index as f32 * em(56.0);
                    let verb = if credential.granted { "Revoke" } else { "Grant" };
                    let kind = match credential.kind {
                        WorkspaceCredentialKind::SshKey => "SSH key",
                        WorkspaceCredentialKind::HttpsToken => "HTTPS token",
                    };
                    let bx = w - width(verb, type_size::SM, Weight::Medium) - em(36.0);
                    let label = fit(&credential.label, bx - em(12.0));
                    let id = credential.id;
                    let short_id = id[..4].iter().map(|byte| format!("{byte:02x}")).collect::<String>();
                    text(ctx, 0.0, y + em(12.0), &label, type_size::MD, Weight::Medium, t.text_primary());
                    text(ctx, 0.0, y + em(32.0), &format!("{kind} · {short_id}"), type_size::SM, Weight::Regular, t.text_secondary());
                    let (page, granted) = (page.clone(), !credential.granted);
                    button(ctx, bx, y + em(4.0), verb, Look::Secondary, !state.busy, move || {
                        update(&page, |p| p.set_grant(id, granted));
                    });
                }
            } else {
                for (index, workspace) in state.workspaces.iter().enumerate() {
                    let y = index as f32 * em(56.0);
                    text(ctx, 0.0, y + em(12.0), &workspace.name, type_size::MD, Weight::Medium, t.text_primary());
                    text(ctx, 0.0, y + em(32.0), &workspace.persona, type_size::SM, Weight::Regular, t.text_secondary());
                    let (page, scroll) = (page.clone(), scroll.clone());
                    let (name, persona) = (workspace.name.clone(), workspace.persona.clone());
                    let bx = w - width("Credentials", type_size::SM, Weight::Medium) - em(36.0);
                    button(ctx, bx, y + em(4.0), "Credentials", Look::Secondary, !state.busy, move || {
                        update(&page, |p| p.select(name.clone(), persona.clone()));
                        scroll.set(Default::default());
                    });
                }
            }
            ctx.pop();
        });
        ScrollView::new(self.workspace_scroll.clone(), Rect::new(x, top, w, (h - top - em(8.0)).max(0.0)), content)
            .backdrop(t.bg_canvas())
            .emit(ctx);
    }
}
