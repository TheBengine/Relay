// Included by admin_ui.rs: one owner-only server policy editor, not role schema.
const UPLOAD_SETTINGS_PATH: &str = "/api/settings/uploads";
const UPLOAD_MIB: u64 = 1024 * 1024;

#[derive(Default)]
struct UploadSettings {
    scope: String,
    baseline: Option<relay_protocol::UploadPolicy>,
    draft: Option<relay_protocol::UploadPolicy>,
    submitted: Option<relay_protocol::UploadPolicy>,
    requested: bool,
    pending: bool,
    saving: bool,
    error: Option<String>,
    saved: bool,
}

fn upload_policy_owner(bootstrap: &Bootstrap) -> bool {
    bootstrap.user.role == "admin"
}

fn upload_policy_key(scope: &str, saving: bool) -> String {
    use sha2::Digest;
    format!("{}{:x}", if saving { "action:uploads:" } else { "uploads:" },
        sha2::Sha256::digest(scope.as_bytes()))
}

fn upload_policy_fields_match(a: &relay_protocol::UploadPolicy, b: &relay_protocol::UploadPolicy) -> bool {
    a.default_file_bytes == b.default_file_bytes && a.max_file_bytes == b.max_file_bytes
        && a.role_limits == b.role_limits
}

fn upload_limit_label(bytes: u64) -> String {
    if bytes == 0 { "Disabled".into() }
    else if bytes.is_multiple_of(UPLOAD_MIB) { format!("{} MiB", bytes / UPLOAD_MIB) }
    else { format!("{:.2} MiB ({bytes} bytes)", bytes as f64 / UPLOAD_MIB as f64) }
}

fn upload_limit_choice(ui: &mut egui::Ui, id: impl std::hash::Hash, current: &mut u64) {
    egui::ComboBox::from_id_source(id).selected_text(upload_limit_label(*current)).show_ui(ui, |ui| {
        for mib in 0..=25 {
            let bytes = mib * UPLOAD_MIB;
            ui.selectable_value(current, bytes, upload_limit_label(bytes));
        }
    });
}

fn decode_upload_policy_reply(data: &Value, submitted: Option<&relay_protocol::UploadPolicy>)
    -> Result<relay_protocol::UploadPolicy, String>
{
    if !["revision", "default_file_bytes", "max_file_bytes", "role_limits"].iter()
        .all(|field| data.get(*field).is_some()) {
        return Err("The server returned incomplete upload limits. Reload before saving.".into());
    }
    let policy: relay_protocol::UploadPolicy = serde_json::from_value(data.clone())
        .map_err(|_| "The server returned invalid upload limits. Reload before saving.")?;
    if !policy.valid() {
        return Err("The server returned invalid upload limits. Reload before saving.".into());
    }
    if submitted.is_some_and(|sent| sent.revision.checked_add(1) != Some(policy.revision)
        || !upload_policy_fields_match(sent, &policy)) {
        return Err("The upload settings changed unexpectedly. Your draft is kept; reload before saving.".into());
    }
    Ok(policy)
}

impl RelayApp {
    fn upload_settings_scope(&self) -> Option<String> {
        self.command_origin().map(|origin| format!("{}|{}|{}", origin.url, origin.server_id, origin.user_id))
    }

    fn request_upload_settings(&mut self, submitted: Option<relay_protocol::UploadPolicy>) {
        let saving = submitted.is_some();
        let state = &mut self.admin_ui.uploads;
        state.requested = true;
        state.pending = true;
        state.saving = saving;
        state.submitted = submitted.clone();
        state.error = None;
        state.saved = false;
        let key = upload_policy_key(&state.scope, saving);
        let body = submitted.map_or(Value::Null, |policy| serde_json::to_value(policy).unwrap());
        self.admin_request(if saving { AdminMethod::Put } else { AdminMethod::Get },
            UPLOAD_SETTINGS_PATH.into(), body, key);
    }

