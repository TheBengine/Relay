//! Persisted appearance choices and shared colors for native egui controls.
//!
//! Apply on startup or a preference/system-theme change. Custom painted UI must
//! use the returned palette and `font_size` too; egui visuals cannot recolor
//! explicit `Color32` or `FontId` values. Fonts and display zoom are preserved.

use eframe::egui::{self, Color32, FontId, Rounding, Stroke, TextStyle, Vec2};
use serde::{Deserialize, Serialize};

/// Unscaled base sizes for the shared text scale. Scale them once: through
/// `AppearanceSettings::font_size` or the Body style the settings controls read.
/// Captions are for small uppercase labels and timestamps only.
pub const TEXT_CAPTION: f32 = 12.0;
pub const TEXT_BODY: f32 = 14.0;
pub const TEXT_HEADING: f32 = 18.0;
pub const TEXT_TITLE: f32 = 24.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    Onyx,
    Ash,
    Light,
    System,
    /// Unknown future theme names retain the rest of the saved preferences.
    #[default]
    #[serde(other)]
    Dark,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    pub theme: Theme,
    /// Text scaling only. Interface zoom remains a separate app preference.
    pub font_scale: f32,
    /// Logical-point gap between message groups; compact density is app-owned.
    pub message_spacing: f32,
    /// Global GIF animation preference, combined with app-owned reduced motion.
    pub gif_autoplay: bool,
    pub convert_emoji_shortcodes: bool,
    pub emoji_suggestions: bool,
    pub mention_suggestions: bool,
    pub show_send_button: bool,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            theme: Theme::Dark,
            font_scale: 1.0,
            message_spacing: 8.0,
            gif_autoplay: true,
            convert_emoji_shortcodes: true,
            emoji_suggestions: true,
            mention_suggestions: true,
            show_send_button: true,
        }
    }
}

impl AppearanceSettings {
    pub fn normalized(mut self) -> Self {
        self.font_scale = bounded(self.font_scale, 1.0, 0.85, 1.4);
        self.message_spacing = bounded(self.message_spacing, 8.0, 0.0, 24.0);
        self
    }

    /// Scale fixed base sizes rather than already-scaled style values.
    pub fn font_size(&self, base: f32) -> f32 {
        bounded(base, 14.0, 1.0, 64.0) * bounded(self.font_scale, 1.0, 0.85, 1.4)
    }

    pub fn palette(&self, system_theme: Option<eframe::Theme>) -> Palette {
        palette(match (self.theme, system_theme) {
            (Theme::System, Some(eframe::Theme::Light)) => Theme::Light,
            (Theme::System, _) => Theme::Dark,
            (theme, _) => theme,
        })
    }

