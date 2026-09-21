//! Settings → Devices (feature-inventory §1.5): the device registry — name,
//! platform, last-seen, presence dot, a "This device" badge, click-to-copy id,
//! and a Rename dialog (Mutate renameDevice).

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, ClipboardItem, Context, Entity, SharedString, Subscription, Task, Window, div,
    prelude::*, px,
};
use std::time::Duration;

use kratos_proto::WorkspaceScope;
use kratos_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::pairing::{
    PeerStatus, TrustedDevice, connectivity_label, parse_peer_status, parse_trusted_devices,
};
use crate::popover;
use crate::popover::Loadable;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// A device that pinged within this window shows a presence dot (engines
/// heartbeat every 15s; 70s tolerates a couple of missed beats).
pub const DEVICE_ONLINE_WINDOW_SECS: i64 = 70;

/// Presence: last-seen within the online window (future timestamps count). Pure.
pub fn device_online(last_seen: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    last_seen
        .is_some_and(|at| now.signed_duration_since(at).num_seconds() <= DEVICE_ONLINE_WINDOW_SECS)
}

/// Compact last-seen line. Pure.
pub fn format_last_seen(last_seen: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(at) = last_seen else {
        return "never seen".to_string();
    };
    let secs = now.signed_duration_since(at).num_seconds();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// Scope-aware copy: a local registry describes only the active local
/// workspace and must not imply that account device metadata is already live.
pub fn devices_subtitle(scope: Option<WorkspaceScope>) -> &'static str {
    match scope {
        Some(WorkspaceScope::Local) => "Manage device details stored in this local workspace.",
        Some(WorkspaceScope::Synced) => "Manage device names and inspect synced device metadata.",
        Some(WorkspaceScope::Development) | None => "Manage device names for this workspace.",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeerAction {
    Invitation,
    Revocation,
}

fn peer_action_can_start(current: Option<PeerAction>) -> bool {
    current.is_none()
}

fn begin_peer_action(slot: &mut Option<PeerAction>, action: PeerAction) -> bool {
    if !peer_action_can_start(*slot) {
        return false;
    }
    *slot = Some(action);
    true
}

pub(crate) fn can_manage_peer(status: &PeerStatus) -> bool {
    // The durable host is currently the only owner identity the engine creates.
    status.signed_in && status.hosting
}

fn take_invitation_for_clipboard(
    invitation: &mut Option<String>,
    copied: &mut bool,
) -> Option<String> {
    let code = invitation.take()?;
    *copied = true;
    Some(code)
}

fn refresh_response_is_current(current: u64, response: u64) -> bool {
    current == response
}

struct RenameDialog {
    device_id: String,
    input: Entity<ComposerInput>,
    _events: Subscription,
}

pub struct DevicesPage {
    state: Entity<AppState>,
    scroll: widgets::PageScroll,
    rename: Option<RenameDialog>,
    /// Device id whose id-chip shows "Copied" right now.
    copied: Option<String>,
    error: Option<SharedString>,
    refresh_task: Option<Task<()>>,
    peer_action_task: Option<Task<()>>,
    rename_task: Option<Task<()>>,
    copy_task: Option<Task<()>>,
    invitation_copy_task: Option<Task<()>>,
    peer_action: Option<PeerAction>,
    refresh_generation: u64,
    invitation_copied: bool,

    peer_status: Loadable<PeerStatus>,
    trusted_devices: Loadable<Vec<TrustedDevice>>,
    invitation: Option<String>,
    _observe: Subscription,
}

impl DevicesPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let mut page = Self {
            state,
            scroll: widgets::PageScroll::default(),
            rename: None,
            copied: None,
            error: None,
            refresh_task: None,
            peer_action_task: None,
            rename_task: None,
            copy_task: None,
            invitation_copy_task: None,
            peer_action: None,
            refresh_generation: 0,
            invitation_copied: false,
            peer_status: Loadable::Idle,
            trusted_devices: Loadable::Idle,
            invitation: None,
            _observe: observe,
        };
        page.refresh_peer(cx);
        page
    }

    fn open_rename(&mut self, device_id: String, current: String, cx: &mut Context<Self>) {
        let input = cx.new(|cx| ComposerInput::new("Device name", cx));
        input.update(cx, |input, cx| input.set_text(current, cx));
        let events = cx.subscribe(&input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_rename(cx);
            }
        });
        self.rename = Some(RenameDialog {
            device_id,
            input,
            _events: events,
        });
        cx.notify();
    }

    fn submit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename.take() else {
            return;
        };
        let name = dialog.input.read(cx).text().trim().to_string();
        if name.is_empty() {
            cx.notify();
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = serde_json::json!({
            "op": "renameDevice",
            "deviceId": dialog.device_id,
            "name": name,
        });
        self.rename_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::MUTATE, params).await;
            this.update(cx, |page, cx| {
                if let Err(err) = result {
                    page.error = Some(format!("Rename failed: {err}").into());
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn copy_id(&mut self, device_id: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(device_id.clone()));
        self.copied = Some(device_id);
        self.copy_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            this.update(cx, |page, cx| {
                page.copied = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn refresh_peer(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.refresh_generation = self.refresh_generation.wrapping_add(1);
        let generation = self.refresh_generation;
        self.peer_status = Loadable::Loading;
        self.trusted_devices = Loadable::Loading;
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let status = engine
                .client()
                .call(methods::PEER_STATUS, serde_json::json!({}))
                .await
                .map_err(|error| error.to_string())
                .and_then(parse_peer_status);
            let devices = match &status {
                Ok(status) if can_manage_peer(status) => Some(
                    engine
                        .client()
                        .call(methods::PEER_DEVICES, serde_json::json!({}))
                        .await,
                ),
                _ => None,
            };
            this.update(cx, |page, cx| {
                if !refresh_response_is_current(page.refresh_generation, generation) {
                    return;
                }
                page.peer_status = match status {
                    Ok(status) => Loadable::Ready(status),
                    Err(error) => Loadable::Error(error),
                };
                page.trusted_devices = match devices {
                    Some(Ok(value)) => parse_trusted_devices(value)
                        .map(Loadable::Ready)
                        .unwrap_or_else(Loadable::Error),
                    Some(Err(error)) => Loadable::Error(error.to_string()),
                    None => Loadable::Ready(Vec::new()),
                };
                cx.notify();
            })
            .ok();
        }));

        cx.notify();
    }

    fn create_invitation(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        if !matches!(&self.peer_status, Loadable::Ready(status) if can_manage_peer(status))
            || !begin_peer_action(&mut self.peer_action, PeerAction::Invitation)
        {
            return;
        }
        self.invitation = None;
        self.invitation_copied = false;
        self.peer_action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::PEER_INVITE, serde_json::json!({}))
                .await;
            this.update(cx, |page, cx| {
                page.peer_action = None;
                match result {
                    Ok(value) => {
                        page.invitation = value
                            .get("code")
                            .and_then(|v| v.as_str())
                            .map(str::to_owned)
                    }
                    Err(error) => {
                        page.error = Some(format!("Could not create invitation: {error}").into())
                    }
                }
                cx.notify();
            })
            .ok();
        }));

        cx.notify();
    }

    fn copy_invitation(&mut self, cx: &mut Context<Self>) {
        let Some(code) =
            take_invitation_for_clipboard(&mut self.invitation, &mut self.invitation_copied)
        else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(code));
        // Do not retain the one-time secret after handing it to the clipboard.
        self.invitation_copy_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            this.update(cx, |page, cx| {
                page.invitation_copied = false;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn revoke_peer_device(&mut self, device_id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        if !matches!(&self.peer_status, Loadable::Ready(status) if can_manage_peer(status))
            || !begin_peer_action(&mut self.peer_action, PeerAction::Revocation)
        {
            return;
        }
        self.peer_action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::PEER_REVOKE,
                    serde_json::json!({ "deviceId": device_id }),
                )
                .await;
            this.update(cx, |page, cx| {
                page.peer_action = None;
                if let Err(error) = result {
                    page.error = Some(format!("Could not revoke device: {error}").into());
                } else {
                    page.refresh_peer(cx);
                }
                cx.notify();
            })
            .ok();
        }));

        cx.notify();
    }

    fn render_rename_dialog(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let dialog = self.rename.as_ref()?;
        let input = dialog.input.clone();
        let card = popover::dialog_card(&theme)
            .child(popover::dialog_title(&theme, "Rename device"))
            .child(
                div()
                    .mt(px(12.0))
                    .child(popover::dialog_field(input.into_any_element())),
            )
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(&theme, "Cancel", "rename-cancel")
                            .id("rename-cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.rename = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        popover::btn_primary(&theme, "Rename")
                            .id("rename-save")
                            .on_click(cx.listener(|this, _, _, cx| this.submit_rename(cx))),
                    ),
            )
            .into_any_element();
        Some(popover::modal("rename-device-dialog", viewport, card))
    }

    fn on_scroll_hovered(&mut self, hovered: &bool, _: &mut Window, cx: &mut Context<Self>) {
        if self.scroll.set_list_hovered(*hovered) {
            cx.notify();
        }
    }
}

