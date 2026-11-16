# R2GFK SunDEC

Программа для декодирования радиорежимов (DMR, аналог FM, CW) через TCI. Работает в связке с ExpertSDR3 и DSD-neo.

## Что умеет

- **DMR** — декодирование, запись в MP3, вывод звука в колонки, расшифровка с ключами (`-H`/`-b`), Talker Alias (с кириллицей), GPS, Text, Тип вызова, Шифрование.
- **Аналог (FM)** — CTCSS/DCS, запись 1:1, модуляция из TCI.
- **CW** — собственный декодер (Goertzel), автоопределение тона, подсветка сокращений, WPM, АРУ.
- **Запись** — WAV → MP3 через `lame.exe`.

## Как это работает

ExpertSDR3 (TCI) → bridge.exe → decoder.exe (DSD-neo) → UDP → R2GFK SunDEC.exe

- `bridge.exe` — мост между ExpertSDR3 и DSD-neo (написан на Rust).
- `decoder.exe` — DSD-neo (внешний декодер).
- `R2GFK SunDEC.exe` — GUI (Rust + egui).
- `lame.exe` — конвертация WAV в MP3.

## Сборка

### Требования
- Rust (stable, MSVC toolchain)
- Windows 10/11

### Bridge
cd rust-bridge\tci-bridge
cargo build --release
Copy-Item "target\release\tci-bridge.exe" "..\..\bridge.exe" -Force

### GUI
cd rust-bridge\gui-controller
cargo build --release
Copy-Item "target\release\gui-controller.exe" "..\..\R2GFK SunDEC.exe" -Force

## Используемые компоненты

- **DSD-neo** — декодер DMR/P25/NXDN/D-STAR/YSF/M17/dPMR/TETRA/EDACS/ProVoice.  
  [github.com/arancormonk/dsd-neo](https://github.com/arancormonk/dsd-neo)
- **cw-dit** — библиотека для CW-декодера.  
  [github.com/swilcox/cw-dit](https://github.com/swilcox/cw-dit)
- **eframe / egui** — графический интерфейс.  
  [github.com/emilk/egui](https://github.com/emilk/egui)
- **cpal** — вывод звука.  
  [github.com/RustAudio/cpal](https://github.com/RustAudio/cpal)
- **rubato** — ресемплинг аудио.  
  [github.com/HEnquist/rubato](https://github.com/HEnquist/rubato)
- **hound** — запись WAV.  
  [github.com/ruuda/hound](https://github.com/ruuda/hound)
- **encoding_rs** — корректная кодировка кириллицы.  
  [github.com/hsivonen/encoding_rs](https://github.com/hsivonen/encoding_rs)
- **lame** — конвертация WAV в MP3.  
  [lame.sourceforge.net](https://lame.sourceforge.net)

## Лицензия

Проект использует DSD-neo как внешний процесс (не включает его код).  
DSD-neo распространяется под GPL-3.0.  
Остальные библиотеки — под MIT/Apache-2.0.

## Демонстрация работы

<table>
<tr>
<td align="center"><img src="https://github.com/user-attachments/assets/c7f18ace-433d-468f-83ae-18efdecb7e61" alt="Скриншот #1" width="450"></td>
<td align="center"><img src="https://github.com/user-attachments/assets/f142fc60-b9f9-4b5a-8418-3c8f9dbb87f9" alt="Скриншот #2" width="450"></td>
</tr>
<tr>
<td align="center"><img src="https://github.com/user-attachments/assets/5d183c7f-3780-4db1-9870-d1566ce1d169" alt="Скриншот #3" width="450"></td>
<td align="center"><img src="https://github.com/user-html-attachments/assets/9cf0eca1-f68c-4104-be78-0b80023e60ec" alt="Скриншот #4" width="450"></td>
</tr>
</table>
