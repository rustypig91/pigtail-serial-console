//! Installed monospace families, discovered once; only the selected face is loaded into egui.

use std::sync::{Arc, OnceLock};

pub(crate) struct SystemFonts {
    database: fontdb::Database,
    families: Vec<String>,
}

pub(crate) fn system_fonts() -> Arc<SystemFonts> {
    static FONTS: OnceLock<Arc<SystemFonts>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut database = fontdb::Database::new();
            database.load_system_fonts();
            Arc::new(SystemFonts::from_database(database))
        })
        .clone()
}

impl SystemFonts {
    fn from_database(database: fontdb::Database) -> Self {
        let mut families: Vec<_> = database
            .faces()
            .filter(|face| face.monospaced)
            .filter_map(|face| face.families.first().map(|(name, _)| name.clone()))
            .filter(|name| !name.eq_ignore_ascii_case("Hack"))
            .collect();
        families.sort_by_key(|name| name.to_lowercase());
        families.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        Self { database, families }
    }

    pub(crate) fn families(&self) -> &[String] {
        &self.families
    }

    pub(crate) fn definitions(&self, selected: &str) -> egui::FontDefinitions {
        let mut fonts = crate::app::app_font_definitions();
        if selected == "Hack" {
            return fonts;
        }
        let id = self
            .database
            .query(&fontdb::Query {
                families: &[fontdb::Family::Name(selected)],
                ..Default::default()
            })
            .filter(|id| self.database.face(*id).is_some_and(|face| face.monospaced));
        let data = id.and_then(|id| {
            self.database.with_face_data(id, |bytes, index| {
                let mut data = egui::FontData::from_owned(bytes.to_vec());
                data.index = index;
                data
            })
        });
        if let Some(data) = data {
            fonts
                .font_data
                .insert("system_console".into(), Arc::new(data));
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .insert(0, "system_console".into());
        } else {
            tracing::warn!(
                font = selected,
                "system console font unavailable; using Hack"
            );
        }
        fonts
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Real bundled font bytes with controlled family metadata, independent of host fonts.
    pub(crate) fn fixture() -> Arc<SystemFonts> {
        let mut database = fontdb::Database::new();
        let fonts = egui::FontDefinitions::default();
        database.load_font_data(fonts.font_data["Hack"].font.to_vec());
        let mut face = database.faces().next().unwrap().clone();
        database.remove_face(face.id);
        for (family, monospaced) in [("Test Mono", true), ("Test Proportional", false)] {
            face.families = vec![(family.into(), fontdb::Language::English_UnitedStates)];
            face.monospaced = monospaced;
            database.push_face_info(face.clone());
        }
        Arc::new(SystemFonts::from_database(database))
    }

    #[test]
    fn discovered_system_face_renders_with_egui() {
        use crate::test_ui::ContextTestExt as _;
        let catalog = system_fonts();
        // Minimal CI images may have no installed monospace faces.
        let Some(family) = catalog.families().first() else {
            return;
        };
        let definitions = catalog.definitions(family);
        assert!(definitions.font_data.contains_key("system_console"));
        let ctx = egui::Context::default();
        ctx.set_fonts(definitions);
        let output = ctx.run_test_ui(Default::default(), |ui| {
            let font = egui::FontId::monospace(16.0);
            let width = ui.fonts_mut(|fonts| fonts.glyph_width(&font, '0'));
            assert!(width > 0.0);
            ui.monospace("System font: 0123456789 iW → café");
        });
        assert!(!output.shapes.is_empty());
    }

    #[test]
    fn installed_fonts_load_the_correct_face_and_missing_fonts_fall_back() {
        let catalog = fixture();
        assert_eq!(catalog.families(), &["Test Mono"]);
        let selected = catalog.definitions("Test Mono");
        assert_eq!(
            selected.families[&egui::FontFamily::Monospace][0],
            "system_console"
        );
        assert_eq!(selected.font_data["system_console"].index, 0);
        assert_eq!(
            selected.font_data["system_console"].font,
            selected.font_data["Hack"].font
        );
        for name in ["Hack", "Missing Mono", "Test Proportional"] {
            let fonts = catalog.definitions(name);
            assert_eq!(fonts.families[&egui::FontFamily::Monospace][0], "Hack");
            assert_eq!(
                fonts.families[&egui::FontFamily::Proportional]
                    .last()
                    .unwrap(),
                "Hack"
            );
            assert!(!fonts.font_data.contains_key("system_console"));
        }
    }
}
