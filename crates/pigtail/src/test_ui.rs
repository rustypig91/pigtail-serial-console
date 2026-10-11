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
        input: egui::RawInput,
        draw: impl FnMut(&mut egui::Ui),
    ) -> egui::FullOutput {
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
