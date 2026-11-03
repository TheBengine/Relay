use super::*;
use crate::{history_transfer::Code, network::CommandOrigin};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use zeroize::Zeroizing;
#[derive(Default)]
pub(in crate::app) struct TransferUi {
    origin: Option<CommandOrigin>,
    generated: Option<Zeroizing<String>>,
    entered: Zeroizing<String>,
    pending: Option<u64>,
    sending: bool,
    cancel: Arc<AtomicBool>,
    result: Option<Result<String, String>>,
}
impl Drop for TransferUi {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}
impl TransferUi {
    fn for_origin(origin: Option<CommandOrigin>) -> Self {
        let mut ui = Self::default();
        ui.origin = origin;
        ui
    }
    pub(in crate::app) fn finish(&mut self, id: u64, result: Result<String, String>) {
        if self.pending != Some(id) {
            return;
        }
        self.pending = None;
        if result.is_ok() {
            self.generated = None;
            self.entered = Zeroizing::new(String::new());
        }
        self.result = Some(result);
    }
}
impl RelayApp {
    pub(super) fn history_transfer_page(&mut self, ui: &mut egui::Ui) {
        let origin = self.command_origin();
        if self.encryption_ui.transfer.origin != origin {
            self.encryption_ui.transfer = TransferUi::for_origin(origin.clone());
        }
        controls::section_heading(ui,"Move history to another PC","Approve on a PC that can already read your messages. Both PCs must be signed in to the same account and community.");
        controls::description(ui,"A temporary encrypted copy travels through the server. The transfer code stays on your PCs. The server stops serving it after 15 minutes, or on restart. This is not an online backup.");
        let Some(origin) = origin else {
            controls::description(
                ui,
                "Sign in and open Friends or a community to transfer its history.",
            );
            return;
        };
        if self.bootstrap.as_ref().is_none_or(|b| {
            !b.capabilities
                .iter()
                .any(|c| c == crate::history_transfer::CAPABILITY)
        }) {
            controls::description(
                ui,
                "Update this server before transferring history between PCs.",
            );
            return;
        }
        let available = self.encryption_ui.transfer.pending.is_none();
        let mut send = false;
        let mut receive = false;
        let mut clear = false;
        let transfer = &mut self.encryption_ui.transfer;
        ui.add_enabled_ui(available,|ui| {
            if transfer.generated.is_none() && ui.add(secondary("This is my new PC")).clicked() {
                match Code::generate() {Ok(code)=>{transfer.generated=Some(code.display());transfer.result=None;},Err(error)=>transfer.result=Some(Err(error))}
            }
            if let Some(code)=&transfer.generated {
                ui.label("On the original PC, open Encryption → Backup and recovery and enter this code:");
                ui.add(egui::Label::new(RichText::new(code.as_str()).monospace()).wrap(true));
                ui.horizontal_wrapped(|ui| {
                    if ui.add(secondary("Copy transfer code")).clicked(){ui.output_mut(|out|out.copied_text=code.to_string());}
                    receive=ui.add(primary("Receive history")).clicked();
                });
                controls::description(ui,"Choose Receive history after approving the code on your original PC. Keep this window open until the transfer finishes.");
            } else {
                ui.add_space(8.0);
                ui.label("On the original PC: paste the code from your new PC");
                ui.add(egui::TextEdit::singleline(&mut *transfer.entered).password(true).char_limit(128).desired_width(ui.available_width()).hint_text("Transfer code from your new PC"));
                send=ui.add_enabled(Code::parse(&transfer.entered).is_ok(),primary("Approve and send history")).clicked();
            }
        });
        if transfer.pending.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(if transfer.sending {
                    "Encrypting and sending history…"
                } else {
                    "Receiving and checking history…"
                });
            });
        }
        if transfer.generated.is_some() || transfer.pending.is_some() {
            clear = ui.add(secondary("Cancel transfer")).clicked();
        }
        if let Some(result) = &transfer.result {
            let (text, color) = match result {
                Ok(text) => (text, c_good()),
                Err(text) => (text, c_bad()),
            };
            ui.add(egui::Label::new(RichText::new(text).color(color)).wrap(true));
        }
        if clear {
            self.encryption_ui.transfer = TransferUi::for_origin(Some(origin));
            return;
        }
        if send || receive {
            let id = self.next_request();
            let transfer = &mut self.encryption_ui.transfer;
            let code = if send {
                Zeroizing::new(transfer.entered.to_string())
            } else {
                Zeroizing::new(transfer.generated.as_ref().unwrap().to_string())
            };
            transfer.cancel = Arc::new(AtomicBool::new(false));
            transfer.pending = Some(id);
            transfer.sending = send;
            transfer.result = None;
            if let Err(error) = self.network.send(Command::HistoryTransfer {
                origin,
                request_id: id,
                code,
                send,
                cancel: transfer.cancel.clone(),
            }) {
                transfer.finish(id, Err(error));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_transfer_stale_completion_cannot_clear_new_code_and_drop_cancels() {
        let mut ui = TransferUi::default();
        ui.pending = Some(4);
        ui.generated = Some(Code::generate().unwrap().display());
        ui.finish(3, Ok("stale".into()));
        assert_eq!(ui.pending, Some(4));
        assert!(ui.generated.is_some());
        ui.finish(4, Err("retry".into()));
        assert!(ui.generated.is_some());
        ui.pending = Some(5);
        ui.finish(5, Ok("received".into()));
        assert!(ui.generated.is_none());
        let cancel = ui.cancel.clone();
        drop(ui);
        assert!(cancel.load(Ordering::Acquire));
    }
}
