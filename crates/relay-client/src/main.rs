#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod accessible_input;
mod audio_wake;
mod build_metadata;
#[cfg(windows)]
mod windows_audio;

#[cfg(windows)]
mod account_webview;
mod adaptive_audio;
#[cfg(feature = "native-rtc")]
mod adaptive_video;
mod app;
mod appearance;
#[cfg(feature = "native-rtc")]
mod application_audio;
#[cfg(feature = "native-rtc")]
mod audio_activity;
#[cfg(windows)]
mod audio_duck;
#[cfg(feature = "native-rtc")]
mod audio_echo;
mod audio_resample;
mod audio_settings;
mod call_alerts;
mod capture;
mod central_account;
mod chat_features;
mod chat_media;
mod chat_state;
mod community;
mod community_friends;
mod connection_errors;
mod connection_path;
mod connection_setup;
mod conversation_state;
mod credential_vault;
mod direct_voice;
mod e2ee;
mod embedded_network;
mod emoji;
mod firewall_check;
mod gif_library;
#[cfg(all(windows, feature = "native-rtc"))]
mod graphics_capture;
#[cfg(all(windows, feature = "native-rtc"))]
mod hardware_encoder;
mod installation_recovery_ui;
#[cfg(windows)]
mod instance_identity;
mod invitation;
mod invitation_clipboard;
mod invitation_launch;
mod invitation_probe;
mod invitation_routes;
mod invite_proof;
mod iroh_route;
mod local_host;
mod media;
mod message_format;
#[cfg(windows)]
mod native_invitation_wake;
mod native_render_probe;
mod network;
mod notifications;
mod online_gifs;
#[cfg(all(windows, feature = "native-rtc"))]
mod process_audio;
mod profile_paths;
mod protection;
mod recovery;
mod route_choice;
#[cfg(feature = "native-rtc")]
mod rtc;
mod safety_number;
mod search_filters;
mod settings_ui;
#[cfg(windows)]
mod single_instance;
mod sound_cues;
mod tailscale_connect;
mod timeline;
mod tls_crypto;
#[cfg(windows)]
mod tray;
mod unread_navigation;
mod update_ui;
#[cfg(windows)]
mod url_protocol;
mod voice_connection;
mod voice_jitter;
mod voice_limiter;
mod voice_output;
mod voice_preroll;
mod voice_recovery;
mod voice_tunnel;
mod voice_ui_state;

fn main() -> eframe::Result<()> {
    tls_crypto::initialize();
    let original_args: Vec<_> = std::env::args_os().collect();
    let args: Vec<String> = original_args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let installation = relay_protocol::installation::inspect_startup(&args);
    if args.iter().any(|arg| arg == "--installation-status") {
        println!("{}", installation.diagnostic());
        std::process::exit(if installation.is_ready() { 0 } else { 2 });
    }
    if args.iter().any(|arg| arg == "--build-info") {
        println!("{}", build_metadata::current().json());
        return Ok(());
    }
    if !installation.is_ready() {
        #[cfg(windows)]
        if matches!(url_protocol::operation(&args), Ok(Some(_))) {
            eprintln!("{}", installation.diagnostic());
            std::process::exit(2);
        }
        return installation_recovery_ui::run(&args);
    }
    if original_args.iter().any(|arg| arg.to_str().is_none()) {
        eprintln!("Relay could not read its startup request.");
        std::process::exit(2);
    }
    let intent = match invitation_launch::parse(&args) {
        Ok(intent) => intent,
        Err(error) => {
            invitation_launch::failed(error);
        }
    };
    let personal = match invitation_launch::personal(
        &args,
        invitation_launch::STORAGE_OVERRIDES
            .iter()
            .any(|name| std::env::var_os(name).is_some()),
    ) {
        Ok(personal) => personal,
        Err(error) => {
            eprintln!("Cannot open Relay profile: {error}");
            std::process::exit(2);
        }
    };
    let initial = match intent {
        invitation_launch::Intent::Ordinary => None,
        invitation_launch::Intent::Invitation(invite) => Some(invite),
    };
    if initial.is_some() && (!personal || !cfg!(windows)) {
        invitation_launch::failed("Open Relay and paste the invitation in Join friends.");
    }
    if let Err(error) = native_render_probe::path_from_args(&args) {
        eprintln!("Cannot enable native render probe: {error}");
        std::process::exit(2);
    }
    #[cfg(windows)]
    match url_protocol::operation(&args) {
        Ok(Some(operation)) => {
            if !url_protocol::installation_allowed(personal, &installation) {
                eprintln!("Invitation links are unchanged for isolated profiles.");
                std::process::exit(2);
            }
            let result = match operation {
                url_protocol::Operation::Register => url_protocol::register_current(),
                url_protocol::Operation::Unregister => url_protocol::unregister_current(),
            };
            std::process::exit(
                if matches!(
                    result,
                    url_protocol::Outcome::Ready
                        | url_protocol::Outcome::Removed
                        | url_protocol::Outcome::Preserved
                ) {
                    0
                } else {
                    2
                },
            );
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    }
    #[cfg(windows)]
    if let Some(invitation) = initial {
        if let Err(error) = single_instance::open(invitation) {
            invitation_launch::failed(error);
        }
        return Ok(());
    }
    let mut launch = invitation_launch::Launch::default();
    #[cfg(windows)]
    let _main_owner = if personal {
        match single_instance::start(None) {
            Ok(single_instance::Start::Owner(owner, inbox)) => {
                launch.inbox = Some(inbox);
                launch.register_protocol =
                    url_protocol::installation_allowed(personal, &installation);
                Some(owner)
            }
            Ok(single_instance::Start::Forwarded) => return Ok(()),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(2);
            }
        }
    } else {
        None
    };
    if let Err(error) = profile_paths::configure(&args) {
        eprintln!("Cannot open Relay profile: {error}");
        std::process::exit(2);
    }
    relay_updater::initialize_tls_provider();
    let icon = image::load_from_memory(include_bytes!("../assets/relay.png"))
        .expect("The bundled Relay icon must be a valid PNG")
        .into_rgba8();
    let (width, height) = icon.dimensions();
    let capture_only = args.iter().any(|argument| argument == "--capture");
    let size = args
        .iter()
        .position(|a| a == "--size")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some([w.parse::<f32>().ok()?, h.parse::<f32>().ok()?]))
        .filter(|s| s[0].is_finite() && s[1].is_finite() && s[0] >= 940.0 && s[1] >= 650.0)
        .unwrap_or([1320.0, 850.0]);
    let position = args
        .iter()
        .position(|arg| arg == "--position")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.split_once(','))
        .and_then(|(x, y)| Some(egui::pos2(x.parse().ok()?, y.parse().ok()?)))
        .filter(|position| position.x.is_finite() && position.y.is_finite());
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Relay")
        .with_decorations(false)
        .with_resizable(true)
        .with_icon(egui::IconData {
            rgba: icon.into_raw(),
            width,
            height,
        })
        .with_active(!capture_only && !args.iter().any(|arg| arg == "--background"))
        .with_visible(!capture_only)
        .with_inner_size(size)
        .with_min_inner_size([940.0, 650.0]);
    if let Some(position) = position {
        viewport = viewport.with_position(position);
    }
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "Relay",
        options,
        Box::new(move |cc| Box::new(app::RelayApp::new(cc, launch))),
    )
}

use eframe::egui;

mod thread_priority;
mod voice_qos;

mod device_queue;