impl popover::ScrollRailHost for DevicesPage {
    fn rail_bar(&mut self) -> &mut popover::MenuScrollbarState {
        self.scroll.rail_bar()
    }

    fn rail_scroll(&self) -> Option<gpui::ScrollHandle> {
        self.scroll.rail_scroll()
    }
}

/// Human platform label (kratos settings.devices.tsx `platformLabel`).
pub fn platform_label(platform: &str) -> &str {
    match platform {
        "macos" | "darwin" => "macOS",
        "linux" => "Linux",
        "windows" => "Windows",
        "web" => "Web",
        "ios" => "iOS",
        "android" => "Android",
        other => other,
    }
}

/// Short device id for the click-to-copy chip (`abcd1234…wxyz`).
pub fn short_id(id: &str) -> String {
    if id.len() > 12 {
        format!("{}…{}", &id[..8], &id[id.len() - 4..])
    } else {
        id.to_string()
    }
}

impl Render for DevicesPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let (devices, local_id, workspace_scope) = {
            let state = self.state.read(cx);
            (
                state.devices.clone(),
                state.local_device_id.clone(),
                state.workspace_scope,
            )
        };
        let copied = self.copied.clone();
        let dialog = self.render_rename_dialog(window.viewport_size(), cx);
        let emerald = theme.success; // emerald-400
        let count = devices.len();

        let rows: Vec<AnyElement> = devices
            .into_iter()
            .enumerate()
            .map(|(ix, device)| {
                let online = device_online(device.last_seen_at, now);
                let is_local = local_id.as_deref() == Some(device.id.as_str());
                let id_copied = copied.as_deref() == Some(device.id.as_str());
                let copy_id = device.id.clone();
                let rename_id = device.id.clone();
                let rename_name = device.name.clone();
                let platform_icon = match device.platform.as_str() {
                    "macos" | "darwin" => crate::icons::LAPTOP,
                    "web" => crate::icons::GLOBAL,
                    "ios" | "android" => crate::icons::SMARTPHONE,
                    _ => crate::icons::MONITOR,
                };
                // Presence lives ON the identity tile: a corner dot (emerald
                // online with a soft glow, faint offline), ringed by the card
                // tone so it "cuts" the tile — kratos settings.devices.tsx
                // `border-2 border-[var(--card)]` +
                // `shadow-[0_0_6px_rgba(52,211,153,0.55)]`.
                let tile = widgets::row_tile(&theme, platform_icon).relative().child(
                    div()
                        .absolute()
                        .bottom(px(-3.0))
                        .right(px(-3.0))
                        .size(px(9.0))
                        .rounded_full()
                        .border_2()
                        .border_color(theme.surface)
                        .when(online, |el| {
                            el.bg(emerald).shadow(vec![gpui::BoxShadow {
                                color: emerald.opacity(0.55),
                                offset: gpui::point(px(0.0), px(0.0)),
                                blur_radius: px(6.0),
                                spread_radius: px(0.0),
                                inset: false,
                            }])
                        })
                        .when(!online, |el| el.bg(crate::theme::ink(0.22))),
                );
                // One quiet meta line: platform · version · (offline: last
                // seen) · id chip.
                let mut meta: Vec<AnyElement> = vec![
                    div()
                        .child(SharedString::from(
                            platform_label(&device.platform).to_string(),
                        ))
                        .into_any_element(),
                ];
                if let Some(version) = device.version.as_deref().filter(|v| !v.is_empty()) {
                    meta.push(
                        div()
                            .child(SharedString::from(format!("v{version}")))
                            .into_any_element(),
                    );
                }
                if !online {
                    meta.push(
                        div()
                            .child(SharedString::from(format!(
                                "Last seen {}",
                                format_last_seen(device.last_seen_at, now)
                            )))
                            .into_any_element(),
                    );
                }
                // "Added {time ago}" — always present (kratos settings.devices.tsx).
                if let Some(created) = device.created_at {
                    meta.push(
                        div()
                            .child(SharedString::from(format!(
                                "Added {}",
                                format_last_seen(Some(created), now)
                            )))
                            .into_any_element(),
                    );
                }
                meta.push(
                    div()
                        .id(("device-id", ix))
                        .font_family(theme.font_mono.clone())
                        .text_size(crate::typography::ui_rems(10.5))
                        .text_color(if id_copied {
                            theme.success_muted.opacity(0.9)
                        } else {
                            theme.text_muted.opacity(0.5)
                        })
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.text_muted))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.copy_id(copy_id.clone(), cx);
                        }))
                        .child(SharedString::from(if id_copied {
                            "Copied".to_string()
                        } else {
                            short_id(&device.id)
                        }))
                        .into_any_element(),
                );

                widgets::card_row(&theme, ix == 0)
                    .child(tile)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, device.name.clone()))
                            .child(widgets::meta_line(&theme, meta)),
                    )
                    .when(is_local, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(px(10.5))
                                .text_color(theme.text_muted)
                                .child(if workspace_scope == Some(WorkspaceScope::Local) {
                                    "Local only"
                                } else {
                                    "This device"
                                }),
                        )
                    })
                    .child(
                        // `opacity-70 hover:opacity-100` (kratos: also rises on
                        // row hover — gpui has no group-hover, so the button's
                        // own hover carries the reveal).
                        widgets::ghost_action(&theme)
                            .id(("device-rename", ix))
                            .opacity(0.7)
                            .hover(|s| {
                                s.opacity(1.0)
                                    .bg(crate::theme::ink(0.06))
                                    .text_color(theme.text)
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_rename(rename_id.clone(), rename_name.clone(), cx);
                            }))
                            .child(
                                crate::icons::icon(crate::icons::PEN)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from("Rename")),
                    )
                    .into_any_element()
            })
            .collect();

        let card = widgets::section_card(&theme);
        let card = if rows.is_empty() {
            card.child(
                div()
                    .px(px(20.0))
                    .py(px(40.0))
                    .text_center()
                    .text_size(crate::typography::ui_rems(14.0))
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from("No devices registered")),
            )
        } else {
            card.children(rows)
        };

        let peer_card = match (&self.peer_status, &self.trusted_devices) {
            (Loadable::Ready(status), trusted) => {
                let status_text = connectivity_label(status);

                let can_manage = can_manage_peer(status);
                let own_id = status.device_id.clone();
                let mut panel = widgets::section_card(&theme).child(
                    div().px(px(16.0)).py(px(13.0)).flex().items_center().justify_between()
                        .child(div().flex().flex_col()
                            .child(widgets::row_title(&theme, "Private sync peer"))
                            .child(div().mt(px(3.0)).text_size(crate::typography::ui_rems(11.0)).text_color(theme.text_muted)
                                .child(SharedString::from(if status.hosting {
                                    format!("{status_text} · this device is the durable always-on peer")
                                } else {
                                    format!("{status_text} · managed by your trusted peer")
                                })))
                        )
                        .child(div().flex().gap(px(8.0))
                            .child(popover::btn_ghost(&theme, "Refresh", "peer-refresh").id("peer-refresh")
                                .on_click(cx.listener(|this, _, _, cx| this.refresh_peer(cx))))
                            .when(can_manage && status.connected, |el| el.child(
                                popover::btn_primary(
                                    &theme,
                                    if self.peer_action == Some(PeerAction::Invitation) {
                                        "Creating…"
                                    } else {
                                        "New invitation"
                                    },
                                ).id("peer-invite")

                                    .when(self.peer_action.is_some(), |button| button.opacity(0.5))
                                    .on_click(cx.listener(|this, _, _, cx| this.create_invitation(cx)))
                            )))
                );

                if !can_manage && status.signed_in {
                    panel = panel.child(
                        div().border_t_1().border_color(theme.border).p(px(14.0))
                            .text_size(crate::typography::ui_rems(12.0)).text_color(theme.text_muted)
                            .child("Trusted devices, invitations, and revocation are managed on the durable owner peer. This paired device can sync but cannot change trust."),
                    );
                }
                if self.invitation_copied {
                    panel = panel.child(
                        div()
                            .border_t_1()
                            .border_color(theme.border)
                            .px(px(16.0))
                            .py(px(12.0))
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text)
                            .child("Invitation copied. The secret was cleared from this window."),
                    );
                }
                if let Some(code) = self.invitation.as_ref() {
                    panel = panel.child(
                        div().border_t_1().border_color(theme.border).px(px(16.0)).py(px(12.0)).flex().items_center().gap(px(10.0))
                            .child(div().flex_1().min_w_0().flex().flex_col()
                                .child(div().text_size(crate::typography::ui_rems(12.0)).text_color(theme.text).child("One-time secret invitation ready"))
                                .child(div().mt(px(2.0)).text_size(crate::typography::ui_rems(10.5)).text_color(theme.text_muted)
                                    .child("Copy it now. Kratos does not save it in UI settings.")))
                            .child(popover::btn_primary(&theme, "Copy invitation")
                                .id("peer-copy-invite").on_click(cx.listener(|this, _, _, cx| this.copy_invitation(cx))))
                    );
                    // Keep the actual secret out of the element tree and logs.
                    let _ = code;
                }
                if can_manage && let Loadable::Ready(rows) = trusted {
                    for (ix, device) in rows.iter().enumerate() {
                        let revoked = device.revoked_at.is_some();
                        let is_self = own_id.as_deref() == Some(device.device_id.as_str());
                        let revoke_id = device.device_id.clone();
                        panel = panel.child(
                            div()
                                .border_t_1()
                                .border_color(theme.border)
                                .px(px(16.0))
                                .py(px(12.0))
                                .flex()
                                .items_center()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .flex()
                                        .flex_col()
                                        .child(widgets::row_title(
                                            &theme,
                                            device
                                                .display_name
                                                .clone()
                                                .unwrap_or_else(|| "Unnamed device".into()),
                                        ))
                                        .child(
                                            div()
                                                .mt(px(2.0))
                                                .text_size(crate::typography::ui_rems(10.5))
                                                .text_color(theme.text_muted)
                                                .child(SharedString::from(format!(
                                                    "{}{} · {}",
                                                    short_id(&device.device_id),
                                                    if device.owner { " · owner" } else { "" },
                                                    if revoked { "revoked" } else { "trusted" }
                                                ))),
                                        )
                                        .when(is_self, |el| {
                                            el.child(
                                                div()
                                                    .text_size(crate::typography::ui_rems(10.5))
                                                    .text_color(theme.text_muted)
                                                    .child("This device"),
                                            )
                                        })
                                        .when(!is_self && !revoked, |el| {
                                            el.child(
                                                widgets::ghost_action(&theme)
                                                    .id(("peer-revoke", ix))
                                                    .child("Revoke")
                                                    .when(self.peer_action.is_some(), |button| {
                                                        button.opacity(0.5)
                                                    })
                                                    .on_click(cx.listener(
                                                        move |this, _, _, cx| {
                                                            this.revoke_peer_device(
                                                                revoke_id.clone(),
                                                                cx,
                                                            )
                                                        },
                                                    )),
                                            )
                                        }),
                                ),
                        );
                    }
                } else if can_manage && let Loadable::Error(message) = trusted {
                    panel = panel.child(div().border_t_1().border_color(theme.border).p(px(14.0)).text_size(crate::typography::ui_rems(12.0)).text_color(theme.danger_muted).child(SharedString::from(format!("Trusted devices unavailable: {message}. The peer may be temporarily offline."))));
                }
                panel.into_any_element()
            }
            (Loadable::Error(message), _) => widgets::section_card(&theme)
                .child(
                    div()
                        .p(px(16.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.danger_muted)
                        .child(SharedString::from(format!(
                            "Peer status unavailable: {message}"
                        ))),
                )
                .into_any_element(),
            _ => widgets::section_card(&theme)
                .child(
                    div()
                        .p(px(16.0))
                        .text_color(theme.text_muted)
                        .child("Loading private sync status…"),
                )
                .into_any_element(),
        };

        let scrollbar = popover::rail(self, "devices-page-scrollbar", &theme, cx);

        div()
            .id("devices-page-host")
            .relative()
            .size_full()
            .on_hover(cx.listener(Self::on_scroll_hovered))
            .child(
                div()
                    .id("devices-page")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll.scroll)
                    .child(
                        widgets::page_column()
                            .child(widgets::page_header(
                                &theme,
                                "Devices",
                                (count > 0).then_some(count),
                            ))
                            .child(widgets::page_subtitle(
                                &theme,
                                devices_subtitle(workspace_scope),
                            ))
                            .when_some(self.error.clone(), |el, message| {
                                el.child(
                                    widgets::error_strip(&theme, message)
                                        .id("devices-error")
                                        .cursor_pointer()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.error = None;
                                            cx.notify();
                                        })),
                                )
                            })
                            .child(peer_card)
                            .child(div().h(px(16.0)))
                            .child(card),
                    ),
            )
            .children(scrollbar)
            .when_some(dialog, |el, dialog| el.child(dialog))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    #[test]
    fn presence_window() {
        let now = Utc::now();
        assert!(device_online(Some(now - TimeDelta::seconds(10)), now));
        assert!(device_online(Some(now - TimeDelta::seconds(70)), now));
        assert!(!device_online(Some(now - TimeDelta::seconds(71)), now));
        assert!(!device_online(None, now));
        // Clock skew (future) counts as online.
        assert!(device_online(Some(now + TimeDelta::seconds(30)), now));
    }

    #[test]
    fn last_seen_formatting() {
        let now = Utc::now();
        assert_eq!(format_last_seen(None, now), "never seen");
        assert_eq!(
            format_last_seen(Some(now - TimeDelta::seconds(30)), now),
            "just now"
        );
        assert_eq!(
            format_last_seen(Some(now - TimeDelta::minutes(5)), now),
            "5m ago"
        );
        assert_eq!(
            format_last_seen(Some(now - TimeDelta::hours(3)), now),
            "3h ago"
        );
        assert_eq!(
            format_last_seen(Some(now - TimeDelta::days(2)), now),
            "2d ago"
        );
    }

    #[test]
    fn local_subtitle_does_not_claim_synced_metadata() {
        let copy = devices_subtitle(Some(WorkspaceScope::Local));
        assert!(copy.contains("local workspace"));
        assert!(!copy.contains("synced"));
    }

    #[test]
    fn only_the_durable_owner_peer_can_manage_trust() {
        let owner = PeerStatus {
            signed_in: true,
            hosting: true,
            ..PeerStatus::default()
        };
        let member = PeerStatus {
            signed_in: true,
            hosting: false,
            ..PeerStatus::default()
        };
        assert!(can_manage_peer(&owner));
        assert!(!can_manage_peer(&member));
    }

    #[test]
    fn copying_invitation_clears_secret_but_keeps_nonsecret_ack() {
        let mut invitation = Some("kratos-pair:secret".to_string());
        let mut copied = false;
        assert_eq!(
            take_invitation_for_clipboard(&mut invitation, &mut copied).as_deref(),
            Some("kratos-pair:secret")
        );
        assert!(invitation.is_none());
        assert!(copied);
    }

    #[test]
    fn stale_refresh_cannot_overwrite_a_newer_request() {
        assert!(refresh_response_is_current(2, 2));
        assert!(!refresh_response_is_current(2, 1));
    }

    #[test]
    fn peer_actions_are_pending_before_completion_and_serialized() {
        let mut pending = None;
        assert!(begin_peer_action(&mut pending, PeerAction::Invitation));
        assert_eq!(pending, Some(PeerAction::Invitation));
        assert!(!begin_peer_action(&mut pending, PeerAction::Revocation));
        pending = None;
        assert!(begin_peer_action(&mut pending, PeerAction::Revocation));
        assert_eq!(pending, Some(PeerAction::Revocation));
        assert!(refresh_response_is_current(9, 9));
    }
}
