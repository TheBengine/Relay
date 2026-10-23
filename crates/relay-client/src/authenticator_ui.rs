//! Secrets are scoped to one open account page, never preferences/admin cache.
use super::*;
use qrcodegen::{QrCode, QrCodeEcc};
use serde_json::{json, Value};
use zeroize::{Zeroize, Zeroizing};

#[derive(Default)]
pub(super) struct State {
    origin: Option<CommandOrigin>,
    pending: Option<String>,
    enabled: Option<bool>,
    status_tried: bool,
    remaining: u64,
    password: Zeroizing<String>,
    code: Zeroizing<String>,
    setup_id: Option<String>,
    secret: Zeroizing<String>,
    uri: Zeroizing<String>,
    expires: Option<Instant>,
    recovery: Vec<Zeroizing<String>>,
    saved: bool,
    editing: bool,
}

pub(super) fn sign_in_field(
    ui: &mut egui::Ui,
    flow: &mut crate::connection_setup::JoinFlow,
    error: Option<&str>,
) -> bool {
    let binding = (flow.url.clone(), flow.username.clone(), flow.registering);
    if flow.factor_binding.as_ref() != Some(&binding) {
        flow.second_factor.zeroize();
        flow.factor_binding = Some(binding);
    }
    if flow.registering {
        return false;
    }
    let required = error.is_some_and(|error| {
        matches!(
            error,
            relay_protocol::AUTHENTICATOR_REQUIRED | relay_protocol::AUTHENTICATOR_INVALID
        )
    });
    let mut enter = false;
    egui::CollapsingHeader::new("Authenticator or recovery code")
        .id_source(("sign-in-factor", &flow.url, &flow.username, required))
        .default_open(required)
        .show(ui, |ui| {
            let mut field = ui.add(
                egui::TextEdit::singleline(&mut flow.second_factor)
                    .password(true)
                    .char_limit(64)
                    .hint_text("6-digit code or recovery code")
                    .desired_width(f32::INFINITY),
            );
            crate::accessible_input::apply(ui, &mut field, &mut flow.second_factor, 64);
            enter = field.lost_focus();
            ui.label(
                RichText::new(
                    "Only needed if you enabled an authenticator on this community account.",
                )
                .size(font_size(TEXT_CAPTION))
                .color(c_muted()),
            );
        });
    enter
}

pub(super) fn secret_field(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut Zeroizing<String>,
    max: usize,
) {
    ui.label(label);
    let mut response = ui.add(
        egui::TextEdit::singleline(&mut **value)
            .password(true)
            .char_limit(max)
            .desired_width(f32::INFINITY),
    );
    crate::accessible_input::apply(ui, &mut response, value, max);
}

fn qr(ui: &mut egui::Ui, uri: &str) {
    let Ok(code) = QrCode::encode_text(uri, QrCodeEcc::Medium) else {
        ui.label("Use the manual key below in your authenticator app.");
        return;
    };
    let modules = code.size() + 8;
    let side = ui.available_width().min(248.0);
    let unit = side / modules as f32;
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(side), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, Color32::WHITE);
    for y in 0..code.size() {
        for x in 0..code.size() {
            if code.get_module(x, y) {
                let min = rect.min + Vec2::new((x + 4) as f32 * unit, (y + 4) as f32 * unit);
                ui.painter().rect_filled(
                    Rect::from_min_size(min, Vec2::splat(unit + 0.15)),
                    0.0,
                    Color32::BLACK,
                );
            }
        }
    }
}

impl RelayApp {
    fn authenticator_request(&mut self, action: &str) {
        if self.authenticator_ui.pending.is_some() {
            return;
        }
        let Some(origin) = self.command_origin() else {
            return;
        };
        if self.authenticator_ui.origin.as_ref() != Some(&origin) {
            return;
        }
        if action == "status" {
            self.authenticator_ui.status_tried = true;
        }
        let generation = self.next_request();
        let key = format!("authenticator:{action}:{generation}");
        let body = if action == "status" {
            Value::Null
        } else {
            json!({"action":action,"password":self.authenticator_ui.password.as_str(),"code":self.authenticator_ui.code.trim(),"setup_id":self.authenticator_ui.setup_id})
        };
        self.admin_request(
            if action == "status" {
                AdminMethod::Get
            } else {
                AdminMethod::Post
            },
            "/api/authenticator".into(),
            body,
            key.clone(),
        );
        if self.admin_requests.values().any(|pending| pending == &key) {
            self.authenticator_ui.pending = Some(key);
        }
        self.authenticator_ui.code.zeroize();
        if action != "setup" {
            self.authenticator_ui.password.zeroize();
        }
    }

