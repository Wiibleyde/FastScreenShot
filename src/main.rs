//! FastScreenShots: Steam-style F12 screenshots for any game.
//!
//! Threads:
//! - main: low-level keyboard hook + tray icon message loop. Must never block,
//!   otherwise every keystroke system-wide waits on it.
//! - capture: grabs the frame when F12 is pressed.
//! - one short-lived thread per screenshot: PNG encoding + disk write.

#![windows_subsystem = "windows"]

mod capture;
mod encode;
mod feedback;

use std::error::Error;
use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{ClientToScreen, MONITOR_DEFAULTTONEAREST, MonitorFromPoint, MonitorFromWindow};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows::Win32::System::Registry::*;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{CreateMutexW, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, PWSTR, w};

const VK_F12: u32 = 0x7B;
const WM_TRAY: u32 = WM_APP + 1;
const ID_OPEN: usize = 1;
const ID_AUTOSTART: usize = 2;
const ID_QUIT: usize = 3;
const APP_NAME: PCWSTR = w!("FastScreenShots");
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");

static CAPTURE_TX: OnceLock<mpsc::Sender<()>> = OnceLock::new();
static F12_HELD: AtomicBool = AtomicBool::new(false);
static PENDING_SAVES: AtomicUsize = AtomicUsize::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static TRAY_ICON: AtomicUsize = AtomicUsize::new(0);

fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        // Single instance; the handle lives until the process exits.
        let _mutex = CreateMutexW(None, false, w!("Local\\FastScreenShots.SingleInstance"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }

        let (tx, rx) = mpsc::channel();
        let _ = CAPTURE_TX.set(tx);
        thread::spawn(move || {
            for () in rx {
                if take_screenshot().is_err() {
                    error_beep();
                }
            }
        });

        let Ok(module) = GetModuleHandleW(None) else { return };
        let instance = HINSTANCE(module.0);
        let class = WNDCLASSW { lpfnWndProc: Some(wnd_proc), hInstance: instance, lpszClassName: APP_NAME, ..Default::default() };
        RegisterClassW(&class);
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);
        let Ok(hwnd) = CreateWindowExW(WINDOW_EX_STYLE::default(), APP_NAME, APP_NAME, WS_OVERLAPPED, 0, 0, 0, 0, None, None, Some(instance), None) else {
            return;
        };
        TRAY_ICON.store(feedback::tray_icon().0 as usize, Ordering::Relaxed);
        tray(hwnd, NIM_ADD);

        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), Some(instance), 0);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }

        if let Ok(hook) = hook {
            let _ = UnhookWindowsHookEx(hook);
        }
        // Let in-flight PNG writes finish instead of leaving truncated files.
        let deadline = Instant::now() + Duration::from_secs(10);
        while PENDING_SAVES.load(Ordering::Acquire) > 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
    }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code == HC_ACTION as i32 {
            let key = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            if key.vkCode == VK_F12 {
                match wparam.0 as u32 {
                    // Ignore auto-repeat: one press, one screenshot.
                    WM_KEYDOWN | WM_SYSKEYDOWN if !F12_HELD.swap(true, Ordering::Relaxed) => {
                        if let Some(tx) = CAPTURE_TX.get() {
                            let _ = tx.send(());
                        }
                    }
                    WM_KEYUP | WM_SYSKEYUP => F12_HELD.store(false, Ordering::Relaxed),
                    _ => {}
                }
            }
        }
        // Never swallow the key: the game still receives F12.
        CallNextHookEx(None, code, wparam, lparam)
    }
}

fn take_screenshot() -> Result<(), Box<dyn Error>> {
    let window = unsafe { GetForegroundWindow() };
    let (monitor, rect) = target_area(window);
    let shot = capture::capture(monitor, rect)?;
    feedback::shutter();

    let path = reserve_file(&game_name(window))?;
    PENDING_SAVES.fetch_add(1, Ordering::AcqRel);
    thread::spawn(move || {
        if encode::save_png(&path, &shot).is_err() {
            let _ = fs::remove_file(&path);
            error_beep();
        }
        PENDING_SAVES.fetch_sub(1, Ordering::AcqRel);
    });
    Ok(())
}

/// The game's client area in physical screen pixels, plus the monitor it is on.
/// An empty rect means "whole monitor".
fn target_area(window: HWND) -> (windows::Win32::Graphics::Gdi::HMONITOR, RECT) {
    unsafe {
        if window.is_invalid() || IsIconic(window).as_bool() {
            let mut cursor = POINT::default();
            let _ = GetCursorPos(&mut cursor);
            return (MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST), RECT::default());
        }
        let mut rect = RECT::default();
        let mut origin = POINT::default();
        if GetClientRect(window, &mut rect).is_err() || !ClientToScreen(window, &mut origin).as_bool() {
            rect = RECT::default();
        }
        let area = RECT { left: origin.x, top: origin.y, right: origin.x + rect.right, bottom: origin.y + rect.bottom };
        (MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST), area)
    }
}

