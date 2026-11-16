// #![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs::File;
use std::io::Write;
use std::net::{TcpListener, UdpSocket};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

const TCI_HEADER_LEN: usize = 64;

fn base_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn write_frequency_file(freq_hz: u64) -> std::io::Result<()> {
    let path = base_dir().join("frequency.txt");
    let mut file = File::create(&path)?;
    write!(file, "{}", freq_hz)?;
    file.flush()?;
    Ok(())
}

fn write_modulation_file(mod_name: &str) -> std::io::Result<()> {
    let path = base_dir().join("modulation.txt");
    let mut file = File::create(&path)?;
    write!(file, "{}", mod_name)?;
    file.flush()?;
    Ok(())
}

fn calculate_peak_level(pcm_bytes: &[u8]) -> f32 {
    if pcm_bytes.len() < 2 { return 0.0; }
    let mut peak: f64 = 0.0;
    for chunk in pcm_bytes.chunks_exact(2) {
        let sample = i16::from_le_bytes([chunk[0], chunk[1]]) as f64;
        let abs_sample = sample.abs();
        if abs_sample > peak { peak = abs_sample; }
    }
    let db = 20.0 * (peak / 32767.0).log10();
    let normalized = ((db + 70.0) / 60.0).clamp(0.0, 1.0);
    normalized as f32
}

fn write_level_file(level: f32) -> std::io::Result<()> {
    let path = base_dir().join("level.txt");
    let mut file = File::create(path)?;
    write!(file, "{:.4}", level)?;
    file.flush()?;
    Ok(())
}

