//! Owner-only local pairing creation captures the exact source and account before background IO.
use super::*;
use relay_protocol::lan_pairing as model;
static CREATING: AtomicUsize = AtomicUsize::new(0);
struct Permit;
impl Drop for Permit {
    fn drop(&mut self) {
        CREATING.fetch_sub(1, Ordering::AcqRel);
    }
}
impl Worker {
    pub(super) fn create_lan_pairing(
        &self,
        origin: CommandOrigin,
        request_id: u64,
        cancel: Arc<AtomicBool>,
    ) -> Result<(), String> {
        let connection = self.command_connection(&origin)?;
        if !connection.chat.supports(model::CAPABILITY) || connection.chat.supports("people-v1") {
            return Err("Update this community's host before creating a local code.".into());
        }
        crate::community::account_destination(&connection.url).map_err(|_| {
            "Use this PC's local connection, HTTPS or Relay's private route to create a code."
        })?;
        let authority = connection
            .chat
            .authority
            .clone()
            .ok_or("This host has no verified identity yet.")?;
        let saved = crate::iroh_route::hosted(&connection.server_id)
            .or_else(|| crate::iroh_route::saved_for(&connection.url));
        let destination = if let Some(route) = saved {
            relay_protocol::host_locator::Destination {
                url: route.address(),
                iroh: Some(handover::IrohDestination {
                    scope: Some(authority.host.host_id.clone()),
                    endpoint_id: route.endpoint_id,
                    relay: route.relay,
                    token: route.token,
                }),
            }
        } else {
            let url = crate::host_route::safe_destination(&connection.url).map_err(|_| {
                "Enable private sharing above, or use the host's HTTPS connection first."
            })?;
            if !url.starts_with("https://") {
                return Err("Enable private sharing above first.".into());
            }
            relay_protocol::host_locator::Destination { url, iroh: None }
        };
        let invite = self
            .request(reqwest::Method::POST, "/api/invites")?
            .json(&serde_json::json!({"max_uses":1}))
            .timeout(Duration::from_secs(4));
        let create = self
            .request(reqwest::Method::POST, model::CREATE_PATH)?
            .timeout(Duration::from_secs(4));
        let revoke = self
            .request(reqwest::Method::POST, model::REVOKE_PATH)?
            .timeout(Duration::from_secs(3));
        CREATING
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 2).then_some(n + 1)
            })
            .map_err(|_| "A local code is already being created. Try again shortly.")?;
        let permit = Permit;
        let lease = shutdown::Lease::acquire(self.background_active.clone());
        let work = self.work.clone();
        let updates = self.updates.clone();
        let wake = self.wake.clone();
        let ctx = self.ctx.clone();
        std::thread::Builder::new()
            .name("relay-lan-code".into())
            .spawn(move || {
                let _permit = permit;
                let _lease = lease;
                let result = (|| {
                    if cancel.load(Ordering::Acquire) || work.hold.load(Ordering::Acquire) {
                        return Err(crate::lan_pairing::FAILED.into());
                    }
                    let value: serde_json::Value =
                        parse(invite.send().map_err(|_| crate::lan_pairing::FAILED)?)?;
                    let invite_code = value["code"]
                        .as_str()
                        .filter(|v| !v.is_empty())
                        .ok_or(crate::lan_pairing::FAILED)?;
                    if cancel.load(Ordering::Acquire) || work.hold.load(Ordering::Acquire) {
                        return Err(crate::lan_pairing::FAILED.into());
                    }
                    let code = crate::lan_pairing::Code::generate()?;
                    let id = code.id(&origin.server_id);
                    let body = model::Create {
                        code: code.wire(),
                        invitation: model::Invitation {
                            server_id: origin.server_id.clone(),
                            code: invite_code.into(),
                            authority,
                            destination,
                        },
                    };
                    let created: model::Created = parse(
                        create
                            .json(&body)
                            .send()
                            .map_err(|_| crate::lan_pairing::FAILED)?,
                    )?;
                    let now = crate::lan_pairing::now_ms()?;
                    if created.id != id
                        || created.expires_ms <= now
                        || created.expires_ms > now.saturating_add(model::LIFETIME_MS + 30_000)
                    {
                        return Err(crate::lan_pairing::FAILED.into());
                    }
                    if cancel.load(Ordering::Acquire) || work.hold.load(Ordering::Acquire) {
                        let _ = revoke.json(&model::Revoke { id }).send();
                        return Err(crate::lan_pairing::FAILED.into());
                    }
                    Ok(crate::lan_pairing::Ready {
                        code: code.display(),
                        id,
                        expires_ms: created.expires_ms,
                    })
                })();
                if !cancel.load(Ordering::Acquire) {
                    let _ = updates.send(Update::LanPairCreated {
                        origin,
                        request_id,
                        result,
                    });
                }
                wake.notify();
                ctx.request_repaint();
            })
            .map_err(|_| "Unable to create a local code. Try again.")?;
        Ok(())
    }
    pub(super) fn revoke_lan_pairing(&self, origin: &CommandOrigin, id: &str) {
        if self.command_connection(origin).is_err() {
            return;
        }
        let Ok(request) = self.request(reqwest::Method::POST, model::REVOKE_PATH) else {
            return;
        };
        let request = request
            .json(&model::Revoke { id: id.into() })
            .timeout(Duration::from_secs(3));
        let lease = shutdown::Lease::acquire(self.background_active.clone());
        let _ = std::thread::Builder::new()
            .name("relay-lan-code-revoke".into())
            .spawn(move || {
                let _lease = lease;
                let _ = request.send();
            });
    }
}
