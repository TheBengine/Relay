fn member_rank(bootstrap: &Bootstrap, user: &str) -> u32 {
    match bootstrap
        .members
        .iter()
        .find(|m| m.id == user)
        .map(|m| m.role.as_str())
        .unwrap_or("member")
    {
        "admin" => 1002,
        "moderator" => 1001,
        _ => bootstrap
            .member_roles
            .iter()
            .find(|m| m.user_id == user)
            .map(|m| {
                bootstrap
                    .roles
                    .iter()
                    .filter(|r| m.role_ids.contains(&r.id))
                    .map(|r| r.position)
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0),
    }
}
impl RelayApp {
    pub(super) fn role_badges(&self, ui: &mut egui::Ui, user: &str) {
        let Some(b) = self.bootstrap.as_ref() else {
            return;
        };
        let Some(assignment) = b.member_roles.iter().find(|m| m.user_id == user) else {
            return;
        };
        ui.horizontal_wrapped(|ui| {
            for role in b
                .roles
                .iter()
                .filter(|r| assignment.role_ids.contains(&r.id))
            {
                egui::Frame::none()
                    .fill(c_card())
                    .rounding(12.0)
                    .inner_margin(Margin::symmetric(8.0, 3.0))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(format!("● {}", role.name))
                                .color(profile_ui::profile_color(role.color))
                                .size(font_size(11.0)),
                        );
                    });
            }
        });
    }
    fn custom_roles_ui(&mut self, ui: &mut egui::Ui, bootstrap: &Bootstrap) {
        self.ensure_admin_data("roles", "/api/roles");
        crate::settings_ui::page_heading(
            ui,
            "Roles",
            "Create roles, colors and permissions. Higher positions rank above lower ones.",
        );
        ui.label("Built-in administrator and moderator ranks stay above custom roles. Delegated role managers can manage only lower roles and permissions they already hold.");
        let pending = self
            .admin_requests
            .values()
            .any(|key| key == "action:roles");
        let mut create = false;
        ui.add_enabled_ui(!pending, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.admin_ui.custom_role_name)
                        .char_limit(64)
                        .hint_text("New role name"),
                );
                create = ui
                    .add_enabled(
                        !self.admin_ui.custom_role_name.trim().is_empty()
                            && member_rank(bootstrap, &bootstrap.user.id) > 1,
                        primary("Create Role"),
                    )
                    .clicked();
            });
        });
        if create {
            let role = CommunityRole {
                name: self.admin_ui.custom_role_name.trim().into(),
                color: 0x5865f2,
                position: 1,
                permissions: 15,
                ..Default::default()
            };
            self.admin_request(
                AdminMethod::Post,
                "/api/roles".into(),
                json!(role),
                "action:roles".into(),
            );
            self.admin_ui.custom_role_name.clear();
        }
        let roles = self
            .admin_data
            .get("roles")
            .cloned()
            .and_then(|data| serde_json::from_value::<Vec<CommunityRole>>(data).ok())
            .unwrap_or_else(|| bootstrap.roles.clone());
        for role in roles {
            ui.add_space(12.0);
            let mut save = None;
            let mut remove = false;
            let own_rank = member_rank(bootstrap, &bootstrap.user.id);
            let editable = role.position < own_rank
                && (bootstrap.has_permission(PERMISSION_ADMIN)
                    || role.permissions & !bootstrap.permissions == 0);
            let draft = self
                .admin_ui
                .custom_role_edits
                .entry(role.id.clone())
                .or_insert_with(|| role.clone());
            egui::CollapsingHeader::new(RichText::new(&role.name).color(profile_ui::profile_color(role.color))).id_source(("custom-role",&role.id)).default_open(true).show(ui,|ui| {
                ui.add_enabled_ui(!pending && editable, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut draft.name).char_limit(64).desired_width(240.0));
                        profile_ui::profile_color_editor(ui,"Role color",&mut draft.color);
                        ui.label("Position");ui.add(egui::DragValue::new(&mut draft.position).clamp_range(1..=own_rank.saturating_sub(1).clamp(1,1000)));
                    });
                    ui.add_space(10.0);
                    role_templates::section(ui,&role.id,bootstrap.member_roles.iter().filter(|m|m.role_ids.contains(&role.id)).count(),draft,role_templates::grantable(|bit|bootstrap.has_permission(bit)),role_templates::lock_reason(pending,editable));
                    ui.add_space(6.0);
                    for (bit,label) in role_templates::PERMISSION_LABELS {
                        let mut enabled=draft.permissions & bit != 0;
                        if ui.add_enabled(bootstrap.has_permission(bit),egui::Checkbox::new(&mut enabled,label)).changed(){if enabled {draft.permissions|=bit;}else{draft.permissions&=!bit;}}
                    }
                    ui.horizontal_wrapped(|ui| {
                        if ui.add_enabled(draft!=&role && draft.valid(),primary("Save Changes")).clicked(){save=Some(draft.clone());}
                        if ui.add_enabled(draft!=&role,secondary("Reset")).clicked(){*draft=role.clone();}
                        if ui.add(secondary("Delete Role")).clicked(){self.admin_ui.role_delete=Some(role.id.clone());}
                    });
                    if self.admin_ui.role_delete.as_ref()==Some(&role.id) {
                        ui.label(RichText::new("Delete this role and its assignments? Channel overrides for it will also be removed.").color(c_bad()));
                        ui.horizontal_wrapped(|ui| {
                            remove=ui.add(secondary("Confirm Delete")).clicked();
                            if ui.add(secondary("Cancel")).clicked(){self.admin_ui.role_delete=None;}
                        });
                    }
                });
            });
            if let Some(draft) = save {
                self.admin_request(
                    AdminMethod::Put,
                    format!("/api/roles/{}", role.id),
                    json!(draft),
                    "action:roles".into(),
                );
            }
            if remove {
                self.admin_request(
                    AdminMethod::Delete,
                    format!("/api/roles/{}", role.id),
                    Value::Null,
                    "action:roles".into(),
                );
            }
        }
        if ui.add(secondary("Refresh Roles")).clicked() {
            self.refresh_admin_data("roles", "/api/roles");
        }
        crate::settings_ui::divider(ui);
        crate::settings_ui::nav_group(ui, "ASSIGN ROLES");
        for user in &bootstrap.members {
            self.member_custom_roles_ui(ui, bootstrap, &user.id, profile_ui::display_name(user));
        }
    }
    fn member_custom_roles_ui(
        &mut self,
        ui: &mut egui::Ui,
        bootstrap: &Bootstrap,
        user_id: &str,
        name: &str,
    ) {
        if !bootstrap.has_permission(PERMISSION_ROLES) {
            return;
        }
        let baseline = bootstrap
            .member_roles
            .iter()
            .find(|entry| entry.user_id == user_id)
            .map(|entry| entry.role_ids.clone())
            .unwrap_or_default();
        let own_rank = member_rank(bootstrap, &bootstrap.user.id);
        let targetable = (bootstrap.user.role == "admin"
            || (user_id != bootstrap.user.id && own_rank > member_rank(bootstrap, user_id)))
            && bootstrap
                .roles
                .iter()
                .filter(|r| baseline.contains(&r.id))
                .all(|r| {
                    r.position < own_rank
                        && (bootstrap.has_permission(PERMISSION_ADMIN)
                            || r.permissions & !bootstrap.permissions == 0)
                });
        let draft = self
            .admin_ui
            .member_role_edits
            .entry(user_id.into())
            .or_insert_with(|| baseline.clone());
        let mut save = None;
        ui.push_id(("member-custom-roles", user_id), |ui| {
            ui.collapsing(format!("Roles for {name}"), |ui| {
                for role in &bootstrap.roles {
                    let mut assigned = draft.contains(&role.id);
                    if ui
                        .add_enabled(
                            targetable
                                && role.position < own_rank
                                && (bootstrap.has_permission(PERMISSION_ADMIN)
                                    || role.permissions & !bootstrap.permissions == 0),
                            egui::Checkbox::new(
                                &mut assigned,
                                RichText::new(&role.name)
                                    .color(profile_ui::profile_color(role.color)),
                            ),
                        )
                        .changed()
                    {
                        if assigned {
                            draft.push(role.id.clone());
                        } else {
                            draft.retain(|id| id != &role.id);
                        }
                    }
                }
                let pending = self
                    .admin_requests
                    .values()
                    .any(|key| key == "action:roles");
                if ui
                    .add_enabled(
                        targetable && !pending && *draft != baseline,
                        primary("Save Roles"),
                    )
                    .clicked()
                {
                    save = Some(draft.clone());
                }
                if ui
                    .add_enabled(!pending && *draft != baseline, secondary("Reset"))
                    .clicked()
                {
                    *draft = baseline;
                }
            });
        });
        if let Some(role_ids) = save {
            self.admin_request(
                AdminMethod::Put,
                format!("/api/members/{user_id}/roles"),
                json!(MemberRoles {
                    user_id: user_id.into(),
                    role_ids
                }),
                "action:roles".into(),
            );
        }
    }
    fn audit_log_ui(&mut self, ui: &mut egui::Ui, bootstrap: &Bootstrap) {
        crate::settings_ui::page_heading(ui,"Audit Log","Recent role, member and server administration. Conversation content is not recorded here.");
        self.ensure_admin_data("audit", "/api/audit-log");
        if ui.add(secondary("Refresh Audit Log")).clicked() {
            self.refresh_admin_data("audit", "/api/audit-log");
        }
        let records = self
            .admin_data
            .get("audit")
            .cloned()
            .and_then(|data| serde_json::from_value::<Vec<AuditEntry>>(data).ok())
            .unwrap_or_default();
        if records.is_empty() {
            ui.label("No recorded actions yet.");
        }
        for entry in records {
            let name = bootstrap
                .members
                .iter()
                .find(|user| user.id == entry.actor_id)
                .map(profile_ui::display_name)
                .unwrap_or("Former member");
            egui::Frame::none()
                .fill(c_card())
                .rounding(6.0)
                .inner_margin(Margin::same(12.0))
                .show(ui, |ui| {
                    ui.label(RichText::new(format!("{name} · {}", entry.action)).strong());
                    ui.add(
                        egui::Label::new(format!("{} · {}", entry.created_at, entry.target_id))
                            .wrap(true),
                    );
                });
            ui.add_space(8.0);
        }
    }
    pub(super) fn member_role_color(&self, user: &User) -> Color32 {
        let Some(bootstrap) = &self.bootstrap else {
            return c_text();
        };
        let assignments = bootstrap
            .member_roles
            .iter()
            .find(|entry| entry.user_id == user.id);
        bootstrap
            .roles
            .iter()
            .filter(|role| {
                role.color != 0
                    && assignments.is_some_and(|entry| entry.role_ids.contains(&role.id))
            })
            .max_by_key(|role| role.position)
            .map(|role| profile_ui::profile_color(role.color))
            .unwrap_or_else(c_text)
    }
}
