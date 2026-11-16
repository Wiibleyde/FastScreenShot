//! PNG encoding through WIC (built into Windows, no extra dependency).

use std::error::Error;
use std::path::Path;

use windows::Win32::Foundation::GENERIC_WRITE;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize};
use windows::core::{GUID, PCWSTR};

use crate::capture::Shot;
use crate::wide;

pub fn save_png(path: &Path, shot: &Shot) -> Result<(), Box<dyn Error>> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let result = write(path, shot);
        CoUninitialize();
        result
    }
}

unsafe fn write(path: &Path, shot: &Shot) -> Result<(), Box<dyn Error>> {
    unsafe {
        let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let stream = factory.CreateStream()?;
        let path = wide(path.as_os_str());
        stream.InitializeFromFilename(PCWSTR(path.as_ptr()), GENERIC_WRITE.0)?;

        let encoder = factory.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
        let mut frame = None;
        let mut options = None;
        encoder.CreateNewFrame(&mut frame, &mut options)?;
        let frame = frame.ok_or("no WIC frame")?;
        frame.Initialize(options.as_ref())?;
        frame.SetSize(shot.width, shot.height)?;
        let mut format: GUID = GUID_WICPixelFormat24bppBGR;
        frame.SetPixelFormat(&mut format)?;
        if format != GUID_WICPixelFormat24bppBGR {
            return Err("PNG encoder rejected 24bpp BGR".into());
        }
        frame.WritePixels(shot.height, shot.width * 3, &shot.bgr)?;
        frame.Commit()?;
        encoder.Commit()?;
        Ok(())
    }
}