    pub(super) fn authenticator_failed(&mut self, key: &str) {
        if self.authenticator_ui.pending.as_deref() == Some(key) {
            self.authenticator_ui.pending = None;
            self.authenticator_ui.password.zeroize();
            self.authenticator_ui.code.zeroize();
        }
    }

    pub(super) fn authenticator_result(&mut self, key: &str, mut data: Value) {
        if self.authenticator_ui.pending.as_deref() != Some(key)
            || self.authenticator_ui.origin.as_ref() != self.command_origin().as_ref()
        {
            return;
        }
        self.authenticator_ui.pending = None;
        let state = &mut self.authenticator_ui;
        if key.starts_with("authenticator:setup:") {
            let setup = data["setup_id"]
                .as_str()
                .filter(|id| uuid::Uuid::parse_str(id).is_ok())
                .map(str::to_owned);
            let secret = data["secret"]
                .as_str()
                .filter(|s| s.len() <= 128)
                .map(str::to_owned);
            let uri = data["otpauth_uri"]
                .as_str()
                .filter(|s| s.starts_with("otpauth://totp/") && s.len() <= 4096)
                .map(str::to_owned);
            if let (Some(id), Some(secret), Some(uri)) = (setup, secret, uri) {
                state.setup_id = Some(id);
                state.secret = Zeroizing::new(secret);
                state.uri = Zeroizing::new(uri);
                state.expires = Some(
                    Instant::now()
                        + Duration::from_secs(data["expires_in"].as_u64().unwrap_or(600).min(600)),
                );
                state.editing = true;
            } else {
                self.error = Some(
                    "The server returned an incomplete authenticator setup. Start again.".into(),
                );
            }
        } else if let Some(enabled) = data["enabled"].as_bool() {
            state.enabled = Some(enabled);
            state.password.zeroize();
            state.code.zeroize();
            state.secret.zeroize();
            state.uri.zeroize();
            state.setup_id = None;
            state.expires = None;
            state.recovery.clear();
            state.saved = false;
            state.editing = false;
            if let Some(codes) = data["recovery_codes"].as_array() {
                // Accept only bounded recovery code text; never server-provided markup.
                for value in codes.iter().take(10) {
                    if let Some(code) = value.as_str().filter(|code| {
                        code.len() == 35 && code.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
                    }) {
                        state.recovery.push(Zeroizing::new(code.to_owned()));
                    }
                }
                state.remaining = state.recovery.len() as u64;
                self.admin_data.remove("sessions");
            } else {
                state.remaining = data["recovery_codes_remaining"]
                    .as_u64()
                    .unwrap_or(0)
                    .min(10);
            }
        } else {
            self.error =
                Some("Authenticator settings could not be read. Refresh and try again.".into());
        }
        // Do not leave the original JSON string buffers alive until the next UI frame.
        fn erase(value: &mut Value) {
            match value {
                Value::String(text) => text.zeroize(),
                Value::Array(values) => values.iter_mut().for_each(erase),
                Value::Object(values) => values.values_mut().for_each(erase),
                _ => {}
            }
        }
        erase(&mut data);
    }

    pub(super) fn discard_hidden_authenticator(&mut self) {
        let visible = self.settings_open
            && self.settings_tab == SettingsTab::Account
            && self.session.is_some();
        if !visible || self.authenticator_ui.origin.as_ref() != self.command_origin().as_ref() {
            self.authenticator_ui = State::default();
        }
    }