    /// Updates native widgets without reloading font files or altering zoom.
    /// The caller also applies the returned palette to its custom painted UI.
    pub fn apply(&self, ctx: &egui::Context, system_theme: Option<eframe::Theme>) -> Palette {
        let colors = self.palette(system_theme);
        ctx.data_mut(|data| {
            data.insert_temp(egui::Id::new("relay-muted-text"), colors.muted);
        });
        let mut style = (*ctx.style()).clone();
        style.visuals = if colors.dark_mode {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        let visuals = &mut style.visuals;
        visuals.override_text_color = Some(colors.text);
        visuals.panel_fill = colors.bg;
        visuals.window_fill = colors.panel;
        visuals.extreme_bg_color = colors.rail;
        visuals.faint_bg_color = colors.hover_message;
        visuals.code_bg_color = colors.card;
        // Discord's link blue: 7:1 on the dark backgrounds, where the accent
        // itself is under 4.5:1 as text.
        visuals.hyperlink_color = if colors.dark_mode {
            Color32::from_rgb(0, 168, 252)
        } else {
            Color32::from_rgb(0, 98, 204)
        };
        visuals.error_fg_color = colors.bad;
        visuals.window_stroke = Stroke::new(1.0_f32, colors.line);
        visuals.window_rounding = Rounding::same(15.0);
        visuals.menu_rounding = Rounding::same(8.0);
        visuals.selection.bg_fill = colors.selection;
        visuals.selection.stroke = Stroke::new(1.0_f32, colors.text);
        visuals.text_cursor = Stroke::new(1.5_f32, colors.accent);

        let widgets = &mut visuals.widgets;
        widgets.noninteractive.bg_fill = colors.panel;
        widgets.noninteractive.weak_bg_fill = colors.panel;
        widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, colors.line);
        widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, colors.text);
        for widget in [
            &mut widgets.inactive,
            &mut widgets.hovered,
            &mut widgets.active,
            &mut widgets.open,
        ] {
            widget.bg_fill = colors.card;
            widget.weak_bg_fill = colors.card;
            widget.bg_stroke = Stroke::new(1.0_f32, colors.line);
            widget.fg_stroke = Stroke::new(1.0_f32, colors.text);
            widget.rounding = Rounding::same(7.0);
        }
        widgets.hovered.bg_fill = colors.hover;
        widgets.hovered.weak_bg_fill = colors.hover;
        for widget in [&mut widgets.active, &mut widgets.open] {
            widget.bg_fill = colors.selection;
            widget.weak_bg_fill = colors.selection;
            widget.bg_stroke = Stroke::new(1.0_f32, colors.accent);
        }
        style.spacing.item_spacing = Vec2::new(8.0, 4.0);
        style.spacing.button_padding = Vec2::new(12.0, 8.0);
        // Assign absolute bases every time, preventing repeated Apply from
        // accumulating scale. Keep any additional app-defined text styles.
        for (kind, base) in [
            (TextStyle::Heading, 22.0),
            (TextStyle::Body, TEXT_BODY),
            (TextStyle::Button, 12.0),
            (TextStyle::Small, TEXT_CAPTION),
            (TextStyle::Monospace, 13.0),
        ] {
            let font = if kind == TextStyle::Monospace {
                FontId::monospace(self.font_size(base))
            } else {
                FontId::proportional(self.font_size(base))
            };
            style.text_styles.insert(kind, font);
        }
        ctx.set_style(style);
        colors
    }
}

fn bounded(value: f32, fallback: f32, low: f32, high: f32) -> f32 {
    if value.is_finite() {
        value.clamp(low, high)
    } else {
        fallback
    }
}

/// Use the palette's opaque secondary text rather than egui's alpha-faded weak
/// text. Unconfigured contexts retain egui's ordinary weak-text fallback.
pub fn muted_text_color(ui: &egui::Ui) -> Color32 {
    ui.ctx()
        .data(|data| data.get_temp::<Color32>(egui::Id::new("relay-muted-text")))
        .unwrap_or_else(|| ui.visuals().weak_text_color())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub dark_mode: bool,
    pub bg: Color32,
    pub rail: Color32,
    pub panel: Color32,
    pub card: Color32,
    pub line: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub accent_text: Color32,
    pub bad: Color32,
    pub good: Color32,
    pub hover: Color32,
    pub selection: Color32,
    pub hover_message: Color32,
    pub mention_message: Color32,
}

