// cw.rs — CW-декодер на cwdit-dsp (Goertzel) + cwdit-morse (Decoder) + подсветка

use cwdit_dsp::Goertzel;
use cwdit_morse::{Decoder, TimingEstimator};
use std::time::{Duration, Instant};

const WARMUP: Duration = Duration::from_secs(5);
const WORD_BREAK_FALLBACK: Duration = Duration::from_secs(3);
const FALLBACK_MIN_CHARS: usize = 20;
const MAX_CHARS: usize = 4000;
const KEEP_CHARS: usize = 2000;

pub struct CwResult {
    pub text: String,
    pub tone_hz: f32,
    pub wpm: f32,
    pub chars: usize,
    pub last_amp: f32,
    pub amp_smooth: f32,
    pub signal_on: bool,
    pub gain: f32,
}

pub struct CwDecoder {
    sample_rate: f32,
    target_freq: f32,
    auto_freq: bool,
    freq_detected: bool,
    freq_buffer: Vec<f32>,
    goertzel: Goertzel,
    block_len: u32,
    tone_on: bool,
    last_toggle_block: u64,
    block_idx: u64,
    threshold: f32,
    noise_floor: f32,
    morse: Decoder,
    text: String,
    chars_count: usize,
    last_amp: f32,
    amp_smooth: f32,
    signal_on: bool,
    signal_off_since: Option<Instant>,
    gain: f32,
    gain_auto: bool,
    gain_manual: f32,
    manual_wpm: f32,
    started_at: Option<Instant>,
    post_warmup_start: Option<Instant>,
    word_break_seen: bool,
    seed_wpm: f32,
}

impl CwDecoder {
    pub fn new(sample_rate: f32) -> Self {
        let block_len = 480u32;
        let goertzel = Goertzel::new(700.0, sample_rate, block_len);
        let seed_wpm = 20.0;
        let timing = TimingEstimator::from_wpm(seed_wpm, sample_rate);
        let morse = Decoder::new(timing).with_adapt(true);

        Self {
            sample_rate,
            target_freq: 700.0,
            auto_freq: true,
            freq_detected: false,
            freq_buffer: Vec::new(),
            goertzel,
            block_len,
            tone_on: false,
            last_toggle_block: 0,
            block_idx: 0,
            threshold: 0.1,
            noise_floor: 0.005,
            morse,
            text: String::new(),
            chars_count: 0,
            last_amp: 0.0,
            amp_smooth: 0.0,
            signal_on: false,
            signal_off_since: None,
            gain: 1.5,
            gain_auto: true,
            gain_manual: 1.5,
            manual_wpm: seed_wpm,
            started_at: None,
            post_warmup_start: None,
            word_break_seen: false,
            seed_wpm,
        }
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.chars_count = 0;
        self.amp_smooth = 0.0;
        self.signal_on = false;
        self.signal_off_since = None;
        self.started_at = None;
        self.post_warmup_start = None;
        self.word_break_seen = false; // <-- ФИКС: сбрасываем, чтобы первый WordBreak снова очистил
    }

    pub fn set_manual_tone(&mut self, freq: f32) {
        self.auto_freq = false;
        self.freq_detected = true;
        self.target_freq = freq;
        self.goertzel = Goertzel::new(freq, self.sample_rate, self.block_len);
    }

    pub fn set_auto_tone(&mut self) {
        self.auto_freq = true;
        self.freq_detected = false;
        self.freq_buffer.clear();
    }

    pub fn set_wpm(&mut self, wpm: f32) {
        if wpm > 5.0 && wpm < 60.0 {
            self.manual_wpm = wpm;
            self.seed_wpm = wpm;
            let timing = TimingEstimator::from_wpm(wpm, self.sample_rate);
            self.morse = Decoder::new(timing).with_adapt(true);
            self.clear();
            self.word_break_seen = false;
        }
    }

    pub fn set_gain(&mut self, g: f32) {
        self.gain_manual = g.clamp(0.5, 10.0);
        if !self.gain_auto {
            self.gain = self.gain_manual;
        }
    }

    pub fn set_gain_auto(&mut self, auto: bool) {
        self.gain_auto = auto;
        if auto {
            self.gain = 1.5;
        } else {
            self.gain = self.gain_manual;
        }
    }

