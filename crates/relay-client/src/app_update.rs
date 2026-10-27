//! R8.2: the network event loop (`updates`), moved unchanged out of `app.rs`;
//! only the method's visibility became `pub(super)`.
use super::*;

pub(super) fn clear_recovered_connection_error(
    previous: &ConnectionStatus,
    next: &ConnectionStatus,
    error: &mut Option<String>,
) {
    // Connected is emitted only after a successful, identity-checked bootstrap.
    // Keep certificate errors visible during the outage and all unrelated errors.
    if matches!(
        previous,
        ConnectionStatus::Reconnecting | ConnectionStatus::Offline
    ) && *next == ConnectionStatus::Connected
        && error
            .as_deref()
            .is_some_and(crate::network::is_connection_error)
    {
        *error = None;
    }
}

impl RelayApp {
    pub(super) fn updates(&mut self, ctx: &egui::Context) {
        self.poll_sound_settings();
        self.poll_outbox(ctx);
        self.poll_background_people_answer(ctx);
        if self
            .preview_release
            .as_ref()
            .is_some_and(|done| !matches!(done.try_recv(), Err(mpsc::TryRecvError::Empty)))
        {
            self.preview_release = None;
        }
        #[cfg(feature = "native-rtc")]
        if let Some(error) = self
            .source_preview
            .as_mut()
            .and_then(|preview| preview.update(ctx))
        {
            self.error = Some(format!("Local preview: {error}"));
            self.stop_source_preview();
        }
        if let Some(previews) = &self.gif_previews {
            while let Ok((path, result)) = previews.animation_rx.try_recv() {
                self.gif_hover_pending = None;
                match result {
                    Ok(media) => self.gif_hover = Some((path, AnimatedImage::new(media))),
                    Err(_) => {
                        self.gif_hover_errors.insert(path);
                    }
                }
            }
            while let Ok((path, result)) = previews.rx.try_recv() {
                self.gif_pending.remove(&path);
                match result {
                    Ok(image) => {
                        if self.gif_textures.len() < 64 {
                            self.gif_textures.insert(
                                path,
                                ctx.load_texture(
                                    "local-gif",
                                    egui::ImageData::Color(image),
                                    egui::TextureOptions::LINEAR,
                                ),
                            );
                        }
                    }
                    Err(error) => {
                        self.gif_errors.insert(path, error);
                    }
                }
            }
        }
        if let Some(error) = self.notifications.take_error() {
            self.notice = Some((format!("Notifications: {error}"), Instant::now()));
        }
        if let Some(error) = self.preference_writer.take_error() {
            self.preferences_storage_error = Some(error.clone());
            self.error = Some(error);
        }
        if let Some(media) = &self.media {
            self.mic_test = media.is_microphone_test_active();
        }
        if let Some(inventory) = self.inventory_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.inventory = inventory;
            self.inventory_rx = None;
            self.inventory_loaded = true;
        }
        while let Ok((camera, generation, result)) = self.capture_rx.try_recv() {
            if generation == self.capture_generation[usize::from(camera)] {
                if camera {
                    self.camera_pending = false;
                } else {
                    self.screen_pending = false;
                }
            }
            match result {
                Ok(capture)
                    if generation == self.capture_generation[usize::from(camera)]
                        && self.voice_channel.is_some() =>
                {
                    if camera {
                        self.camera = Some(capture);
                    } else {
                        self.screen = Some(capture);
                    }
                    self.sync_voice_state();
                }
                Ok(capture) => release_capture(Some(capture)),
                Err(error) if generation == self.capture_generation[usize::from(camera)] => {
                    self.error = Some(error);
                }
                Err(_) => {}
            }
        }
        let mut call_transitions = std::collections::VecDeque::new();
        while let Some(update) = call_transitions
            .pop_front()
            .or_else(|| self.network.try_update().ok())
        {
            #[cfg(feature = "qa-automation")]
            crate::qa_automation::log_update(&update);
            let mut verified_move = None;
            let update = match update {
                Update::CommunityMoved {
                    origin,
                    url,
                    session,
                    bootstrap,
                } => {
                    let requested = self.resume_destination.as_deref() == Some(origin.url.as_str())
                        && self
                            .prefs
                            .communities
                            .iter()
                            .any(|v| v.url == origin.url && v.server_id == origin.server_id);
                    if self.command_origin().as_ref() != Some(&origin) && !requested {
                        continue;
                    }
                    if bootstrap.server_id != origin.server_id
                        || bootstrap.user.id != origin.user_id
                        || session.user.id != origin.user_id
                    {
                        continue;
                    }
                    verified_move = Some(origin);
                    Update::Connected {
                        url,
                        session,
                        bootstrap,
                    }
                }
                Update::AccountLoginComplete {
                    url,
                    session,
                    bootstrap,
                    request_id,
                } => {
                    if !self.account_login_current(request_id, &url) {
                        self.send(Command::ForgetCommunity { url });
                        continue;
                    }
                    if self
                        .account_start
                        .pending
                        .as_ref()
                        .is_some_and(|a| a.is_people())
                        && !bootstrap.capabilities.iter().any(|c| c == "people-v1")
                    {
                        self.account_start.pending = None;
                        self.account_start.friends_error = Some("This address is not the Relay Friends service.".into());
                        self.send(Command::ForgetCommunity { url });
                        self.error = Some("This address is not the Relay Friends service.".into());
                        continue;
                    }
                    Update::Connected {
                        url,
                        session,
                        bootstrap,
                    }
                }
                update => update,
            };
            if !community_update_allowed(
                &update,
                &self.login.url,
                self.command_origin().as_ref(),
                &self.communities,
            ) {
                continue;
            }
            match update {
                Update::HandoverHostReady { remembered } => {
                    self.own_local_server = true;
                    self.notice=Some((if remembered{"This PC is hosting the transferred community."}else{"This PC is hosting the transferred community, but automatic restart could not be saved. Its data and keys were kept."}.into(),Instant::now()));
                }
                Update::Handover(view) => self.handover_received(view),
                Update::AccountLoginComplete { .. } | Update::CommunityMoved { .. } => {
                    unreachable!("resolved above")
                }
                Update::AccountLoginChallenge {
                    url,
                    data,
                    request_id,
                } => self.account_login_challenge(ctx, &url, data, request_id),
                Update::Connected {
                    url,
                    session,
                    bootstrap,
                } => {
                    if !self.account_start.bypass
                        && bootstrap.account_login_only
                        && self.central_account.account.as_ref().is_none_or(|account| {
                            !bootstrap.linked_identities.iter().any(|l| {
                                l.user_id == session.user.id
                                    && l.issuer == account.issuer
                                    && l.handle == account.handle
                            })
                        })
                    {
                        self.send(Command::ForgetCommunity { url });
                        continue;
                    }
                    if self.reject_cancelled_navigation(&url) {
                        self.send(Command::CancelNavigation {
                            cancelled: CommandOrigin { url, server_id: bootstrap.server_id.clone(), user_id: session.user.id.clone() },
                            restore: self.cancelled_navigation_restore(),
                        });
                        continue;
                    }
                    let preserve_friends_shell = self.friends_shell_selected()
                        && (self.account_start.auto.is_some() || self.people_url().as_deref() == Some(url.as_str()));
                    self.friends_connection_adopted(&url, bootstrap.capabilities.iter().any(|c| c == "people-v1"));
                    // Consume the explicit answer before normal connection
                    // adoption tears down the previous idle media context.
                    let background_answer = self.take_background_people_answer(&url, &bootstrap);
                    self.resume_destination = None;
                    let people = self
                        .account_start
                        .pending
                        .take()
                        .is_some_and(|attempt| attempt.is_people());
                    // During automatic entry a reconnected saved server stays in the
                    // background while the start page waits for Friends.
                    self.account_start.home = false;
                    if people {
                        self.account_start.auto = None;
                    }
                    self.voice_retry = None;
                    self.voice_join_attempt = None;
                    let created_open = self.hosting_setup.new_host
                        && self.login.registering
                        && self.hosting_setup.creation_protection
                            == relay_protocol::ProtectionLevel::Open
                        && crate::local_host::is_local_url(&url)
                        && bootstrap.user.role == "admin"
                        && bootstrap.protection.level == relay_protocol::ProtectionLevel::Open;
                    if self.hosting_setup.new_host
                        && self.login.registering
                        && crate::local_host::is_local_url(&url)
                        && bootstrap.user.role == "admin"
                    {
                        // The first account exists: tie the chosen connection to this exact server.
                        self.remember_creation_route(&bootstrap.server_id);
                    }
                    self.account_feedback.finished();
                    self.hosting_setup.cancel_invitation_copy();
                    // The link used to join this community may carry device verification.
                    // Take it before the form state is reset; it is sent once below.
                    let invite_proof = [&self.add_community.setup, &self.login.setup]
                        .into_iter()
                        .filter_map(|setup| setup.invitation.as_ref())
                        .find(|invitation| invitation.server == url)
                        .and_then(|invitation| invitation.proof.clone());
                    self.login.setup = Default::default();
                    self.add_community.setup = Default::default();
                    self.add_community.invite.clear();
                    if self.hosting_setup.resume_share && crate::local_host::is_local_url(&url) {
                        self.hosting_setup.resume_share = false;
                        self.hosting_setup.open = true;
                    }
                    self.hosting_setup.new_host = false;
                    // The first account committed the choices; the next server starts afresh.
                    self.hosting_setup.creation_confirmed = false;
                    let bootstrap = *bootstrap;
                    if let Some(previous) = &self.bootstrap {
                        cache_community_snapshot(
                            &mut self.community_reads,
                            &mut self.community_social,
                            &self.login.url,
                            previous,
                        );
                        self.community_friends.remember(&self.login.url, previous);
                    }
                    cache_community_snapshot(
                        &mut self.community_reads,
                        &mut self.community_social,
                        &url,
                        &bootstrap,
                    );
                    self.community_friends.remember(&url, &bootstrap);
                    if self.session.is_some() {
                        self.save_preferences();
                        self.stop_media();
                    }
                    if let Some(origin) = &verified_move {
                        crate::community::moved(&mut self.prefs.communities, origin, &url);
                        self.community_statuses.remove(&origin.url);
                        self.community_reads.remove(&origin.url);
                        self.community_social.remove(&origin.url);
                        self.community_friends.remove(&origin.url);
                    }
                    self.login.url = url;
                    self.login.username = session.user.username.clone();
                    self.login.password.clear();
                    self.login.invite.clear();
                    let initial_channel = self.initial_channel.take();
                    let initial_direct = self.initial_direct.take();
                    let scope = crate::community::conversation_scope(
                        &self.prefs.communities,
                        &self.login.url,
                        &bootstrap.server_id,
                        &session.user.id,
                    );
                    let remembered =
                        self.prefs
                            .conversations
                            .last(&scope, None)
                            .filter(|target| match target {
                                Target::Channel(id) => {
                                    bootstrap.channels.iter().any(|channel| &channel.id == id)
                                }
                                Target::Direct(id) => bootstrap.members.iter().any(|member| {
                                    &member.id == id
                                        && crate::direct_targets::actionable(&bootstrap, id)
                                }),
                            });
                    let target = initial_direct
                        .and_then(|name| {
                            bootstrap
                                .members
                                .iter()
                                .find(|m| {
                                    m.username == name
                                        && crate::direct_targets::actionable(&bootstrap, &m.id)
                                })
                                .map(|m| Target::Direct(m.id.clone()))
                        })
                        .or_else(|| {
                            initial_channel.and_then(|name| {
                                bootstrap
                                    .channels
                                    .iter()
                                    .find(|c| c.name == name)
                                    .map(|c| Target::Channel(c.id.clone()))
                            })
                        })
                        .or(remembered)
                        .or_else(|| {
                            bootstrap
                                .server_presentation
                                .welcome_channel_id
                                .as_ref()
                                .filter(|id| bootstrap.channels.iter().any(|c| c.id == **id))
                                .map(|id| Target::Channel(id.clone()))
                        })
                        .or_else(|| {
                            bootstrap
                                .channels
                                .iter()
                                .find(|c| c.kind == "text")
                                .map(|c| Target::Channel(c.id.clone()))
                        });
                    let target = pending_notification_target(
                        self.notification_target.take(),
                        &self.login.url,
                        &bootstrap,
                    )
                    .or(target);
                    self.session = Some(session);
                    self.bootstrap = Some(Arc::new(bootstrap));
                    self.unread_navigation = None;
                    self.current = None;
                    let mut restored_drafts =
                        self.scoped_drafts.load(&scope).unwrap_or_else(|| {
                            if self.prefs.remember_drafts {
                                self.prefs.conversations.drafts(&scope)
                            } else {
                                Vec::new()
                            }
                        });
                    restored_drafts.retain(|(target, _)| {
                        target_available(target, self.bootstrap.as_ref().unwrap())
                    });
                    self.prefs.conversations.retain_targets(&scope, |target| {
                        target_available(target, self.bootstrap.as_ref().unwrap())
                    });
                    self.drafts.restore(restored_drafts);
                    self.draft.clear();
                    self.attachments.clear();
                    self.reply = None;
                    self.pending_sends.clear();
                    self.outgoing_messages.clear();
                    self.cooldowns = Default::default();
                    self.typing_users = Default::default();
                    self.typing_activity = None;
                    self.typing_sent = None;
                    self.pins = Default::default();
                    self.pending_edits.clear();
                    self.marked_read.clear();
                    self.uploads.clear();
                    self.send_ids.clear();
                    self.restore_outbox();
                    self.connection_status =
                        if self.bootstrap.as_ref().is_some_and(|b| b.host_paused) {
                            ConnectionStatus::Reconnecting
                        } else {
                            ConnectionStatus::Connected
                        };
                    self.community_statuses
                        .insert(self.login.url.clone(), self.connection_status.clone());
                    self.add_community_open = false;
                    self.add_community.password.clear();
                    self.admin_data.clear();
                    self.admin_requests.clear();
                    self.private_calls = private_call_ui::State::default();
                    self.authenticator_ui = authenticator_ui::State::default();
                    self.login.second_factor.clear();
                    self.add_community.second_factor.clear();
                    self.admin_ui = admin_ui::ServerUi::default();
                    if self.bootstrap.as_ref().is_some_and(|b| b.host_paused) {
                        // Reopen the recovery controls without normal startup's
                        // invitation attestation, consent, history or call writes.
                        // Keep the saved conversation selection for after resume.
                        self.profile_ui.clear_session();
                        self.encryption_ui.clear_session();
                        self.message_protection.clear();
                        self.protection_ui.forget_connection();
                        self.error = None;
                        self.settings_open = true;
                        self.settings_tab = SettingsTab::Server;
                        self.admin_ui.select_section("hosts");
                        self.finish_server_action();
                        continue;
                    }
                    if !self.in_people() {
                        self.maybe_open_welcome();
                    }
                    self.profile_ui.clear_session();
                    self.encryption_ui.clear_session();
                    if let (Some(proof), Some(origin)) = (invite_proof, self.command_origin()) {
                        self.send(Command::AttestInvitation { origin, proof });
                    }
                    self.message_protection.clear();
                    self.protection_ui.forget_connection();
                    self.sync_open_consent();
                    if let Some(saved) = self
                        .prefs
                        .communities
                        .iter_mut()
                        .find(|saved| saved.url == self.login.url)
                    {
                        saved.username = self.login.username.clone();
                        saved.name = self.bootstrap.as_ref().unwrap().server_name.clone();
                        saved.server_id = self.bootstrap.as_ref().unwrap().server_id.clone();
                        saved.iroh = crate::iroh_route::saved_for(&saved.url).or(saved.iroh.take());
                    } else {
                        self.prefs.communities.push(SavedCommunity {
                            url: self.login.url.clone(),
                            server_id: self.bootstrap.as_ref().unwrap().server_id.clone(),
                            name: self.bootstrap.as_ref().unwrap().server_name.clone(),
                            username: self.login.username.clone(),
                            last_target: target.clone(),
                            iroh: crate::iroh_route::saved_for(&self.login.url),
                            ..Default::default()
                        });
                        if self.prefs.communities.len() > 12 {
                            self.prefs.communities.remove(0);
                        }
                    }
                    self.error = None;
                    self.ensure_admin_data("categories", "/api/categories");
                    if created_open {
                        let revision = self.bootstrap.as_ref().unwrap().protection.revision;
                        self.record_open_consent(Some(revision));
                    }
                    self.save_preferences();
                    if let Err(error) = self.remember_current_local() {
                        self.notice = Some((error, Instant::now()));
                    }
                    if let Some(target) = target.filter(|_| !preserve_friends_shell) {
                        self.selected_nav = !matches!(target, Target::Direct(_));
                        self.select(target);
                    }
                    if preserve_friends_shell || (self.in_people() && self.current.is_none()) {
                        self.selected_nav = false;
                        self.home_page = Some(SettingsTab::Friends);
                        self.admin_ui.friend_all_servers = false;
                    }
                    if self.initial_pins {
                        self.initial_pins = false;
                        self.pins.open = true;
                        self.request_pins();
                    }
                    if let Some(query) = self.initial_search.take() {
                        self.search = query;
                        self.search_history(false);
                    }
                    if let Some(section) = self.initial_community_tab.take() {
                        self.admin_ui.select_section(&section);
                    }
                    if self.initial_new_community {
                        self.initial_new_community = false;
                        self.open_community_dialog();
                    }
                    self.finish_server_action();
                    if let Some(call) = background_answer {
                        self.answer_private_call(&call, "accept");
                    }
                }
                Update::Disconnected => {
                    self.server_actions.clear();
                    self.voice_retry = None;
                    self.hosting_setup.cancel_invitation_copy();
                    self.community_statuses
                        .insert(self.login.url.clone(), ConnectionStatus::SignInRequired);
                    self.connection_status = ConnectionStatus::Offline;
                    self.community_reads.remove(&self.login.url);
                    self.community_social.remove(&self.login.url);
                    self.community_friends.remove(&self.login.url);
                    self.profile_ui.clear_session();
                    self.encryption_ui.clear_session();
                    self.message_protection.clear();
                    self.stop_typing();
                    self.cooldowns = Default::default();
                    self.typing_users = Default::default();
                    self.pins = Default::default();
                    self.save_preferences();
                    self.stop_media();
                    self.unread_navigation = None;
                    self.session = None;
                    self.bootstrap = None;
                    self.messages.clear();
                    self.messages_ready = false;
                    self.current = None;
                    self.draft.clear();
                    self.reply = None;
                    self.attachments.clear();
                    self.drafts = DraftStore::default();
                    self.pending_sends.clear();
                    self.outgoing_messages.clear();
                    self.outbox_pending.clear();
                    self.pending_edits.clear();
                    self.joining_voice = None;
                    self.share_picker = None;
                    self.chat_media.clear();
                    self.chat_media_pending.clear();
                    self.chat_media_errors.clear();
                    self.media_preview = None;
                    self.editing = None;
                    self.voice_channel = None;
                    self.video.clear();
                    self.admin_ui = admin_ui::ServerUi::default();
                }
                Update::Bootstrap(b) => {
                    if self.voice_channel.is_some()
                        && self
                            .bootstrap
                            .as_ref()
                            .is_some_and(|old| old.protection != b.protection)
                    {
                        self.stop_media();
                        self.send(Command::LeaveVoice);
                        self.notice=Some(("The server's encryption choice changed. Rejoin voice when you are ready.".into(),Instant::now()));
                    }
                    if self.voice_connected() {
                        if let (Some(previous), Some(channel)) =
                            (&self.bootstrap, &self.voice_channel)
                        {
                            if let Some(cue) = crate::sound_cues::room_change(previous, &b, channel)
                            {
                                if let Some(media) = &mut self.media {
                                    let _ = media.play_tone(cue);
                                }
                            }
                        }
                    }
                    if self.bootstrap.as_ref().is_none_or(|old| {
                        old.server_presentation != b.server_presentation
                            || old.social.blocked != b.social.blocked
                            || old
                                .members
                                .iter()
                                .map(|u| &u.id)
                                .ne(b.members.iter().map(|u| &u.id))
                    }) {
                        self.admin_data
                            .retain(|key, _| !key.starts_with("system-notices:"));
                    }
                    for user in &b.members {
                        if self
                            .bootstrap
                            .as_ref()
                            .and_then(|old| old.members.iter().find(|member| member.id == user.id))
                            .is_none_or(|old| {
                                old.profile != user.profile
                                    || old.username != user.username
                                    || old.role != user.role
                            })
                        {
                            self.apply_profile_user(user);
                        }
                    }
                    self.cooldowns.retain_slowmode(|target| match target {
                        Target::Channel(id) => b.channels.iter().any(|c| {
                            c.id == *id
                                && c.slowmode_seconds > 0
                                && !c.slowmode_bypass
                                && self.bootstrap.as_ref().is_some_and(|previous| {
                                    previous.channels.iter().any(|old| {
                                        old.id == c.id
                                            && old.slowmode_seconds == c.slowmode_seconds
                                            && old.slowmode_bypass == c.slowmode_bypass
                                    })
                                })
                        }),
                        Target::Direct(_) => false,
                    });
                    let allowed = |target: &Target| match target {
                        Target::Channel(id) => b.channels.iter().any(|channel| &channel.id == id),
                        Target::Direct(id) => b
                            .members
                            .iter()
                            .any(|member| &member.id == id && member.id != b.user.id),
                    };
                    self.drafts.retain(allowed);
                    if let Some(scope) = self.conversation_scope() {
                        self.prefs.conversations.retain_targets(&scope, allowed);
                    }
                    if self.current.as_ref().is_some_and(|target| !allowed(target)) {
                        self.stop_typing();
                        self.typing_users = Default::default();
                        self.pins = Default::default();
                        self.current = None;
                        self.unread_navigation = None;
                        self.messages.clear();
                        self.search_messages.clear();
                        self.history_pending = None;
                        self.search_pending = None;
                        self.draft.clear();
                        self.attachments.clear();
                        self.reply = None;
                        self.chat_media.clear();
                        self.chat_media_pending.clear();
                        self.chat_media_errors.clear();
                        self.media_preview = None;
                        self.send(Command::ClearSelection);
                        self.notice = Some((
                            "This conversation is no longer available.".into(),
                            Instant::now(),
                        ));
                    }
                    if self.voice_channel.as_ref().is_some_and(|channel| {
                        !b.voice_members
                            .iter()
                            .any(|m| m.channel_id == *channel && m.user.id == b.user.id)
                    }) {
                        let connected = self.voice_connected();
                        let channel = self.voice_channel.clone();
                        self.stop_media();
                        if connected {
                            if let Some(media) = &mut self.media {
                                let _ = media.play_tone(AudioCue::CallEnd);
                            }
                        }
                        self.voice_channel = None;
                        self.video.clear();
                        self.voice_failure(
                            channel,
                            "Voice disconnected. Join the room again to reconnect.".into(),
                        );
                    }
                    if self
                        .voice_channel
                        .as_ref()
                        .and_then(|id| b.channels.iter().find(|channel| &channel.id == id))
                        .is_some_and(|channel| !channel.can_stream)
                    {
                        if self.source_active(false) || self.source_starting(false) {
                            self.stop_capture(false);
                        }
                        if self.source_active(true) || self.source_starting(true) {
                            self.stop_capture(true);
                        }
                    }
                    let capture_voice = self
                        .capture_path
                        .as_ref()
                        .and(self.current.as_ref())
                        .and_then(|target| match target {
                            Target::Channel(id) => Some(id.as_str()),
                            _ => None,
                        });
                    self.video.retain(|_, tile| {
                        (tile.metadata.url == "owned-capture://synthetic"
                            && b.voice_members.iter().any(|member| {
                                member.user.id == tile.metadata.user.id
                                    && Some(member.channel_id.as_str()) == capture_voice
                            }))
                            || crate::voice_ui_state::video_member(
                                &b,
                                self.voice_channel.as_deref(),
                                &tile.metadata.user.id,
                                &tile.metadata.kind,
                            )
                            .is_some()
                    });
                    if self
                        .stream_preview
                        .as_ref()
                        .is_some_and(|key| !self.video.contains_key(key))
                    {
                        self.stream_preview = None;
                    }
                    if let Some(media) = &self.media {
                        for member in &b.voice_members {
                            if let Some(volume) = self
                                .prefs
                                .peer_volumes
                                .get(&self.peer_volume_key(&member.user.id))
                            {
                                media.set_user_volume(&member.user.id, *volume);
                            }
                        }
                    }
                    cache_community_snapshot(
                        &mut self.community_reads,
                        &mut self.community_social,
                        &self.login.url,
                        &b,
                    );
                    self.community_friends.remember(&self.login.url, &b);
                    self.bootstrap = Some(Arc::new(b));
                    self.protection_ui.settled();
                    self.sync_open_consent();
                    if self.stage_listener() && !self.muted {
                        self.muted = true;
                        self.sync_voice_state();
                    }
                    if self.current.is_none() {
                        if let Some(target) = self.remembered_target(false) {
                            self.select(target);
                        }
                    }
                }
                Update::Messages { target, messages } => {
                    if self.current.as_ref() == Some(&target) && self.unread_navigation.is_none() {
                        let messages = crate::unread_navigation::live_page(
                            messages,
                            self.unread_history_start,
                        );
                        timeline::merge(&mut self.messages, messages, self.history_has_newer);
                        self.messages_ready = true;
                    }
                }
                Update::History {
                    target,
                    page,
                    request_id,
                } => {
                    if self.current.as_ref() != Some(&target)
                        || self
                            .history_pending
                            .is_none_or(|(pending, _)| pending != request_id)
                    {
                        continue;
                    }
                    let older = self.history_pending.take().unwrap().1;
                    self.history_before = page.next_before;
                    if older {
                        let before_height = self
                            .messages
                            .iter()
                            .map(|message| self.row_heights.estimate(message))
                            .sum::<f32>();
                        let before_first = self.messages.first().map(|message| message.sequence);
                        self.history_has_newer |=
                            timeline::merge(&mut self.messages, page.messages, true);
                        let inserted_height = self
                            .messages
                            .iter()
                            .take_while(|message| {
                                before_first.is_some_and(|sequence| message.sequence < sequence)
                            })
                            .map(|message| self.row_heights.estimate(message))
                            .sum::<f32>();
                        if let (Some(scope), Some(current)) =
                            (self.conversation_scope(), self.current.as_ref())
                        {
                            let old_offset = self
                                .prefs
                                .conversations
                                .scroll(&scope, current)
                                .unwrap_or(before_height);
                            self.restore_scroll = Some(old_offset + inserted_height);
                        }
                    } else {
                        self.messages.clear();
                        timeline::merge(&mut self.messages, page.messages, false);
                        self.history_has_newer = self
                            .messages
                            .last()
                            .is_some_and(|message| message.sequence < page.latest_sequence);
                    }
                    self.messages_ready = true;
                }
                Update::UnreadHistory {
                    origin,
                    target,
                    page,
                    next_after,
                    request_id,
                } => {
                    if self.unread_navigation.as_ref().is_none_or(|nav| {
                        !nav.accepts(
                            &origin,
                            &target,
                            request_id,
                            self.command_origin().as_ref(),
                            self.current.as_ref(),
                        )
                    }) {
                        continue;
                    }
                    let navigation = self.unread_navigation.as_mut().unwrap();
                    navigation.pending = None;
                    if let Some(first) = navigation.first_incoming(&page.messages) {
                        let message_id = first.id.clone();
                        self.history_before = page.messages.first().map(|message| message.sequence);
                        self.unread_history_start = self.history_before;
                        self.history_has_newer = next_after.is_some();
                        self.messages.clear();
                        timeline::merge(&mut self.messages, page.messages, true);
                        self.messages_ready = true;
                        self.jump_message = Some(message_id);
                        self.scroll_to_end = false;
                        self.bottom_follow.cancel();
                        self.unread_navigation = None;
                    } else if let Some(after) = next_after.filter(|after| *after > navigation.after)
                    {
                        navigation.after = after;
                        self.load_unread_page();
                    } else if next_after.is_some() {
                        navigation.error =
                            Some("Unread history did not advance. Try again.".into());
                    } else {
                        self.notice = Some((
                            "No unread replies remain in this conversation.".into(),
                            Instant::now(),
                        ));
                        self.open_latest();
                    }
                }
                Update::SearchResults {
                    target,
                    query,
                    page,
                    request_id,
                } => {
                    if self.current.as_ref() != Some(&target)
                        || self.search_pending != Some(request_id)
                        || self.search_applied != query
                        || self.search.trim() != query
                    {
                        continue;
                    }
                    self.search_pending = None;
                    self.search_before = page.next_before;
                    timeline::merge(&mut self.search_messages, page.messages, true);
                }
                Update::ProfileLoaded {
                    url,
                    user,
                    request_id,
                } => {
                    if url == self.login.url
                        && self.profile_ui.card_request == Some(request_id)
                        && self
                            .profile_ui
                            .card
                            .as_ref()
                            .is_some_and(|card| card.id == user.id)
                    {
                        self.profile_ui.card_request = None;
                        self.apply_profile_user(&user);
                    }
                }
                Update::ProfileSaved {
                    url,
                    user,
                    request_id,
                } => {
                    if url == self.login.url
                        && self.profile_ui.saved(&url, user.clone(), request_id, true)
                    {
                        self.apply_profile_user(&user);
                        self.notice = Some(("Profile saved".into(), Instant::now()));
                    }
                }
                Update::ProfileSaveFailed {
                    url,
                    user,
                    request_id,
                    error,
                } => {
                    if url == self.login.url {
                        if let Some(user) = user {
                            if self.profile_ui.saved(&url, user.clone(), request_id, false) {
                                self.apply_profile_user(&user);
                                self.error=Some(format!("Your text profile was saved, but the avatar still needs attention: {error}"));
                            }
                        } else if self.profile_ui.request_failed(request_id) {
                            self.error = Some(error);
                        }
                    }
                }
                Update::RequestFailed { request_id, error } => {
                    if self.replica_request_failed(request_id, &error) {
                        continue;
                    }
                    if self.account_login_failed(request_id, &error) {
                        continue;
                    }
                    let error = match self.account_feedback.failed(request_id, error) {
                        account_feedback_ui::AuthAnswer::NotAccount(error) => error,
                        account_feedback_ui::AuthAnswer::OnField => {
                            self.error = None;
                            continue;
                        }
                        account_feedback_ui::AuthAnswer::General(error) => {
                            self.error = Some(error);
                            continue;
                        }
                    };
                    if let Some(navigation) = self
                        .unread_navigation
                        .as_mut()
                        .filter(|nav| nav.pending == Some(request_id))
                    {
                        navigation.pending = None;
                        navigation.error = Some(error);
                        continue;
                    }
                    if self.profile_ui.request_failed(request_id) {
                        self.error = Some(error);
                        continue;
                    }
                    if self
                        .pins
                        .request
                        .as_ref()
                        .is_some_and(|(pending, _)| *pending == request_id)
                    {
                        self.pins.request = None;
                        self.error = Some(error);
                    } else if self
                        .pins
                        .action
                        .as_ref()
                        .is_some_and(|(pending, _)| *pending == request_id)
                    {
                        self.pins.action = None;
                        self.error = Some(error);
                    } else if self
                        .history_pending
                        .is_some_and(|(pending, _)| pending == request_id)
                    {
                        self.history_pending = None;
                        if self.restore_anchor.take().is_some() {
                            self.restore_scroll = None;
                            self.load_history(false);
                        } else {
                            self.messages_ready = true;
                            self.error = Some(error);
                        }
                    } else if self.search_pending == Some(request_id) {
                        self.search_pending = None;
                        self.error = Some(error);
                    } else if let Some(key) = self.admin_requests.remove(&request_id) {
                        if key == "hosting" || key == "action:hosting" {
                            self.hosting_failed(&key, error);
                            continue;
                        }
                        if key == "action:people-request" {
                            self.people_request_failed(error);
                            continue;
                        }
                        if key.starts_with("account-link:") {
                            self.account_link_failed(&key, error);
                            continue;
                        }
                        if key.starts_with("authenticator:") {
                            self.authenticator_failed(&key);
                        }
                        if key == "action:discord-status" {
                            self.refresh_admin_discord_status();
                        }
                        self.error = Some(error);
                    } else if self
                        .rtc_request
                        .is_some_and(|(pending, _)| pending == request_id)
                    {
                        self.rtc_request = None;
                        self.notice = Some((format!("Video connection: {error}"), Instant::now()));
                    }
                }
                Update::SentRich { origin, target, client_id } => {
                    if self.command_origin().as_ref() != Some(&origin) {
                        continue;
                    }
                    self.acknowledge_outbox(&origin, &target, &client_id);
                    if !self
                        .send_ids
                        .get(&target)
                        .is_some_and(|attempt| attempt.client_id == client_id)
                    {
                        continue;
                    }
                    for (conversation, message, failed, confirmed) in &mut self.outgoing_messages {
                        if conversation == &target
                            && message.client_id.as_deref() == Some(&client_id)
                        {
                            *failed = false;
                            *confirmed = true;
                        }
                    }
                    if let Some(key) = self.readiness_key(&target) {
                        self.readiness.settled(&key);
                    }
                    let seconds = self.slowmode_for(&target);
                    if seconds > 0 {
                        self.cooldowns.record(
                            target.clone(),
                            "slowmode",
                            u64::from(seconds) * 1000,
                            Instant::now(),
                        );
                    }
                    if self
                        .send_ids
                        .get(&target)
                        .is_some_and(|attempt| attempt.client_id == client_id)
                    {
                        self.send_ids.remove(&target);
                        self.pending_sends.remove(&target);
                    }
                    self.defer_preferences();
                    if self.current.as_ref() == Some(&target) && self.unread_navigation.is_none() {
                        self.scroll_to_end = true;
                        if self.history_has_newer {
                            self.history_pending = None;
                            self.restore_anchor = None;
                            self.restore_scroll = None;
                            self.load_history(false);
                        }
                    }
                }
                Update::UploadProgress {
                    target,
                    operation_id,
                    path,
                    transferred,
                    total,
                } => {
                    let upload = self.uploads.entry(operation_id).or_insert(UploadView {
                        target: Some(target),
                        kind: TransferKind::Upload,
                        path,
                        transferred: 0,
                        total,
                        error: None,
                        cancelled: false,
                    });
                    upload.transferred = transferred;
                    upload.total = total;
                }
                Update::UploadFinished {
                    target,
                    operation_id,
                    attachment,
                } => {
                    self.uploads.remove(&operation_id);
                    let active = self.command_origin();
                    if !self.staged_uploads.uploaded(&operation_id, active.as_ref(), &target) { continue; }
                    let send_gif = self.gif_upload_sends.remove(&operation_id).is_some_and(
                        |(origin, original_target)| {
                            self.command_origin().as_ref() == Some(&origin)
                                && original_target == target
                                && self.current.as_ref() == Some(&target)
                        },
                    );
                    if send_gif && self.send_gif_attachment(attachment.clone()) {
                        continue;
                    }
                    self.store_current_draft();
                    self.drafts.apply_uploaded(&target, attachment);
                    self.restore_current_draft();
                    self.defer_preferences();
                }
                Update::UploadFailed {
                    target,
                    operation_id,
                    error,
                    cancelled,
                } => {
                    self.gif_upload_sends.remove(&operation_id);
                    self.staged_uploads.failed(&operation_id, &error);
                    if let Some(upload) = self.uploads.get_mut(&operation_id) {
                        upload.target = Some(target);
                        upload.error = Some(error);
                        upload.cancelled = cancelled;
                    }
                }
                Update::Communities(communities) => {
                    for community in &communities {
                        if let Some(saved) = self
                            .prefs
                            .communities
                            .iter_mut()
                            .find(|saved| saved.url == community.url)
                        {
                            saved.name = community.name.clone();
                            saved.server_id = community.server_id.clone();
                            saved.username = community.user.username.clone();
                        }
                    }
                    self.communities = communities;
                    let mut owners: Vec<_> = self
                        .communities
                        .iter()
                        .map(|community| CommandOrigin {
                            url: community.url.clone(),
                            server_id: community.server_id.clone(),
                            user_id: community.user.id.clone(),
                        })
                        .collect();
                    if let Some(origin) = self.command_origin() {
                        if !owners.contains(&origin) {
                            owners.push(origin);
                        }
                    }
                    self.community_friends.retain(&owners);
                    self.defer_preferences();
                }
                Update::GameActivityResult {
                    origin,
                    request_id,
                    result,
                } => {
                    self.game_activity_result(origin, request_id, result);
                }
                Update::PeopleCallDeclined {
                    origin,
                    call_id,
                    request_id,
                    result,
                } => {
                    self.people_call_declined(origin, call_id, request_id, result);
                }
                Update::CommunityBootstrap { url, bootstrap } => {
                    self.remember_people_call(&url, &bootstrap);
                    self.remember_game_service(&url, &bootstrap);
                    cache_community_snapshot(
                        &mut self.community_reads,
                        &mut self.community_social,
                        &url,
                        &bootstrap,
                    );
                    self.community_friends.remember(&url, &bootstrap);
                    let scope = ConversationMemory::scope(&url, &bootstrap.user.id);
                    self.scoped_drafts
                        .retain_targets(&scope, |target| target_available(target, &bootstrap));
                    self.prefs
                        .conversations
                        .retain_targets(&scope, |target| target_available(target, &bootstrap));
                }
                Update::ConnectionState { url, status } => {
                    if url != self.login.url
                        && !self.prefs.communities.iter().any(|saved| saved.url == url)
                    {
                        continue;
                    }
                    self.community_statuses.insert(url.clone(), status.clone());
                    if status == ConnectionStatus::SignInRequired {
                        self.community_friends.remove(&url);
                    }
                    if status == ConnectionStatus::SignInRequired && url == self.login.url {
                        self.stop_media();
                        self.video.clear();
                        self.profile_ui.clear_session();
                        self.encryption_ui.clear_session();
                        self.message_protection.clear();
                        self.open_saved_community_login(url.clone());
                    }
                    if url == self.login.url {
                        clear_recovered_connection_error(
                            &self.connection_status,
                            &status,
                            &mut self.error,
                        );
                        self.connection_status = status;
                    }
                }
                Update::Changes { origin, changes } => {
                    self.sync_notification_accounts();
                    let notification_origin = origin.clone();
                    let url = origin.url;
                    let mut message_sound = false;
                    // Claims run even while focused/notifications are disabled:
                    // a painted message must not alert later as a replay.
                    let message_claims = self
                        .call_alerts
                        .claim_ui_messages(&notification_origin, &changes);
                    if self.prefs.notifications && !ctx.input(|input| input.focused) {
                        let user_id = origin.user_id;
                        for (change, claim) in changes.iter().zip(message_claims) {
                            let Ok(guard) = claim else { continue };
                            if change.kind != "created" || change.deleted {
                                continue;
                            }
                            let Some(message) = &change.message else {
                                continue;
                            };
                            if message.author.id == user_id {
                                continue;
                            }
                            let direct = message.channel_id.is_none();
                            let target = match &message.channel_id {
                                Some(id) => Target::Channel(id.clone()),
                                None => Target::Direct(message.author.id.clone()),
                            };
                            if !self.message_notification_allowed(
                                &url, &user_id, message, &target, direct,
                            ) {
                                continue;
                            }
                            let community_name = self
                                .prefs
                                .communities
                                .iter()
                                .find(|saved| saved.url == url)
                                .map(|saved| saved.name.as_str())
                                .unwrap_or("Relay");
                            let title = if guard.is_some() && !self.prefs.notification_previews {
                                "New Relay message".into()
                            } else {
                                format!(
                                    "{community_name} · {}",
                                    profile_ui::display_name(&message.author)
                                )
                            };
                            let body = encryption_ui::notification_body(
                                message,
                                self.prefs.notification_previews,
                            );
                            match self.notifications.show(Notification {
                                title,
                                body,
                                origin: notification_origin.clone(),
                                target,
                                background_only: guard.is_some(),
                                guard: guard.clone(),
                            }) {
                                Ok(()) => message_sound = true,
                                Err(error) => {
                                    if let Some(guard) = &guard {
                                        self.call_alerts.retry_ui_message(
                                            &notification_origin, guard, change,
                                        );
                                    }
                                    self.notice =
                                        Some((format!("Notifications: {error}"), Instant::now()))
                                }
                            }
                        }
                    }
                    // Coalesce one change batch; muted/DND/blocked/self/history events
                    // never reach the existing notification permission gate above.
                    if message_sound {
                        if let Some(media) = &mut self.media {
                            let _ = media.play_tone(AudioCue::Message);
                        }
                    }
                    if url != self.login.url {
                        continue;
                    }
                    for change in changes {
                        let relevant = self.current.as_ref().is_some_and(|target| match target {
                            Target::Channel(id) => change.channel_id.as_ref() == Some(id),
                            Target::Direct(id) => {
                                change.channel_id.is_none()
                                    && (change.user_id == *id
                                        || change.recipient_id.as_ref() == Some(id))
                            }
                        });
                        if !relevant {
                            continue;
                        }
                        self.pins.apply_change(&change);
                        self.row_heights.invalidate(&change.message_id);
                        if change.deleted {
                            self.messages
                                .retain(|message| message.id != change.message_id);
                            self.search_messages
                                .retain(|message| message.id != change.message_id);
                        } else if let Some(message) = change.message {
                            if let Some(result) = self
                                .search_messages
                                .iter_mut()
                                .find(|result| result.id == message.id)
                            {
                                *result = message.clone();
                            }
                            timeline::merge(
                                &mut self.messages,
                                vec![message],
                                self.history_has_newer,
                            );
                        }
                    }
                }
                Update::AdminResult { request_id, data } => {
                    if let Some(key) = self.admin_requests.remove(&request_id) {
                        self.handle_admin_result(key, data);
                    }
                }
                Update::ReplicaToken {
                    origin,
                    request_id,
                    replica_id,
                    outcome,
                } => self.replica_token_result(origin, request_id, replica_id, outcome),
                Update::MentionInbox {
                    request_id,
                    messages,
                    remaining,
                    errors,
                } => self.apply_mention_inbox(request_id, messages, remaining, errors),
                Update::CommunitySearch {
                    request_id,
                    query,
                    messages,
                    remaining,
                    errors,
                } => self.apply_community_search(request_id, query, messages, remaining, errors),
                Update::BackupSaved { operation_id, path } => {
                    self.uploads.remove(&operation_id);
                    self.notice = Some((
                        format!(
                            "Backup saved: {}",
                            path.file_name().unwrap_or_default().to_string_lossy()
                        ),
                        Instant::now(),
                    ));
                }
                Update::DownloadProgress {
                    operation_id,
                    path,
                    transferred,
                    total,
                } => {
                    if let Some(transfer) = self.uploads.get_mut(&operation_id) {
                        transfer.path = path;
                        transfer.transferred = transferred;
                        transfer.total = total;
                    }
                }
                Update::DownloadFinished { operation_id, path } => {
                    self.uploads.remove(&operation_id);
                    self.notice = Some((
                        format!(
                            "Downloaded {}",
                            path.file_name().unwrap_or_default().to_string_lossy()
                        ),
                        Instant::now(),
                    ));
                }
                Update::DownloadFailed {
                    operation_id,
                    error,
                    cancelled,
                } => {
                    if let Some(transfer) = self.uploads.get_mut(&operation_id) {
                        transfer.error = Some(error);
                        transfer.cancelled = cancelled;
                    }
                }
                Update::BackupFailed {
                    operation_id,
                    error,
                    cancelled,
                } => {
                    if let Some(transfer) = self.uploads.get_mut(&operation_id) {
                        transfer.error = Some(error);
                        transfer.cancelled = cancelled;
                    }
                }
                Update::CryptoStatus { status } => self.encryption_ui.apply_status(status),
                Update::HistoryBackup {
                    origin,
                    request_id,
                    result,
                } => {
                    if origin == self.command_origin() {
                        self.encryption_ui.history.finish(request_id, result);
                    }
                }
                Update::MessageProtections {
                    origin,
                    protections,
                } => {
                    if origin.url == self.login.url && self.session.is_some() {
                        for (message_id, protection) in protections {
                            self.message_protection.insert(message_id, protection);
                        }
                    }
                }
                Update::ReactionSnapshot {
                    url,
                    target,
                    message_id,
                    target_client_id,
                    rows,
                } => {
                    if url == self.login.url && self.current.as_ref() == Some(&target) {
                        if let Some(session) = &self.session {
                            let snapshot = chat_ui::ReactionSnapshot {
                                message_id: &message_id,
                                target_client_id: target_client_id.as_deref(),
                                rows: &rows,
                            };
                            let protection = self.message_protection.get(&message_id);
                            let mut changed = false;
                            for message in self
                                .messages
                                .iter_mut()
                                .chain(self.search_messages.iter_mut())
                            {
                                changed |= chat_ui::apply_reaction_snapshot(
                                    message,
                                    &target,
                                    &session.user.id,
                                    &snapshot,
                                    protection,
                                );
                            }
                            for pin in &mut self.pins.items {
                                changed |= chat_ui::apply_reaction_snapshot(
                                    &mut pin.message,
                                    &target,
                                    &session.user.id,
                                    &snapshot,
                                    protection,
                                );
                            }
                            if changed {
                                self.row_heights.invalidate(&message_id);
                            }
                        }
                    }
                }
                Update::WhisperInvalidated { peer } => {
                    if let Some(media) = &self.media {
                        media.invalidate_whisper(&peer);
                    }
                }
                Update::WhisperKeys {
                    origin,
                    channel_id,
                    peer,
                    keys,
                } => {
                    if self.command_origin().as_ref() == Some(&origin)
                        && self.voice_channel.as_ref() == Some(&channel_id)
                    {
                        match keys {
                            Ok(keys) => {
                                if let Some(media) = &self.media {
                                    if let Err(error) = media.install_whisper(&peer, keys) {
                                        self.notice = Some((error, Instant::now()));
                                    }
                                }
                            }
                            Err(error) => {
                                self.notice =
                                    Some((format!("Whisper unavailable: {error}"), Instant::now()))
                            }
                        }
                    }
                }
                Update::VoiceEncryptionPaused {
                    channel_id,
                    error,
                    reconnecting,
                } => {
                    if self.voice_channel.as_ref() == Some(&channel_id)
                        || self.joining_voice.as_ref() == Some(&channel_id)
                    {
                        let current = self.voice_channel.clone();
                        let pending = self.joining_voice.clone();
                        let request_id = self.voice_request;
                        self.stop_media_for_call_change();
                        self.voice_channel = current;
                        self.joining_voice = pending;
                        self.voice_request = request_id;
                        self.voice_encryption_error = Some(error.clone());
                        self.voice_recovery_pending = reconnecting;
                        self.notice = Some((error, Instant::now()));
                    }
                }
                Update::VoiceEncryptionChanged {
                    url,
                    channel_id,
                    ticket,
                    keys,
                    gateway,
                } => {
                    if url == self.login.url && self.voice_channel.as_ref() == Some(&channel_id) {
                        self.stop_media_for_call_change();
                        self.voice_channel = Some(channel_id);
                        self.start_protected_voice_with_gateway(&url, ticket, keys, gateway);
                        self.request_video_connection();
                    }
                }
                Update::RtcToken {
                    ticket,
                    request_id,
                    keys,
                } => {
                    if self.rtc_request != Some((request_id, self.voice_request))
                        || self.voice_channel.is_none()
                    {
                        continue;
                    }
                    self.rtc_request = None;
                    #[cfg(feature = "native-rtc")]
                    match crate::rtc::RtcSession::connect(ticket, ctx.clone(), keys) {
                        Ok(rtc) => {
                            if let Some(media) = &self.media {
                                rtc.set_application_audio_sink(media.application_audio_sink());
                            }
                            self.rtc = Some(rtc);
                        }
                        Err(error) => self.error = Some(error),
                    }
                    #[cfg(not(feature = "native-rtc"))]
                    {
                        let _ = (ticket, keys);
                        self.notice=Some(("This build cannot publish protected video. Install the native RTC build for video and sharing.".into(),Instant::now()));
                    }
                }
                #[cfg(test)]
                Update::Uploaded { target, attachment } => {
                    self.store_current_draft();
                    self.drafts.apply_uploaded(&target, attachment);
                    self.restore_current_draft();
                    self.defer_preferences();
                }
                #[cfg(test)]
                Update::Sent {
                    target,
                    content,
                    attachment_ids,
                } => {
                    self.store_current_draft();
                    self.drafts.apply_sent(&target, &content, &attachment_ids);
                    self.restore_current_draft();
                    self.defer_preferences();
                    self.pending_sends.remove(&target);
                    self.error = None;
                    if self.current.as_ref() == Some(&target) && self.unread_navigation.is_none() {
                        self.scroll_to_end = true;
                    }
                }
                Update::SendFailed { origin, target, error } => {
                    if origin.as_ref() != self.command_origin().as_ref() {
                        continue;
                    }
                    // A real failure replaces any wait shown for this conversation.
                    if let Some(key) = self.readiness_key(&target) {
                        self.readiness.settled(&key);
                    }
                    self.fail_outgoing(&target);
                    self.pending_sends.remove(&target);
                    self.error = Some(error);
                }
                Update::SendWaiting {
                    target,
                    origin,
                    wait,
                } => {
                    // A wait from an older server or account is remembered for
                    // that connection only; it never changes the active queue.
                    let active = self.command_origin();
                    let key = conversation_readiness_ui::ReadinessKey {
                        origin,
                        target: target.clone(),
                    };
                    if self.readiness.wait_reported(active.as_ref(), key, wait) {
                        self.fail_outgoing(&target);
                        self.pending_sends.remove(&target);
                    }
                }
                Update::SendRateLimited {
                    target,
                    origin,
                    error,
                    code,
                    retry_after_ms,
                } => {
                    if origin.as_ref() != self.command_origin().as_ref() {
                        continue;
                    }
                    // The newest result for this conversation and connection.
                    self.readiness.rate_limited(origin, &target);
                    self.fail_outgoing(&target);
                    self.pending_sends.remove(&target);
                    self.cooldowns
                        .record(target, &code, retry_after_ms, Instant::now());
                    self.notice = Some((error, Instant::now()));
                }
                Update::EditRateLimited {
                    message_id,
                    error,
                    code,
                    retry_after_ms,
                } => {
                    self.pending_edits.remove(&message_id);
                    if let Some(target) = self.current.clone() {
                        self.cooldowns
                            .record(target, &code, retry_after_ms, Instant::now());
                    }
                    self.notice = Some((error, Instant::now()));
                }
                Update::PinsLoaded {
                    url,
                    channel_id,
                    pins,
                    request_id,
                } => {
                    let loaded =
                        self.pins.request.as_ref().is_some_and(|(pending, id)| {
                            *pending == request_id && *id == channel_id
                        });
                    let mutated = self
                        .pins
                        .action
                        .as_ref()
                        .is_some_and(|(pending, _)| *pending == request_id);
                    if url != self.login.url
                        || self.current != Some(Target::Channel(channel_id))
                        || (!loaded && !mutated)
                    {
                        continue;
                    }
                    if loaded {
                        self.pins.request = None;
                    }
                    if mutated {
                        self.pins.action = None;
                    }
                    self.pins.items = pins;
                }
                Update::PinsChanged { url, channel_id } => {
                    if url == self.login.url
                        && self.current == Some(Target::Channel(channel_id))
                        && self.pins.open
                        && self.pins.action.is_none()
                    {
                        self.request_pins();
                    }
                }
                Update::Typing { url, target, state } => {
                    if url == self.login.url && self.current.as_ref() == Some(&target) {
                        let own_id = self
                            .session
                            .as_ref()
                            .map(|s| s.user.id.as_str())
                            .unwrap_or_default();
                        self.typing_users.replace(state, own_id, Instant::now());
                    }
                }
                Update::Edited(id) => {
                    self.pending_edits.remove(&id);
                    if self
                        .editing
                        .as_ref()
                        .is_some_and(|(edited, _)| *edited == id)
                    {
                        self.editing = None;
                    }
                }
                Update::EditFailed { message_id, error } => {
                    self.pending_edits.remove(&message_id);
                    self.error = Some(error);
                }
                Update::LanPairCreated {origin,request_id,result} => self.lan_pair_created(origin,request_id,result),
                Update::Invite {
                    origin,
                    code,
                    request_id,
                    proof,
                } => {
                    if self.command_origin().as_ref() == Some(&origin) {
                        self.receive_invite(ctx, &code, request_id, proof);
                    }
                }
                Update::InviteFailed {
                    origin,
                    request_id,
                    error,
                } => {
                    if self.command_origin().as_ref() == Some(&origin)
                        && self.hosting_setup.fail_invitation_copy(request_id)
                    {
                        self.error = Some(error);
                    }
                }
                Update::ServerChecked {
                    resolved,
                    url,
                    request_id,
                    result,
                } => {
                    self.login.accept_checked(&url,request_id,result.clone(),resolved.clone());
                    self.add_community.accept_checked(&url,request_id,result,resolved);
                }
                Update::CallSwitchResult { request, result } => {
                    self.call_switch_result(request, result, &mut call_transitions)
                }
                Update::CallSwitchStart(ready) => self.start_switched_call(ready),
                Update::VoiceJoined {
                    url,
                    ticket,
                    channel_id,
                    request_id,
                    keys,
                    gateway,
                } => {
                    if !crate::voice_ui_state::accepts_join(
                        self.joining_voice.as_deref(),
                        self.voice_request,
                        &channel_id,
                        request_id,
                    ) {
                        continue;
                    }
                    self.joining_voice = None;
                    self.video.clear();
                    self.voice_join_attempt = Some(request_id);
                    self.voice_channel = Some(channel_id);
                    self.start_protected_voice_with_gateway(&url, ticket, keys, gateway);
                    self.request_video_connection();
                }
                Update::VoiceLeft => {
                    if self.joining_voice.is_none() {
                        let connected = self.voice_connected();
                        self.stop_media();
                        if connected {
                            if let Some(media) = &mut self.media {
                                let _ = media.play_tone(AudioCue::CallEnd);
                            }
                        }
                    }
                }
                Update::VoiceCallFailed {
                    origin,
                    channel_id,
                    request_id,
                    error,
                } => {
                    if self.command_origin().as_ref() == Some(&origin)
                        && crate::voice_ui_state::accepts_call_failure(
                            self.voice_channel.as_deref(),
                            self.voice_join_attempt,
                            self.joining_voice.is_some(),
                            &channel_id,
                            request_id,
                        )
                    {
                        let connected = self.voice_connected();
                        self.stop_media();
                        self.voice_failure(Some(channel_id), error);
                        if connected {
                            if let Some(media) = &mut self.media {
                                let _ = media.play_tone(AudioCue::CallEnd);
                            }
                        }
                    }
                }
                Update::ChatMediaFetched {
                    attachment_id,
                    media,
                } => {
                    if attachment_id.starts_with("avatar:") {
                        self.profile_ui.fetched_avatar(ctx, attachment_id, media);
                        continue;
                    }
                    if attachment_id.starts_with("asset:") {
                        self.fetched_asset(ctx, attachment_id, media);
                        continue;
                    }
                    self.chat_media_pending.remove(&attachment_id);
                    if !self
                        .messages
                        .iter()
                        .chain(self.search_messages.iter())
                        .any(|m| m.attachments.iter().any(|a| a.id == attachment_id))
                    {
                        continue;
                    }
                    let bytes = media.decoded_bytes;
                    let mut total = self
                        .chat_media
                        .values()
                        .map(AnimatedImage::decoded_bytes)
                        .sum::<usize>()
                        .saturating_sub(
                            self.chat_media
                                .get(&attachment_id)
                                .map_or(0, AnimatedImage::decoded_bytes),
                        );
                    while total.saturating_add(bytes) > 32 * 1024 * 1024 {
                        let oldest = self
                            .chat_media
                            .iter()
                            .filter(|(id, _)| {
                                self.media_preview.as_ref() != Some(id) && **id != attachment_id
                            })
                            .min_by_key(|(_, image)| image.last_used())
                            .map(|(id, _)| id.clone());
                        let Some(oldest) = oldest else {
                            break;
                        };
                        if let Some(image) = self.chat_media.remove(&oldest) {
                            total = total.saturating_sub(image.decoded_bytes());
                        }
                    }
                    if total.saturating_add(bytes) <= 32 * 1024 * 1024 {
                        if let Some(image) = self.chat_media.get_mut(&attachment_id) {
                            image.replace_media(media);
                        } else {
                            self.chat_media
                                .insert(attachment_id, AnimatedImage::new(media));
                        }
                    } else {
                        self.chat_media_errors.insert(
                            attachment_id,
                            "Preview memory limit reached. Download to view this file.".into(),
                        );
                    }
                }
                Update::ChatMediaFailed {
                    attachment_id,
                    error,
                } => {
                    if attachment_id.starts_with("asset:") {
                        self.failed_asset(attachment_id);
                        continue;
                    }
                    if attachment_id.starts_with("avatar:") {
                        self.profile_ui.failed_avatar(attachment_id);
                        continue;
                    }
                    self.chat_media_pending.remove(&attachment_id);
                    self.chat_media_errors.insert(attachment_id, error);
                }
                #[cfg(test)]
                Update::Frames(frames) => {
                    for (metadata, bytes) in frames {
                        if let Ok(image) = decode_frame(&bytes) {
                            let key = format!("{}:{}", metadata.user.id, metadata.kind);
                            if !reserve_video_tile(&mut self.video, &key, image.size) {
                                continue;
                            }
                            let texture =
                                ctx.load_texture(&key, image, egui::TextureOptions::LINEAR);
                            self.video.insert(
                                key,
                                VideoTile {
                                    metadata,
                                    texture,
                                    updated: Instant::now(),
                                },
                            );
                        }
                    }
                }
                Update::DecodedFrames {
                    generation,
                    channel_id,
                    frames,
                } => {
                    self.network.acknowledge_live_media();
                    if generation != self.network.live_media_generation()
                        || self.voice_channel.as_ref() != Some(&channel_id)
                    {
                        continue;
                    }
                    for (metadata, frame) in frames {
                        // A queued frame must not recreate a tile removed by a
                        // newer roster update while transport teardown catches up.
                        if self.bootstrap.as_ref().is_none_or(|bootstrap| {
                            crate::voice_ui_state::video_member(
                                bootstrap,
                                Some(&channel_id),
                                &metadata.user.id,
                                &metadata.kind,
                            )
                            .is_none()
                        }) {
                            continue;
                        }
                        let key = format!("{}:{}", metadata.user.id, metadata.kind);
                        let Some(frame) =
                            display_video_frame(frame, ctx.input(|input| input.max_texture_side))
                        else {
                            continue;
                        };
                        if !reserve_video_tile(&mut self.video, &key, frame.size) {
                            continue;
                        }
                        if let Some(tile) = self.video.get_mut(&key) {
                            tile.texture
                                .set(egui::ImageData::Color(frame), egui::TextureOptions::LINEAR);
                            tile.metadata = metadata;
                            tile.updated = Instant::now();
                        } else {
                            let texture = ctx.load_texture(
                                &key,
                                egui::ImageData::Color(frame),
                                egui::TextureOptions::LINEAR,
                            );
                            self.video.insert(
                                key,
                                VideoTile {
                                    metadata,
                                    texture,
                                    updated: Instant::now(),
                                },
                            );
                        }
                    }
                }
                Update::ReadMarked { origin, target, requested_sequence, result } => {
                    if self.command_origin().as_ref() != Some(&origin) {
                        continue;
                    }
                    if requested_sequence == u64::MAX {
                        self.finish_inbox_read(&target, result.is_ok());
                    }
                    let now = Instant::now();
                    let first_failure = self.marked_read.get_mut(&target).is_some_and(|attempt| {
                        attempt.completed(requested_sequence, result.is_ok(), now)
                    });
                    match result {
                        Ok(state) => {
                            if let Some(bootstrap) = self.bootstrap.as_mut() {
                                let reads = &mut Arc::make_mut(bootstrap).read_states;
                                if let Some(old) = reads.iter_mut().find(|old| {
                                    old.channel_id == state.channel_id && old.peer_id == state.peer_id
                                }) {
                                    if state.last_read_sequence >= old.last_read_sequence {
                                        *old = state;
                                    }
                                } else {
                                    reads.push(state);
                                }
                                self.community_reads.insert(origin.url, reads.clone());
                            }
                        }
                        Err(error) if requested_sequence == u64::MAX => self.error = Some(error),
                        Err(_) if first_failure => {
                            self.notice = Some((
                                "Read status could not be saved yet. Relay will retry while this conversation is open.".into(),
                                now,
                            ));
                        }
                        Err(_) => {}
                    }
                }
                Update::AccountSessionsForgotten { request_id, result } => {
                    self.account_sessions_forgotten(request_id, result);
                }
                Update::Notice(text) => self.notice = Some((text, Instant::now())),
                Update::LocalHostFailed(error) => {
                    self.hosting_setup.local_ready = false;
                    if self.account_start.opening_local.is_some() { self.error = Some(error.clone()); }
                    self.account_start.hosting_error = Some(error);
                }
                Update::LocalOpened(saved) => self.local_server_opened(saved),
                Update::Error(e) => {
                    self.friends_connection_failed(&e);
                    self.cancel_background_people_answer();
                    self.error = Some(e);
                }
                Update::VoiceJoinFailed {
                    channel_id,
                    request_id,
                    error,
                } => {
                    if crate::voice_ui_state::accepts_join(
                        self.joining_voice.as_deref(),
                        self.voice_request,
                        &channel_id,
                        request_id,
                    ) {
                        self.joining_voice = None;
                        self.voice_failure(Some(channel_id), error);
                    }
                }
                Update::Busy(busy) => self.busy = busy,
                Update::LocalRestored { iroh_only, address } => {
                    self.account_start.hosting_error = None;
                    self.hosting_setup.local_ready = true;
                    self.hosting_setup.iroh_only = iroh_only;
                    self.hosting_setup.hosted_address = address;
                    self.own_local_server = true;
                }
                Update::LocalReady {
                    created,
                    setup_required,
                } => {
                    // An explicit older-server selection waits for its identity
                    // verification and LocalOpened before using any account.
                    if self.account_start.opening_local.is_some() { continue; }
                    self.account_start.hosting_error = None;
                    self.account_start.creation_request = None;
                    self.hosting_setup.hosted_address = self
                        .bootstrap
                        .as_ref()
                        .and_then(|b| crate::local_host::shared_address(&b.server_id));
                    self.login.setup.edited(&mut self.login.invite);
                    if self.session.is_none() {
                        self.login.url = "http://127.0.0.1:4580".into();
                        self.hosting_setup
                            .local_account_ready(&mut self.login, setup_required);
                    } else {
                        self.hosting_setup.local_ready = true;
                        if self.hosting_setup.new_host {
                            self.claim_new_server(setup_required);
                        }
                    }
                    if self.hosting_setup.new_host && self.central_account.account.is_some()
                        && self.account_start.pending.is_none() {
                        self.claim_new_server(setup_required);
                    }
                    self.hosting_setup.entry = host_setup_ui::LoginEntry::Account;
                    self.own_local_server |= created;
                    self.error = None;
                    self.notice = Some((
                        match setup_required {
                            Some(true) => {
                                "Your server is ready. Create the first account to manage it."
                            }
                            Some(false) => "Your server is ready. Sign in to continue.",
                            None if created => {
                                "Local server is ready. Sign in or create the first account."
                            }
                            None => "Local server is already running. Sign in to enter.",
                        }
                        .into(),
                        Instant::now(),
                    ));
                }
                Update::TailscaleReady { address } => {
                    self.account_start.hosting_error = None;
                    self.hosting_setup.tailscale_ready(address);
                    self.login.setup.edited(&mut self.login.invite);
                    if self.session.is_none() {
                        self.login.url = "http://127.0.0.1:4580".into();
                    }
                    self.login.registering = false;
                    self.own_local_server = true;
                    self.error = None;
                    self.notice = Some((
                        "Private host is running. Sign in and share access with your friend."
                            .into(),
                        Instant::now(),
                    ));
                }
                Update::IrohReady { route, only } => {
                    self.hosting_setup.iroh_ready(only);
                    self.login.setup.edited(&mut self.login.invite);
                    if self.session.is_none() {
                        self.login.url = "http://127.0.0.1:4580".into();
                    }
                    self.login.registering = false;
                    self.own_local_server = true;
                    self.error = None;
                    self.notice = Some((
                        if self.session.is_some() {
                            "Friends can now join without an account. Choose Invite friends."
                        } else {
                            "Your server is running. Create your account, then invite friends."
                        }
                        .into(),
                        Instant::now(),
                    ));
                    drop(route);
                }
                Update::IrohInvitationsStopped { origin } => {
                    if self.command_origin().as_ref() == Some(&origin) {
                        self.hosting_setup.invitations_stopped();
                        self.error = None;
                        self.notice = Some((
                            "Old iroh invitations stopped. Copy a new invitation for anyone reconnecting.".into(),
                            Instant::now(),
                        ));
                    }
                }
                Update::LocalStopped => {
                    self.hosting_setup.local_ready = false;
                    self.hosting_setup.iroh_only = false;
                    self.hosting_setup.hosted_address = None;
                    self.hosting_setup.connection_changed();
                    self.own_local_server = false;
                    self.notice = Some(("Local server stopped".into(), Instant::now()));
                }
            }
        }
        // Drain auth/identity updates before consuming native clicks. A queued
        // prior-account click must not close settings or request focus first.
        self.sync_notification_accounts();
        self.handle_notification_clicks(ctx);

