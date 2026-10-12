//! GrapHydra Studio: a desktop workbench for `.graph` databases.
// No console window behind the GUI in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod demo;
mod graph_view;
mod settings;
mod theme;

use eframe::egui;

fn main() -> eframe::Result {
    let initial = std::env::args_os().nth(1).map(std::path::PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("GrapHydra Studio")
            .with_inner_size([1400.0, 860.0])
            .with_min_inner_size([900.0, 560.0])
            .with_maximized(true)
            .with_icon(icon()),
        ..Default::default()
    };
    eframe::run_native("GrapHydra Studio", options, Box::new(move |cc| Ok(Box::new(app::StudioApp::new(cc, initial)))))
}

/// A small three-node graph glyph, drawn procedurally so the binary needs no assets.
fn icon() -> egui::IconData {
    const S: usize = 64;
    let mut rgba = vec![0u8; S * S * 4];
    let nodes = [(18.0f32, 20.0f32, [42, 120, 214]), (46.0, 22.0, [235, 104, 52]), (32.0, 46.0, [27, 175, 122])];
    let seg_dist = |px: f32, py: f32, (ax, ay): (f32, f32), (bx, by): (f32, f32)| {
        let (dx, dy) = (bx - ax, by - ay);
        let t = (((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
        ((px - ax - t * dx).powi(2) + (py - ay - t * dy).powi(2)).sqrt()
    };
    for y in 0..S {
        for x in 0..S {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let i = (y * S + x) * 4;
            // Rounded dark tile.
            let (cx, cy) = ((px - 32.0).abs() - 24.0, (py - 32.0).abs() - 24.0);
            let corner = (cx.max(0.0).powi(2) + cy.max(0.0).powi(2)).sqrt();
            if corner > 7.0 {
                continue;
            }
            let mut c = [26u8, 26, 25];
            for (a, b) in [(0, 1), (1, 2), (2, 0)] {
                if seg_dist(px, py, (nodes[a].0, nodes[a].1), (nodes[b].0, nodes[b].1)) < 1.6 {
                    c = [180, 179, 171];
                }
            }
            for (nx, ny, col) in nodes {
                if ((px - nx).powi(2) + (py - ny).powi(2)).sqrt() < 7.5 {
                    c = col;
                }
            }
            rgba[i..i + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
    egui::IconData { rgba, width: S as u32, height: S as u32 }
}
