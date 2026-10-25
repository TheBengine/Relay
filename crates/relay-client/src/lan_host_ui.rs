//! The "Find on my local network" switch for a server this PC hosts, in
//! Server Settings next to Hosting status.
//!
//! The switch only saves the host's choice. It never restarts a running
//! server: the choice is applied by the next start, and the page says so. What
//! is shown as "on" is what the running instance was actually started with.
use super::*;
use crate::appearance::{TEXT_BODY, TEXT_CAPTION};
use crate::local_host::LanActual;

#[derive(Default)]
pub(super) struct State {
    view: Option<View>,
    error: Option<String>,
}

/// What was read for one running instance. A different instance is read again.
#[derive(Clone, PartialEq, Eq)]
struct View {
    instance_id: String,
    choice: bool,
    actual: crate::local_host::LanActual,
}

impl State {
    pub(super) fn reset(&mut self) {
        self.view = None;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Standing {
    Off,
    On,
    /// Saved on; the running server was started without it.
    StartsNextTime,
    /// Saved off; the running server was started with it.
    StopsNextTime,
    /// Saved on, but the running Tailscale connection cannot use pairing-only mode.
    NotWhileShared,
    /// Nothing proves how the running server was started.
    Unknown,
}

fn standing(choice: bool, actual: LanActual, shared_privately: bool) -> Standing {
    match (choice, actual) {
        // Without a record of the running instance, "off" is never claimed.
        (_, LanActual::Unknown) => Standing::Unknown,
        (false, LanActual::Off) => Standing::Off,
        (true, LanActual::On) => Standing::On,
        (false, LanActual::On) => Standing::StopsNextTime,
        (true, LanActual::Off) if shared_privately => Standing::NotWhileShared,
        (true, LanActual::Off) => Standing::StartsNextTime,
    }
}

fn headline(standing: Standing) -> &'static str {
    match standing {
        Standing::Off => "Off. This community is not announced on your local network.",
        Standing::On => "On. Devices on your private local network can find this community.",
        Standing::StartsNextTime => "Saved. It starts working the next time this server starts.",
        Standing::StopsNextTime => "Saved as off. The running server keeps announcing itself until it is stopped and started again.",
        Standing::NotWhileShared => "Saved. This Tailscale host uses its existing invitation instead of local code discovery. Each participant needs their own Tailscale account and access. Local code discovery works with Relay's built-in private connection.",
        Standing::Unknown => "Relay can't tell whether the running server is announcing itself on your network: it was not started by this Relay, or its record is missing. Your saved choice applies the next time it is started from Relay. To be sure now, stop it and start it again here.",
    }
}

const WHAT_IT_DOES: &str = "Devices on your private local network can find the community name and ask for a short-lived code from the host. Only the sealed code exchange is exposed on the local network; login, administration and calls keep their existing connection. The code carries a secure invitation, and sign-in still uses HTTPS or Relay's encrypted private connection. Discovery never adds a person or replaces a saved server identity. Windows may ask whether Relay may use the network; Relay changes no firewall or router setting.";

const WHEN_IT_APPLIES: &str = "Relay never restarts your server for this switch. To apply a change, use Stop this server above and then start the server from Relay again. If the community has no owner account yet, it starts on this PC only until one exists.";

impl RelayApp {
    pub(super) fn lan_host_section(&mut self, ui: &mut egui::Ui) {
        if !crate::local_host::is_local_url(&self.login.url) {
            return;
        }
        let Ok(data) = crate::local_host::data_directory() else {
            return;
        };
        // Only the running instance's own identity says what it was started with.
        let live = self.admin_data.get("metadata").and_then(|metadata| {
            Some((
                metadata.get("server_id")?.as_str()?.to_owned(),
                metadata.get("instance_id")?.as_str()?.to_owned(),
            ))
        });
        ui.add_space(6.0);
        ui.label(RichText::new("Find on my local network").strong());
        let Some((server_id, instance_id)) = live else {
            ui.label(
                RichText::new("Checking the running server…")
                    .size(font_size(TEXT_BODY))
                    .color(c_muted()),
            );
            return;
        };
        if self
            .lan_host
            .view
            .as_ref()
            .is_none_or(|view| view.instance_id != instance_id)
        {
            self.lan_host.view = Some(View {
                choice: crate::local_host::lan_choice(&data),
                actual: crate::local_host::lan_actual(&data, &server_id, &instance_id),
                instance_id,
            });
        }
        let Some(view) = self.lan_host.view.clone() else {
            return;
        };
        let shared_privately = matches!(self.own_server_sharing().flatten(), Some(host_setup_ui::Sharing::Tailscale | host_setup_ui::Sharing::Both));
        let mut choice = view.choice;
        if ui
            .checkbox(
                &mut choice,
                "Let people on my local network find this community",
            )
            .changed()
        {
            match crate::local_host::set_lan_choice(&data, choice) {
                Ok(()) => {
                    self.lan_host.error = None;
                    if let Some(shown) = self.lan_host.view.as_mut() {
                        shown.choice = choice;
                    }
                }
                Err(error) => self.lan_host.error = Some(error),
            }
        }
        let shown = self.lan_host.view.as_ref().unwrap_or(&view);
        let now = standing(shown.choice, shown.actual, shared_privately);
        ui.label(
            RichText::new(headline(now))
                .size(font_size(TEXT_BODY))
                .color(match now {
                    Standing::On => c_good(),
                    _ => c_muted(),
                }),
        );
        if let Some(error) = &self.lan_host.error {
            ui.label(
                RichText::new(error)
                    .size(font_size(TEXT_BODY))
                    .color(c_bad()),
            );
        }
        ui.label(
            RichText::new(WHEN_IT_APPLIES)
                .size(font_size(TEXT_CAPTION))
                .color(c_muted()),
        );
        egui::CollapsingHeader::new("What this changes")
            .id_source("lan-host-explanation")
            .default_open(false)
            .show(ui, |ui| {
                ui.label(
                    RichText::new(WHAT_IT_DOES)
                        .size(font_size(TEXT_CAPTION))
                        .color(c_muted()),
                );
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_shows_what_is_running_and_what_will_apply_next_time() {
        use LanActual::{Off, On, Unknown};
        assert_eq!(standing(false, Off, false), Standing::Off);
        assert_eq!(standing(true, On, false), Standing::On);
        // A saved change is never shown as if it had taken effect.
        assert_eq!(standing(true, Off, false), Standing::StartsNextTime);
        assert_eq!(standing(false, On, false), Standing::StopsNextTime);
        // Tailscale keeps its existing path; iroh can use the pairing-only listener.
        assert_eq!(standing(true, Off, true), Standing::NotWhileShared);
        assert_eq!(standing(false, Off, true), Standing::Off);
        // No evidence about the running server is never shown as private.
        for choice in [false, true] {
            for private in [false, true] {
                assert_eq!(standing(choice, Unknown, private), Standing::Unknown);
            }
        }
    }
}