/// Deterministic palette lookup; System falls back to Dark without OS evidence.
/// Use `AppearanceSettings::palette` for the framework's actual system theme.
pub fn palette(theme: Theme) -> Palette {
    match theme {
        Theme::Dark | Theme::System => Palette {
            dark_mode: true,
            bg: rgb(26, 26, 30),
            rail: rgb(17, 18, 20),
            panel: rgb(20, 21, 24),
            card: rgb(35, 36, 40),
            line: rgb(46, 48, 54),
            text: rgb(242, 243, 245),
            muted: rgb(174, 178, 187),
            accent: rgb(88, 101, 242),
            accent_text: rgb(255, 255, 255),
            bad: rgb(242, 63, 66),
            good: rgb(35, 165, 90),
            hover: rgb(44, 46, 51),
            selection: rgb(53, 55, 60),
            hover_message: rgb(31, 32, 36),
            mention_message: rgb(52, 43, 27),
        },
        Theme::Onyx => Palette {
            dark_mode: true,
            bg: rgb(9, 10, 12),
            rail: rgb(3, 4, 5),
            panel: rgb(6, 7, 9),
            card: rgb(20, 21, 24),
            line: rgb(36, 38, 43),
            text: rgb(242, 243, 245),
            muted: rgb(174, 178, 187),
            accent: rgb(88, 101, 242),
            accent_text: rgb(255, 255, 255),
            bad: rgb(242, 63, 66),
            good: rgb(35, 165, 90),
            hover: rgb(29, 31, 35),
            selection: rgb(41, 43, 48),
            hover_message: rgb(15, 16, 19),
            mention_message: rgb(38, 31, 20),
        },
        Theme::Ash => Palette {
            dark_mode: true,
            bg: rgb(49, 51, 56),
            rail: rgb(30, 31, 34),
            panel: rgb(43, 45, 49),
            card: rgb(56, 58, 64),
            line: rgb(74, 78, 86),
            text: rgb(242, 243, 245),
            muted: rgb(181, 186, 193),
            accent: rgb(88, 101, 242),
            accent_text: rgb(255, 255, 255),
            bad: rgb(242, 63, 66),
            good: rgb(35, 165, 90),
            hover: rgb(67, 70, 78),
            selection: rgb(67, 70, 78),
            hover_message: rgb(55, 57, 63),
            mention_message: rgb(67, 57, 39),
        },
        Theme::Light => Palette {
            dark_mode: false,
            bg: rgb(255, 255, 255),
            rail: rgb(227, 229, 233),
            panel: rgb(242, 243, 245),
            card: rgb(235, 237, 241),
            line: rgb(202, 207, 216),
            text: rgb(37, 39, 44),
            muted: rgb(84, 92, 106),
            accent: rgb(88, 101, 242),
            accent_text: rgb(255, 255, 255),
            bad: rgb(184, 35, 65),
            good: rgb(18, 124, 75),
            hover: rgb(221, 225, 232),
            selection: rgb(220, 212, 249),
            hover_message: rgb(245, 246, 249),
            mention_message: rgb(255, 246, 221),
        },
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_fields_and_future_theme_preserve_saved_choices() {
        let legacy: AppearanceSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy, AppearanceSettings::default());
        let future: AppearanceSettings = serde_json::from_str(
            r#"{"theme":"future_theme","font_scale":1.2,"gif_autoplay":false}"#,
        )
        .unwrap();
        assert_eq!(future.theme, Theme::Dark);
        assert_eq!(future.font_scale, 1.2);
        assert!(!future.gif_autoplay);
        assert_eq!(future.message_spacing, 8.0);
    }

    #[test]
    fn serialization_preserves_each_theme_and_preference() {
        for theme in [
            Theme::Dark,
            Theme::Onyx,
            Theme::Ash,
            Theme::Light,
            Theme::System,
        ] {
            let saved = AppearanceSettings {
                theme,
                font_scale: 1.15,
                message_spacing: 13.0,
                gif_autoplay: false,
                ..Default::default()
            };
            let loaded: AppearanceSettings =
                serde_json::from_str(&serde_json::to_string(&saved).unwrap()).unwrap();
            assert_eq!(loaded, saved);
        }
    }

    #[test]
    fn persisted_out_of_range_preferences_migrate_without_erasing_choices() {
        let migrated: AppearanceSettings = serde_json::from_str(
            r#"{"theme":"light","font_scale":2.0,"message_spacing":-4.0,"gif_autoplay":false,"old_density":"compact"}"#,
        )
        .unwrap();
        let migrated = migrated.normalized();
        assert_eq!(migrated.theme, Theme::Light);
        assert_eq!(migrated.font_scale, 1.4);
        assert_eq!(migrated.message_spacing, 0.0);
        assert!(!migrated.gif_autoplay);
        // Persisting the migrated settings must not bring back invalid values.
        let reloaded: AppearanceSettings =
            serde_json::from_str(&serde_json::to_string(&migrated).unwrap()).unwrap();
        assert_eq!(reloaded.normalized(), migrated);
    }

    #[test]
    fn chat_box_choices_are_independent_and_survive_serialization_and_normalization() {
        for bits in 0..16 {
            let settings = AppearanceSettings {
                convert_emoji_shortcodes: bits & 1 != 0,
                emoji_suggestions: bits & 2 != 0,
                show_send_button: bits & 4 != 0,
                mention_suggestions: bits & 8 != 0,
                ..Default::default()
            };
            let loaded: AppearanceSettings =
                serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
            assert_eq!(loaded.normalized(), settings);
        }
    }

    #[test]
    fn invalid_numbers_are_bounded_and_normalizing_is_idempotent() {
        for (scale, spacing, expected_scale, expected_spacing) in [
            (f32::NAN, f32::INFINITY, 1.0, 8.0),
            (f32::NEG_INFINITY, f32::NAN, 1.0, 8.0),
            (-9.0, -3.0, 0.85, 0.0),
            (f32::MAX, f32::MAX, 1.4, 24.0),
        ] {
            let settings = AppearanceSettings {
                font_scale: scale,
                message_spacing: spacing,
                ..Default::default()
            }
            .normalized();
            assert_eq!(settings.font_scale, expected_scale);
            assert_eq!(settings.message_spacing, expected_spacing);
            assert_eq!(settings.clone().normalized(), settings);
            assert!(settings.font_size(f32::NAN).is_finite());
        }
    }

    #[test]
    fn system_theme_uses_framework_evidence_with_dark_fallback() {
        let settings = AppearanceSettings {
            theme: Theme::System,
            ..Default::default()
        };
        assert_eq!(settings.palette(None), palette(Theme::Dark));
        assert_eq!(
            settings.palette(Some(eframe::Theme::Dark)),
            palette(Theme::Dark)
        );
        assert_eq!(
            settings.palette(Some(eframe::Theme::Light)),
            palette(Theme::Light)
        );
        let explicit = AppearanceSettings {
            theme: Theme::Onyx,
            ..settings
        };
        assert_eq!(
            explicit.palette(Some(eframe::Theme::Light)),
            palette(Theme::Onyx)
        );
    }

    #[test]
    fn applying_repeatedly_does_not_compound_fonts_or_change_zoom() {
        let ctx = egui::Context::default();
        ctx.set_zoom_factor(1.25);
        // egui activates a requested display zoom at the next frame boundary.
        ctx.begin_frame(egui::RawInput::default());
        let _ = ctx.end_frame();
        let settings = AppearanceSettings {
            theme: Theme::Light,
            font_scale: 1.3,
            ..Default::default()
        };
        settings.apply(&ctx, None);
        let once = ctx.style();
        settings.apply(&ctx, None);
        let twice = ctx.style();
        assert_eq!(once.text_styles, twice.text_styles);
        assert_eq!(
            twice.text_styles[&TextStyle::Body].size,
            settings.font_size(TEXT_BODY)
        );
        // Small labels follow the caption size, scaled once like the body.
        assert_eq!(
            twice.text_styles[&TextStyle::Small].size,
            settings.font_size(TEXT_CAPTION)
        );
        assert!(!twice.visuals.dark_mode);
        assert_eq!(ctx.zoom_factor(), 1.25);
        ctx.begin_frame(egui::RawInput::default());
        egui::CentralPanel::default().show(&ctx, |ui| {
            assert_eq!(muted_text_color(ui), palette(Theme::Light).muted);
        });
        let _ = ctx.end_frame();
        AppearanceSettings::default().apply(&ctx, None);
        assert_eq!(ctx.style().text_styles[&TextStyle::Body].size, 14.0);
        ctx.begin_frame(egui::RawInput::default());
        egui::CentralPanel::default().show(&ctx, |ui| {
            assert_eq!(muted_text_color(ui), palette(Theme::Dark).muted);
        });
        let _ = ctx.end_frame();
    }

    #[test]
    fn each_palette_keeps_text_and_primary_buttons_legible() {
        fn luminance(color: Color32) -> f64 {
            fn linear(value: u8) -> f64 {
                let s = value as f64 / 255.0;
                if s <= 0.04045 {
                    s / 12.92
                } else {
                    ((s + 0.055) / 1.055).powf(2.4)
                }
            }
            linear(color.r()) * 0.2126 + linear(color.g()) * 0.7152 + linear(color.b()) * 0.0722
        }
        fn contrast(a: Color32, b: Color32) -> f64 {
            let a = luminance(a);
            let b = luminance(b);
            (a.max(b) + 0.05) / (a.min(b) + 0.05)
        }
        for theme in [Theme::Dark, Theme::Onyx, Theme::Ash, Theme::Light] {
            let p = palette(theme);
            for (foreground, background) in [
                (p.text, p.bg),
                (p.text, p.card),
                (p.muted, p.panel),
                (p.accent_text, p.accent),
                (p.text, p.selection),
            ] {
                assert!(
                    contrast(foreground, background) >= 4.5,
                    "Insufficient contrast in {theme:?}"
                );
            }
        }
    }
}
