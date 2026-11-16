#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::net::UdpSocket;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use cpal::traits::{HostTrait, DeviceTrait, StreamTrait};
use rubato::VecResampler;

#[cfg(windows)]
use windows::Win32::Foundation::{HWND, LPARAM};
#[cfg(windows)]
use windows::core::BOOL;
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, GetWindowTextW, IsIconic, IsWindowVisible,
    ShowWindow, SW_MINIMIZE, SW_RESTORE,
    GetForegroundWindow,
    SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOACTIVATE,
};

mod analog;
mod cw;
mod cw_gui;

const CREATE_NO_WINDOW: u32 = 0x08000000;
const DETACHED_PROCESS: u32 = 0x00000008;

const STATE_HIDDEN: u8 = 0;
const STATE_NORMAL: u8 = 1;
const STATE_TOPMOST: u8 = 2;

const ANALOG_SQL_THRESHOLD: f32 = 0.001;
const CW_SQL_THRESHOLD: f32 = 0.02;
const ANALOG_TONE_HOLD_SECS: u64 = 10;
const ANALOG_DISPLAY_HOLD_SECS: u64 = 10;

static SUNDEC_HWND: AtomicIsize = AtomicIsize::new(0);

fn base_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn level_file() -> PathBuf { base_dir().join("level.txt") }
fn log_file_path() -> PathBuf { base_dir().join("dsd_log.txt") }
fn frequency_file() -> PathBuf { base_dir().join("frequency.txt") }
fn modulation_file() -> PathBuf { base_dir().join("modulation.txt") }

fn recordings_dir() -> PathBuf {
    let dir = base_dir().join("RECORDER MP3");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn read_frequency() -> u64 {
    fs::read_to_string(frequency_file())
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

fn read_modulation() -> String {
    fs::read_to_string(modulation_file())
        .ok()
        .map(|s| s.trim().to_uppercase())
        .unwrap_or_default()
}

fn read_audio_level() -> f32 {
    fs::read_to_string(level_file()).ok().and_then(|s| s.trim().parse::<f32>().ok()).unwrap_or(0.0).clamp(0.0, 1.0)
}

fn format_frequency(hz: u64) -> String {
    if hz == 0 { return "".to_string(); }
    let mhz = hz as f64 / 1_000_000.0;
    let s = format!("{:.5}", mhz);
    if s.len() >= 2 {
        let split_pos = s.len() - 2;
        format!("{}.{} МГц", &s[..split_pos], &s[split_pos..])
    } else {
        format!("{} МГц", s)
    }
}

#[cfg(windows)]
fn find_process_pid(image_name: &str) -> Option<u32> {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {}", image_name), "/FO", "CSV", "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.split(',').nth(1)?.trim_matches('"').parse().ok()
}

#[cfg(windows)]
fn find_sundec_hwnd() -> Option<HWND> {
    let pid = std::process::id();
    struct SearchData { target_pid: u32, found: Option<HWND> }
    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let data = &mut *(lparam.0 as *mut SearchData);
        let mut window_pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut window_pid));
        if window_pid != data.target_pid { return BOOL(1); }
        let mut buf = [0u16; 256];
        let len = GetWindowTextW(hwnd, &mut buf);
        if len > 0 {
            let title = String::from_utf16_lossy(&buf[..len as usize]);
            if title.contains("SunDEC") {
                data.found = Some(hwnd);
                return BOOL(0);
            }
        }
        BOOL(1)
    }
    let mut data = SearchData { target_pid: pid, found: None };
    unsafe { let _ = EnumWindows(Some(enum_proc), LPARAM(&mut data as *mut _ as isize)); }
    data.found
}

#[cfg(windows)]
fn is_sdr_process_visible(sdr_pid: u32) -> bool {
    struct Data { target_pid: u32, found_visible: bool }
    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let data = &mut *(lparam.0 as *mut Data);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != data.target_pid { return BOOL(1); }
        if IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool() {
            data.found_visible = true;
            return BOOL(0);
        }
        BOOL(1)
    }
    let mut data = Data { target_pid: sdr_pid, found_visible: false };
    unsafe { let _ = EnumWindows(Some(enum_proc), LPARAM(&mut data as *mut _ as isize)); }
    data.found_visible
}

#[cfg(windows)]
fn is_sdr_process_focused(sdr_pid: u32) -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() { return false; }
        let mut fg_pid = 0u32;
        GetWindowThreadProcessId(fg, Some(&mut fg_pid));
        if fg_pid == std::process::id() { return true; }
        if fg_pid == sdr_pid { return is_sdr_process_visible(sdr_pid); }
        false
    }
}

#[cfg(windows)]
fn sundec_hide() {
    let hwnd_val = SUNDEC_HWND.load(Ordering::SeqCst);
    if hwnd_val != 0 {
        unsafe {
            let hwnd = HWND(hwnd_val as *mut core::ffi::c_void);
            let _ = ShowWindow(hwnd, SW_MINIMIZE);
        }
    }
}

