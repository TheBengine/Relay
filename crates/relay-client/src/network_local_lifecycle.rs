//! App-owned local hosting. Remote servers and other installations are untouched.
use super::*;
use crate::local_host::{AppHost, Identity};

pub(super) struct OwnedLocal {
    data: PathBuf,
    executable: PathBuf,
    identity: Identity,
}

impl Worker {
    fn local_restored(&self, data: &std::path::Path) {
        if let Some(plan) = AppHost::load(data) {
            self.emit(Update::LocalRestored {
                iroh_only: plan.mode == "iroh",
                address: plan.address.map(|address| format!("http://{address}:4580")),
            });
        }
    }
    pub(super) fn remember_local_host(
        &mut self,
        server: PathBuf,
        data: PathBuf,
    ) -> Result<(), String> {
        let identity = self
            .local_identity()
            .ok_or("Your local server is not ready yet.")?;
        let managed = crate::local_host::read_record(&data.join("managed-instance.json"))
            .map_err(|_| "Cannot confirm your local server's saved identity.")?;
        let mut plan = AppHost {
            server_id: identity.server_id.clone(),
            mode: "local".into(),
            address: None,
            // Saving where the server runs now must not undo the member's choice.
            lan_discovery: crate::local_host::lan_choice(&data),
        };
        crate::local_host::validate_app_host(&plan, &managed, &data, &server)?;
        if managed["instance_id"].as_str() != Some(identity.instance_id.as_str())
            || managed["pid"].as_u64() != Some(identity.pid.into())
        {
            return Err("Your local server changed while it was starting.".into());
        }
        if data.join("stack-instance.json").exists() {
            let stack = crate::local_host::read_record(&data.join("stack-instance.json"))
                .map_err(|_| "Cannot read your local server's sharing choice.")?;
            crate::local_host::validate_sharing_switch(
                &identity.server_id,
                &identity,
                &managed,
                &stack,
            )?;
            match stack["hosting_mode"].as_str() {
                Some("embedded") => {
                    let address = stack["embedded_ipv4"]
                        .as_str()
                        .ok_or("Missing private sharing address.")?;
                    if address == crate::iroh_route::ONLY_ADDRESS {
                        plan.mode = "iroh".into();
                    } else {
                        plan.mode = "embedded".into();
                        plan.address = Some(address.into());
                    }
                }
                Some("tailscale") => {
                    plan.mode = "tailscale".into();
                    plan.address = stack["tailscale_ipv4"].as_str().map(str::to_owned);
                }
                _ => {}
            }
        }
        // Track ownership even when saving the next-start choice fails.
        self.owned_local = Some(OwnedLocal {
            data: data.clone(),
            executable: server,
            identity,
        });
        plan.save(&data)
    }

    pub(super) fn restore_app_host(
        &mut self,
        server: PathBuf,
        data: PathBuf,
        expected: String,
    ) -> Result<(), String> {
        if self.handover_runtime.is_some()
            || self
                .handover_job
                .as_ref()
                .is_some_and(|(j, _)| j.is_hosting())
        {
            return Err("The transferred community is already running or being prepared. No other local host was started.".into());
        }
        let managed = crate::local_host::read_record(&data.join("managed-instance.json"))
            .map_err(|_| "Cannot read your local server's saved identity.")?;
        let plan = match AppHost::load(&data) {
            Some(plan) => plan,
            None if data.join("app-host.json").exists() => return Err(
                "Your saved hosting choice could not be read. Start your server again from Relay."
                    .into(),
            ),
            None => {
                // Migrate older app hosts from their saved community identity.
                let mut plan = AppHost {
                    server_id: expected.clone(),
                    mode: "local".into(),
                    address: None,
                    lan_discovery: false,
                };
                if let Ok(stack) = crate::local_host::read_record(&data.join("stack-instance.json"))
                {
                    if stack["server_id"].as_str() == Some(expected.as_str()) {
                        match stack["hosting_mode"].as_str() {
                            Some("embedded")
                                if stack["embedded_ipv4"].as_str()
                                    == Some(crate::iroh_route::ONLY_ADDRESS) =>
                            {
                                plan.mode = "iroh".into()
                            }
                            Some("embedded") => {
                                plan.mode = "embedded".into();
                                plan.address = stack["embedded_ipv4"].as_str().map(str::to_owned);
                            }
                            Some("tailscale") => {
                                plan.mode = "tailscale".into();
                                plan.address = stack["tailscale_ipv4"].as_str().map(str::to_owned);
                            }
                            _ => {}
                        }
                    }
                }
                plan
            }
        };
        if plan.server_id != expected || !data.join("relay.sqlite3").is_file() {
            return Err("Your saved local server has changed. Its data was left untouched.".into());
        }
        crate::local_host::validate_app_host(&plan, &managed, &data, &server)?;
        if self
            .owned_local
            .as_ref()
            .is_some_and(|owned| owned.identity.server_id == expected)
            && self
                .local_identity()
                .is_some_and(|live| live.server_id == expected)
        {
            self.local_restored(&data);
            return Ok(());
        }
        self.restoring_local = true;
        if self.local_server_ready() {
            let result = self
                .remember_local_host(server, data.clone())
                .and_then(|_| match plan.mode.as_str() {
                    "iroh" => {
                        self.attach_embedded_host(&data, crate::iroh_route::ONLY_ADDRESS, true)
                    }
                    "embedded" => {
                        self.attach_embedded_host(&data, plan.address.as_deref().unwrap(), false)
                    }
                    _ => Ok(()),
                });
            self.restoring_local = false;
            if result.is_ok() {
                self.local_restored(&data);
            }
            return result;
        }
        let command = match plan.mode.as_str() {
            "iroh" => Command::HostIroh {
                server: server.clone(),
                data: data.clone(),
            },
            "embedded" => Command::HostEmbedded {
                server: server.clone(),
                data: data.clone(),
                address: plan.address.unwrap(),
            },
            "tailscale" => Command::HostTailscale {
                server: server.clone(),
                data: data.clone(),
                address: plan.address.unwrap(),
            },
            _ => Command::HostLocal {
                server: server.clone(),
                data: data.clone(),
            },
        };
        let result = self.handle(command);
        self.restoring_local = false;
        if result.is_ok() {
            self.local_restored(&data);
        }
        result
    }

