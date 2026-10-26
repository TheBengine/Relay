//! Persist uncertain sends before dispatch; reopening a conversation never sends them.
use super::*;
use crate::network::SendBinding;
use durable_outbox::Entry;

pub(super) struct PendingDispatch {
    origin: CommandOrigin,
    scope: String,
    target: Target,
    client_id: String,
    command: Command,
    receipt: Receiver<Result<(), String>>,
}

#[derive(Debug, PartialEq, Eq)]
enum PersistenceState {
    Waiting,
    Saved,
    Failed(String),
    Cancelled,
}

fn persistence_state(
    receipt: &Receiver<Result<(), String>>,
    origin: &CommandOrigin,
    active: Option<&CommandOrigin>,
    scope_matches: bool,
    entry_present: bool,
) -> PersistenceState {
    if active != Some(origin) || !scope_matches || !entry_present {
        return PersistenceState::Cancelled;
    }
    match receipt.try_recv() {
        Ok(Ok(())) => PersistenceState::Saved,
        Ok(Err(error)) => PersistenceState::Failed(error),
        Err(mpsc::TryRecvError::Disconnected) => {
            PersistenceState::Failed("The local storage worker stopped.".into())
        }
        Err(mpsc::TryRecvError::Empty) => PersistenceState::Waiting,
    }
}

impl RelayApp {
    fn outgoing_binding(&self) -> Option<SendBinding> {
        let bootstrap = self.bootstrap.as_ref()?;
        let accepted = self
            .prefs
            .communities
            .iter()
            .find(|saved| saved.url == self.login.url && saved.server_id == bootstrap.server_id)
            .and_then(|saved| saved.open_protection_accepted);
        let plain = crate::protection::plain_allowed(&bootstrap.protection, accepted);
        let status = &self.encryption_ui.status;
        Some(SendBinding {
            protection: bootstrap.protection.clone(),
            plain,
            device_id: if plain {
                String::new()
            } else {
                status.own_device_id.clone()
            },
            fingerprint: if plain {
                String::new()
            } else {
                status.own_fingerprint.clone()
            },
        })
    }

    pub(super) fn stage_outgoing(
        &mut self,
        target: Target,
        message: Message,
        clear_composer: bool,
    ) {
        let (Some(scope), Some(origin)) = (self.conversation_scope(), self.command_origin()) else {
            return;
        };
        let Some(client_id) = message.client_id.clone() else {
            return;
        };
        // A retry retains its first protection and device binding, even after a reset.
        let entry = if let Some(saved) = self.outbox.get(&scope, &origin, &target, &client_id) {
            saved.clone()
        } else {
            let Some(binding) = self.outgoing_binding() else {
                return;
            };
            if !binding.plain && (binding.device_id.is_empty() || binding.fingerprint.is_empty()) {
                self.error = Some("Encryption is still getting ready. Your draft is unchanged; try again shortly.".into());
                return;
            }
            match Entry::from_message(
                scope.clone(),
                origin.clone(),
                target.clone(),
                binding,
                &message,
            ) {
                Ok(entry) => entry,
                Err(error) => {
                    self.error = Some(error);
                    return;
                }
            }
        };
        if let Err(error) = self.outbox.insert(entry.clone()) {
            self.error = Some(error);
            return;
        }
        let Some((attempt, mut command)) =
            chat_ui::outgoing_retry(&target, &entry.message(&message.author), origin.clone())
        else {
            return;
        };
        if let Command::SendRich { binding, .. } = &mut command {
            *binding = Some(entry.binding.clone());
        }
        self.outgoing_messages.retain(|(saved, local, _, _)| {
            saved != &target || local.client_id.as_deref() != Some(&client_id)
        });
        self.outgoing_messages
            .push((target.clone(), entry.message(&message.author), false, false));
        self.send_ids.insert(target.clone(), attempt);
        self.pending_sends.insert(target.clone());
        self.scroll_to_end = true;
        if clear_composer {
            self.stop_typing();
            self.draft.clear();
            self.attachments.clear();
            self.reply = None;
            self.store_current_draft();
        }
        self.prepare_preferences();
        if !self.prefs.remember_drafts {
            // The privacy choice explicitly opts out of restart recovery; RAM still
            // retains the message across community switches during this session.
            self.dispatch_outgoing(command, &target);
            self.save_preferences();
            return;
        }
        let receipt = preferences_snapshot(&self.prefs, &self.audio_applied)
            .map_err(|_| "Cannot prepare the saved message. It has not been sent.".to_string())
            .and_then(|bytes| self.preference_writer.save_confirmed(bytes));
        match receipt {
            Ok(receipt) => {
                self.outbox_pending.push(PendingDispatch {
                    origin,
                    scope,
                    target,
                    client_id,
                    command,
                    receipt,
                });
                self.capture_ctx
                    .request_repaint_after(Duration::from_millis(50));
            }
            Err(error) => self.outbox_storage_failed(&target, error),
        }
    }

    fn dispatch_outgoing(&mut self, command: Command, target: &Target) {
        if let Err(error) = self.network.send(command) {
            self.fail_outgoing(target);
            self.pending_sends.remove(target);
            self.error = Some(error);
        }
    }

    fn outbox_storage_failed(&mut self, target: &Target, error: String) {
        self.fail_outgoing(target);
        self.pending_sends.remove(target);
        self.preferences_storage_error = Some(error.clone());
        self.error = Some(format!("This message has not been sent because it could not be saved on this PC. It remains in this conversation; retry before closing Relay. {error}"));
    }

