//! Host-issued local codes and the explicit LAN first-use form.
use super::*;
use crate::connection_setup::FlowKind;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[derive(Default)]
pub(super) struct HostState {
    owner: Option<CommandOrigin>,
    request: Option<u64>,
    ready: Option<crate::lan_pairing::Ready>,
    cancel: Option<Cancel>,
    error: Option<String>,
}
struct Cancel(Arc<AtomicBool>);
impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
impl RelayApp {
    pub(super) fn lan_pairing_tick(&mut self) {
        let current = self.command_origin();
        if !self.hosting_setup.open || self.lan_pairing_host.owner != current {
            if let (Some(origin), Some(ready)) =
                (&self.lan_pairing_host.owner, &self.lan_pairing_host.ready)
            {
                if current.as_ref() == Some(origin) {
                    let _ = self.network.send(Command::RevokeLanPairing {
                        origin: origin.clone(),
                        id: ready.id.clone(),
                    });
                }
            }
            self.lan_pairing_host = HostState {
                owner: current,
                ..Default::default()
            };
        }
        if self
            .lan_pairing_host
            .ready
            .as_ref()
            .is_some_and(|r| crate::lan_pairing::now_ms().map_or(true, |now| now >= r.expires_ms))
        {
            self.lan_pairing_host.ready = None;
        }
    }
    pub(super) fn lan_pair_created(
        &mut self,
        origin: CommandOrigin,
        request_id: u64,
        result: Result<crate::lan_pairing::Ready, String>,
    ) {
        if !self.hosting_setup.open
            || self.command_origin().as_ref() != Some(&origin)
            || self.lan_pairing_host.owner.as_ref() != Some(&origin)
            || self.lan_pairing_host.request != Some(request_id)
        {
            return;
        }
        self.lan_pairing_host.request = None;
        self.lan_pairing_host.cancel = None;
        match result {
            Ok(ready) => {
                self.lan_pairing_host.ready = Some(ready);
                self.lan_pairing_host.error = None;
            }
            Err(error) => self.lan_pairing_host.error = Some(error),
        }
    }
    pub(super) fn lan_pairing_host_controls(&mut self, ui: &mut egui::Ui) {
        let supported = self.bootstrap.as_ref().is_some_and(|b| {
            b.capabilities
                .iter()
                .any(|c| c == relay_protocol::lan_pairing::CAPABILITY)
                && b.has_permission(PERMISSION_ADMIN)
        });
        if !supported {
            return;
        }
        let mut create = false;
        let mut revoke = false;
        ui.collapsing("Connect someone on this network",|ui|{
            ui.label("Your friend chooses Find on local network, selects this server, then pastes your local code. It works for one person and expires after two minutes.");
            if let Some(ready)=&self.lan_pairing_host.ready{
                ui.label(RichText::new(ready.code.as_str()).monospace().strong());
                if ui.add(secondary("Copy local code")).clicked(){ui.output_mut(|out|out.copied_text=ready.code.to_string());}
                let seconds=crate::lan_pairing::now_ms().map(|now|ready.expires_ms.saturating_sub(now)/1000).unwrap_or(0);
                ui.label(RichText::new(format!("Expires in {seconds} seconds")).color(c_muted()));
                ui.ctx().request_repaint_after(Duration::from_secs(1));
                if ui.add(secondary("Revoke code")).clicked(){revoke=true;}
            }else if self.lan_pairing_host.request.is_some(){ui.label("Creating local code...");}
            else if ui.add_enabled(!self.busy,secondary("Create local code")).clicked(){create=true;}
            ui.label(RichText::new("On the host, enable Find on my local network in Server Settings and restart the server to apply it. Also enable Relay's built-in private sharing, or use HTTPS. Login and server protection work as usual.").color(c_muted()));
            if let Some(error)=&self.lan_pairing_host.error{ui.label(RichText::new(error).color(c_bad()));}
        });
        if revoke {
            if let (Some(origin), Some(ready)) =
                (self.command_origin(), self.lan_pairing_host.ready.take())
            {
                let _ = self.network.send(Command::RevokeLanPairing {
                    origin,
                    id: ready.id,
                });
            }
        }
        if create {
            let Some(origin) = self.command_origin() else {
                return;
            };
            let request_id = self.next_request();
            let cancel = Arc::new(AtomicBool::new(false));
            self.lan_pairing_host = HostState {
                owner: Some(origin.clone()),
                request: Some(request_id),
                cancel: Some(Cancel(cancel.clone())),
                ..Default::default()
            };
            if let Err(error) = self.network.send(Command::CreateLanPairing {
                origin,
                request_id,
                cancel,
            }) {
                self.lan_pairing_host.request = None;
                self.lan_pairing_host.error = Some(error);
            }
        }
    }
    pub(super) fn lan_pairing_join_controls(&mut self, ui: &mut egui::Ui, kind: FlowKind) {
        let flow = if kind == FlowKind::AddCommunity {
            &mut self.add_community
        } else {
            &mut self.login
        };
        let finished = flow
            .setup
            .lan_pairing
            .job
            .as_ref()
            .and_then(|job| job.poll());
        if let Some(result) = finished {
            let still_selected = flow.setup.lan_pairing.found.as_ref().is_some_and(|f| {
                crate::community::normalize_url(&flow.url).ok().as_deref() == Some(f.url().as_str())
            });
            flow.setup.lan_pairing.job = None;
            if !still_selected {
                flow.setup.lan_pairing = Default::default();
                return;
            }
            match result {
                Ok(invitation) => {
                    flow.setup.edited(&mut flow.invite);
                    flow.url = invitation.server.clone();
                    flow.invite = invitation.code.clone();
                    flow.registering = true;
                    flow.setup.invitation = Some(invitation);
                    flow.setup.route_note =
                        Some("Secure invitation received from the host's local code".into());
                    return;
                }
                Err(error) => flow.setup.lan_pairing.error = Some(error),
            }
        }
        let Some(found) = flow.setup.lan_pairing.found.clone() else {
            return;
        };
        let mut start = false;
        let mut cancel = false;
        ui.group(|ui|{
            ui.label(RichText::new(format!("Local code for {}",found.name)).strong());
            let waiting=flow.setup.lan_pairing.job.is_some();
            ui.add_enabled_ui(!waiting,|ui|{field(ui,"LOCAL CODE",&mut flow.setup.lan_pairing.code,true);});
            ui.label(RichText::new("Ask the host to open Invite friends > Connect someone on this network. Paste its LAN- code here.").color(c_muted()));
            if waiting{ui.label("Checking the local code...");ui.ctx().request_repaint_after(Duration::from_millis(100));}
            else if ui.add(secondary("Use local code")).clicked(){start=true;}
            if ui.add(secondary("Cancel")).clicked(){cancel=true;}
            if let Some(error)=&flow.setup.lan_pairing.error{ui.label(RichText::new(error).color(c_bad()));}
        });
        if cancel {
            flow.setup.lan_pairing = Default::default();
            return;
        }
        if !start {
            return;
        }
        let text = flow.setup.lan_pairing.code.clone();
        let urls: Vec<_> = self
            .prefs
            .communities
            .iter()
            .filter(|s| {
                s.server_id.eq_ignore_ascii_case(&found.server_id)
                    || crate::community::normalize_url(&s.url).ok().as_deref()
                        == Some(found.url().as_str())
            })
            .map(|s| s.url.clone())
            .collect();
        let pins = self
            .bootstrap
            .as_ref()
            .filter(|b| b.server_id.eq_ignore_ascii_case(&found.server_id))
            .and_then(|b| {
                self.command_origin().and_then(|origin| {
                    crate::host_route::bootstrap_pin(
                        &origin.url,
                        &b.server_id,
                        b.host_authority.as_ref(),
                        None,
                    )
                    .ok()
                    .flatten()
                })
            })
            .into_iter()
            .collect();
        let ctx = ui.ctx().clone();
        let result = crate::lan_pairing::JoinJob::start(found, &text, urls, pins, move || {
            ctx.request_repaint()
        });
        let flow = if kind == FlowKind::AddCommunity {
            &mut self.add_community
        } else {
            &mut self.login
        };
        match result {
            Ok(job) => {
                flow.setup.lan_pairing.job = Some(job);
                flow.setup.lan_pairing.error = None;
            }
            Err(error) => flow.setup.lan_pairing.error = Some(error),
        }
    }
}
