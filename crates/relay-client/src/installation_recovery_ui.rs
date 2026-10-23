//! A repair-only window that does not construct or open a Relay profile.

use eframe::egui;

pub(crate) fn run(args: &[String]) -> eframe::Result<()> {
    let icon = image::load_from_memory(include_bytes!("../assets/relay.png"))
        .expect("The bundled Relay icon must be a valid PNG")
        .into_rgba8();
    let (width, height) = icon.dimensions();
    let position = args
        .iter()
        .position(|argument| argument == "--position")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.split_once(','))
        .and_then(|(x, y)| Some(egui::pos2(x.parse().ok()?, y.parse().ok()?)))
        .filter(|position| position.x.is_finite() && position.y.is_finite());
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Relay")
        .with_decorations(true)
        .with_resizable(true)
        .with_icon(egui::IconData {
            rgba: icon.into_raw(),
            width,
            height,
        })
        .with_active(!args.iter().any(|argument| argument == "--background"))
        .with_inner_size([460.0, 180.0])
        .with_min_inner_size([420.0, 180.0]);
    if let Some(position) = position {
        viewport = viewport.with_position(position);
    }
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "Relay Installation Repair",
        options,
        Box::new(|_| Box::new(InstallationRecoveryApp)),
    )
}

struct InstallationRecoveryApp;

impl eframe::App for InstallationRecoveryApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default()
            .frame(egui::Frame::none().inner_margin(24.0))
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(
                        "Installation needs repair. Run the Relay installer again.",
                    )
                    .size(18.0),
                );
                ui.add_space(24.0);
                if ui
                    .add_sized([88.0, 32.0], egui::Button::new("Close"))
                    .clicked()
                {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
    }
}