fn write_smeter_file(level_dbm: f32, unix_ms: u64) -> std::io::Result<()> {
    let path = base_dir().join("smeter.txt");
    let mut file = File::create(&path)?;
    write!(file, "{:.1}\n{}\n", level_dbm, unix_ms)?;
    file.flush()?;
    Ok(())
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn parse_sensor_level(text: &str) -> Option<f32> {
    if let Some(rest) = text.split("sql_level:").nth(1) {
        let dbm_part = rest.split(',').nth(1)?;
        let dbm_clean = dbm_part.trim().trim_end_matches(';');
        if let Ok(dbm) = dbm_clean.parse::<f32>() {
            if dbm.is_finite() { return Some(dbm); }
        }
        return None;
    }
    if let Some(rest) = text.split("RX_SENSORS:").nth(1) {
        let dbm_part = rest.split(',').nth(1)?;
        let dbm_clean = dbm_part.trim().trim_end_matches(';');
        if let Ok(dbm) = dbm_clean.parse::<f32>() {
            if dbm.is_finite() { return Some(dbm); }
        }
        return None;
    }
    if let Some(rest) = text.split("RX_CHANNEL_SENSORS:").nth(1) {
        let mut parts = rest.split(',');
        let _trx = parts.next()?;
        let ch = parts.next()?.trim();
        let dbm_part = parts.next()?;
        if ch != "0" { return None; }
        let dbm_clean = dbm_part.trim().trim_end_matches(';');
        if let Ok(dbm) = dbm_clean.parse::<f32>() {
            if dbm.is_finite() { return Some(dbm); }
        }
        return None;
    }
    None
}

fn maybe_write_smeter(last: &mut Option<f32>, dbm: f32) {
    let changed = match last {
        Some(prev) => (dbm - *prev).abs() > 0.05,
        None => true,
    };
    if changed {
        let _ = write_smeter_file(dbm, now_unix_ms());
        *last = Some(dbm);
    }
}

fn parse_tci_audio_frame(data: &[u8], mode: &str) -> Option<(u32, Vec<u8>)> {
    if data.len() < TCI_HEADER_LEN { return None; }
    let u32_at = |off: usize| -> u32 {
        u32::from_le_bytes([data[off], data[off+1], data[off+2], data[off+3]])
    };
    let format = u32_at(8);
    let channels = u32_at(28).max(1) as usize;
    let length = u32_at(20) as usize;
    let stream_type = u32_at(24);
    let payload = &data[TCI_HEADER_LEN..];

    if mode == "cw" {
        if stream_type != 1 { return None; }
    } else if mode == "analog" {
        if stream_type != 1 && stream_type != 4 { return None; }
    } else {
        if stream_type != 1 { return None; }
    }

    let pcm = match format {
        0 => {
            let frames = length / channels;
            let need = length * 2;
            if payload.len() < need { return None; }
            let mut result = Vec::with_capacity(frames * 2);
            for i in 0..frames {
                let mut acc = 0i32;
                for c in 0..channels {
                    let off = (i * channels + c) * 2;
                    acc += i16::from_le_bytes([payload[off], payload[off+1]]) as i32;
                }
                let avg = acc / channels as i32;
                let sample = (avg / 2) as i16;
                result.extend_from_slice(&sample.to_le_bytes());
            }
            result
        }
        3 => {
            let frames = length / channels;
            let need = length * 4;
            if payload.len() < need { return None; }
            let mut mono = Vec::with_capacity(frames * 2);
            if mode == "cw" {
                // Для CW — берём ЛЕВЫЙ канал (каждый второй сэмпл)
                for i in 0..frames {
                    let off = i * channels * 4;
                    let sample_f = f32::from_le_bytes([payload[off], payload[off+1], payload[off+2], payload[off+3]]);
                    let sample = (sample_f.clamp(-1.0, 1.0) * 32767.0) as i16;
                    mono.extend_from_slice(&sample.to_le_bytes());
                }
            } else {
                for i in 0..frames {
                    let mut acc = 0.0f32;
                    for c in 0..channels {
                        let off = (i * channels + c) * 4;
                        acc += f32::from_le_bytes([payload[off], payload[off+1], payload[off+2], payload[off+3]]);
                    }
                    let avg = acc / channels as f32;
                    let sample = (avg.clamp(-1.0, 1.0) * 32767.0) as i16;
                    mono.extend_from_slice(&sample.to_le_bytes());
                }
            }
            mono
        }
        _ => return None,
    };

    Some((stream_type, pcm))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let tci_port: u16 = if args.len() > 1 { args[1].parse().unwrap_or(50001) } else { 50001 };
    let dsd_port: u16 = if args.len() > 2 { args[2].parse().unwrap_or(7355) } else { 7355 };
    let mode: String = if args.len() > 3 { args[3].clone() } else { "dmr".to_string() };

    println!("Режим bridge: {}", mode);

    let is_cw = mode == "cw";
    let udp_out = if is_cw { Some(UdpSocket::bind("127.0.0.1:0")?) } else { None };
    let listener = if is_cw { None } else { Some(TcpListener::bind(format!("127.0.0.1:{}", dsd_port))?) };

    if listener.is_some() {
        println!("TCP-сервер слушает 127.0.0.1:{}", dsd_port);
    }

    loop {
        println!("Попытка подключения к TCI...");
        let url = format!("ws://127.0.0.1:{}", tci_port);

        match connect_async(&url).await {
            Ok((ws_stream, _)) => {
                println!("Подключено к TCI: {}", url);
                let (mut write, mut read) = ws_stream.split();

                write.send(Message::Text("protocol:ExpertSDR3,2.1;".to_string())).await?;

                let mut ready = false;
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                while tokio::time::Instant::now() < deadline {
                    if let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_millis(500), read.next()).await {
                        if let Message::Text(text) = msg {
                            if text.contains("ready;") { ready = true; break; }
                        }
                    }
                }

                if ready {
                    println!("TCI готов.");
                    write.send(Message::Text("VFO:0,0;".to_string())).await?;
                    tokio::time::sleep(Duration::from_millis(100)).await;

                    if mode == "analog" {
                        write.send(Message::Text("MODULATION:0,NFM;".to_string())).await?;
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        write.send(Message::Text("MUTE:false;".to_string())).await?;
                        write.send(Message::Text("AUDIO_STREAM_CHANNELS:1;".to_string())).await?;
                        write.send(Message::Text("AUDIO_STREAM_SAMPLE_TYPE:float32;".to_string())).await?;
                        write.send(Message::Text("AUDIO_SAMPLERATE:48000;".to_string())).await?;
                        write.send(Message::Text("AUDIO_START:0;".to_string())).await?;
                        write.send(Message::Text("LINE_OUT_START:0;".to_string())).await?;
                    } else if mode == "cw" {
                        write.send(Message::Text("MODULATION:0,CW;".to_string())).await?;
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        write.send(Message::Text("AUDIO_STREAM_CHANNELS:1;".to_string())).await?;
                        write.send(Message::Text("AUDIO_STREAM_SAMPLE_TYPE:float32;".to_string())).await?;
                        write.send(Message::Text("AUDIO_SAMPLERATE:48000;".to_string())).await?;
                        write.send(Message::Text("AUDIO_START:0;".to_string())).await?;
                    } else {
                        write.send(Message::Text("MODULATION:0,WFM;".to_string())).await?;
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        write.send(Message::Text("RX_FILTER_BAND:0,-7500,7500;".to_string())).await?;
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        write.send(Message::Text("AUDIO_STREAM_CHANNELS:1;".to_string())).await?;
                        write.send(Message::Text("AUDIO_STREAM_SAMPLE_TYPE:float32;".to_string())).await?;
                        write.send(Message::Text("AUDIO_SAMPLERATE:48000;".to_string())).await?;
                        write.send(Message::Text("AUDIO_START:0;".to_string())).await?;
                        write.send(Message::Text("MUTE:true;".to_string())).await?;
                    }

                    println!("DEBUG: Все команды инициализации отправлены.");

                    let mut last_check = Instant::now();
                    let mut is_wfm = true;
                    let mut last_vfo: u64 = 0;
                    let mut last_smeter: Option<f32> = None;

                    if is_cw {
                        loop {
                            match tokio::time::timeout(Duration::from_millis(100), read.next()).await {
                                Ok(Some(Ok(msg))) => {
                                    match msg {
                                        Message::Binary(data) => {
                                            if let Some((_st, pcm_bytes)) = parse_tci_audio_frame(&data, &mode) {
                                                let level = calculate_peak_level(&pcm_bytes);
                                                let _ = write_level_file(level);
                                                if let Some(ref udp) = udp_out {
                                                    let _ = udp.send_to(&pcm_bytes, "127.0.0.1:23458");
                                                }
                                            }
                                        }
                                        Message::Text(text) => {
                                            let text_lower = text.to_lowercase();
                                            if text_lower.contains("vfo:0,0,") {
                                                let vfo_part = text_lower.split("vfo:0,0,").nth(1).unwrap_or("");
                                                let vfo_str = vfo_part.split(';').next().unwrap_or("").trim();
                                                if let Ok(vfo) = vfo_str.parse::<u64>() {
                                                    last_vfo = vfo;
                                                    let _ = write_frequency_file(vfo);
                                                }
                                            }
                                            if text_lower.contains("modulation:0,") {
                                                let mod_part = text_lower.split("modulation:0,").nth(1).unwrap_or("");
                                                let mod_name = mod_part.split(';').next().unwrap_or("").trim();
                                                if !mod_name.is_empty() {
                                                    let _ = write_modulation_file(mod_name);
                                                }
                                            }
                                        }
                                        Message::Close(_) => break,
                                        _ => {}
                                    }
                                }
                                Ok(Some(Err(_))) => break,
                                Ok(None) => break,
                                Err(_) => {}
                            }
                            if last_check.elapsed() > Duration::from_secs(1) {
                                last_check = Instant::now();
                                let _ = write.send(Message::Text("MODULATION:0;".to_string())).await;
                            }
                        }
                    } else {
                        loop {
                            let mut tcp_stream = None;
                            while tcp_stream.is_none() {
                                if let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_millis(10), read.next()).await {
                                    if let Message::Text(text) = msg {
                                        if text.contains("sql_level:") || text.contains("RX_SENSORS:") || text.contains("RX_CHANNEL_SENSORS:") {
                                            println!("DEBUG: TCI sensor: {}", text.trim());
                                        }
                                        if let Some(dbm) = parse_sensor_level(&text) {
                                            maybe_write_smeter(&mut last_smeter, dbm);
                                        }
                                        let text_lower = text.to_lowercase();
                                        if text_lower.contains("vfo:0,0,") {
                                            let vfo_part = text_lower.split("vfo:0,0,").nth(1).unwrap_or("");
                                            let vfo_str = vfo_part.split(';').next().unwrap_or("").trim();
                                            if let Ok(vfo) = vfo_str.parse::<u64>() {
                                                last_vfo = vfo;
                                                let _ = write_frequency_file(vfo);
                                            }
                                        }
                                        if text_lower.contains("modulation:0,") {
                                            let mod_part = text_lower.split("modulation:0,").nth(1).unwrap_or("");
                                            let mod_name = mod_part.split(';').next().unwrap_or("").trim();
                                            if !mod_name.is_empty() {
                                                let _ = write_modulation_file(mod_name);
                                            }
                                        }
                                    }
                                }
                                if let Some(ref listener) = listener {
                                    if let Ok((stream, _)) = listener.accept() {
                                        println!("DSD-neo подключился!");
                                        tcp_stream = Some(stream);
                                    } else {
                                        tokio::time::sleep(Duration::from_millis(100)).await;
                                    }
                                } else {
                                    tokio::time::sleep(Duration::from_millis(100)).await;
                                }
                            }

                            let mut tcp_stream = tcp_stream.unwrap();

                            loop {
                                match tokio::time::timeout(Duration::from_millis(100), read.next()).await {
                                    Ok(Some(Ok(msg))) => {
                                        match msg {
                                            Message::Binary(data) => {
                                                if let Some((stream_type, pcm_bytes)) = parse_tci_audio_frame(&data, &mode) {
                                                    if stream_type == 1 {
                                                        let level = calculate_peak_level(&pcm_bytes);
                                                        let _ = write_level_file(level);
                                                    }
                                                    if stream_type == 4 || mode != "analog" {
                                                        if is_wfm {
                                                            if let Err(_) = tcp_stream.write_all(&pcm_bytes) {
                                                                eprintln!("DSD-neo отключился. Ждём нового подключения...");
                                                                break;
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                            Message::Text(text) => {
                                                if text.contains("sql_level:") || text.contains("RX_SENSORS:") || text.contains("RX_CHANNEL_SENSORS:") {
                                                    println!("DEBUG: TCI sensor: {}", text.trim());
                                                }
                                                if let Some(dbm) = parse_sensor_level(&text) {
                                                    maybe_write_smeter(&mut last_smeter, dbm);
                                                }

                                                let text_lower = text.to_lowercase();

                                                if text_lower.contains("vfo:0,0,") {
                                                    let vfo_part = text_lower.split("vfo:0,0,").nth(1).unwrap_or("");
                                                    let vfo_str = vfo_part.split(';').next().unwrap_or("").trim();
                                                    if let Ok(vfo) = vfo_str.parse::<u64>() {
                                                        last_vfo = vfo;
                                                        let _ = write_frequency_file(vfo);
                                                    }
                                                }

                                                if text_lower.contains("modulation:0,") {
                                                    let mod_part = text_lower.split("modulation:0,").nth(1).unwrap_or("");
                                                    let mod_name = mod_part.split(';').next().unwrap_or("").trim();
                                                    if !mod_name.is_empty() {
                                                        let _ = write_modulation_file(mod_name);
                                                    }

                                                    if mod_name == "wfm" || mod_name == "nfm" {
                                                        if !is_wfm {
                                                            let _ = write.send(Message::Text("AUDIO_STREAM_CHANNELS:1;".to_string())).await;
                                                            let _ = write.send(Message::Text("AUDIO_STREAM_SAMPLE_TYPE:float32;".to_string())).await;
                                                            let _ = write.send(Message::Text("AUDIO_SAMPLERATE:48000;".to_string())).await;
                                                            let _ = write.send(Message::Text("AUDIO_START:0;".to_string())).await;
                                                            let _ = write.send(Message::Text("MUTE:true;".to_string())).await;
                                                        }
                                                        is_wfm = true;
                                                    } else {
                                                        if is_wfm {
                                                            let _ = write.send(Message::Text("MUTE:false;".to_string())).await;
                                                        }
                                                        is_wfm = false;
                                                    }
                                                }
                                            }
                                            Message::Close(_) => break,
                                            _ => {}
                                        }
                                    }
                                    Ok(Some(Err(_))) => break,
                                    Ok(None) => break,
                                    Err(_) => {}
                                }

                                if last_check.elapsed() > Duration::from_secs(1) {
                                    last_check = Instant::now();
                                    let _ = write.send(Message::Text("MODULATION:0;".to_string())).await;
                                    let _ = write.send(Message::Text("RX_FILTER_BAND:0;".to_string())).await;
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                println!("Ошибка подключения к TCI: {}. Ждём 2 секунды...", e);
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}