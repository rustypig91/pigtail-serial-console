//! Headless egui frames explicitly discard texture uploads; no GPU consumes them.

pub(crate) trait ContextTestExt {
    fn run_test_ui(
        &self,
        input: egui::RawInput,
        draw: impl FnMut(&mut egui::Ui),
    ) -> egui::FullOutput;
    fn end_test_pass(&self) -> egui::FullOutput;
}

impl ContextTestExt for egui::Context {
    fn run_test_ui(
        &self,
        mut input: egui::RawInput,
        draw: impl FnMut(&mut egui::Ui),
    ) -> egui::FullOutput {
        // Native integrations always provide finite window bounds. egui's
        // unbounded initial viewport otherwise turns ANSI's available space
        // into a 65,535 x 65,535 grid in tests using RawInput::default().
        input.screen_rect.get_or_insert_with(|| {
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1024.0, 768.0))
        });
        let mut output = self.run_ui(input, draw);
        output.textures_delta.clear();
        output
    }

    fn end_test_pass(&self) -> egui::FullOutput {
        let mut output = self.end_pass();
        output.textures_delta.clear();
        output
    }
}
