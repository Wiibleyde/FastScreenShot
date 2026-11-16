//! Shutter sound and tray icon, both synthesized at runtime so the exe ships
//! no resource files.

use std::sync::LazyLock;

use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO, SM_CXSMICON};
use windows::core::PCWSTR;

/// Plays a short camera-shutter click (asynchronous, returns immediately).
pub fn shutter() {
    static WAV: LazyLock<Vec<u8>> = LazyLock::new(shutter_wav);
    unsafe {
        let _ = PlaySoundW(PCWSTR(WAV.as_ptr().cast()), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
}

/// Two decaying noise bursts (shutter open/close), 16-bit mono PCM WAV.
fn shutter_wav() -> Vec<u8> {
    const RATE: u32 = 22_050;
    let samples = RATE * 110 / 1000;
    let data_len = samples * 2;

    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());

    let mut seed = 0x2545_f491u32;
    for i in 0..samples {
        let t = i as f32 / RATE as f32;
        let burst = |start: f32, gain: f32| if t >= start { gain * (-(t - start) / 0.007).exp() } else { 0.0 };
        let envelope = burst(0.0, 0.55) + burst(0.055, 0.4);
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let noise = (seed >> 16) as u16 as i16 as f32;
        wav.extend_from_slice(&((noise * envelope) as i16).to_le_bytes());
    }
    wav
}

/// Small camera glyph for the notification area, antialiased by 4x4 supersampling.
pub fn tray_icon() -> HICON {
    unsafe {
        let size = GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem()).max(16);
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let Ok(color) = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
            return HICON::default();
        };
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (size * size) as usize);
        for y in 0..size {
            for x in 0..size {
                pixels[(y * size + x) as usize] = shade(x, y, size as f32);
            }
        }
        let mask = CreateBitmap(size, size, 1, 1, None);
        let icon = CreateIconIndirect(&ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color });
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon.unwrap_or_default()
    }
}

fn shade(x: i32, y: i32, size: f32) -> u32 {
    const BODY: [f32; 3] = [0x3a as f32, 0x8e as f32, 0xe6 as f32];
    const LENS_RING: [f32; 3] = [255.0, 255.0, 255.0];
    const LENS: [f32; 3] = [0x1e as f32, 0x22 as f32, 0x2a as f32];

    let (mut rgb, mut alpha) = ([0.0f32; 3], 0.0f32);
    for sy in 0..4 {
        for sx in 0..4 {
            let u = (x as f32 + (sx as f32 + 0.5) / 4.0) / size;
            let v = (y as f32 + (sy as f32 + 0.5) / 4.0) / size;
            let lens_d = ((u - 0.5).powi(2) + (v - 0.57).powi(2)).sqrt();
            let color = if lens_d < 0.13 {
                LENS
            } else if lens_d < 0.21 {
                LENS_RING
            } else if rounded_rect(u, v, 0.04, 0.27, 0.96, 0.88, 0.1) || (0.3..0.62).contains(&u) && (0.14..0.3).contains(&v) {
                BODY
            } else {
                continue;
            };
            for c in 0..3 {
                rgb[c] += color[c] / 16.0;
            }
            alpha += 1.0 / 16.0;
        }
    }
    // 32bpp DIB icons are premultiplied BGRA.
    let [r, g, b] = rgb.map(|c| c.round() as u32);
    (((alpha * 255.0).round() as u32) << 24) | (r << 16) | (g << 8) | b
}

fn rounded_rect(u: f32, v: f32, l: f32, t: f32, r: f32, b: f32, radius: f32) -> bool {
    if !(l..r).contains(&u) || !(t..b).contains(&v) {
        return false;
    }
    let cx = u.clamp(l + radius, r - radius);
    let cy = v.clamp(t + radius, b - radius);
    (u - cx).powi(2) + (v - cy).powi(2) <= radius * radius
}