    /// Consume only matching, validated replies. A stale or malformed response
    /// never becomes a saved policy through the generic admin success path.
    fn upload_settings_received(&mut self, key: &str, data: &Value) -> bool {
        if !key.starts_with("uploads:") && !key.starts_with("action:uploads:") { return false }
        let Some(scope) = self.upload_settings_scope() else { return true };
        let state = &mut self.admin_ui.uploads;
        if scope != state.scope || !state.pending || key != upload_policy_key(&scope, state.saving) {
            return true;
        }
        state.pending = false;
        let policy = match decode_upload_policy_reply(data, state.submitted.as_ref()) {
            Ok(policy) => policy,
            Err(error) => {
                state.error = Some(error);
                return true;
            }
        };
        state.saved = state.saving;
        state.submitted = None;
        state.baseline = Some(policy.clone());
        state.draft = Some(policy);
        true
    }

    pub(super) fn upload_settings_ui(&mut self, ui: &mut egui::Ui, bootstrap: &Bootstrap) {
        if !upload_policy_owner(bootstrap) { return }
        if !bootstrap.capabilities.iter().any(|capability| capability == "upload-policy-v1") {
            settings_heading(ui, "Uploads", "File allowances for this community.");
            ui.label("Update this server to configure upload limits and role allowances. Its current file limit still applies.");
            return;
        }
        let Some(scope) = self.upload_settings_scope() else { return };
        if self.admin_ui.uploads.scope != scope {
            self.admin_ui.uploads = UploadSettings { scope, ..Default::default() };
        }
        if !self.admin_ui.uploads.requested { self.request_upload_settings(None); }
        let state = &mut self.admin_ui.uploads;
        if state.pending && !self.admin_requests.values().any(|key|
            key == &upload_policy_key(&state.scope, state.saving)) {
            state.pending = false;
            state.error = Some("The request did not complete. Your draft is kept. Reload the current server settings before saving again.".into());
        }
        settings_heading(ui, "Uploads", "File allowances for this community.");
        crate::settings_ui::description(ui,
            "Choose the default and role allowances for files. Only server administrators can edit this policy.");
        crate::settings_ui::description(ui,
            "Limits cover the whole transferred file, including encryption overhead. Relay supports at most 25 MiB per file and 50 MiB of pending files.");
        let mut reload = false;
        let mut save = None;
        if let Some(error) = &state.error { ui.colored_label(c_bad(), error); }
        if state.pending {
            ui.horizontal(|ui| { ui.spinner(); ui.label(if state.saving { "Saving upload limits…" } else { "Loading upload limits…" }); });
        }
        let pending = state.pending;
        let has_error = state.error.is_some();
        if let Some(draft) = state.draft.as_mut() {
            ui.add_enabled_ui(!pending, |ui| {
                ui.horizontal(|ui| { ui.label("Server ceiling"); upload_limit_choice(ui, "upload-ceiling", &mut draft.max_file_bytes); });
                ui.horizontal(|ui| { ui.label("Default for members"); upload_limit_choice(ui, "upload-default", &mut draft.default_file_bytes); });
                if draft.default_file_bytes > draft.max_file_bytes {
                    ui.colored_label(c_bad(), "The default must not exceed the server ceiling.");
                }
                crate::settings_ui::description(ui,
                    "Role entries inherit the default unless set explicitly. The largest explicit allowance among a member's roles wins, capped by the server ceiling. Disabled means zero only when no other explicit role grants more. Server administrators use the ceiling.");
                for role in &bootstrap.roles {
                    let mut choice = draft.role_limits.get(&role.id).copied();
                    ui.horizontal(|ui| {
                        ui.label(&role.name);
                        egui::ComboBox::from_id_source(("upload-role", &role.id))
                            .selected_text(choice.map_or_else(|| "Inherit default".into(), upload_limit_label))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut choice, None, "Inherit default");
                                for mib in 0..=25 {
                                    ui.selectable_value(&mut choice, Some(mib * UPLOAD_MIB), upload_limit_label(mib * UPLOAD_MIB));
                                }
                            });
                    });
                    if let Some(bytes) = choice { draft.role_limits.insert(role.id.clone(), bytes); }
                    else { draft.role_limits.remove(&role.id); }
                }
                let unavailable: Vec<_> = draft.role_limits.keys()
                    .filter(|id| !bootstrap.roles.iter().any(|role| role.id.as_str() == id.as_str())).cloned().collect();
                for id in unavailable {
                    ui.horizontal(|ui| {
                        ui.label("An unavailable role still has an override.");
                        if ui.button("Remove override").clicked() { draft.role_limits.remove(&id); }
                    });
                }
            });
            let dirty = state.baseline.as_ref().is_some_and(|baseline| !upload_policy_fields_match(baseline, draft));
            if dirty { ui.label("Unsaved upload limits"); }
            else if state.saved { ui.label("Upload limits saved."); }
            ui.horizontal(|ui| {
                if ui.add_enabled(dirty && draft.valid() && !pending && !has_error, primary("Save upload limits")).clicked() {
                    save = Some(draft.clone());
                }
                if ui.add_enabled(!pending, secondary(if dirty { "Reload and discard edits" } else { "Reload settings" })).clicked() { reload = true; }
            });
        } else if ui.add_enabled(!state.pending, secondary("Retry loading upload limits")).clicked() {
            reload = true;
        }
        if reload { self.request_upload_settings(None); }
        if let Some(policy) = save { self.request_upload_settings(Some(policy)); }
    }
}

