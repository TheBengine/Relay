//! Recovery hints are queued before publication. Community credentials never
//! reach the directory, and neither issuance nor retry blocks the call worker.
use super::*;
use relay_protocol::host_locator as model;
use sha2::{Digest, Sha256};

static ISSUING: AtomicUsize = AtomicUsize::new(0);
static PUBLISHING: AtomicBool = AtomicBool::new(false);
const POLL: Duration = Duration::from_secs(30);
const UNSAVED: &str = "The invitation works while this host is online. Relay could not save its recovery address; try sharing it again.";
const QUEUED: &str = "The recovery address is saved on this PC. Relay will retry sending it when the directory is available.";

struct Issuing;
impl Drop for Issuing {
    fn drop(&mut self) {
        ISSUING.fetch_sub(1, Ordering::AcqRel);
    }
}
struct Publishing;
impl Drop for Publishing {
    fn drop(&mut self) {
        PUBLISHING.store(false, Ordering::Release);
    }
}
fn clock() -> Result<u64, String> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| UNSAVED)?
        .as_millis()
        .min(u64::MAX as u128) as u64)
}

/// At most one bounded publisher. The captured directory also chooses the
/// queue scope: a provider change must not send an old queue somewhere else.
fn send_due(
    base: &str,
    work: &shutdown::Work,
    cancelled: &AtomicBool,
    renewal: Option<&crate::handover_runtime::LocatorTask>,
) -> Result<bool, String> {
    if PUBLISHING.swap(true, Ordering::AcqRel) {
        return Ok(false);
    }
    let _permit = Publishing;
    let mut queue = crate::host_locator_outbox::Outbox::for_service(base)?;
    for _ in 0..16 {
        if work.hold.load(Ordering::Acquire) || cancelled.load(Ordering::Acquire) {
            return Ok(false);
        }
        if let Some(record) = queue.due(clock()?) {
            match crate::host_locator::publish_at(base, &record) {
                Ok(()) => queue.sent(&record)?,
                Err(_) => {
                    queue.failed(&record, clock()?)?;
                    return Err(QUEUED.into());
                }
            }
            continue;
        }
        let Some(renewal) = renewal else {
            return Ok(false);
        };
        match renewal.advance(&mut queue, clock()?)? {
            crate::host_locator_renewal::Advance::Queued { .. } => continue,
            crate::host_locator_renewal::Advance::Complete
            | crate::host_locator_renewal::Advance::Full => return Ok(false),
        }
    }
    Ok(true)
}