    pub(super) fn ensure_app_host(
        &mut self,
        url: &str,
        expected: Option<&str>,
    ) -> Result<(), String> {
        if !crate::local_host::is_local_url(url) || self.local_server_ready() {
            return Ok(());
        }
        // A remembered login can arrive while the received host is still in
        // standby. It must not silently launch this installation's old default
        // database on the same port through the internal restore path.
        if self
            .handover_job
            .as_ref()
            .is_some_and(|(j, _)| j.is_hosting())
        {
            return Err("This PC's transferred community is still starting. Try connecting again when it is ready.".into());
        }
        if self.handover_runtime.is_some() {
            return Err(
                "The transferred community is not responding. Restart that host from Relay.".into(),
            );
        }
        match crate::handover_runtime::remembered() {
            Ok(Some(_)) => {
                self.restore_handover_host()?;
                return Err("This PC's transferred community is starting. Try connecting again when it is ready.".into());
            }
            Err(fault) => {
                return Err(format!(
                    "The saved transferred host could not be opened. {}",
                    fault.message()
                ))
            }
            Ok(None) => {}
        }
        let data = crate::local_host::data_directory()?;
        let Some(expected) = expected
            .map(str::to_owned)
            .or_else(|| AppHost::load(&data).map(|p| p.server_id))
        else {
            return Ok(());
        };
        let server = crate::local_host::server_binary()
            .ok_or("The bundled local server is missing. Reinstall Relay to restore it.")?;
        self.restore_app_host(server, data, expected)
    }

    /// A fresh health identity plus the private runtime token authorizes stopping
    /// just this instance. No stored PID is used for killing a process.
    pub(super) fn close_app_host(&mut self) -> Result<(), String> {
        let Some(owned) = self.owned_local.take() else {
            return Ok(());
        };
        let Some(live) = self.local_identity() else {
            return Ok(());
        };
        if live.server_id != owned.identity.server_id
            || live.instance_id != owned.identity.instance_id
            || live.pid != owned.identity.pid
        {
            return Err("The local server changed; Relay left the new instance running.".into());
        }
        let read = |name: &str| {
            crate::local_host::read_record(&owned.data.join(name))
                .map_err(|_| "Cannot confirm the local server for normal shutdown.")
        };
        let runtime = read("server-runtime.json")?;
        let managed = read("managed-instance.json")?;
        let plan = AppHost {
            server_id: live.server_id.clone(),
            mode: "local".into(),
            address: None,
            lan_discovery: false,
        };
        crate::local_host::validate_app_host(&plan, &managed, &owned.data, &owned.executable)?;
        for record in [&runtime, &managed] {
            if record["server_id"].as_str() != Some(live.server_id.as_str())
                || record["instance_id"].as_str() != Some(live.instance_id.as_str())
                || record["pid"].as_u64() != Some(live.pid.into())
            {
                return Err("The local server's shutdown identity does not match.".into());
            }
        }
        let response = self.http.post("http://127.0.0.1:4580/api/local/shutdown")
            .bearer_auth(runtime["shutdown_token"].as_str().ok_or("Missing local shutdown proof.")?)
            .json(&serde_json::json!({"instance_id":live.instance_id,"confirm_server_id":live.server_id}))
            .timeout(Duration::from_secs(2)).send().map_err(network_error)?;
        checked(response)?;
        let until = Instant::now() + Duration::from_secs(4);
        while Instant::now() < until {
            if !owned.data.join("server-runtime.json").exists()
                && !owned.data.join("stack-instance.json").exists()
            {
                self.local_server = None;
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(80));
        }
        Err("Your local server is still closing. Wait a moment before installing an update.".into())
    }
}