    pub(super) fn authenticator_settings(&mut self, ui: &mut egui::Ui) {
        if self
            .bootstrap
            .as_ref()
            .is_some_and(|b| b.account_login_only)
        {
            ui.label(
                "Manage your password, authenticator and recovery at the Relay account service.",
            );
            return;
        }
        if !self
            .bootstrap
            .as_ref()
            .is_some_and(|b| b.capabilities.iter().any(|cap| cap == "authenticator-v1"))
        {
            return;
        }
        let origin = self.command_origin();
        if self.authenticator_ui.origin != origin {
            self.authenticator_ui = State {
                origin,
                ..State::default()
            };
        }
        settings_heading(ui, "Authenticator", "Optional extra protection for this community account. Your chat encryption keys stay separate.");
        if !self.authenticator_ui.status_tried && self.authenticator_ui.pending.is_none() {
            self.authenticator_request("status");
        }
        let pending = self.authenticator_ui.pending.is_some();
        if !self.authenticator_ui.recovery.is_empty() {
            ui.label(RichText::new("Save these recovery codes now").strong());
            ui.label("Each code works once, together with your password. Store them somewhere safe. Closing this page hides them permanently.");
            for code in &self.authenticator_ui.recovery {
                ui.monospace(code.as_str());
            }
            if ui.button("Copy recovery codes").clicked() {
                ui.output_mut(|out| {
                    out.copied_text = self
                        .authenticator_ui
                        .recovery
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                });
            }
            ui.checkbox(
                &mut self.authenticator_ui.saved,
                "I have saved my recovery codes",
            );
            if ui
                .add_enabled(self.authenticator_ui.saved, secondary("Done"))
                .clicked()
            {
                self.authenticator_ui.recovery.clear();
            }
            ui.separator();
            return;
        }
        if let Some(enabled) = self.authenticator_ui.enabled {
            ui.label(if enabled {
                "Enabled · a code is required when signing in again."
            } else {
                "Not enabled. You can keep using your password."
            });
            if enabled {
                ui.label(format!(
                    "{} recovery codes remaining",
                    self.authenticator_ui.remaining
                ));
            }
            if !self.authenticator_ui.editing {
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(
                            !pending,
                            secondary(if enabled {
                                "Manage authenticator"
                            } else {
                                "Set up authenticator"
                            }),
                        )
                        .clicked()
                    {
                        self.authenticator_ui.editing = true;
                    }
                    if ui.add_enabled(!pending, secondary("Refresh")).clicked() {
                        self.authenticator_request("status");
                    }
                });
            } else {
                if self
                    .authenticator_ui
                    .expires
                    .is_some_and(|expires| Instant::now() >= expires)
                {
                    self.authenticator_ui.secret.zeroize();
                    self.authenticator_ui.uri.zeroize();
                    self.authenticator_ui.setup_id = None;
                    self.authenticator_ui.expires = None;
                    ui.label("Setup expired. Start again to get a new key.");
                }
                if !self.authenticator_ui.uri.is_empty() {
                    ui.label(
                        "1. Scan this QR code with your authenticator app. Keep the key private.",
                    );
                    qr(ui, &self.authenticator_ui.uri);
                    ui.collapsing("Enter a key manually", |ui| {
                        ui.monospace(self.authenticator_ui.secret.as_str());
                    });
                    ui.label("2. Enter the current 6-digit code to finish. Nothing is enabled until it matches.");
                }
                secret_field(
                    ui,
                    "Current password",
                    &mut self.authenticator_ui.password,
                    256,
                );
                if enabled || self.authenticator_ui.setup_id.is_some() {
                    secret_field(
                        ui,
                        if enabled {
                            "Authenticator or recovery code"
                        } else {
                            "6-digit authenticator code"
                        },
                        &mut self.authenticator_ui.code,
                        64,
                    );
                }
                let ready = !pending && !self.authenticator_ui.password.is_empty();
                ui.horizontal_wrapped(|ui| {
                    if !enabled {
                        let setup = self.authenticator_ui.setup_id.is_some();
                        if ui
                            .add_enabled(
                                ready && (!setup || self.authenticator_ui.code.len() == 6),
                                primary(if setup {
                                    "Enable authenticator"
                                } else {
                                    "Show setup code"
                                }),
                            )
                            .clicked()
                        {
                            self.authenticator_request(if setup { "enable" } else { "setup" });
                        }
                    } else {
                        if ui
                            .add_enabled(
                                ready && !self.authenticator_ui.code.is_empty(),
                                primary("New recovery codes"),
                            )
                            .clicked()
                        {
                            self.authenticator_request("recovery");
                        }
                        if ui
                            .add_enabled(
                                ready && !self.authenticator_ui.code.is_empty(),
                                secondary("Disable authenticator"),
                            )
                            .clicked()
                        {
                            self.authenticator_request("disable");
                        }
                    }
                    if ui.add_enabled(!pending, secondary("Cancel")).clicked() {
                        let origin = self.authenticator_ui.origin.clone();
                        let enabled = self.authenticator_ui.enabled;
                        let remaining = self.authenticator_ui.remaining;
                        self.authenticator_ui = State {
                            origin,
                            enabled,
                            status_tried: true,
                            remaining,
                            ..State::default()
                        };
                    }
                });
                ui.label(RichText::new("Enabling, disabling or replacing recovery codes signs out your other sessions. Use a new authenticator code if the previous one was just used.").size(font_size(TEXT_CAPTION)).color(c_muted()));
            }
        } else if pending {
            ui.label("Loading account protection…");
        } else {
            ui.label("Account protection could not be loaded.");
            if ui.button("Try again").clicked() {
                self.authenticator_request("status");
            }
        }
        ui.add_space(18.0);
    }
}