    pub(super) fn poll_outbox(&mut self, ctx: &egui::Context) {
        let mut waiting = Vec::new();
        for pending in std::mem::take(&mut self.outbox_pending) {
            let state = persistence_state(
                &pending.receipt,
                &pending.origin,
                self.command_origin().as_ref(),
                self.conversation_scope().as_deref() == Some(&pending.scope),
                self.outbox
                    .get(
                        &pending.scope,
                        &pending.origin,
                        &pending.target,
                        &pending.client_id,
                    )
                    .is_some(),
            );
            if state == PersistenceState::Cancelled {
                // A changed session leaves a recoverable, manually retried entry.
                if self
                    .send_ids
                    .get(&pending.target)
                    .is_some_and(|attempt| attempt.client_id == pending.client_id)
                {
                    self.fail_outgoing(&pending.target);
                    self.pending_sends.remove(&pending.target);
                    self.send_ids.remove(&pending.target);
                }
                continue;
            }
            match state {
                PersistenceState::Saved => {
                    self.preferences_storage_error = None;
                    self.dispatch_outgoing(pending.command, &pending.target);
                }
                PersistenceState::Failed(error) => {
                    self.outbox_storage_failed(&pending.target, error)
                }
                PersistenceState::Waiting => waiting.push(pending),
                PersistenceState::Cancelled => unreachable!(),
            }
        }
        self.outbox_pending = waiting;
        if !self.outbox_pending.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    pub(super) fn restore_outbox(&mut self) {
        self.outbox_pending.clear();
        let (Some(scope), Some(origin), Some(bootstrap)) = (
            self.conversation_scope(),
            self.command_origin(),
            self.bootstrap.as_ref(),
        ) else {
            return;
        };
        self.outgoing_messages = self
            .outbox
            .scoped(&scope, &origin)
            .filter(|entry| target_available(&entry.target, bootstrap))
            .map(|entry| {
                (
                    entry.target.clone(),
                    entry.message(&bootstrap.user),
                    true,
                    false,
                )
            })
            .collect();
    }

    pub(super) fn acknowledge_outbox(
        &mut self,
        origin: &CommandOrigin,
        target: &Target,
        client_id: &str,
    ) {
        if self.command_origin().as_ref() != Some(origin) {
            return;
        }
        if let Some(scope) = self.conversation_scope() {
            if self.outbox.remove(&scope, origin, target, client_id) {
                self.save_preferences();
            }
        }
    }

    pub(super) fn reconcile_outbox_history(&mut self) {
        let (Some(scope), Some(origin), Some(target)) = (
            self.conversation_scope(),
            self.command_origin(),
            self.current.clone(),
        ) else {
            return;
        };
        let mut changed = false;
        for message in &self.messages {
            changed |= self.outbox.acknowledge(&scope, &origin, &target, message);
        }
        if changed {
            self.save_preferences();
        }
    }

    pub(super) fn remove_outgoing(&mut self, message: &Message) {
        let (Some(scope), Some(origin), Some(target), Some(client_id)) = (
            self.conversation_scope(),
            self.command_origin(),
            self.current.clone(),
            message.client_id.as_ref(),
        ) else {
            return;
        };
        if self.pending_sends.contains(&target) {
            return;
        }
        self.outbox.remove(&scope, &origin, &target, client_id);
        self.outgoing_messages
            .retain(|(saved, local, _, _)| saved != &target || local.id != message.id);
        if self
            .send_ids
            .get(&target)
            .is_some_and(|attempt| &attempt.client_id == client_id)
        {
            self.send_ids.remove(&target);
        }
        // Encryption history is deliberately untouched: an uncertain send may
        // already have reached the server, and its nonce must never be reused.
        self.save_preferences();
    }

    pub(super) fn saving_outgoing(&self, message: &Message) -> bool {
        self.outbox_pending
            .iter()
            .any(|pending| message.client_id.as_ref() == Some(&pending.client_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> CommandOrigin {
        CommandOrigin {
            url: "https://synthetic.example".into(),
            server_id: "community".into(),
            user_id: "account".into(),
        }
    }

    #[test]
    fn dispatch_waits_for_the_exact_successful_snapshot_and_never_treats_failure_as_saved() {
        let origin = origin();
        let (tx, rx) = mpsc::channel();
        assert_eq!(
            persistence_state(&rx, &origin, Some(&origin), true, true),
            PersistenceState::Waiting
        );
        tx.send(Err("disk full".into())).unwrap();
        assert_eq!(
            persistence_state(&rx, &origin, Some(&origin), true, true),
            PersistenceState::Failed("disk full".into())
        );
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(())).unwrap();
        assert_eq!(
            persistence_state(&rx, &origin, Some(&origin), true, true),
            PersistenceState::Saved
        );
    }

    #[test]
    fn successful_snapshot_cannot_dispatch_after_account_switch_or_history_acknowledgement() {
        let origin = origin();
        let mut another = origin.clone();
        another.user_id = "another-account".into();
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(())).unwrap();
        assert_eq!(
            persistence_state(&rx, &origin, Some(&another), true, true),
            PersistenceState::Cancelled
        );
        assert_eq!(
            persistence_state(&rx, &origin, Some(&origin), false, true),
            PersistenceState::Cancelled
        );
        assert_eq!(
            persistence_state(&rx, &origin, Some(&origin), true, false),
            PersistenceState::Cancelled
        );
    }
}