impl Worker {
    pub(super) fn publish_invitation_locator(&self, code: &str) {
        let Some(c) = &self.connection else {
            return;
        };
        if !c.chat.supports("host-locator-issuer-v1") || c.chat.supports("people-v1") {
            return;
        }
        let Some(base) = self.host_locator_service.clone() else {
            return;
        };
        let Some(anchor) = c.chat.authority.clone() else {
            return;
        };
        let destination = if let Some(route) = crate::iroh_route::hosted(&c.server_id) {
            model::Destination {
                url: route.address(),
                iroh: Some(handover::IrohDestination {
                    scope: Some(anchor.host.host_id.clone()),
                    endpoint_id: route.endpoint_id,
                    relay: route.relay,
                    token: route.token,
                }),
            }
        } else if let Ok(url) = crate::host_route::safe_destination(&c.url) {
            let iroh = crate::iroh_route::saved_for(&url).map(|r| handover::IrohDestination {
                scope: Some(anchor.host.host_id.clone()),
                endpoint_id: r.endpoint_id,
                relay: r.relay,
                token: r.token,
            });
            model::Destination { url, iroh }
        } else {
            return;
        };
        let issue = model::Issue {
            invite_id: format!("{:x}", Sha256::digest(code.trim().as_bytes())),
            anchor: anchor.clone(),
            destination,
        };
        // This request is bound to the existing community URL and bearer now;
        // later account switches cannot redirect its credentials.
        let Ok(request) = self.request(reqwest::Method::POST, model::ISSUE_PATH) else {
            return;
        };
        let request = request.timeout(Duration::from_secs(3)).json(&issue);
        self.spawn_locator_issue(request, base, anchor);
    }
    pub(super) fn spawn_locator_issue(
        &self,
        request: RequestBuilder,
        base: String,
        anchor: handover::RoutePin,
    ) {
        if ISSUING
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 2).then_some(n + 1)
            })
            .is_err()
        {
            self.emit(Update::Notice(UNSAVED.into()));
            return;
        }
        let issuing = Issuing;
        let lease = shutdown::Lease::acquire(self.background_active.clone());
        let work = self.work.clone();
        let cancelled = self.live.shutdown.clone();
        let updates = self.updates.clone();
        let wake = self.wake.clone();
        let ctx = self.ctx.clone();
        let more = self.host_locator_more.clone();
        if std::thread::Builder::new()
            .name("relay-host-locator-issue".into())
            .spawn(move || {
                let _issuing = issuing;
                let _lease = lease;
                let result = (|| -> Result<(), String> {
                    let record: model::Record = parse(request.send().map_err(|_| UNSAVED)?)?;
                    if record.anchor != anchor || record.current != anchor {
                        return Err(UNSAVED.into());
                    }
                    let mut queue = crate::host_locator_outbox::Outbox::for_service(&base)?;
                    queue.enqueue(record, clock()?)?; // Durable BEFORE any publication.
                    send_due(&base, &work, &cancelled, None).map(|pending| {
                        if pending {
                            more.store(true, Ordering::Release);
                        }
                    })
                })();
                if let Err(error) = result {
                    // Library/network diagnostics may contain URLs. Only fixed
                    // public storage/retry explanations are sent to the UI.
                    let notice = if error == QUEUED { QUEUED } else { UNSAVED };
                    let _ = updates.send(Update::Notice(notice.into()));
                }
                wake.notify();
                ctx.request_repaint();
            })
            .is_err()
        {
            self.emit(Update::Notice(UNSAVED.into()));
        }
    }
    pub(super) fn poll_host_locator(&mut self) {
        if self.host_locator_more.swap(false, Ordering::AcqRel) {
            self.host_locator_retry_at = Instant::now();
        }
        if Instant::now() < self.host_locator_retry_at {
            return;
        }
        self.host_locator_retry_at = Instant::now() + POLL;
        if PUBLISHING.load(Ordering::Acquire) {
            return;
        }
        let Some(base) = self.host_locator_service.clone() else {
            return;
        };
        let renewal = self
            .handover_runtime
            .as_ref()
            .filter(|runtime| runtime.is_active())
            .map(|runtime| runtime.locator_task(&base))
            .transpose();
        let work = self.work.clone();
        let cancelled = self.live.shutdown.clone();
        let more = self.host_locator_more.clone();
        let problem = self.host_locator_problem.clone();
        let updates = self.updates.clone();
        let lease = shutdown::Lease::acquire(self.background_active.clone());
        let wake = self.wake.clone();
        let _=std::thread::Builder::new().name("relay-host-locator-retry".into()).spawn(move || {
            let result=match renewal {
                Ok(renewal)=>send_due(&base,&work,&cancelled,renewal.as_ref()),
                Err(_)=>Err("This PC's host is running, but Relay could not refresh its shared invitation links. Its data and keys were kept.".into()),
            };
            match result {
                Ok(pending)=>{problem.store(false,Ordering::Release);if pending {more.store(true,Ordering::Release);}}
                Err(error)=>if !problem.swap(true,Ordering::AcqRel) {
                    let notice=if error==QUEUED {QUEUED}else{"Relay could not refresh this PC's saved recovery links. Its data and keys were kept."};
                    let _=updates.send(Update::Notice(notice.into()));
                },
            }
            drop(lease);wake.notify();
        });
    }
}