    pub fn process(&mut self, samples: &[f32]) -> CwResult {
        if samples.is_empty() {
            return self.result();
        }

        // Никакой адаптации — gain всегда фиксирован (1.5 или ручной)
        let amplified: Vec<f32> = samples.iter().map(|s| (s * self.gain).clamp(-1.0, 1.0)).collect();

        if self.auto_freq && !self.freq_detected {
            self.freq_buffer.extend_from_slice(&amplified);
            if self.freq_buffer.len() as f32 >= self.sample_rate * 0.5 {
                let mut best_freq = 700.0;
                let mut best_amp = 0.0;
                let mut f = 300.0;
                while f <= 1200.0 {
                    let mut test_goertzel = Goertzel::new(f, self.sample_rate, self.block_len);
                    let mut test_amp = 0.0;
                    for &s in &self.freq_buffer {
                        if let Some(m) = test_goertzel.push(s) {
                            test_amp = m;
                        }
                    }
                    if test_amp > best_amp {
                        best_amp = test_amp;
                        best_freq = f;
                    }
                    f += 10.0;
                }
                self.target_freq = best_freq;
                self.freq_detected = true;
                self.goertzel = Goertzel::new(best_freq, self.sample_rate, self.block_len);
            }
            return self.result();
        }

        let now = Instant::now();
        let start = *self.started_at.get_or_insert(now);
        let warming = now.duration_since(start) < WARMUP;
        if !warming && self.post_warmup_start.is_none() {
            self.post_warmup_start = Some(now);
        }

        for &s in &amplified {
            if let Some(mag) = self.goertzel.push(s) {
                self.last_amp = mag;
                self.amp_smooth = self.amp_smooth * 0.85 + mag * 0.15;
                self.block_idx += 1;

                let now_on = if self.tone_on {
                    mag > self.threshold * 0.5
                } else {
                    mag > self.threshold * 1.5
                };

                if mag < 0.1 {
                    self.noise_floor = self.noise_floor * 0.99 + mag * 0.01;
                    self.threshold = (self.noise_floor * 4.0).clamp(0.01, 0.15);
                }

                if now_on != self.tone_on {
                    let elapsed_blocks = self.block_idx - self.last_toggle_block;
                    let elapsed_samples = ((elapsed_blocks * self.block_len as u64) as u32).max(1);
                    let batch = self.morse.push(self.tone_on, elapsed_samples);

                    if !warming {
                        for decoded in batch {
                            match decoded {
                                cwdit_morse::Decoded::Char(ch) => {
                                    self.text.push(ch);
                                    self.chars_count += 1;
                                }
                                cwdit_morse::Decoded::WordBreak => {
                                    // Первый пробел после warmup — всегда чистим весь текст
                                    if !self.word_break_seen {
                                        self.text.clear();
                                        self.chars_count = 0;
                                        self.word_break_seen = true;
                                    } else if !self.text.is_empty() && !self.text.ends_with(' ') {
                                        self.text.push(' ');
                                    }
                                }
                                cwdit_morse::Decoded::Unknown => {}
                            }
                        }
                    }

                    if !warming && !self.word_break_seen {
                        if let Some(pw) = self.post_warmup_start {
                            if now.duration_since(pw) >= WORD_BREAK_FALLBACK
                                && self.chars_count >= FALLBACK_MIN_CHARS
                            {
                                self.text.clear();
                                self.chars_count = 0;
                                self.word_break_seen = true;
                            }
                        }
                    }

                    self.tone_on = now_on;
                    self.last_toggle_block = self.block_idx;
                }
            }
        }

        if self.chars_count > MAX_CHARS {
            let target_len = self.text.len().saturating_sub(KEEP_CHARS);
            let mut cut = target_len;
            while cut < self.text.len() && !self.text.is_char_boundary(cut) { cut += 1; }
            while cut < self.text.len() && !self.text[cut..].starts_with(' ') { cut += 1; }
            if cut < self.text.len() { cut += 1; } else { cut = self.text.len(); }
            self.text = self.text[cut..].to_string();
            self.chars_count = self.text.chars().count();
        }

        let now2 = Instant::now();
        if !self.signal_on {
            if self.amp_smooth > 0.02 {
                self.signal_on = true;
                self.signal_off_since = None;
            }
        } else if self.amp_smooth < 0.005 {
            if self.signal_off_since.is_none() {
                self.signal_off_since = Some(now2);
            } else if now2.duration_since(self.signal_off_since.unwrap()) >= Duration::from_millis(1500) {
                self.signal_on = false;
                self.signal_off_since = None;
            }
        } else {
            self.signal_off_since = None;
        }

        self.result()
    }

    pub fn result(&self) -> CwResult {
        let wpm = self.morse.timing().wpm(self.sample_rate);
        CwResult {
            text: self.text.clone(),
            tone_hz: self.target_freq,
            wpm,
            chars: self.chars_count,
            last_amp: self.last_amp,
            amp_smooth: self.amp_smooth,
            signal_on: self.signal_on,
            gain: self.gain,
        }
    }
}

pub fn highlight_cw(text: &str) -> egui::text::LayoutJob {
    let highlights: &[(&str, egui::Color32)] = &[
        ("CQ", egui::Color32::from_rgb(255, 220, 100)),
        ("73", egui::Color32::from_rgb(255, 220, 100)),
        ("88", egui::Color32::from_rgb(255, 220, 100)),
        ("QTH", egui::Color32::from_rgb(255, 170, 80)),
        ("QSL", egui::Color32::from_rgb(255, 170, 80)),
        ("QRZ", egui::Color32::from_rgb(255, 170, 80)),
        ("DE", egui::Color32::from_rgb(150, 200, 255)),
        ("RST", egui::Color32::from_rgb(150, 200, 255)),
        ("599", egui::Color32::from_rgb(150, 200, 255)),
        ("5NN", egui::Color32::from_rgb(150, 200, 255)),
        ("NAME", egui::Color32::from_rgb(150, 200, 255)),
        ("R2GFK", egui::Color32::from_rgb(255, 130, 200)),
        ("R2GBOB", egui::Color32::from_rgb(255, 130, 200)),
    ];
    let mut job = egui::text::LayoutJob::default();
    let default_color = egui::Color32::from_rgb(120, 255, 120);
    let default_font = egui::FontId::monospace(16.0);
    let mut rest = text;
    while !rest.is_empty() {
        let upper = rest.to_uppercase();
        let mut found: Option<(usize, &str, egui::Color32)> = None;
        for (pat, color) in highlights {
            if let Some(pos) = upper.find(pat) {
                if found.map(|(p, _, _)| pos < p).unwrap_or(true) { found = Some((pos, pat, *color)); }
            }
        }
        if let Some((pos, pat, color)) = found {
            if pos > 0 {
                job.append(&rest[..pos], 0.0, egui::TextFormat { font_id: default_font.clone(), color: default_color, ..Default::default() });
            }
            let end = pos + pat.len();
            job.append(&rest[pos..end], 0.0, egui::TextFormat { font_id: default_font.clone(), color, ..Default::default() });
            rest = &rest[end..];
        } else {
            job.append(rest, 0.0, egui::TextFormat { font_id: default_font.clone(), color: default_color, ..Default::default() });
            break;
        }
    }
    job
}