        self.video.retain(|_, tile| {
            tile.updated.elapsed() < Duration::from_secs(3)
                || (self.capture_path.is_some() && tile.metadata.url == "owned-capture://synthetic")
        });
        #[cfg(feature = "native-rtc")]
        if let Some(rtc) = &self.rtc {
            let gallery_visible = self.current.as_ref()
                == self
                    .voice_channel
                    .as_ref()
                    .map(|id| Target::Channel(id.clone()))
                    .as_ref()
                && self.voice_channel.is_some()
                && !self.settings_open;
            rtc.set_video_visible(
                self.remote_video_window_visible(ctx)
                    && (gallery_visible || self.stream_preview.is_some()),
            );
            if rtc.is_sharing(false) {
                self.screen_pending = false;
            }
            if rtc.is_sharing(true) {
                self.camera_pending = false;
            }
            let frames = rtc.frames();
            let keys: HashSet<_> = frames
                .iter()
                .map(|frame| format!("{}:{}", frame.user_id, frame.kind))
                .collect();
            self.video
                .retain(|key, tile| !tile.metadata.url.is_empty() || keys.contains(key));
            for frame in frames {
                let Some(user) = self
                    .bootstrap
                    .as_ref()
                    .and_then(|bootstrap| {
                        crate::voice_ui_state::video_member(
                            bootstrap,
                            self.voice_channel.as_deref(),
                            &frame.user_id,
                            &frame.kind,
                        )
                    })
                    .cloned()
                else {
                    continue;
                };
                let key = format!("{}:{}", frame.user_id, frame.kind);
                let frame_id = frame.timestamp_us.to_string();
                if self
                    .video
                    .get(&key)
                    .is_some_and(|tile| tile.metadata.frame_id == frame_id)
                {
                    continue;
                }
                let Some(image) =
                    display_video_frame(frame.image, ctx.input(|input| input.max_texture_side))
                else {
                    continue;
                };
                if !reserve_video_tile(&mut self.video, &key, image.size) {
                    continue;
                }
                let metadata = MediaFrame {
                    user,
                    kind: frame.kind,
                    frame_id,
                    url: String::new(),
                    width: image.size[0] as u32,
                    height: image.size[1] as u32,
                };
                if let Some(tile) = self.video.get_mut(&key) {
                    tile.texture
                        .set(egui::ImageData::Color(image), egui::TextureOptions::LINEAR);
                    tile.metadata = metadata;
                    tile.updated = Instant::now();
                } else {
                    let texture = ctx.load_texture(
                        &key,
                        egui::ImageData::Color(image),
                        egui::TextureOptions::LINEAR,
                    );
                    self.video.insert(
                        key,
                        VideoTile {
                            metadata,
                            texture,
                            updated: Instant::now(),
                        },
                    );
                }
            }
        }
        #[cfg(feature = "native-rtc")]
        if let Some(error) = self
            .rtc
            .as_ref()
            .and_then(crate::rtc::RtcSession::take_error)
        {
            self.screen_pending = false;
            self.camera_pending = false;
            self.error = Some(format!("Video: {error}"));
        }
        #[cfg(feature = "native-rtc")]
        if crate::rtc::RtcSession::discard_ended(&mut self.rtc) {
            self.screen_pending = false;
            self.camera_pending = false;
            self.rtc_request = None;
            self.share_picker = None;
            self.preview_capture = None;
            self.stop_source_preview();
            self.video.clear();
            self.pinned_stream = None;
            self.focused_stream = None;
            self.stream_preview = None;
            self.sync_voice_state();
        }
        if let Some(failure) = self
            .media
            .as_mut()
            .and_then(MediaEngine::take_device_change_failure)
        {
            crate::audio_settings::restore_device_selection(
                &mut self.audio_applied,
                &failure.attempted,
                &failure.restored,
            );
            crate::audio_settings::restore_device_selection(
                &mut self.prefs.audio,
                &failure.attempted,
                &failure.restored,
            );
            self.error = Some(format!("Could not switch audio devices. The previous devices are still in use; choose another input/output in Voice & Video. Details: {}", failure.error));
            self.save_preferences();
        }
        if let Some(error) = self.media.as_mut().and_then(MediaEngine::take_error) {
            if self.voice_channel.is_none() && self.joining_voice.is_none() {
                if let Some(media) = &mut self.media {
                    media.stop_microphone_test();
                }
                self.mic_test = false;
                self.error = Some(format!("Microphone preview: {error}"));
            } else {
                let channel = self
                    .voice_channel
                    .clone()
                    .or_else(|| self.joining_voice.clone());
                self.stop_media();
                self.voice_failure(channel, error);
                self.send(Command::LeaveVoice);
            }
        }
        if let Some(error) = self.media.as_mut().and_then(MediaEngine::take_sound_error) {
            self.error = Some(format!("Sound effect: {error}"));
        }
        if self.audio_connecting && self.voice_connected() {
            self.audio_connecting = false;
            if let Some(media) = &mut self.media {
                let _ = media.play_tone(AudioCue::Join);
            }
            self.notice = Some(("Voice connected".into(), Instant::now()));
        }
        if let Some(screen) = &mut self.screen {
            if let Some(error) = screen.take_error() {
                self.error = Some(error);
                self.network.next_capture_generation(false);
                release_capture(self.screen.take());
                self.send(Command::StopCapture("screen".into()));
                self.sync_voice_state();
            }
        }
        if let Some(camera) = &mut self.camera {
            if let Some(error) = camera.take_error() {
                self.error = Some(error);
                self.network.next_capture_generation(true);
                release_capture(self.camera.take());
                self.send(Command::StopCapture("camera".into()));
                self.sync_voice_state();
            }
        }
        if self.preview_release.is_none() {
            if let Some((camera, source)) = self.preview_capture.take() {
                self.start_capture(camera, source);
            }
        }
        let sources = [self.source_active(false), self.source_active(true)];
        for (needed, active) in self.video_restart_needed.iter_mut().zip(sources) {
            if active {
                *needed = false;
            }
        }
        if sources != self.last_source_state {
            self.last_source_state = sources;
            self.sync_voice_state();
        }
        if self
            .notice
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(5))
        {
            self.notice = None;
        }
    }
}

