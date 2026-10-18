fn main() -> eframe::Result {
    let opts = eframe::NativeOptions::default();
    eframe::run_ui_native("probe", opts, |ui, _frame| {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.label(egui_phosphor::regular::DATABASE.to_string());
        });
    })
}