/// Executable name of the window's process ("eldenring"), else its title, else "Bureau".
fn game_name(window: HWND) -> String {
    let mut name = String::new();
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(window, Some(&mut pid));
        if let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            if QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok() {
                let path = PathBuf::from(std::ffi::OsString::from_wide(&buf[..len as usize]));
                name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            }
            let _ = CloseHandle(process);
        }
        if name.is_empty() {
            let mut buf = [0u16; 256];
            let len = GetWindowTextW(window, &mut buf);
            name = String::from_utf16_lossy(&buf[..len.max(0) as usize]);
        }
    }
    let name: String = name.chars().map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c }).collect();
    let name = name.trim().trim_end_matches('.');
    if name.is_empty() { "Bureau".to_owned() } else { name.to_owned() }
}

fn screenshots_dir() -> Result<PathBuf, Box<dyn Error>> {
    unsafe {
        let raw = SHGetKnownFolderPath(&FOLDERID_Pictures, KF_FLAG_DEFAULT, None)?;
        let pictures = PathBuf::from(std::ffi::OsString::from_wide(raw.as_wide()));
        CoTaskMemFree(Some(raw.0 as *const _));
        Ok(pictures.join("FastScreenShots"))
    }
}

/// Creates `<Pictures>\FastScreenShots\<game>\YYYY-MM-DD_HH-MM-SS[_N].png`
/// immediately so two quick presses never pick the same name.
fn reserve_file(game: &str) -> Result<PathBuf, Box<dyn Error>> {
    let dir = screenshots_dir()?.join(game);
    fs::create_dir_all(&dir)?;
    let t = unsafe { GetLocalTime() };
    let stamp = format!("{:04}-{:02}-{:02}_{:02}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond);
    for n in 1.. {
        let file = if n == 1 { format!("{stamp}.png") } else { format!("{stamp}_{n}.png") };
        let path = dir.join(file);
        match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    unreachable!()
}

fn error_beep() {
    unsafe {
        let _ = MessageBeep(MB_ICONHAND);
    }
}

pub fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TRAY => match lparam.0 as u32 {
                WM_LBUTTONUP => open_folder(),
                WM_RBUTTONUP => show_menu(hwnd),
                _ => {}
            },
            WM_COMMAND => match wparam.0 & 0xFFFF {
                ID_OPEN => open_folder(),
                ID_AUTOSTART => set_autostart(!autostart_enabled()),
                ID_QUIT => {
                    let _ = DestroyWindow(hwnd);
                }
                _ => {}
            },
            WM_DESTROY => {
                tray(hwnd, NIM_DELETE);
                PostQuitMessage(0);
            }
            // Explorer restarted: the icon must be added again.
            m if m == TASKBAR_CREATED.load(Ordering::Relaxed) && m != 0 => tray(hwnd, NIM_ADD),
            _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
        }
        LRESULT(0)
    }
}

fn tray(hwnd: HWND, action: NOTIFY_ICON_MESSAGE) {
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: HICON(TRAY_ICON.load(Ordering::Relaxed) as *mut _),
        ..Default::default()
    };
    for (dst, src) in data.szTip.iter_mut().zip("FastScreenShots — F12 pour capturer".encode_utf16()) {
        *dst = src;
    }
    unsafe {
        let _ = Shell_NotifyIconW(action, &data);
    }
}

fn show_menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let autostart = if autostart_enabled() { MF_CHECKED } else { MF_UNCHECKED };
        let _ = AppendMenuW(menu, MF_STRING, ID_OPEN, w!("Ouvrir le dossier des captures"));
        let _ = AppendMenuW(menu, MF_STRING | autostart, ID_AUTOSTART, w!("Lancer au démarrage de Windows"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, ID_QUIT, w!("Quitter"));
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        // Required so the menu closes when clicking elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, cursor.x, cursor.y, None, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
    }
}

fn open_folder() {
    let Ok(dir) = screenshots_dir() else { return };
    let _ = fs::create_dir_all(&dir);
    let dir = wide(dir.as_os_str());
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(dir.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

/// `"C:\path\FastScreenShots.exe"`, as stored in the Run key.
fn launch_command() -> Vec<u16> {
    let mut buf = [0u16; 1024];
    let len = unsafe { GetModuleFileNameW(None, &mut buf) } as usize;
    let mut cmd = vec![u16::from(b'"')];
    cmd.extend_from_slice(&buf[..len]);
    cmd.extend([u16::from(b'"'), 0]);
    cmd
}

/// Enabled only if the Run entry points at this exe (a moved exe shows unchecked).
fn autostart_enabled() -> bool {
    let mut buf = [0u16; 1100];
    let mut size = (buf.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, APP_NAME, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr().cast()), Some(&mut size))
    };
    status == ERROR_SUCCESS && buf[..size as usize / 2] == launch_command()[..]
}

fn set_autostart(enable: bool) {
    unsafe {
        if enable {
            let cmd = launch_command();
            let _ = RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, APP_NAME, REG_SZ.0, Some(cmd.as_ptr().cast()), (cmd.len() * 2) as u32);
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, APP_NAME);
        }
    }
}
