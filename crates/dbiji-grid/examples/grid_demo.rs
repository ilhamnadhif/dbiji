//! Interactive demo: 1,000,000 generated rows × 100 columns of mixed types,
//! editing enabled, all views and panels.
//!
//! ```text
//! cargo run --release -p dbiji-grid --example grid_demo [-- rows cols] [--paged]
//! ```
//!
//! Rows arrive like from a database: the first 1,000 immediately, the rest
//! streamed by a background thread into the shared row store while the grid
//! stays interactive. With `--paged` only pages are fetched: scrolling near
//! the end emits `FetchMore` and the toolbar offers "fetch all".

#[path = "support/datagen.rs"]
mod datagen;
#[path = "support/demo_app.rs"]
mod demo_app;

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let paged = args.iter().any(|a| a == "--paged");
    let nums: Vec<usize> = args.iter().filter_map(|a| a.parse().ok()).collect();
    let rows = nums.first().copied().unwrap_or(1_000_000);
    let cols = nums.get(1).copied().unwrap_or(100);
    let options = eframe::NativeOptions { viewport: egui::ViewportBuilder::default().with_inner_size([1400.0, 860.0]).with_title("DBiji grid demo"), ..Default::default() };
    eframe::run_native(
        "dbiji-grid-demo",
        options,
        Box::new(move |cc| {
            let mut fonts = egui::FontDefinitions::default();
            egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
            cc.egui_ctx.set_fonts(fonts);
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            let mut app = demo_app::DemoApp::new(rows, cols, 1_000);
            app.fetch_all_at_start = !paged;
            Ok(Box::new(Demo(app)))
        }),
    )
}

struct Demo(demo_app::DemoApp);

impl eframe::App for Demo {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.0.ui(ui);
    }
}
