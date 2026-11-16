//! Grabs pixels from a monitor with DXGI Desktop Duplication.
//!
//! Everything is created on demand and dropped right after the grab so the
//! process holds no GPU resources while idle.

use std::error::Error;

use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::core::Interface;

/// Tightly packed 24-bit BGR image (stride = width * 3).
pub struct Shot {
    pub width: u32,
    pub height: u32,
    pub bgr: Vec<u8>,
}

/// Captures `rect` (virtual-desktop physical pixels) from the output that shows
/// `monitor`. An empty `rect` or one outside the monitor captures the whole monitor.
pub fn capture(monitor: HMONITOR, rect: RECT) -> Result<Shot, Box<dyn Error>> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let (adapter, output, desc) = find_output(&factory, monitor)?;

        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
        let device = device.ok_or("no D3D11 device")?;
        let context = context.ok_or("no D3D11 context")?;

        let duplication = output.cast::<IDXGIOutput1>()?.DuplicateOutput(&device)?;

        // Desktop-relative crop, clamped to the output.
        let origin = desc.DesktopCoordinates;
        let out_w = (origin.right - origin.left) as u32;
        let out_h = (origin.bottom - origin.top) as u32;
        let clamp = |v: i32, max: u32| v.clamp(0, max as i32) as u32;
        let mut left = clamp(rect.left - origin.left, out_w);
        let mut top = clamp(rect.top - origin.top, out_h);
        let mut right = clamp(rect.right - origin.left, out_w);
        let mut bottom = clamp(rect.bottom - origin.top, out_h);
        if right <= left || bottom <= top {
            (left, top, right, bottom) = (0, 0, out_w, out_h);
        }
        let (width, height) = (right - left, bottom - top);

        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        device.CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
        let staging = staging.ok_or("no staging texture")?;
        let region = D3D11_BOX { left, top, front: 0, right, bottom, back: 1 };

        // The first frame after DuplicateOutput is the current desktop image.
        // If it carries no present timestamp, a running game delivers a fresh
        // frame within a few milliseconds; a static screen just times out and
        // the first image is kept.
        let mut grabbed = false;
        for timeout_ms in [500, 100] {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            if let Err(e) = duplication.AcquireNextFrame(timeout_ms, &mut info, &mut resource) {
                if grabbed && e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                    break;
                }
                return Err(e.into());
            }
            let texture: ID3D11Texture2D = resource.ok_or("no desktop frame")?.cast()?;
            let mut src_desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut src_desc);
            if src_desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                duplication.ReleaseFrame()?;
                return Err("unexpected desktop pixel format".into());
            }
            context.CopySubresourceRegion(&staging, 0, 0, 0, 0, &texture, 0, Some(&region));
            duplication.ReleaseFrame()?;
            grabbed = true;
            if info.LastPresentTime != 0 {
                break;
            }
        }

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let mut bgr = Vec::with_capacity((width * height * 3) as usize);
        for y in 0..height as usize {
            let row = std::slice::from_raw_parts(
                (mapped.pData as *const u8).add(y * mapped.RowPitch as usize),
                width as usize * 4,
            );
            for px in row.chunks_exact(4) {
                bgr.extend_from_slice(&px[..3]);
            }
        }
        context.Unmap(&staging, 0);

        Ok(Shot { width, height, bgr })
    }
}

fn find_output(
    factory: &IDXGIFactory1,
    monitor: HMONITOR,
) -> Result<(IDXGIAdapter1, IDXGIOutput, DXGI_OUTPUT_DESC), Box<dyn Error>> {
    unsafe {
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                let desc = output.GetDesc()?;
                if desc.Monitor == monitor {
                    return Ok((adapter, output, desc));
                }
                o += 1;
            }
            a += 1;
        }
    }
    Err("monitor not found among DXGI outputs".into())
}