#[cfg(windows)]
fn sundec_show() {
    let hwnd_val = SUNDEC_HWND.load(Ordering::SeqCst);
    if hwnd_val != 0 {
        unsafe {
            let hwnd = HWND(hwnd_val as *mut core::ffi::c_void);
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
    }
}

#[cfg(windows)]
fn sundec_minimize() {
    let hwnd_val = SUNDEC_HWND.load(Ordering::SeqCst);
    if hwnd_val != 0 {
        unsafe {
            let hwnd = HWND(hwnd_val as *mut core::ffi::c_void);
            let _ = ShowWindow(hwnd, SW_MINIMIZE);
        }
    }
}

#[cfg(windows)]
fn sundec_set_normal() {
    let hwnd_val = SUNDEC_HWND.load(Ordering::SeqCst);
    if hwnd_val != 0 {
        unsafe {
            let hwnd = HWND(hwnd_val as *mut core::ffi::c_void);
            let _ = SetWindowPos(hwnd, Some(HWND_NOTOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }
}

#[cfg(windows)]
fn sundec_set_topmost() {
    let hwnd_val = SUNDEC_HWND.load(Ordering::SeqCst);
    if hwnd_val != 0 {
        unsafe {
            let hwnd = HWND(hwnd_val as *mut core::ffi::c_void);
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct SavedState {
    tci_port: String,
    volume: f32,
    dmr_only: bool,
    dmr_key: String,
    key_type: String,
    #[serde(default)]
    always_on_top: bool,
    #[serde(default)]
    window_pos: Option<(f32, f32)>,
    #[serde(default)]
    sync_with_sdr: bool,
    #[serde(default)]
    auto_start: bool,
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default = "default_cw_auto_tone")]
    cw_auto_tone: bool,
    #[serde(default = "default_cw_manual_tone")]
    cw_manual_tone: f32,
    #[serde(default = "default_cw_manual_wpm")]
    cw_manual_wpm: f32,
    #[serde(default = "default_cw_gain_auto")]
    cw_gain_auto: bool,
    #[serde(default = "default_cw_gain_manual")]
    cw_gain_manual: f32,
}

fn default_mode() -> String { "dmr".to_string() }
fn default_cw_auto_tone() -> bool { true }
fn default_cw_manual_tone() -> f32 { 700.0 }
fn default_cw_manual_wpm() -> f32 { 20.0 }
fn default_cw_gain_auto() -> bool { true }
fn default_cw_gain_manual() -> f32 { 1.5 }

impl Default for SavedState {
    fn default() -> Self {
        Self {
            tci_port: "50001".to_string(),
            volume: 50.0,
            dmr_only: true,
            dmr_key: "".to_string(),
            key_type: "none".to_string(),
            always_on_top: false,
            window_pos: None,
            sync_with_sdr: false,
            auto_start: false,
            mode: "dmr".to_string(),
            cw_auto_tone: true,
            cw_manual_tone: 700.0,
            cw_manual_wpm: 20.0,
            cw_gain_auto: true,
            cw_gain_manual: 1.5,
        }
    }
}

fn load_settings() -> SavedState {
    fs::read_to_string(base_dir().join("settings.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_settings(state: &SavedState) {
    if let Ok(json) = serde_json::to_string_pretty(state) {
        let _ = fs::write(base_dir().join("settings.json"), json);
    }
}

struct App {
    tci_bridge_process: Option<Child>,
    decoder_process: Option<Child>,
    udp_reader_stop: Arc<AtomicBool>,
    udp_reader_thread: Option<std::thread::JoinHandle<()>>,
    cpal_stream: Option<cpal::Stream>,
    is_running: bool,
    is_recording: bool,
    wav_writer: Arc<Mutex<Option<hound::WavWriter<std::io::BufWriter<std::fs::File>>>>>,
    recording_wav_path: Option<PathBuf>,
    recording_mp3_path: Option<PathBuf>,
    recording_accumulated_secs: Arc<Mutex<u64>>,
    recording_active_since: Arc<Mutex<Option<Instant>>>,
    last_carrier_time: Arc<Mutex<Option<Instant>>>,
    volume_shared: Arc<Mutex<f32>>,
    protocol: Arc<Mutex<String>>,
    dmr_cc: Arc<Mutex<String>>,
    dmr_tg: Arc<Mutex<String>>,
    dmr_src: Arc<Mutex<String>>,
    dmr_slot: Arc<Mutex<String>>,
    dmr_alias: Arc<Mutex<String>>,
    dmr_gps: Arc<Mutex<String>>,
    dmr_text: Arc<Mutex<String>>,
    dmr_call_type: Arc<Mutex<String>>,
    dmr_encrypted: Arc<Mutex<String>>,
    sync_active: Arc<Mutex<bool>>,
    error_count: Arc<Mutex<u32>>,
    last_sync_time: Arc<Mutex<Option<Instant>>>,
    audio_level: Arc<Mutex<f32>>,
    parser_stop: Arc<AtomicBool>,
    dmr_only: bool,
    tci_port: String,
    volume: f32,
    restart_until: Option<Instant>,
    dmr_key: String,
    key_type: String,
    show_cipher_window: bool,
    show_about_window: bool,
    always_on_top: bool,
    show_context_menu: bool,
    menu_just_opened: bool,
    last_src: Arc<Mutex<String>>,
    last_src_time: Arc<Mutex<Option<Instant>>>,
    window_pos: Option<(f32, f32)>,
    sync_with_sdr: bool,
    auto_start: bool,
    auto_start_triggered: bool,
    sync_thread: Option<std::thread::JoinHandle<()>>,
    sync_stop: Arc<AtomicBool>,
    hwnd_cached: bool,
    mode: String,
    tone: Arc<Mutex<String>>,
    recording_start_freq: u64,
    analog_udp_level: Arc<Mutex<f32>>,
    analog_has_signal: Arc<Mutex<bool>>,
    analog_last_raw_signal: Arc<Mutex<Option<Instant>>>,
    last_analog_freq: Arc<Mutex<u64>>,
    last_analog_tone: Arc<Mutex<String>>,
    last_analog_time: Arc<Mutex<Option<Instant>>>,
    last_analog_level: Arc<Mutex<f32>>,
    analog_display_until: Arc<Mutex<Option<Instant>>>,
    cw_view: cw_gui::CwView,
}

impl Default for App {
    fn default() -> Self {
        Self {
            tci_bridge_process: None,
            decoder_process: None,
            udp_reader_stop: Arc::new(AtomicBool::new(false)),
            udp_reader_thread: None,
            cpal_stream: None,
            is_running: false,
            is_recording: false,
            wav_writer: Arc::new(Mutex::new(None)),
            recording_wav_path: None,
            recording_mp3_path: None,
            recording_accumulated_secs: Arc::new(Mutex::new(0)),
            recording_active_since: Arc::new(Mutex::new(None)),
            last_carrier_time: Arc::new(Mutex::new(None)),
            volume_shared: Arc::new(Mutex::new(50.0)),
            protocol: Arc::new(Mutex::new("---".to_string())),
            dmr_cc: Arc::new(Mutex::new("---".to_string())),
            dmr_tg: Arc::new(Mutex::new("---".to_string())),
            dmr_src: Arc::new(Mutex::new("---".to_string())),
            dmr_slot: Arc::new(Mutex::new("---".to_string())),
            dmr_alias: Arc::new(Mutex::new("".to_string())),
            dmr_gps: Arc::new(Mutex::new("".to_string())),
            dmr_text: Arc::new(Mutex::new("".to_string())),
            dmr_call_type: Arc::new(Mutex::new("".to_string())),
            dmr_encrypted: Arc::new(Mutex::new("---".to_string())),
            sync_active: Arc::new(Mutex::new(false)),
            error_count: Arc::new(Mutex::new(0)),
            last_sync_time: Arc::new(Mutex::new(None)),
            audio_level: Arc::new(Mutex::new(0.0)),
            parser_stop: Arc::new(AtomicBool::new(false)),
            dmr_only: true,
            tci_port: "50001".to_string(),
            volume: 50.0,
            restart_until: None,
            dmr_key: "".to_string(),
            key_type: "none".to_string(),
            show_cipher_window: false,
            show_about_window: false,
            always_on_top: false,
            show_context_menu: false,
            menu_just_opened: false,
            last_src: Arc::new(Mutex::new("---".to_string())),
            last_src_time: Arc::new(Mutex::new(None)),
            window_pos: None,
            sync_with_sdr: false,
            auto_start: false,
            auto_start_triggered: false,
            sync_thread: None,
            sync_stop: Arc::new(AtomicBool::new(false)),
            hwnd_cached: false,
            mode: "dmr".to_string(),
            tone: Arc::new(Mutex::new("---".to_string())),
            recording_start_freq: 0,
            analog_udp_level: Arc::new(Mutex::new(0.0)),
            analog_has_signal: Arc::new(Mutex::new(false)),
            analog_last_raw_signal: Arc::new(Mutex::new(None)),
            last_analog_freq: Arc::new(Mutex::new(0)),
            last_analog_tone: Arc::new(Mutex::new("---".to_string())),
            last_analog_time: Arc::new(Mutex::new(None)),
            last_analog_level: Arc::new(Mutex::new(0.0)),
            analog_display_until: Arc::new(Mutex::new(None)),
            cw_view: cw_gui::CwView::default(),
        }
    }
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let saved = load_settings();
        let mut app = Self::default();
        app.tci_port = saved.tci_port;
        app.volume = saved.volume;
        app.dmr_only = saved.dmr_only;
        app.dmr_key = saved.dmr_key;
        app.key_type = saved.key_type;
        app.always_on_top = saved.always_on_top;
        app.window_pos = saved.window_pos;
        app.sync_with_sdr = saved.sync_with_sdr;
        app.auto_start = saved.auto_start;
        app.mode = saved.mode.clone();
        *app.volume_shared.lock().unwrap() = saved.volume;

        app.cw_view.auto_tone = saved.cw_auto_tone;
        app.cw_view.manual_tone = saved.cw_manual_tone;
        app.cw_view.manual_wpm = saved.cw_manual_wpm;
        app.cw_view.gain_auto = saved.cw_gain_auto;
        app.cw_view.gain_manual = saved.cw_gain_manual;

        if app.mode == "cw" {
            let mut dec = cw::CwDecoder::new(48000.0);
            if !app.cw_view.auto_tone {
                dec.set_manual_tone(app.cw_view.manual_tone);
            }
            dec.set_wpm(app.cw_view.manual_wpm);
            dec.set_gain_auto(app.cw_view.gain_auto);
            if !app.cw_view.gain_auto {
                dec.set_gain(app.cw_view.gain_manual);
            }
            *app.cw_view.decoder.lock().unwrap() = Some(dec);
        }

        if app.always_on_top {
            cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
        }

        app
    }

    fn start_sync_thread(&mut self) {
        if self.sync_thread.is_some() { return; }
        self.sync_stop.store(false, Ordering::SeqCst);

        let stop = self.sync_stop.clone();
        let handle = std::thread::spawn(move || {
            let mut sdr_pid: Option<u32> = None;
            let mut last_pid_check = Instant::now() - Duration::from_secs(10);
            const HIDDEN_TICKS: u32 = 10;
            const VISIBLE_TICKS: u32 = 15;
            let mut hidden_streak: u32 = 0;
            let mut visible_streak: u32 = 0;
            let mut applied_state: u8 = STATE_NORMAL;

            loop {
                if stop.load(Ordering::SeqCst) { break; }

                if sdr_pid.is_none() || last_pid_check.elapsed() > Duration::from_secs(1) {
                    last_pid_check = Instant::now();
                    sdr_pid = find_process_pid("SdrApplication.exe");
                }

                if let Some(pid) = sdr_pid {
                    let visible_now = is_sdr_process_visible(pid);
                    let focused = is_sdr_process_focused(pid);

                    if !visible_now {
                        hidden_streak = hidden_streak.saturating_add(1);
                        visible_streak = 0;
                    } else {
                        visible_streak = visible_streak.saturating_add(1);
                        hidden_streak = 0;
                    }

                    let desired_state: u8 = if hidden_streak >= HIDDEN_TICKS {
                        STATE_HIDDEN
                    } else if visible_streak >= VISIBLE_TICKS {
                        if focused { STATE_TOPMOST } else { STATE_NORMAL }
                    } else {
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    };

                    if desired_state != applied_state {
                        match desired_state {
                            STATE_HIDDEN => { sundec_hide(); }
                            STATE_NORMAL => { sundec_show(); sundec_set_normal(); }
                            STATE_TOPMOST => { sundec_show(); sundec_set_topmost(); }
                            _ => {}
                        }
                        applied_state = desired_state;
                    }
                }

                std::thread::sleep(Duration::from_millis(100));
            }
        });
        self.sync_thread = Some(handle);
    }

    fn stop_sync_thread(&mut self) {
        self.sync_stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.sync_thread.take() {
            let _ = handle.join();
        }
    }

    fn do_start(&mut self) {
        self.stop_recording();
        self.stop_audio_pipeline();
        self.parser_stop.store(true, Ordering::SeqCst);
        kill_all_processes(&mut self.tci_bridge_process, &mut self.decoder_process);

        let _ = std::fs::remove_file(level_file());
        let _ = std::fs::remove_file(frequency_file());
        *self.sync_active.lock().unwrap() = false;
        *self.audio_level.lock().unwrap() = 0.0;
        *self.analog_udp_level.lock().unwrap() = 0.0;
        *self.analog_has_signal.lock().unwrap() = false;
        *self.analog_last_raw_signal.lock().unwrap() = None;
        *self.last_carrier_time.lock().unwrap() = None;
        *self.last_analog_freq.lock().unwrap() = 0;
        *self.last_analog_tone.lock().unwrap() = "---".to_string();
        *self.last_analog_time.lock().unwrap() = None;
        *self.last_analog_level.lock().unwrap() = 0.0;
        *self.analog_display_until.lock().unwrap() = None;
        *self.error_count.lock().unwrap() = 0;
        *self.last_sync_time.lock().unwrap() = None;
        *self.protocol.lock().unwrap() = "---".to_string();
        *self.dmr_cc.lock().unwrap() = "---".to_string();
        *self.dmr_tg.lock().unwrap() = "---".to_string();
        *self.dmr_src.lock().unwrap() = "---".to_string();
        *self.dmr_slot.lock().unwrap() = "---".to_string();
        *self.dmr_alias.lock().unwrap() = "".to_string();
        *self.dmr_gps.lock().unwrap() = "".to_string();
        *self.dmr_text.lock().unwrap() = "".to_string();
        *self.dmr_call_type.lock().unwrap() = "".to_string();
        *self.dmr_encrypted.lock().unwrap() = "---".to_string();
        *self.last_src.lock().unwrap() = "---".to_string();
        *self.last_src_time.lock().unwrap() = None;
        *self.tone.lock().unwrap() = "---".to_string();
        
        self.cw_view.reset();
        *self.cw_view.amp.lock().unwrap() = 0.0;
        *self.cw_view.tone_hz.lock().unwrap() = 0.0;
        *self.cw_view.wpm.lock().unwrap() = 0.0;
        *self.cw_view.chars.lock().unwrap() = 0;
        self.cw_view.last_clear_freq = 0;

        let bridge_child = Command::new(base_dir().join("bridge.exe"))
            .arg(&self.tci_port).arg("7355").arg(&self.mode)
            .current_dir(base_dir())
            .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
            .spawn().expect("Failed to start bridge.exe");
        self.tci_bridge_process = Some(bridge_child);

        if self.mode == "cw" {
            *self.cw_view.decoder.lock().unwrap() = Some(cw::CwDecoder::new(48000.0));
            if let Some(dec) = self.cw_view.decoder.lock().unwrap().as_mut() {
                if !self.cw_view.auto_tone {
                    dec.set_manual_tone(self.cw_view.manual_tone);
                }
                dec.set_wpm(self.cw_view.manual_wpm);
                dec.set_gain_auto(self.cw_view.gain_auto);
                if !self.cw_view.gain_auto {
                    dec.set_gain(self.cw_view.gain_manual);
                }
            }
            self.start_audio_pipeline();
            self.is_running = true;
            return;
        }

        let log_file = log_file_path();
        let _ = std::fs::remove_file(&log_file);

        let mut base_args = if self.mode == "analog" {
            "-i tcp:127.0.0.1:7355 -o udp:127.0.0.1:23456 --frontend none --api 9911 -fA".to_string()
        } else if self.mode == "dmr" {
            "-i tcp:127.0.0.1:7355 -o udp:127.0.0.1:23456 --frontend none --api 9911 -fs".to_string()
        } else {
            "-i tcp:127.0.0.1:7355 -o udp:127.0.0.1:23456 --frontend none --api 9911".to_string()
        };
        if self.key_type == "hytera" && !self.dmr_key.is_empty() { base_args.push_str(&format!(" -H {}", self.dmr_key)); }
        else if self.key_type == "basic" && !self.dmr_key.is_empty() { base_args.push_str(&format!(" -b {}", self.dmr_key)); }
        let log_file_handle = std::fs::File::create(&log_file).expect("Не удалось создать лог-файл");
        let args_vec: Vec<&str> = base_args.split_whitespace().collect();
        let decoder_child = Command::new(base_dir().join("decoder.exe"))
            .args(&args_vec)
            .current_dir(base_dir())
            .env("DSD_NEO_TCP_PREBUF_MS", "2000")
            .env("DSD_NEO_SNR_SQL_DB", "10")
            .stderr(std::process::Stdio::from(log_file_handle))
            .creation_flags(CREATE_NO_WINDOW)
            .spawn().expect("Failed to start decoder.exe");
        self.decoder_process = Some(decoder_child);
        self.parser_stop.store(false, Ordering::SeqCst);

        let proto_clone = self.protocol.clone();
        let cc_clone = self.dmr_cc.clone();
        let tg_clone = self.dmr_tg.clone();
        let src_clone = self.dmr_src.clone();
        let slot_clone = self.dmr_slot.clone();
        let alias_clone = self.dmr_alias.clone();
        let gps_clone = self.dmr_gps.clone();
        let text_clone = self.dmr_text.clone();
        let call_type_clone = self.dmr_call_type.clone();
        let encrypted_clone = self.dmr_encrypted.clone();
        let sync_clone = self.sync_active.clone();
        let err_clone = self.error_count.clone();
        let last_sync_clone = self.last_sync_time.clone();
        let stop_clone = self.parser_stop.clone();
        let last_src_clone = self.last_src.clone();
        let last_src_time_clone = self.last_src_time.clone();
        let tone_clone = self.tone.clone();
        let mode_clone = self.mode.clone();

        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            let mut position = 0u64;
            let mut call_active = false;
            loop {
                if stop_clone.load(Ordering::SeqCst) { break; }
                if let Ok(mut file) = std::fs::File::open(&log_file) {
                    let _ = file.seek(SeekFrom::Start(position));
                    let reader = BufReader::new(file);
                    for line in reader.lines() {
                        if stop_clone.load(Ordering::SeqCst) { break; }
                        if let Ok(raw_text) = line {
                            position += raw_text.len() as u64 + 1;
                            let text = strip_ansi(&raw_text);

                            if mode_clone == "analog" {
                                if let Some(tone) = analog::parse_analog_tone(&text) {
                                    if tone != "none" {
                                        *tone_clone.lock().unwrap() = tone;
                                    }
                                }
                            }

                            if text.contains("Sync: +") || text.contains("Sync: -") {
                                if !text.contains("Color Code=XX") {
                                    *sync_clone.lock().unwrap() = true;
                                    *last_sync_clone.lock().unwrap() = Some(Instant::now());
                                    call_active = true;
                                }
                            } else if text.contains("Sync: no sync") || text.contains("Exiting") {
                                *sync_clone.lock().unwrap() = false;
                                call_active = false;
                            }

                            if text.contains("ERR") && !text.contains("NO ERR") {
                                if call_active { *err_clone.lock().unwrap() += 1; }
                            }
                            if text.contains("Sync: ") {
                                for word in text.split_whitespace() {
                                    if (word.starts_with('+') || word.starts_with('-')) && word.len() > 1 {
                                        let proto = word.trim_start_matches('+').trim_start_matches('-');
                                        if proto.chars().all(|c| c.is_ascii_alphanumeric()) {
                                            *proto_clone.lock().unwrap() = proto.to_string(); break;
                                        }
                                    }
                                }
                            }
                            if text.contains("Color Code=") {
                                if let Some(cc_part) = text.split("Color Code=").nth(1) {
                                    if let Some(cc) = cc_part.split_whitespace().next() {
                                        if cc != "XX" {
                                            let cc_clean = cc.trim_start_matches('0');
                                            *cc_clone.lock().unwrap() = if cc_clean.is_empty() { "0".to_string() } else { cc_clean.to_string() };
                                        }
                                    }
                                }
                            }
                            if text.contains("TGT=") {
                                if let Some(tgt_part) = text.split("TGT=").nth(1) {
                                    if let Some(tgt) = tgt_part.split(|c| c == ' ' || c == ';').next() {
                                        *tg_clone.lock().unwrap() = tgt.to_string();
                                    }
                                }
                            }
                            if text.contains("SRC=") {
                                if let Some(src_part) = text.split("SRC=").nth(1) {
                                    if let Some(src) = src_part.split(|c| c == ' ' || c == ';').next() {
                                        *src_clone.lock().unwrap() = src.to_string();
                                        *last_src_clone.lock().unwrap() = src.to_string();
                                        *last_src_time_clone.lock().unwrap() = Some(Instant::now());
                                    }
                                }
                            }
                            if text.contains("SLOT 1") { *slot_clone.lock().unwrap() = "1".to_string(); }
                            else if text.contains("SLOT 2") { *slot_clone.lock().unwrap() = "2".to_string(); }

                            if text.contains("Talker Alias:") {
                                if let Some(alias_part) = text.split("Talker Alias:").nth(1) {
                                    let alias_bytes: Vec<u8> = alias_part.chars().map(|c| c as u32 as u8).collect();
                                    let (decoded, _, _) = encoding_rs::WINDOWS_1251.decode(&alias_bytes);
                                    let fixed_alias = decoded.into_owned();
                                    *alias_clone.lock().unwrap() = fixed_alias.trim().to_string();
                                }
                            }
                            if text.contains("GPS:") || text.contains("Embedded GPS:") {
                                let gps_part = if let Some(g) = text.split("Embedded GPS:").nth(1) {
                                    g
                                } else if let Some(g) = text.split("GPS:").nth(1) {
                                    g
                                } else {
                                    ""
                                };
                                if !gps_part.is_empty() {
                                    *gps_clone.lock().unwrap() = gps_part.trim().to_string();
                                }
                            }
                            if text.contains("Text:") {
                                if let Some(txt) = text.split("Text:").nth(1) {
                                    *text_clone.lock().unwrap() = txt.trim().to_string();
                                }
                            }
                            if text.contains("Group Call") {
                                *call_type_clone.lock().unwrap() = "Групповой".to_string();
                            } else if text.contains("Private Call") {
                                *call_type_clone.lock().unwrap() = "Личный".to_string();
                            } else if text.contains("Data Call") {
                                *call_type_clone.lock().unwrap() = "Данные".to_string();
                            }
                            if text.contains("Encrypted LC") || text.contains("ALGID") {
                                let mut enc_type = "???".to_string();
                                let text_upper = text.to_uppercase();
                                if text_upper.contains("AES") { enc_type = "AES".to_string(); }
                                else if text_upper.contains("DES") { enc_type = "DES".to_string(); }
                                else if text_upper.contains("RC4") { enc_type = "RC4".to_string(); }
                                else if text_upper.contains("ALGID") {
                                    if let Some(idx) = text_upper.find("ALGID") {
                                        let rest = &text_upper[idx..];
                                        if let Some(colon) = rest.find(':') {
                                            let val = rest[colon+1..].trim().split_whitespace().next().unwrap_or("");
                                            if val == "0X21" { enc_type = "RC4".to_string(); }
                                            else if val == "0X22" { enc_type = "DES".to_string(); }
                                            else if val == "0X24" { enc_type = "AES-128".to_string(); }
                                            else if val == "0X25" { enc_type = "AES-256".to_string(); }
                                            else { enc_type = format!("ALGID {}", val); }
                                        }
                                    }
                                }
                                *encrypted_clone.lock().unwrap() = enc_type;
                            }
                        }
                    }
                }
                if let Some(last_sync) = *last_sync_clone.lock().unwrap() {
                    if last_sync.elapsed() > Duration::from_secs(10) {
                        *sync_clone.lock().unwrap() = false;
                        *proto_clone.lock().unwrap() = "---".to_string();
                        *cc_clone.lock().unwrap() = "---".to_string();
                        *tg_clone.lock().unwrap() = "---".to_string();
                        *src_clone.lock().unwrap() = "---".to_string();
                        *slot_clone.lock().unwrap() = "---".to_string();
                        *alias_clone.lock().unwrap() = "".to_string();
                        *gps_clone.lock().unwrap() = "".to_string();
                        *text_clone.lock().unwrap() = "".to_string();
                        *call_type_clone.lock().unwrap() = "".to_string();
                        *encrypted_clone.lock().unwrap() = "---".to_string();
                        *err_clone.lock().unwrap() = 0;
                        call_active = false;
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });

        self.start_audio_pipeline();
        self.is_running = true;
    }

    fn start_audio_pipeline(&mut self) {
        self.stop_audio_pipeline();
        self.udp_reader_stop.store(false, Ordering::SeqCst);

        let mut audio_buffer_opt: Option<Arc<Mutex<Vec<f32>>>> = None;
        let mut actual_rate = 8000u32;

        if self.mode != "analog" && self.mode != "cw" {
            let host = cpal::default_host();
            if let Some(device) = host.default_output_device() {
                let mut chosen_config: Option<cpal::SupportedStreamConfig> = None;
                if let Ok(configs) = device.supported_output_configs() {
                    for config in configs {
                        if config.channels() >= 2 && config.min_sample_rate().0 <= 8000 && config.max_sample_rate().0 >= 8000 {
                            chosen_config = Some(config.with_sample_rate(cpal::SampleRate(8000)));
                            break;
                        }
                    }
                }
                let final_config = match chosen_config {
                    Some(c) => c,
                    None => match device.default_output_config() {
                        Ok(c) => c,
                        Err(_) => { return; }
                    }
                };

                let _sample_format = final_config.sample_format();
                let stream_config: cpal::StreamConfig = final_config.into();
                actual_rate = stream_config.sample_rate.0;

                let audio_buffer: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
                let audio_buffer_clone = audio_buffer.clone();

                if let Ok(stream) = device.build_output_stream(
                    &stream_config,
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        let mut buf = audio_buffer_clone.lock().unwrap();
                        let len = data.len().min(buf.len());
                        data[..len].copy_from_slice(&buf[..len]);
                        buf.drain(..len);
                        for sample in &mut data[len..] { *sample = 0.0; }
                    },
                    |_| {},
                    None,
                ) {
                    let _ = stream.play();
                    self.cpal_stream = Some(stream);
                    audio_buffer_opt = Some(audio_buffer);
                }
            }
        }

        let stop_flag = self.udp_reader_stop.clone();
        let wav_writer_arc = self.wav_writer.clone();
        let sync_active_arc = self.sync_active.clone();
        let last_sync_time_arc = self.last_sync_time.clone();
        let last_carrier_arc = self.last_carrier_time.clone();
        let acc_secs = self.recording_accumulated_secs.clone();
        let active_since = self.recording_active_since.clone();
        let volume_arc = self.volume_shared.clone();
        let analog_udp_level_arc = self.analog_udp_level.clone();
        let analog_has_signal_arc = self.analog_has_signal.clone();
        let analog_last_raw_signal_arc = self.analog_last_raw_signal.clone();
        let last_analog_freq_arc = self.last_analog_freq.clone();
        let last_analog_tone_arc = self.last_analog_tone.clone();
        let last_analog_time_arc = self.last_analog_time.clone();
        let _last_analog_level_arc = self.last_analog_level.clone();
        let tone_arc = self.tone.clone();
        let mode_for_thread = self.mode.clone();
        let cw_decoder_arc = self.cw_view.decoder.clone();
        let cw_text_arc = self.cw_view.text.clone();
        let cw_tone_hz_arc = self.cw_view.tone_hz.clone();
        let cw_wpm_arc = self.cw_view.wpm.clone();
        let cw_chars_arc = self.cw_view.chars.clone();
        let cw_amp_arc = self.cw_view.amp.clone();

        let handle = std::thread::spawn(move || {
            let udp_port = if mode_for_thread == "analog" || mode_for_thread == "cw" { 23458 } else { 23456 };
            let socket = match UdpSocket::bind(format!("127.0.0.1:{}", udp_port)) {
                Ok(s) => s,
                Err(_) => return,
            };
            let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
            let mut buf = [0u8; 4096];

            let mut cw_samples: Vec<f32> = Vec::new();

            let mut resampler = if actual_rate != 8000 && audio_buffer_opt.is_some() {
                let params = rubato::SincInterpolationParameters {
                    sinc_len: 256,
                    f_cutoff: 0.95,
                    interpolation: rubato::SincInterpolationType::Linear,
                    oversampling_factor: 256,
                    window: rubato::WindowFunction::BlackmanHarris2,
                };
                rubato::SincFixedIn::<f32>::new(
                    actual_rate as f64 / 8000.0,
                    1.0,
                    params,
                    160,
                    2,
                ).ok()
            } else {
                None
            };

            let mut last_udp_packet = Instant::now();

            while !stop_flag.load(Ordering::SeqCst) {
                match socket.recv_from(&mut buf) {
                    Ok((size, _)) => {
                        last_udp_packet = Instant::now();
                        let now = Instant::now();

                        let mut peak: f32 = 0.0;
                        for chunk in buf[..size].chunks_exact(2) {
                            let s = i16::from_le_bytes([chunk[0], chunk[1]]);
                            let a = (s as f32 / 32768.0).abs();
                            if a > peak { peak = a; }
                        }
                        *analog_udp_level_arc.lock().unwrap() = peak;

                        let should_write = if mode_for_thread == "analog" || mode_for_thread == "cw" {
                            true
                        } else {
                            let sync_on = *sync_active_arc.lock().unwrap();
                            let last_sync = *last_sync_time_arc.lock().unwrap();
                            let since_sync = last_sync.map(|t| now.duration_since(t)).unwrap_or(Duration::from_secs(3600));
                            sync_on && since_sync < Duration::from_secs(2)
                        };

                        if mode_for_thread == "analog" || mode_for_thread == "cw" {
                            let analog_signal_now = *analog_has_signal_arc.lock().unwrap();
                            if analog_signal_now {
                                *analog_last_raw_signal_arc.lock().unwrap() = Some(now);
                                *last_analog_freq_arc.lock().unwrap() = read_frequency();
                                *last_analog_tone_arc.lock().unwrap() = tone_arc.lock().unwrap().clone();
                                *last_analog_time_arc.lock().unwrap() = Some(now);
                            }
                        }

                        if should_write {
                            *last_carrier_arc.lock().unwrap() = Some(now);
                        }

                        let mut acc = acc_secs.lock().unwrap();
                        let mut act = active_since.lock().unwrap();
                        if should_write {
                            if act.is_none() { *act = Some(now); }
                        } else {
                            if let Some(start) = act.take() { *acc += start.elapsed().as_secs(); }
                        }
                        drop(acc);
                        drop(act);

                        if should_write && mode_for_thread != "cw" {
                            if let Ok(mut writer_opt) = wav_writer_arc.lock() {
                                if let Some(ref mut writer) = *writer_opt {
                                    for chunk in buf[..size].chunks_exact(2) {
                                        let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
                                        let _ = writer.write_sample(sample);
                                    }
                                }
                            }
                        }

                        if mode_for_thread == "cw" {
                            for chunk in buf[..size].chunks_exact(2) {
                                let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
                                cw_samples.push(sample as f32 / 32768.0);
                            }
                            if cw_samples.len() >= 4800 {
                                let block: Vec<f32> = cw_samples.drain(..4800).collect();
                                if let Some(decoder) = cw_decoder_arc.lock().unwrap().as_mut() {
                                    let result = decoder.process(&block);
                                    *cw_text_arc.lock().unwrap() = result.text;
                                    *cw_tone_hz_arc.lock().unwrap() = result.tone_hz;
                                    *cw_wpm_arc.lock().unwrap() = result.wpm;
                                    *cw_chars_arc.lock().unwrap() = result.chars;
                                    *cw_amp_arc.lock().unwrap() = result.amp_smooth;
                                }
                            }
                        }

                        if let Some(ref audio_buffer) = audio_buffer_opt {
                            let mut f32_samples = Vec::with_capacity(size / 2);
                            for chunk in buf[..size].chunks_exact(2) {
                                let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
                                f32_samples.push(sample as f32 / 32768.0);
                            }

                            let resampled = if let Some(ref mut r) = resampler {
                                let mut left = Vec::with_capacity(f32_samples.len() / 2);
                                let mut right = Vec::with_capacity(f32_samples.len() / 2);
                                for chunk in f32_samples.chunks_exact(2) {
                                    left.push(chunk[0]);
                                    right.push(chunk[1]);
                                }
                                let waves_in = vec![left, right];

                                match r.process(&waves_in, None) {
                                    Ok(waves_out) => {
                                        let mut out = Vec::with_capacity(waves_out[0].len() * 2);
                                        for i in 0..waves_out[0].len() {
                                            out.push(waves_out[0][i]);
                                            out.push(waves_out[1][i]);
                                        }
                                        out
                                    }
                                    Err(_) => f32_samples,
                                }
                            } else {
                                f32_samples
                            };

                            let vol = (*volume_arc.lock().unwrap() / 100.0).clamp(0.0, 1.0);
                            let resampled: Vec<f32> = resampled.iter().map(|s| s * vol).collect();

                            if let Ok(mut audio_buf) = audio_buffer.lock() {
                                audio_buf.extend_from_slice(&resampled);
                                let max_len = (actual_rate as usize) * 2;
                                if audio_buf.len() > max_len {
                                    let excess = audio_buf.len() - max_len;
                                    audio_buf.drain(..excess);
                                }
                            }
                        }
                    }
                    Err(_) => {
                        if last_udp_packet.elapsed() > Duration::from_secs(1) {
                            let mut acc = acc_secs.lock().unwrap();
                            let mut act = active_since.lock().unwrap();
                            if let Some(start) = act.take() { *acc += start.elapsed().as_secs(); }
                        }
                    }
                }
            }
        });
        self.udp_reader_thread = Some(handle);
    }

    fn stop_audio_pipeline(&mut self) {
        self.udp_reader_stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.udp_reader_thread.take() {
            let _ = handle.join();
        }
        self.cpal_stream = None;
    }

    fn start_recording(&mut self) {
        if self.is_recording { return; }

        let now = chrono::Local::now();
        let freq_hz = read_frequency();
        self.recording_start_freq = freq_hz;

        let freq_str = if freq_hz > 0 {
            let mhz = freq_hz as f64 / 1_000_000.0;
            let s = format!("{:.5}", mhz);
            if s.len() >= 2 {
                let split_pos = s.len() - 2;
                format!("_{}.{}MHz", &s[..split_pos], &s[split_pos..])
            } else {
                format!("_{}MHz", s)
            }
        } else { "".to_string() };

        let tone_val = {
            let t = self.tone.lock().unwrap().clone();
            if t == "---" || t.is_empty() {
                self.last_analog_tone.lock().unwrap().clone()
            } else {
                t
            }
        };
        let mode_suffix = analog::recording_suffix(&self.mode, &tone_val);
        let base_name = format!("record_{}{}{}", now.format("%Y-%m-%d_%H-%M-%S"), freq_str, mode_suffix);
        let wav_path = recordings_dir().join(format!("{}.wav", base_name));
        let mp3_path = recordings_dir().join(format!("{}.mp3", base_name));

        let sample_rate = if self.mode == "analog" || self.mode == "cw" { 48000 } else { 8000 };
        let channels = if self.mode == "analog" || self.mode == "cw" { 1 } else { 2 };
        match hound::WavWriter::create(&wav_path, hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        }) {
            Ok(writer) => {
                *self.wav_writer.lock().unwrap() = Some(writer);
                *self.recording_accumulated_secs.lock().unwrap() = 0;
                *self.recording_active_since.lock().unwrap() = None;
                *self.last_carrier_time.lock().unwrap() = None;
                self.recording_wav_path = Some(wav_path);
                self.recording_mp3_path = Some(mp3_path);
                self.is_recording = true;
            }
            Err(e) => {
                eprintln!("Не удалось создать WAV: {}", e);
                self.is_recording = false;
            }
        }
    }

    fn stop_recording(&mut self) {
        if !self.is_recording { return; }

        if let Some(active_since) = self.recording_active_since.lock().unwrap().take() {
            *self.recording_accumulated_secs.lock().unwrap() += active_since.elapsed().as_secs();
        }

        if let Ok(mut writer_opt) = self.wav_writer.lock() {
            if let Some(writer) = writer_opt.take() { let _ = writer.finalize(); }
        }

        self.is_recording = false;

        if let (Some(wav_path), Some(mp3_path)) = (self.recording_wav_path.take(), self.recording_mp3_path.take()) {
            if wav_path.exists() {
                let mut cmd = Command::new(base_dir().join("lame.exe"));
                cmd.args(&["-b", "128", "--silent", wav_path.to_str().unwrap(), mp3_path.to_str().unwrap()]);
                cmd.current_dir(base_dir());
                cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
                let _ = cmd.spawn().and_then(|mut child| child.wait());
                let _ = std::fs::remove_file(&wav_path);
            }
        }
    }

    fn save_and_exit(&mut self) {
        self.stop_sync_thread();
        self.stop_recording();
        self.stop_audio_pipeline();
        self.parser_stop.store(true, Ordering::SeqCst);
        kill_all_processes(&mut self.tci_bridge_process, &mut self.decoder_process);
        let state = SavedState {
            tci_port: self.tci_port.clone(), volume: self.volume, dmr_only: self.dmr_only,
            dmr_key: self.dmr_key.clone(), key_type: self.key_type.clone(), always_on_top: self.always_on_top,
            window_pos: self.window_pos, sync_with_sdr: self.sync_with_sdr,
            auto_start: self.auto_start, mode: self.mode.clone(),
            cw_auto_tone: self.cw_view.auto_tone,
            cw_manual_tone: self.cw_view.manual_tone,
            cw_manual_wpm: self.cw_view.manual_wpm,
            cw_gain_auto: self.cw_view.gain_auto,
            cw_gain_manual: self.cw_view.gain_manual,
        };
        save_settings(&state);
        std::process::exit(0);
    }
}

fn kill_all_processes(bridge: &mut Option<Child>, decoder: &mut Option<Child>) {
    if let Some(mut child) = decoder.take() { let _ = child.kill(); let _ = child.wait(); }
    if let Some(mut child) = bridge.take() { let _ = child.kill(); let _ = child.wait(); }
    let _ = Command::new("taskkill").args(&["/F", "/IM", "decoder.exe"]).creation_flags(CREATE_NO_WINDOW).output();
    let _ = Command::new("taskkill").args(&["/F", "/IM", "bridge.exe"]).creation_flags(CREATE_NO_WINDOW).output();
    let _ = Command::new("taskkill").args(&["/F", "/IM", "dsd-neo.exe"]).creation_flags(CREATE_NO_WINDOW).output();
    let _ = Command::new("taskkill").args(&["/F", "/IM", "tci-bridge.exe"]).creation_flags(CREATE_NO_WINDOW).output();
}

fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_escape = false;
    for c in s.chars() {
        if c == '\x1b' { in_escape = true; }
        else if in_escape { if c == 'm' { in_escape = false; } }
        else { result.push(c); }
    }
    result
}

fn draw_led_meter(ui: &mut egui::Ui, level: f32) {
    let num_segments = 26;
    let segment_width = 6.0;
    let segment_height = 24.0;
    let spacing = 1.0;
    let total_width = num_segments as f32 * (segment_width + spacing);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(total_width, segment_height), egui::Sense::hover());
    let painter = ui.painter();
    let active_segments = (level * num_segments as f32).round() as usize;
    for i in 0..num_segments {
        let x = rect.min.x + i as f32 * (segment_width + spacing);
        let seg_rect = egui::Rect::from_min_size(egui::pos2(x, rect.min.y), egui::vec2(segment_width, segment_height));
        let color = if i < active_segments { egui::Color32::GREEN } else { egui::Color32::from_gray(40) };
        painter.rect_filled(seg_rect, 2.0, color);
    }
}

fn format_duration_since(instant: Option<Instant>) -> String {
    if let Some(t) = instant {
        let secs = t.elapsed().as_secs();
        if secs < 60 { format!("{} сек", secs) }
        else { format!("{} мин {} сек", secs / 60, secs % 60) }
    } else { "---".to_string() }
}

impl eframe::App for App {
    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        let state = SavedState {
            tci_port: self.tci_port.clone(), volume: self.volume, dmr_only: self.dmr_only,
            dmr_key: self.dmr_key.clone(), key_type: self.key_type.clone(), always_on_top: self.always_on_top,
            window_pos: self.window_pos, sync_with_sdr: self.sync_with_sdr,
            auto_start: self.auto_start, mode: self.mode.clone(),
            cw_auto_tone: self.cw_view.auto_tone,
            cw_manual_tone: self.cw_view.manual_tone,
            cw_manual_wpm: self.cw_view.manual_wpm,
            cw_gain_auto: self.cw_view.gain_auto,
            cw_gain_manual: self.cw_view.gain_manual,
        };
        save_settings(&state);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop_sync_thread();
        self.stop_recording();
        self.stop_audio_pipeline();
        let state = SavedState {
            tci_port: self.tci_port.clone(), volume: self.volume, dmr_only: self.dmr_only,
            dmr_key: self.dmr_key.clone(), key_type: self.key_type.clone(), always_on_top: self.always_on_top,
            window_pos: self.window_pos, sync_with_sdr: self.sync_with_sdr,
            auto_start: self.auto_start, mode: self.mode.clone(),
            cw_auto_tone: self.cw_view.auto_tone,
            cw_manual_tone: self.cw_view.manual_tone,
            cw_manual_wpm: self.cw_view.manual_wpm,
            cw_gain_auto: self.cw_view.gain_auto,
            cw_gain_manual: self.cw_view.gain_manual,
        };
        save_settings(&state);
        self.parser_stop.store(true, Ordering::SeqCst);
        kill_all_processes(&mut self.tci_bridge_process, &mut self.decoder_process);
        let _ = std::fs::remove_file(level_file());
        let _ = std::fs::remove_file(frequency_file());
        self.is_running = false;
        *self.sync_active.lock().unwrap() = false;
        *self.audio_level.lock().unwrap() = 0.0;
        *self.analog_udp_level.lock().unwrap() = 0.0;
        *self.analog_has_signal.lock().unwrap() = false;
        *self.analog_last_raw_signal.lock().unwrap() = None;
        *self.analog_display_until.lock().unwrap() = None;
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.hwnd_cached {
            if let Some(hwnd) = find_sundec_hwnd() {
                SUNDEC_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
                self.hwnd_cached = true;
            }
        }

        if self.sync_with_sdr && self.sync_thread.is_none() {
            self.start_sync_thread();
        }

        if self.auto_start && !self.auto_start_triggered {
            self.auto_start_triggered = true;
            std::thread::sleep(Duration::from_secs(2));
            if !self.is_running { self.do_start(); }
        }

        if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
            self.window_pos = Some((rect.min.x, rect.min.y));
        }

        *self.volume_shared.lock().unwrap() = self.volume;

        let sync_on = *self.sync_active.lock().unwrap();
        let last_sync = *self.last_sync_time.lock().unwrap();

        if self.mode == "cw" {
            let current_freq = read_frequency();
            if self.cw_view.last_clear_freq > 0 && current_freq > 0 {
                let diff = if current_freq > self.cw_view.last_clear_freq {
                    current_freq - self.cw_view.last_clear_freq
                } else {
                    self.cw_view.last_clear_freq - current_freq
                };
                if diff > 1_000 {
                    self.cw_view.clear_text();
                    if let Some(dec) = self.cw_view.decoder.lock().unwrap().as_mut() {
                        dec.clear();
                    }
                }
            }
            self.cw_view.last_clear_freq = current_freq;
        }

        let has_signal_now = if self.mode == "analog" {
            let sql_open = read_audio_level() > ANALOG_SQL_THRESHOLD;
            if !sql_open {
                let last_raw = *self.analog_last_raw_signal.lock().unwrap();
                let within_hold = last_raw.map(|t| t.elapsed() < Duration::from_secs(ANALOG_TONE_HOLD_SECS)).unwrap_or(false);
                if !within_hold {
                    *self.tone.lock().unwrap() = "---".to_string();
                }
            }
            *self.analog_has_signal.lock().unwrap() = sql_open;
            sql_open
        } else if self.mode == "cw" {
            self.cw_view.decoder.lock().unwrap()
                .as_ref()
                .map(|d| d.result().signal_on)
                .unwrap_or(false)
        } else {
            self.is_running && sync_on && last_sync.map(|t| t.elapsed() < Duration::from_secs(2)).unwrap_or(false)
        };

        if self.mode == "analog" || self.mode == "cw" {
            if has_signal_now {
                *self.last_analog_level.lock().unwrap() = *self.audio_level.lock().unwrap();
                *self.analog_display_until.lock().unwrap() = Some(Instant::now() + Duration::from_secs(ANALOG_DISPLAY_HOLD_SECS));
            }
        }

        let display_hold = if self.mode == "analog" || self.mode == "cw" {
            self.analog_display_until.lock().unwrap()
                .map(|t| Instant::now() < t)
                .unwrap_or(false)
        } else { false };

        if self.is_recording {
            let current_freq = read_frequency();
            let diff = if current_freq > self.recording_start_freq { current_freq - self.recording_start_freq } else { self.recording_start_freq - current_freq };
            if diff > 10000 && current_freq > 0 && self.recording_start_freq > 0 {
                self.stop_recording();
                self.start_recording();
            }
        }

        if self.is_running {
            let tci_port_ok = std::net::TcpStream::connect_timeout(
                &format!("127.0.0.1:{}", self.tci_port).parse().unwrap(),
                std::time::Duration::from_millis(200),
            ).is_ok();
            let mut need_restart = !tci_port_ok;
            if let Some(child) = &mut self.tci_bridge_process {
                match child.try_wait() {
                    Ok(Some(_)) => need_restart = true,
                    Ok(None) => {},
                    Err(_) => need_restart = true,
                }
            } else { need_restart = true; }
            if need_restart && self.mode != "cw" {
                kill_all_processes(&mut self.tci_bridge_process, &mut self.decoder_process);
                let bridge_child = Command::new(base_dir().join("bridge.exe"))
                    .arg(&self.tci_port).arg("7355").arg(&self.mode)
                    .current_dir(base_dir())
                    .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
                    .spawn().expect("Failed to restart bridge.exe");
                self.tci_bridge_process = Some(bridge_child);
                let log_file = log_file_path();
                let _ = std::fs::remove_file(&log_file);
                let mut base_args = if self.mode == "analog" {
                    "-i tcp:127.0.0.1:7355 -o udp:127.0.0.1:23456 --frontend none --api 9911 -fA".to_string()
                } else if self.mode == "dmr" {
                    "-i tcp:127.0.0.1:7355 -o udp:127.0.0.1:23456 --frontend none --api 9911 -fs".to_string()
                } else {
                    "-i tcp:127.0.0.1:7355 -o udp:127.0.0.1:23456 --frontend none --api 9911".to_string()
                };
                if self.key_type == "hytera" && !self.dmr_key.is_empty() { base_args.push_str(&format!(" -H {}", self.dmr_key)); }
                else if self.key_type == "basic" && !self.dmr_key.is_empty() { base_args.push_str(&format!(" -b {}", self.dmr_key)); }
                let log_file_handle = std::fs::File::create(&log_file).expect("Не удалось создать лог");
                let args_vec: Vec<&str> = base_args.split_whitespace().collect();
                let decoder_child = Command::new(base_dir().join("decoder.exe"))
                    .args(&args_vec)
                    .current_dir(base_dir())
                    .env("DSD_NEO_TCP_PREBUF_MS", "2000")
                    .env("DSD_NEO_SNR_SQL_DB", "10")
                    .stderr(std::process::Stdio::from(log_file_handle))
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn().expect("Failed to restart decoder.exe");
                self.decoder_process = Some(decoder_child);
                *self.sync_active.lock().unwrap() = false;
                self.restart_until = Some(Instant::now() + Duration::from_secs(2));
                if self.cpal_stream.is_some() { self.start_audio_pipeline(); }
            }
        }

        let show_restart_msg = if let Some(until) = self.restart_until {
            if Instant::now() < until { true } else { self.restart_until = None; false }
        } else { false };

        if self.is_running {
            let raw = read_audio_level();
            let mut current = self.audio_level.lock().unwrap();
            if has_signal_now && raw > 0.02 { *current = (*current + (1.0 - *current) * 0.04).clamp(0.0, 1.0); }
            else { *current = (*current * 0.97).clamp(0.0, 1.0); }
        } else { *self.audio_level.lock().unwrap() = 0.0; }

        if self.show_cipher_window {
            egui::Window::new("Настройки шифрования")
                .collapsible(false).resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Тип ключа:");
                        egui::ComboBox::from_id_source("key_type_modal")
                            .selected_text(match self.key_type.as_str() {
                                "hytera" => "Hytera: Basic (10/32/64) или AES (128/256)",
                                "basic" => "Motorola: Basic Privacy (1-255)",
                                _ => "Нет ключа",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.key_type, "none".to_string(), "Нет ключа");
                                ui.selectable_value(&mut self.key_type, "hytera".to_string(), "Hytera: Basic (10/32/64) или AES (128/256)");
                                ui.selectable_value(&mut self.key_type, "basic".to_string(), "Motorola: Basic Privacy (1-255)");
                            });
                    });
                    ui.add_space(5.0);
                    ui.horizontal(|ui| {
                        ui.label("Ключ:");
                        ui.add_sized([320.0, 20.0], egui::TextEdit::singleline(&mut self.dmr_key));
                    });
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("Применить").clicked() {
                            if self.is_running && (self.mode == "dmr" || self.mode == "auto") {
                                self.stop_recording();
                                self.stop_audio_pipeline();
                                self.parser_stop.store(true, Ordering::SeqCst);
                                kill_all_processes(&mut self.tci_bridge_process, &mut self.decoder_process);
                                self.do_start();
                            }
                        }
                        if ui.button("Закрыть").clicked() {
                            self.show_cipher_window = false;
                        }
                    });
                });
        }

        if self.show_about_window {
            egui::Window::new("О программе")
                .collapsible(false).resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.heading(format!("R2GFK SunDEC v{}", env!("CARGO_PKG_VERSION")));
                    ui.add_space(5.0);
                    ui.label("Программа для декодирования радиорежимов (DMR, аналог FM, CW) через TCI.");
                    ui.label("Работает в связке с ExpertSDR3 и DSD-neo.");
                    ui.add_space(10.0);

                    ui.label(egui::RichText::new("Наши компоненты (самописные):").strong());
                    ui.label("• R2GFK SunDEC (GUI) — интерфейс программы.");
                    ui.label("• TCI Bridge (tci-bridge) — мост между ExpertSDR3 и DSD-neo.");
                    ui.label("• CW-декодер (GUI и обвязка) — на основе cw-dit.");
                    ui.add_space(10.0);

                    ui.label(egui::RichText::new("Внешние компоненты:").strong());
                    ui.hyperlink_to("DSD-neo — декодер DMR/P25/NXDN/D-STAR/YSF/M17/dPMR/TETRA/EDACS/ProVoice", "https://github.com/arancormonk/dsd-neo");
                    ui.hyperlink_to("cw-dit — библиотека для CW-декодера (Goertzel, Morse)", "https://github.com/swilcox/cw-dit");
                    ui.hyperlink_to("eframe / egui — графический интерфейс", "https://github.com/emilk/egui");
                    ui.hyperlink_to("cpal — вывод звука в колонки", "https://github.com/RustAudio/cpal");
                    ui.hyperlink_to("rubato — ресемплинг аудио", "https://github.com/HEnquist/rubato");
                    ui.hyperlink_to("hound — запись WAV", "https://github.com/ruuda/hound");
                    ui.hyperlink_to("encoding_rs — корректная кодировка кириллицы", "https://github.com/hsivonen/encoding_rs");
                    ui.hyperlink_to("lame — конвертация WAV в MP3", "https://lame.sourceforge.net");

                    ui.add_space(10.0);
                    if ui.button("Закрыть").clicked() {
                        self.show_about_window = false;
                    }
                });
        }

        let _central_response = egui::CentralPanel::default().show(ctx, |ui| {
            let header_height = 60.0;
            let (header_rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), header_height), egui::Sense::hover());

            let btn_size = 20.0;
            let btn_gap = 4.0;
            let close_rect = egui::Rect::from_min_size(
                egui::pos2(header_rect.max.x - btn_size - 4.0, header_rect.min.y + 2.0),
                egui::vec2(btn_size, btn_size),
            );
            let minimize_rect = egui::Rect::from_min_size(
                egui::pos2(close_rect.min.x - btn_size - btn_gap, header_rect.min.y + 2.0),
                egui::vec2(btn_size, btn_size),
            );
            let settings_rect = egui::Rect::from_min_size(
                egui::pos2(header_rect.min.x + 4.0, header_rect.min.y + 2.0),
                egui::vec2(btn_size, btn_size),
            );
            let play_rect = if self.mode == "cw" {
                Some(egui::Rect::from_min_size(
                    egui::pos2(settings_rect.max.x + btn_gap, header_rect.min.y + 2.0),
                    egui::vec2(btn_size, btn_size),
                ))
            } else { None };
            let clear_rect = if self.mode == "cw" {
                Some(egui::Rect::from_min_size(
                    egui::pos2(settings_rect.min.x, settings_rect.max.y + 2.0),
                    egui::vec2(btn_size, btn_size),
                ))
            } else { None };
            let drag_rect = egui::Rect::from_min_size(
                header_rect.min,
                egui::vec2(header_rect.width() - (btn_size * 2.0 + btn_gap + 8.0), header_height),
            );

            let drag_response = ui.interact(drag_rect, egui::Id::new("header_drag"), egui::Sense::click_and_drag());
            if drag_response.drag_started() { ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag); }

            let mut header_ui = ui.child_ui(header_rect, egui::Layout::top_down(egui::Align::Center));
            header_ui.vertical_centered(|ui| {
                ui.add_space(4.0);
                ui.heading(egui::RichText::new("R2GFK SunDEC").size(28.0).strong());
                ui.label(egui::RichText::new("DMR, P25, NXDN, D-STAR, YSF, M17, dPMR, TETRA, EDACS, ProVoice").size(11.0));
            });

            let mut settings_ui = ui.child_ui(settings_rect, egui::Layout::centered_and_justified(egui::Direction::LeftToRight));
            let settings_btn = settings_ui.add_sized([btn_size, btn_size], egui::Button::new("⚙").small());
            if settings_btn.on_hover_text("Настройки").clicked() {
                self.show_context_menu = true;
                self.menu_just_opened = true;
            }

            if let Some(play_rect) = play_rect {
                let mut play_ui = ui.child_ui(play_rect, egui::Layout::centered_and_justified(egui::Direction::LeftToRight));
                let play_text = if self.is_running { "⏹" } else { "▶" };
                let play_tooltip = if self.is_running { "Остановить" } else { "Запустить" };
                let play_btn = play_ui.add_sized([btn_size, btn_size], egui::Button::new(play_text).small());
                if play_btn.on_hover_text(play_tooltip).clicked() {
                    if self.is_running {
                        self.parser_stop.store(true, Ordering::SeqCst);
                        self.stop_recording();
                        self.stop_audio_pipeline();
                        kill_all_processes(&mut self.tci_bridge_process, &mut self.decoder_process);
                        let _ = std::fs::remove_file(level_file());
                        let _ = std::fs::remove_file(frequency_file());
                        *self.sync_active.lock().unwrap() = false;
                        *self.analog_udp_level.lock().unwrap() = 0.0;
                        *self.analog_has_signal.lock().unwrap() = false;
                        *self.analog_last_raw_signal.lock().unwrap() = None;
                        *self.last_analog_freq.lock().unwrap() = 0;
                        *self.last_analog_tone.lock().unwrap() = "---".to_string();
                        *self.last_analog_time.lock().unwrap() = None;
                        *self.last_analog_level.lock().unwrap() = 0.0;
                        *self.analog_display_until.lock().unwrap() = None;
                        *self.error_count.lock().unwrap() = 0;
                        *self.last_sync_time.lock().unwrap() = None;
                        *self.audio_level.lock().unwrap() = 0.0;
                        *self.last_src.lock().unwrap() = "---".to_string();
                        *self.last_src_time.lock().unwrap() = None;
                        *self.tone.lock().unwrap() = "---".to_string();
                        self.cw_view.reset();
                        self.is_running = false;
                    } else {
                        self.do_start();
                    }
                }
            }

            if let Some(cr) = clear_rect {
                let mut clear_ui = ui.child_ui(cr, egui::Layout::centered_and_justified(egui::Direction::LeftToRight));
                let clear_btn = clear_ui.add_sized([btn_size, btn_size], egui::Button::new("🗑").small());
                if clear_btn.on_hover_text("Очистить окно CW").clicked() {
                    self.cw_view.clear_text();
                    if let Some(dec) = self.cw_view.decoder.lock().unwrap().as_mut() {
                        dec.clear();
                    }
                }
            }

            let mut minimize_ui = ui.child_ui(minimize_rect, egui::Layout::centered_and_justified(egui::Direction::LeftToRight));
            if minimize_ui.add_sized([btn_size, btn_size], egui::Button::new("—").small()).clicked() { sundec_minimize(); }

            let mut close_ui = ui.child_ui(close_rect, egui::Layout::centered_and_justified(egui::Direction::LeftToRight));
            if close_ui.add_sized([btn_size, btn_size], egui::Button::new("❌").small()).clicked() { self.save_and_exit(); }

            ui.add_space(5.0);
            ui.add_sized([ui.available_width(), 1.0], egui::Separator::default());
            ui.add_space(2.0);

            if show_restart_msg {
                egui::Area::new("restart_banner".into()).order(egui::Order::Foreground)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0f32, -100.0])
                    .show(ctx, |ui| {
                        egui::Frame::none().fill(egui::Color32::from_rgb(40, 40, 40))
                            .stroke(egui::Stroke::new(1.0_f32, egui::Color32::RED))
                            .rounding(6.0f32).inner_margin(egui::Margin::symmetric(12.0f32, 6.0))
                            .show(ui, |ui| {
                                ui.label(egui::RichText::new("⚠ Связь с TCI потеряна.").color(egui::Color32::RED).size(14.0).strong());
                            });
                    });
            }

            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.vertical(|ui| {
                    if self.mode != "analog" && self.mode != "cw" {
                        ui.horizontal(|ui| {
                            ui.label("Громкость:");
                            let _ = ui.add_sized([100.0, 20.0], egui::Slider::new(&mut self.volume, 0.0..=100.0).show_value(false));
                            ui.add_sized([40.0, 20.0], egui::Label::new(format!("{:.0}%", self.volume)));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.add_space(10.0);
                                let freq_display = if self.is_running { format_frequency(read_frequency()) } else { "".to_string() };
                                ui.label(egui::RichText::new(freq_display).size(18.0).strong().color(egui::Color32::from_rgb(200, 200, 200)));
                            });
                        });
                        ui.add_space(2.0);
                    }

                    ui.horizontal(|ui| {
                        let old_mode = self.mode.clone();
                        ui.radio_value(&mut self.mode, "dmr".to_string(), "Только DMR");
                        ui.add_space(10.0);
                        ui.radio_value(&mut self.mode, "auto".to_string(), "Все виды (Авто)");
                        ui.add_space(10.0);
                        ui.radio_value(&mut self.mode, "analog".to_string(), "Аналог (FM)");
                        ui.add_space(10.0);
                        ui.radio_value(&mut self.mode, "cw".to_string(), "CW");
                        if old_mode != self.mode && self.is_running { self.do_start(); }
                    });
                });
            });

            ui.add_space(5.0);

            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let tci_status = if !self.is_running {
                        (egui::Color32::from_gray(150), "TCI")
                    } else {
                        let port_ok = std::net::TcpStream::connect_timeout(
                            &format!("127.0.0.1:{}", self.tci_port).parse().unwrap(),
                            std::time::Duration::from_millis(200),
                        ).is_ok();
                        let process_ok = if let Some(child) = &mut self.tci_bridge_process { matches!(child.try_wait(), Ok(None)) } else { false };
                        if port_ok && process_ok { (egui::Color32::from_rgb(0, 150, 255), "TCI") } else { (egui::Color32::RED, "TCI") }
                    };
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 8.0, tci_status.0);
                    ui.add_space(5.0);
                    ui.label(egui::RichText::new(tci_status.1).color(tci_status.0).size(13.0));

                    let center_offset = (ui.available_width() * 0.25).max(40.0);
                    ui.add_space(center_offset);

                    let sync_label = if self.mode == "cw" { "Сигнал" } else { "Несущая" };
                    let sync_status = if !self.is_running {
                        (egui::Color32::from_gray(150), "Ожидание")
                    } else if has_signal_now {
                        (egui::Color32::from_rgb(0, 255, 0), sync_label)
                    } else {
                        (egui::Color32::from_gray(150), "Ожидание")
                    };
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 8.0, sync_status.0);
                    ui.add_space(5.0);
                    ui.label(egui::RichText::new(sync_status.1).color(sync_status.0).size(13.0));

                    if self.mode != "analog" && self.mode != "cw" {
                        ui.add_space(20.0);
                        let errs = *self.error_count.lock().unwrap();
                        let err_color = if errs > 0 { egui::Color32::from_rgb(255, 200, 0) } else { egui::Color32::from_gray(150) };
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                        ui.painter().circle_filled(rect.center(), 8.0, err_color);
                        ui.add_space(5.0);
                        ui.label(if errs > 0 {
                            egui::RichText::new(format!("Потери: {}", errs)).color(egui::Color32::from_rgb(255, 200, 0)).size(13.0)
                        } else {
                            egui::RichText::new("Потери").color(egui::Color32::from_gray(150)).size(13.0)
                        });
                    } else {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_space(10.0);
                            let freq_display = if self.is_running { format_frequency(read_frequency()) } else { "".to_string() };
                            ui.label(egui::RichText::new(freq_display).size(18.0).strong().color(egui::Color32::from_rgb(200, 200, 200)));
                        });
                    }
                });
            });

            ui.add_space(5.0);

            if self.mode != "cw" {
                ui.horizontal(|ui| {
                    let btn_text = if self.is_running { "⏹ Остановить" } else { "▶ Запустить" };
                    if ui.add_sized([90.0, 24.0], egui::Button::new(btn_text)).clicked() {
                        if self.is_running {
                            self.parser_stop.store(true, Ordering::SeqCst);
                            self.stop_recording();
                            self.stop_audio_pipeline();
                            kill_all_processes(&mut self.tci_bridge_process, &mut self.decoder_process);
                            let _ = std::fs::remove_file(level_file());
                            let _ = std::fs::remove_file(frequency_file());
                            *self.sync_active.lock().unwrap() = false;
                            *self.analog_udp_level.lock().unwrap() = 0.0;
                            *self.analog_has_signal.lock().unwrap() = false;
                            *self.analog_last_raw_signal.lock().unwrap() = None;
                            *self.last_analog_freq.lock().unwrap() = 0;
                            *self.last_analog_tone.lock().unwrap() = "---".to_string();
                            *self.last_analog_time.lock().unwrap() = None;
                            *self.last_analog_level.lock().unwrap() = 0.0;
                            *self.analog_display_until.lock().unwrap() = None;
                            *self.error_count.lock().unwrap() = 0;
                            *self.last_sync_time.lock().unwrap() = None;
                            *self.audio_level.lock().unwrap() = 0.0;
                            *self.last_src.lock().unwrap() = "---".to_string();
                            *self.last_src_time.lock().unwrap() = None;
                            *self.tone.lock().unwrap() = "---".to_string();
                            self.is_running = false;
                        } else {
                            self.do_start();
                        }
                    }

                    ui.add_space(5.0);

                    let can_record = self.is_running || self.is_recording;
                    let (rec_text, rec_color) = if self.is_recording {
                        let acc = *self.recording_accumulated_secs.lock().unwrap();
                        let act = *self.recording_active_since.lock().unwrap();
                        let total_secs = acc + act.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                        if has_signal_now {
                            let blink_on = act.map(|t| (t.elapsed().as_millis() / 500) % 2 == 0).unwrap_or(true);
                            let dot = if blink_on { "🔴" } else { "⚫" };
                            (format!("{} {:02}:{:02}", dot, total_secs / 60, total_secs % 60), egui::Color32::LIGHT_RED)
                        } else {
                            (format!("🔴 {:02}:{:02}", total_secs / 60, total_secs % 60), egui::Color32::LIGHT_RED)
                        }
                    } else {
                        ("⏺ Запись".to_string(), ui.style().visuals.widgets.inactive.fg_stroke.color)
                    };
                    let rec_button = ui.add_enabled(
                        can_record,
                        egui::Button::new(egui::RichText::new(rec_text).color(rec_color)).min_size(egui::vec2(90.0, 24.0)),
                    );
                    if rec_button.clicked() {
                        if self.is_recording { self.stop_recording(); } else { self.start_recording(); }
                    }

                    ui.add_space(15.0);
                    let meter_width = 120.0;
                    let meter_x = ui.cursor().min.x;
                    let meter_y = ui.cursor().min.y;
                    ui.allocate_ui_at_rect(egui::Rect::from_min_size(egui::pos2(meter_x, meter_y), egui::vec2(meter_width, 24.0)),
                        |ui| { let level = *self.audio_level.lock().unwrap(); draw_led_meter(ui, level); });
                });

                ui.add_space(2.0);

                let status_height = 24.0;
                let (status_rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), status_height),
                    egui::Sense::hover(),
                );

                let mut status_ui = ui.child_ui(
                    status_rect,
                    egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                );

                if self.mode == "analog" {
                    let last_time = *self.last_analog_time.lock().unwrap();
                    if let Some(t) = last_time {
                        let freq = format_frequency(*self.last_analog_freq.lock().unwrap());
                        let tone = self.last_analog_tone.lock().unwrap().clone();
                        let tone_str = if tone != "---" && !tone.is_empty() { tone.replace("Hz", "Гц") } else { "".to_string() };
                        let timer_str = format_duration_since(Some(t));

                        let mut job = egui::text::LayoutJob::default();
                        job.append(&freq, 0.0, egui::TextFormat {
                            font_id: egui::FontId::proportional(14.0),
                            color: egui::Color32::LIGHT_BLUE,
                            ..Default::default()
                        });
                        if !tone_str.is_empty() {
                            job.append(" | ", 0.0, egui::TextFormat {
                                font_id: egui::FontId::proportional(14.0),
                                color: egui::Color32::from_gray(150),
                                ..Default::default()
                            });
                            job.append(&tone_str, 0.0, egui::TextFormat {
                                font_id: egui::FontId::proportional(14.0),
                                color: egui::Color32::from_rgb(255, 105, 180),
                                ..Default::default()
                            });
                        }
                        job.append(&format!(" ({} назад)", timer_str), 0.0, egui::TextFormat {
                            font_id: egui::FontId::proportional(14.0),
                            color: egui::Color32::LIGHT_BLUE,
                            ..Default::default()
                        });
                        status_ui.label(job);
                    }
                } else {
                    let last_src_val = self.last_src.lock().unwrap().clone();
                    if last_src_val != "---" {
                        let last_time = *self.last_src_time.lock().unwrap();
                        let timer_str = format_duration_since(last_time);

                        let mut job = egui::text::LayoutJob::default();
                        job.append("Последний принятый ID: ", 0.0, egui::TextFormat {
                            font_id: egui::FontId::proportional(14.0),
                            color: egui::Color32::LIGHT_BLUE,
                            ..Default::default()
                        });
                        job.append(&last_src_val, 0.0, egui::TextFormat {
                            font_id: egui::FontId::proportional(14.0),
                            color: egui::Color32::from_rgb(255, 105, 180),
                            ..Default::default()
                        });
                        job.append(&format!(" ({} назад)", timer_str), 0.0, egui::TextFormat {
                            font_id: egui::FontId::proportional(14.0),
                            color: egui::Color32::LIGHT_BLUE,
                            ..Default::default()
                        });
                        status_ui.label(job);
                    }
                }

                ui.add_space(2.0);
                ui.add_sized([ui.available_width(), 1.0], egui::Separator::default());
                ui.add_space(2.0);
            }

            if self.mode == "cw" {
                ui.add_space(5.0);
                self.cw_view.draw_controls(ui);
                ui.add_space(5.0);
                ui.add_sized([ui.available_width(), 1.0], egui::Separator::default());
                ui.add_space(5.0);
            }

            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                if self.mode == "analog" {
                    let tone = self.last_analog_tone.lock().unwrap().clone();
                    let freq = format_frequency(*self.last_analog_freq.lock().unwrap());
                    let level = *self.last_analog_level.lock().unwrap();
                    let modulation = read_modulation();
                    analog::draw_analog_fields(ui, &tone, &freq, has_signal_now, level, display_hold, &modulation);
                } else if self.mode == "cw" {
                    self.cw_view.draw_text(ui, has_signal_now);
                } else {
                    // --- ДВЕ КОЛОНКИ ДЛЯ DMR ---
                    let alias = self.dmr_alias.lock().unwrap().clone();
                    let gps_raw = self.dmr_gps.lock().unwrap().clone();
                    let txt = self.dmr_text.lock().unwrap().clone();
                    let call_type = self.dmr_call_type.lock().unwrap().clone();
                    let enc_type = self.dmr_encrypted.lock().unwrap().clone();

                    let (lat, lon) = if gps_raw.is_empty() {
                        ("---".to_string(), "---".to_string())
                    } else {
                        let mut lat_s = "---".to_string();
                        let mut lon_s = "---".to_string();
                        if let Some(start_paren) = gps_raw.find('(') {
                            if let Some(end_paren) = gps_raw[start_paren..].find(')') {
                                let inside = &gps_raw[start_paren+1..start_paren+end_paren];
                                let parts: Vec<&str> = inside.split(',').map(|s| s.trim()).collect();
                                if parts.len() == 2 {
                                    lat_s = parts[0].to_string();
                                    lon_s = parts[1].to_string();
                                }
                            }
                        }
                        if lat_s == "---" || lon_s == "---" {
                            if let Some(lat_start) = gps_raw.find("Lat:") {
                                if let Some(lat_end) = gps_raw[lat_start..].find(|c| c == ' ' || c == '°' || c == '(') {
                                    lat_s = gps_raw[lat_start+4..lat_start+lat_end].trim().to_string();
                                }
                            }
                            if let Some(lon_start) = gps_raw.find("Lon:") {
                                if let Some(lon_end) = gps_raw[lon_start..].find(|c| c == ' ' || c == '°' || c == '(') {
                                    lon_s = gps_raw[lon_start+4..lon_start+lon_end].trim().to_string();
                                }
                            }
                        }
                        (lat_s, lon_s)
                    };

                    let alias_val = if alias.is_empty() { "---".to_string() } else { alias };
                    let type_val = if call_type.is_empty() { "---".to_string() } else { call_type };
                    let txt_val = if txt.is_empty() { "---".to_string() } else { txt };
                    
                    let alias_color = if alias_val == "---" { egui::Color32::GRAY } else { egui::Color32::LIGHT_GREEN };
                    let lat_color = if lat == "---" { egui::Color32::GRAY } else { egui::Color32::LIGHT_BLUE };
                    let lon_color = if lon == "---" { egui::Color32::GRAY } else { egui::Color32::LIGHT_BLUE };
                    let enc_color = if enc_type == "---" { egui::Color32::GRAY } else { egui::Color32::RED };
                    let txt_color = if txt_val == "---" { egui::Color32::GRAY } else { egui::Color32::YELLOW };

                    let font_size = 15.0;
                    let key_color = egui::Color32::GRAY;

                    ui.columns(2, |cols| {
                        egui::Grid::new("left_grid").num_columns(2).spacing([10.0, 2.0]).show(&mut cols[0], |ui| {
                            ui.label(egui::RichText::new("Протокол:").size(font_size).color(key_color));
                            ui.label(egui::RichText::new(format!("{}", *self.protocol.lock().unwrap())).size(font_size).strong());
                            ui.end_row();

                            ui.label(egui::RichText::new("Цвет. код:").size(font_size).color(key_color));
                            ui.label(egui::RichText::new(format!("{}", *self.dmr_cc.lock().unwrap())).size(font_size).strong());
                            ui.end_row();

                            ui.label(egui::RichText::new("Таймслот:").size(font_size).color(key_color));
                            ui.label(egui::RichText::new(format!("{}", *self.dmr_slot.lock().unwrap())).size(font_size).strong());
                            ui.end_row();

                            ui.label(egui::RichText::new("Группа:").size(font_size).color(key_color));
                            ui.label(egui::RichText::new(format!("{}", *self.dmr_tg.lock().unwrap())).size(font_size).strong());
                            ui.end_row();

                            ui.label(egui::RichText::new("Радио ID:").size(font_size).color(key_color));
                            ui.label(egui::RichText::new(format!("{}", *self.dmr_src.lock().unwrap())).size(font_size).strong());
                            ui.end_row();
                        });

                        egui::Grid::new("right_grid").num_columns(2).spacing([10.0, 2.0]).show(&mut cols[1], |ui| {
                            ui.label(egui::RichText::new("Алиас:").size(font_size).color(key_color));
                            ui.add(egui::Label::new(egui::RichText::new(&alias_val).size(font_size).color(alias_color)).wrap(true));
                            ui.end_row();

                            ui.label(egui::RichText::new("Широта:").size(font_size).color(key_color));
                            ui.label(egui::RichText::new(&lat).size(font_size).color(lat_color));
                            ui.end_row();

                            ui.label(egui::RichText::new("Долгота:").size(font_size).color(key_color));
                            ui.label(egui::RichText::new(&lon).size(font_size).color(lon_color));
                            ui.end_row();

                            ui.label(egui::RichText::new("Тип:").size(font_size).color(key_color));
                            ui.label(egui::RichText::new(&type_val).size(font_size));
                            ui.end_row();

                            ui.label(egui::RichText::new("Шифр.:").size(font_size).color(key_color));
                            ui.label(egui::RichText::new(&enc_type).size(font_size).color(enc_color).strong());
                            ui.end_row();
                        });
                    });

                    // Прижимаем строку Сообщение к низу с помощью обычного add_space
                    let available = ui.available_height();
                    if available > 40.0 {
                        ui.add_space(available - 40.0);
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Сообщение:").size(font_size).color(key_color));
                        ui.add(egui::Label::new(egui::RichText::new(&txt_val).size(font_size).color(txt_color)).wrap(true));
                    });
                }
            });
            
            // Заполняем оставшееся пространство
            ui.allocate_space(ui.available_size());
        });

        if self.show_context_menu {
            let mut close = false;
            let window_response = egui::Window::new("menu_window")
                .title_bar(false).resizable(false).collapsible(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_min_width(280.0);
                    ui.label(egui::RichText::new(format!("R2GFK SunDEC v{}", env!("CARGO_PKG_VERSION"))).size(12.0).color(egui::Color32::GRAY));
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label("Порт TCI:");
                        ui.add_sized([80.0, 20.0], egui::TextEdit::singleline(&mut self.tci_port));
                    });
                    ui.separator();
                    let mut new_value = self.always_on_top;
                    if ui.checkbox(&mut new_value, "Поверх всего").changed() {
                        self.always_on_top = new_value;
                        if self.always_on_top { sundec_set_topmost(); } else { sundec_set_normal(); }
                        close = true;
                    }
                    let mut sync = self.sync_with_sdr;
                    if ui.checkbox(&mut sync, "Прилипнуть к ExpertSDR3").changed() {
                        self.sync_with_sdr = sync;
                        if !sync { self.stop_sync_thread(); }
                    }
                    let mut auto = self.auto_start;
                    if ui.checkbox(&mut auto, "Автозапуск при старте").changed() { self.auto_start = auto; }
                    if ui.button("🔑 Настройки ключа").clicked() {
                        self.show_cipher_window = true;
                        close = true;
                    }
                    if ui.button("📋 Копировать CW").clicked() {
                        let text_to_copy = self.cw_view.text.lock().unwrap().clone();
                        ui.ctx().copy_text(text_to_copy);
                        close = true;
                    }
                    if ui.button("📁 Открыть папку с записями").clicked() {
                        let dir = recordings_dir();
                        let _ = Command::new("explorer.exe").arg(dir.to_str().unwrap()).creation_flags(CREATE_NO_WINDOW).spawn();
                        close = true;
                    }
                    ui.separator();
                    if ui.button("ℹ О программе").clicked() {
                        self.show_about_window = true;
                        close = true;
                    }
                });
            if let Some(resp) = window_response {
                let menu_rect = resp.response.rect;
                if ctx.input(|i| i.pointer.primary_clicked()) {
                    if let Some(pos) = ctx.input(|i| i.pointer.interact_pos()) {
                        if !menu_rect.contains(pos) && !self.menu_just_opened { close = true; }
                    }
                }
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) { close = true; }
            if close { self.show_context_menu = false; }
            self.menu_just_opened = false;
        }

        ctx.request_repaint_after(Duration::from_millis(50));
    }
}

fn main() {
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../icon.png")).expect("Не удалось загрузить icon.png");
    let saved = load_settings();

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([420.0, 460.0])
        .with_title("SunDEC")
        .with_icon(icon)
        .with_decorations(false)
        .with_resizable(false);

    if let Some((x, y)) = saved.window_pos {
        viewport = viewport.with_position([x, y]);
    } else {
        viewport = viewport.with_position([67.0, 276.0]);
    }

    if saved.always_on_top {
        viewport = viewport.with_always_on_top();
    }

    let native_options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native("SunDEC", native_options, Box::new(|cc| Box::new(App::new(cc)))).expect("Failed to run GUI");
}