use eframe::egui;

pub fn parse_analog_tone(text: &str) -> Option<String> {
    let text_lower = text.to_lowercase();
    if text_lower.contains("received tone:") {
        if let Some(tone_part) = text.split("Received tone:").nth(1) {
            let tone = tone_part.trim();
            if tone != "none" && !tone.is_empty() {
                return Some(tone.to_string());
            }
        }
    }
    None
}

pub fn split_tone(tone: &str) -> (String, String) {
    if tone.starts_with("CTCSS") {
        let v = tone.trim_start_matches("CTCSS").trim().to_string();
        (v, "---".to_string())
    } else if tone.starts_with("DCS") {
        let v = tone.trim_start_matches("DCS").trim().to_string();
        ("---".to_string(), v)
    } else {
        ("---".to_string(), "---".to_string())
    }
}

pub fn draw_analog_fields(
    ui: &mut egui::Ui,
    tone: &str,
    frequency: &str,
    has_signal: bool,
    level: f32,
    hold: bool,
    modulation: &str,
) {
    let active = has_signal || hold;

    let (ctcss_val, dcs_val) = if active {
        split_tone(tone)
    } else {
        ("---".to_string(), "---".to_string())
    };

    let dash_color = egui::Color32::from_gray(200);
    let value_color = egui::Color32::WHITE;

    egui::Grid::new("analog_grid").num_columns(2).spacing([40.0, 10.0]).show(ui, |ui| {
        ui.label(egui::RichText::new("Режим:").size(18.0));

        let mod_upper = modulation.trim().to_uppercase();
        let is_nfm = mod_upper.is_empty() || mod_upper == "NFM";
        let (mode_text, mode_color) = if !active {
            ("---".to_string(), dash_color)
        } else if is_nfm {
            let text = if mod_upper.is_empty() {
                "FM (NFM)".to_string()
            } else {
                "FM (NFM)".to_string()
            };
            let color = if has_signal {
                egui::Color32::LIGHT_GREEN
            } else if hold {
                egui::Color32::from_gray(180)
            } else {
                dash_color
            };
            (text, color)
        } else {
            (mod_upper.clone(), egui::Color32::RED)
        };

        ui.label(egui::RichText::new(mode_text).size(18.0).strong().color(mode_color));
        ui.end_row();

        ui.label(egui::RichText::new("CTCSS:").size(18.0));
        let ctcss_text = if active && ctcss_val != "---" {
            ctcss_val.replace("Hz", "Гц")
        } else {
            "---".to_string()
        };
        let ctcss_color = if has_signal && ctcss_val != "---" {
            egui::Color32::LIGHT_BLUE
        } else if hold && ctcss_val != "---" {
            egui::Color32::from_gray(180)
        } else {
            dash_color
        };
        ui.label(egui::RichText::new(ctcss_text).size(18.0).strong().color(ctcss_color));
        ui.end_row();

        ui.label(egui::RichText::new("DCS:").size(18.0));
        let dcs_text = if active && dcs_val != "---" {
            dcs_val.clone()
        } else {
            "---".to_string()
        };
        let dcs_color = if has_signal && dcs_val != "---" {
            egui::Color32::LIGHT_BLUE
        } else if hold && dcs_val != "---" {
            egui::Color32::from_gray(180)
        } else {
            dash_color
        };
        ui.label(egui::RichText::new(dcs_text).size(18.0).strong().color(dcs_color));
        ui.end_row();

        ui.label(egui::RichText::new("Уровень:").size(18.0));
        let dbfs_text = if active && level > 0.0001 {
            let db = 20.0 * level.log10();
            format!("{:.1} dBFS", db)
        } else {
            "---".to_string()
        };
        let dbfs_color = if has_signal && level > 0.0001 {
            egui::Color32::LIGHT_GREEN
        } else if hold && level > 0.0001 {
            egui::Color32::from_gray(180)
        } else {
            dash_color
        };
        ui.label(egui::RichText::new(dbfs_text).size(18.0).strong().color(dbfs_color));
        ui.end_row();

        ui.label(egui::RichText::new("Частота:").size(18.0));
        let freq_text = if active && !frequency.is_empty() && frequency != "---" {
            frequency.to_string()
        } else {
            "---".to_string()
        };
        let freq_color = if active && !frequency.is_empty() && frequency != "---" {
            value_color
        } else if hold {
            egui::Color32::from_gray(180)
        } else {
            dash_color
        };
        ui.label(egui::RichText::new(freq_text).size(18.0).strong().color(freq_color));
        ui.end_row();
    });
}

pub fn recording_suffix(mode: &str, tone: &str) -> String {
    if mode == "analog" {
        let mut suffix = "_ANALOG".to_string();
        if tone != "---" && !tone.is_empty() {
            let clean_tone = tone.replace(" ", "_").replace("/", "_");
            suffix.push('_');
            suffix.push_str(&clean_tone);
        }
        suffix
    } else {
        "_DIGI".to_string()
    }
}