//! GigaMux native desktop shell (egui/eframe — no webview).

use eframe::egui;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 640.0])
            .with_title("GigaMux"),
        ..Default::default()
    };
    eframe::run_native(
        "GigaMux",
        options,
        Box::new(|_cc| Ok(Box::new(GigaMuxApp::default()))),
    )
}

#[derive(Default)]
struct GigaMuxApp {}

impl eframe::App for GigaMuxApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("GigaMux");
            ui.label(format!("core {}", gigamux_core::version()));
            ui.separator();
            ui.label("Native egui shell — encode UI TBD.");
        });
    }
}