#[cfg(test)]
mod upload_settings_tests {
    use super::*;
    #[test]
    fn upload_policy_owner_and_keys_do_not_grant_delegated_admin_authority() {
        let mut bootstrap = Bootstrap { user: User { role: "member".into(), ..Default::default() },
            permissions: PERMISSION_ADMIN, ..Default::default() };
        assert!(!upload_policy_owner(&bootstrap));
        bootstrap.user.role = "admin".into();
        assert!(upload_policy_owner(&bootstrap));
        assert_ne!(upload_policy_key("server-a|owner", false), upload_policy_key("server-b|owner", false));
        assert_ne!(upload_policy_key("server-a|owner", false), upload_policy_key("server-a|owner", true));
    }
    #[test]
    fn upload_editor_preserves_inherit_zero_and_exact_custom_byte_allowances() {
        let mut policy = relay_protocol::UploadPolicy { max_file_bytes: 25 * UPLOAD_MIB, ..Default::default() };
        policy.role_limits.insert("disabled".into(), 0);
        policy.role_limits.insert("custom".into(), 1_234_567);
        assert!(policy.valid());
        assert!(!policy.role_limits.contains_key("inherit"));
        assert_eq!(upload_limit_label(0), "Disabled");
        assert!(upload_limit_label(1_234_567).contains("1234567 bytes"));
        let restored: relay_protocol::UploadPolicy = serde_json::from_value(serde_json::to_value(&policy).unwrap()).unwrap();
        assert!(upload_policy_fields_match(&policy, &restored));
    }
    #[test]
    fn upload_policy_reply_requires_complete_valid_and_exact_revision_acknowledgement() {
        let sent = relay_protocol::UploadPolicy { revision: 9, max_file_bytes: 25 * UPLOAD_MIB, ..Default::default() };
        assert!(decode_upload_policy_reply(&json!({}), None).is_err());
        let mut reply = sent.clone();
        assert!(decode_upload_policy_reply(&serde_json::to_value(&reply).unwrap(), Some(&sent)).is_err());
        reply.revision += 1;
        assert!(decode_upload_policy_reply(&serde_json::to_value(&reply).unwrap(), Some(&sent)).is_ok());
        reply.default_file_bytes = 5 * UPLOAD_MIB;
        assert!(decode_upload_policy_reply(&serde_json::to_value(&reply).unwrap(), Some(&sent)).is_err());
        reply.max_file_bytes = relay_protocol::MAX_SUPPORTED_UPLOAD_BYTES as u64 + 1;
        assert!(decode_upload_policy_reply(&serde_json::to_value(&reply).unwrap(), None).is_err());
    }
}