impl eframe::App for RelayApp {
    fn on_exit(&mut self, _glow: Option<&eframe::glow::Context>) {
        self.hide_for_exit();
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if raw_input.viewport_id == egui::ViewportId::ROOT {
            self.native_close(_ctx, raw_input.viewport().close_requested());
        }
        desktop_ui::preserve_minimized_layout(raw_input);
        #[cfg(feature = "qa-automation")]
        crate::qa_automation::hook(&mut self.qa_input, _ctx, raw_input);
        #[cfg(feature = "qa-automation")]
        self.qa_commands(_ctx);
        if self
            .focused_stream
            .as_ref()
            .is_some_and(|key| !self.video.contains_key(key))
        {
            self.focused_stream = None;
        }
        if self.stream_fullscreen && self.stream_preview.is_none() {
            self.stream_fullscreen = false;
            _ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(
                self.stream_restore_fullscreen,
            ));
        }
        if self.capture_path.is_some() {
            capture_only_input(raw_input);
        }
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if self.exiting {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        self.preview_activity(ctx);
        self.online_gifs_tick(ctx);
        self.gaming_voice_features(ctx);

        self.network
            .set_update_hold(self.update_ui.preparing_install());
        if !self.update_ui.preparing_install() {
            self.network.reopen_if_stopped(ctx);
        }
        self.updates(ctx);
        self.replica_copy_tick();
        self.handover_tick();
        self.lan_pairing_tick();
        self.game_activity_tick(ctx);
        self.central_account_tick(ctx);
        self.account_login_tick(ctx);
        self.account_link_tick(ctx);
        #[cfg(windows)]
        self.account_surface_tick(ctx);
        self.invitations_tick(ctx);
        #[cfg(windows)]
        if self.invitation_registration_pending {
            self.invitation_registration_pending = false;
            self.invitation_link_status(crate::url_protocol::register_current());
        }
        self.hosting_setup_dialog(ctx);
        self.call_switch_dialog(ctx);
        let update_call_active = self.voice_channel.is_some()
            || self.audio_connecting
            || self.joining_voice.is_some()
            || self.private_calls.intent.is_some()
            || self.call_switch_pending()
            || self.rtc_request.is_some();
        if self.update_ui.tick(
            ctx,
            update_call_active,
            self.own_local_server,
            self.busy
                || self.external_invitation.is_pending()
                || !self.pending_sends.is_empty()
                || self.network.update_busy(),
            self.prefs.automatic_update_checks,
            || self.network.poll_shutdown(),
        ) {
            // No further UI actions can start media/hosting after installer consent.
            self.hide_for_exit();
            return;
        }
        // A verified download can begin handoff in this tick. Hold new network
        // commands immediately, before the remaining UI can enqueue work.
        self.network.set_update_hold(self.update_ui.preparing_install());
        if self.capture_path.is_some() && (self.messages_ready || self.current_name().2) {
            if let Some(panel) = self.initial_capture_panel.take() {
                self.prepare_capture_panel(&panel);
                self.start = Instant::now();
            }
            if let Some(content) = self.capture_send.take() {
                self.draft = content;
                self.submit_composer();
            }
        }
        if self.capture_path.is_some() && !self.capture_authenticated && self.session.is_none() {
            if let Some(panel) = self.initial_capture_panel.take() {
                self.prepare_join_capture_panel(&panel);
            }
        }
        if self.session.is_some() && !self.modal_open() {
            if ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::Comma)) {
                self.settings_open = true;
            } else if ctx.input_mut(|input| {
                input.consume_key(egui::Modifiers::COMMAND, egui::Key::Slash)
            }) {
                self.settings_tab = SettingsTab::Keybinds;
                self.settings_open = true;
            } else if matches!(self.current, Some(Target::Channel(_)))
                && self.home_page.is_none()
                && !self.current_name().2
                && self.has_chat_capability("pins-v1")
                && ctx.input_mut(|input| {
                    input.consume_key(egui::Modifiers::COMMAND, egui::Key::P)
                })
            {
                self.pins.open = true;
                self.request_pins();
            }
        }
        if self.session.is_some() && !self.modal_open() {
            if ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::K)) {
                self.quick_switch = true;
                self.quick_query.clear();
                self.quick_selection = quick_switch_ui::Selection::default();
                ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("quick-switch-input")));
            }
            if !self.current_name().2
                && ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::F))
            {
                self.history_search_open = true;
                ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("history-search")));
            }
        }
        self.dismiss_emoji_completion(ctx);
        // An open profile card or Join friends closes itself through its dialog
        // header, so Escape does not also clear search or other overlays behind
        // it. Invite friends is drawn before this point and consumes Escape itself.
        // The community welcome and Crop Image do the same while they are drawn
        // (Crop only in its own conversation scope), so Escape there closes
        // that dialog alone instead of also closing Settings or the profile editor.
        if self.profile_ui.card.is_none()
            && !self.server_actions.removal_open()
            && !self.add_community_open
            && !self.welcome_dialog_shown()
            && !self.crop_dialog_shown()
            && !self.safety_dialog_shown()
            && ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            let closing_overlay = self.online_gifs.close_message_preview()
                || self.settings_open
                || self.compact_members_open
                || self.history_search_open
                || self.pins.open
                || self.quick_switch
                || self.add_community_open
                || self.share_picker.is_some()
                || self.media_preview.is_some()
                || self.stream_preview.is_some()
                || self.gif_picker
                || self.transfers_open
                || self.server_menu
                || self.delete_confirm.is_some()
                || self.create_channel_open
                || self.invite_dialog.is_some()
                || self.hosting_setup.open
                || self.message_menu.is_some();
            let closing_overlay = self.close_community_overlays()
                || closing_overlay
                || self.profile_ui.card.is_some()
                || self.profile_ui.menu_anchor.is_some();
            self.profile_ui.card = None;
            self.profile_ui.card_request = None;
            self.profile_ui.menu_anchor = None;
            self.compact_members_open = false;
            if self.history_search_open || !self.search.is_empty() {
                self.history_search_open = false;
                self.search.clear();
                self.search_history(false);
            }
            self.quick_switch = false;
            self.add_community_open = false;
            self.add_community.password.clear();
            self.settings_open = false;
            self.share_picker = None;
            self.stop_source_preview();
            self.media_preview = None;
            self.stream_preview = None;
            self.gif_picker = false;
            self.online_gifs.close_picker();
            self.gif_previews = None;
            self.gif_textures.clear();
            self.gif_pending.clear();
            self.gif_errors.clear();
            self.transfers_open = false;
            self.gif_hover = None;
            self.gif_hover_pending = None;
            self.gif_hover_errors.clear();
            self.pins.open = false;
            self.server_menu = false;
            self.delete_confirm = None;
            self.create_channel_open = false;
            self.invite_dialog = None;
            self.hosting_setup.open = false;
            self.hosting_setup.cancel_invitation_copy();
            self.message_menu = None;
            if let Some(media) = &mut self.media {
                media.stop_microphone_test();
            }
            self.mic_test = false;
            chat_ui::dismiss_inline_on_escape(closing_overlay, &mut self.editing, &mut self.reply);
            if closing_overlay && self.current.is_some() {
                ctx.memory_mut(|memory| {
                    memory.request_focus(egui::Id::new(("composer-editor", self.current.clone())))
                });
            }
            self.defer_preferences();
        }
        self.update_appearance(ctx, frame.info().system_theme);
        self.desktop_titlebar(ctx);
        self.update_ui.show_banner(
            ctx,
            update_call_active,
            self.own_local_server,
            self.busy
                || self.external_invitation.is_pending()
                || !self.pending_sends.is_empty()
                || self.network.update_busy(),
        );
        #[cfg(windows)]
        let account_surface = self.account_surface_view(ctx);
        #[cfg(not(windows))]
        let account_surface = false;
        let account_gateway = account_surface || self.account_front(ctx);
        if !account_gateway {
            if self.session.is_none() && !self.signed_in_shell() {
                self.login_view(ctx);
            } else {
                self.rail(ctx);
                self.sidebar(ctx);
                self.members(ctx);
                self.main_view(ctx);
            }
        }
        if self.settings_open && !account_gateway {
            self.settings_view(ctx);
        }
        self.discard_hidden_discord_token();
        self.discard_hidden_authenticator();
        if !account_gateway {
            self.dialogs(ctx);
        }
        if self.capture_icons {
            // Input is disabled in capture mode. Draw the actual shared controls without joining a call.
            desktop_ui::dialog(ctx, "Relay icon preview")
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label("Voice panel and call controls");
                    for active in [false, true] {
                        ui.horizontal(|ui| {
                            call_control(ui, Icon::Mic, active, false, "Microphone");
                            call_control(ui, Icon::Headphones, active, false, "Deafen");
                            call_control(ui, Icon::Camera, active, false, "Camera");
                            call_control(ui, Icon::Screen, active, false, "Screen share");
                        });
                    }
                    ui.horizontal(|ui| {
                        icon_button(ui, Icon::HangUp, false, "Disconnect voice");
                        call_control(ui, Icon::HangUp, false, true, "Disconnect");
                    });
                });
        }
        self.desktop_resize(ctx);
        self.typing_heartbeat(ctx);
        self.trim_preview_cache(ctx);
        if self.mic_test {
            ctx.request_repaint_after(Duration::from_millis(50));
        } else if self.audio_connecting || self.media.as_ref().is_some_and(MediaEngine::is_starting)
        {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if self.voice_channel.is_some() {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
        if let Some((_, at)) = &self.notice {
            ctx.request_repaint_after(Duration::from_secs(5).saturating_sub(at.elapsed()));
        }
        if let Some(at) = self.preferences_dirty_at {
            if at.elapsed() >= Duration::from_millis(750) {
                self.save_preferences();
            } else {
                ctx.request_repaint_after(Duration::from_millis(750).saturating_sub(at.elapsed()));
            }
        }
        if let Some(at) = self.search_changed_at {
            if at.elapsed() >= Duration::from_millis(350) {
                self.search_changed_at = None;
                self.search_history(false);
            } else {
                ctx.request_repaint_after(Duration::from_millis(350).saturating_sub(at.elapsed()));
            }
        }
        if self.capture_path.is_some() && !self.capture_requested {
            ctx.request_repaint_after(
                Duration::from_secs(3)
                    .saturating_sub(self.start.elapsed())
                    .max(Duration::from_millis(100)),
            );
        }
        if self.capture_path.is_some()
            && !self.capture_requested
            && self.start.elapsed() > Duration::from_secs(3)
            && self.pending_sends.is_empty()
            && !self.community_assets_pending()
            && (!self.capture_authenticated
                || (self.session.is_some()
                    && self.initial_capture_panel.is_none()
                    && !self.admin_requests.values().any(|key| {
                        key == "community-search" || key.starts_with("system-notices:")
                    })
                    && (self.messages_ready || self.current_name().2)))
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
            self.capture_requested = true;
        }
        if let Some(path) = &self.capture_path {
            ctx.input(|input| {
                for event in &input.events {
                    if let egui::Event::Screenshot { image, .. } = event {
                        let bytes: Vec<u8> =
                            image.pixels.iter().flat_map(|c| c.to_array()).collect();
                        let _ = image::save_buffer(
                            path,
                            &bytes,
                            image.size[0] as u32,
                            image.size[1] as u32,
                            image::ColorType::Rgba8,
                        );
                    }
                }
            });
        }
        if let Some(probe) = self.native_render_probe.as_mut() {
            let images = ctx.input(|input| {
                input
                    .events
                    .iter()
                    .filter_map(|event| match event {
                        egui::Event::Screenshot { viewport_id, image }
                            if *viewport_id == egui::ViewportId::ROOT =>
                        {
                            Some(image.clone())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            });
            for image in images {
                probe.complete(&image);
            }
            probe.update(ctx);
        }
    }
}
