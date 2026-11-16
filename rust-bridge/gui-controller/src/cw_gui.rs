// cw_gui.rs — отдельное окно CW (виджет + состояние)

use eframe::egui;
use std::sync::{Arc, Mutex};

use crate::cw;

pub struct CwView {
    pub decoder: Arc<Mutex<Option<cw::CwDecoder>>>,
    pub text: Arc<Mutex<String>>,
    pub tone_hz: Arc<Mutex<f32>>,
    pub wpm: Arc<Mutex<f32>>,
    pub chars: Arc<Mutex<usize>>,
    pub amp: Arc<Mutex<f32>>,
    pub auto_tone: bool,
    pub manual_tone: f32,
    pub manual_wpm: f32,
    pub gain_auto: bool,
    pub gain_manual: f32,
    pub last_clear_freq: u64,
}

impl Default for CwView {
    fn default() -> Self {
        Self {
            decoder: Arc::new(Mutex::new(None)),
            text: Arc::new(Mutex::new(String::new())),
            tone_hz: Arc::new(Mutex::new(0.0)),
            wpm: Arc::new(Mutex::new(0.0)),
            chars: Arc::new(Mutex::new(0)),
            amp: Arc::new(Mutex::new(0.0)),
            auto_tone: true,
            manual_tone: 700.0,
            manual_wpm: 20.0,
            gain_auto: true,
            gain_manual: 1.5,
            last_clear_freq: 0,
        }
    }
}

impl CwView {
    pub fn reset(&mut self) {
        *self.decoder.lock().unwrap() = None;
        *self.text.lock().unwrap() = String::new();
        *self.tone_hz.lock().unwrap() = 0.0;
        *self.wpm.lock().unwrap() = 0.0;
        *self.chars.lock().unwrap() = 0;
        *self.amp.lock().unwrap() = 0.0;
    }

    pub fn clear_text(&mut self) {
        if let Some(dec) = self.decoder.lock().unwrap().as_mut() {
            dec.clear();
        }
        *self.text.lock().unwrap() = String::new();
        *self.chars.lock().unwrap() = 0;
    }

    pub fn draw_controls(&mut self, ui: &mut egui::Ui) {
        let tone_hz = *self.tone_hz.lock().unwrap();
        let wpm = *self.wpm.lock().unwrap();
        let chars = *self.chars.lock().unwrap();
        let amp = *self.amp.lock().unwrap();

        // Строка 1: Управление (слева направо)
        ui.horizontal(|ui| {
            let prev_auto = self.auto_tone;
            ui.radio_value(&mut self.auto_tone, true, "Авто");
            ui.radio_value(&mut self.auto_tone, false, "Ручной");
            if self.auto_tone != prev_auto {
                if let Some(dec) = self.decoder.lock().unwrap().as_mut() {
                    if self.auto_tone {
                        dec.set_auto_tone();
                    } else {
                        dec.set_manual_tone(self.manual_tone);
                    }
                }
            }

            // Поле тона ВСЕГДА на месте, но неактивно при "Авто"
            ui.add_space(5.0);
            let tone_resp = ui.add_enabled(
                !self.auto_tone,
                egui::DragValue::new(&mut self.manual_tone)
                    .clamp_range(300.0..=1500.0)
                    .speed(1.0)
                    .suffix(" Гц")
            );
            if tone_resp.changed() {
                if let Some(dec) = self.decoder.lock().unwrap().as_mut() {
                    dec.set_manual_tone(self.manual_tone);
                }
            }

            // WPM идёт сразу за полем тона
            ui.add_space(10.0);
            ui.label(egui::RichText::new("WPM:").size(14.0).color(egui::Color32::from_gray(180)));
            let wpm_resp = ui.add(egui::DragValue::new(&mut self.manual_wpm)
                .clamp_range(10.0..=40.0)
                .speed(0.5));
            if wpm_resp.changed() {
                if let Some(dec) = self.decoder.lock().unwrap().as_mut() {
                    dec.set_wpm(self.manual_wpm);
                }
            }

            // АРУ прижимаем вправо (над индикатором)
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Сначала поле усиления (оно будет справа), потом АРУ (слева от него)
                let gain_resp = ui.add_enabled(
                    !self.gain_auto,
                    egui::DragValue::new(&mut self.gain_manual)
                        .clamp_range(0.5..=10.0)
                        .speed(0.1)
                        .suffix("x")
                );
                if gain_resp.changed() {
                    if let Some(dec) = self.decoder.lock().unwrap().as_mut() {
                        dec.set_gain(self.gain_manual);
                    }
                }
                let mut gain_auto = self.gain_auto;
                if ui.checkbox(&mut gain_auto, "АРУ").changed() {
                    self.gain_auto = gain_auto;
                    if let Some(dec) = self.decoder.lock().unwrap().as_mut() {
                        dec.set_gain_auto(gain_auto);
                    }
                }
            });
        }); // <-- ЗАКРЫВАЕМ первую ui.horizontal

        // Строка 2: Факты + индикатор
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(format!("Тон: {:.0} Гц", tone_hz)).size(14.0).color(egui::Color32::LIGHT_BLUE));
            ui.separator();
            ui.label(egui::RichText::new(format!("WPM: {:.1}", wpm)).size(14.0).color(egui::Color32::LIGHT_BLUE));
            ui.separator();
            ui.label(egui::RichText::new(format!("Символов: {}", chars)).size(14.0).color(egui::Color32::LIGHT_BLUE));

            // Индикатор
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(100.0, 12.0), egui::Sense::hover());
                let painter = ui.painter();
                painter.rect_filled(rect, 2.0, egui::Color32::from_gray(40));
                let fill_w = (amp * 4.0).clamp(0.0, 1.0) * rect.width();
                if fill_w > 0.0 {
                    let fill_rect = egui::Rect::from_min_size(rect.min, egui::vec2(fill_w, rect.height()));
                    painter.rect_filled(fill_rect, 2.0, egui::Color32::from_rgb(80, 200, 80));
                }
            });
        });
    }

    pub fn draw_text(&mut self, ui: &mut egui::Ui, has_signal_now: bool) {
        let text = self.text.lock().unwrap().clone();

        egui::Frame::none()
            .fill(egui::Color32::from_gray(18))
            .inner_margin(egui::Margin::same(8.0))
            .rounding(4.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(200.0)
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        let mut job = cw::highlight_cw(&text);
                        job.wrap.max_width = ui.available_width();
                        job.wrap.break_anywhere = false;
                        job.halign = egui::Align::LEFT;
                        ui.label(job);
                    });
            });

        if has_signal_now {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
        }
    }
}