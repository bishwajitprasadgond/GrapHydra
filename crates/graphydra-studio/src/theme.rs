//! Light/dark palettes. Categorical colors are the validated,
//! colorblind-safe set also used by the benchmark charts; labels get slots
//! in a fixed order (alphabetical), so a label keeps its color across views.

use eframe::egui::{self, Color32, CornerRadius, Visuals};

#[derive(Clone)]
pub struct Palette {
    pub surface: Color32,
    pub ink: Color32,
    pub ink2: Color32,
    pub muted: Color32,
    pub border: Color32,
    pub edge: Color32,
    pub accent: Color32,
    pub danger: Color32,
    pub success: Color32,
    pub other: Color32,
    pub series: [Color32; 8],
}

const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

impl Palette {
    /// Color for a label slot. Labels past the eighth share neutral gray:
    /// reusing a hue would make two different labels look identical.
    pub fn label_color(&self, slot: Option<&usize>) -> Color32 {
        slot.and_then(|s| self.series.get(*s)).copied().unwrap_or(self.other)
    }
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            surface: hex(0x1a1a19),
            ink: hex(0xffffff),
            ink2: hex(0xc3c2b7),
            muted: hex(0x898781),
            border: hex(0x2c2c2a),
            edge: hex(0x5a5955),
            accent: hex(0x3987e5),
            danger: hex(0xe66767),
            success: hex(0x0ca30c),
            other: hex(0x898781),
            series: [
                hex(0x3987e5),
                hex(0xd95926),
                hex(0x199e70),
                hex(0xc98500),
                hex(0xd55181),
                hex(0x008300),
                hex(0x9085e9),
                hex(0xe66767),
            ],
        }
    } else {
        Palette {
            surface: hex(0xfcfcfb),
            ink: hex(0x0b0b0b),
            ink2: hex(0x52514e),
            muted: hex(0x898781),
            border: hex(0xe1e0d9),
            edge: hex(0xb4b3ab),
            accent: hex(0x2a78d6),
            danger: hex(0xc62f2f),
            success: hex(0x006300),
            other: hex(0x898781),
            series: [
                hex(0x2a78d6),
                hex(0xeb6834),
                hex(0x1baf7a),
                hex(0xeda100),
                hex(0xe87ba4),
                hex(0x008300),
                hex(0x4a3aa7),
                hex(0xe34948),
            ],
        }
    }
}

pub fn apply(ctx: &egui::Context, dark: bool) {
    let p = palette(dark);
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    v.panel_fill = if dark { hex(0x141413) } else { hex(0xf4f3f0) };
    v.window_fill = p.surface;
    v.extreme_bg_color = p.surface;
    v.faint_bg_color = if dark { hex(0x222220) } else { hex(0xefeeea) };
    v.selection.bg_fill = p.accent;
    // Text on the accent fill (selected tabs, rows) must stay readable in both themes.
    v.selection.stroke = egui::Stroke::new(1.0_f32, Color32::WHITE);
    v.hyperlink_color = p.accent;
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(6);
    // egui keeps separate light and dark styles and follows the OS; pin ours.
    let which = if dark { egui::Theme::Dark } else { egui::Theme::Light };
    ctx.set_theme(which);
    ctx.set_visuals_of(which, v);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 4.0);
    });
}
