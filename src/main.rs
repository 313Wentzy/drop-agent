// Console Toggle:
// This will SHOW the console in dev mode (cargo run)
// and HIDE it in release mode (cargo build --release)
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use anyhow::Result;
use log::{error, info, warn};
use std::{sync::Arc, time::{Duration, SystemTime, UNIX_EPOCH, Instant}, path::PathBuf};
use discord_presence_rs::activities::{Activity, Assets, Button, Timestamps};
use discord_presence_rs::discord_connection::Client;
use std::io::Cursor;

use futures_util::{SinkExt, StreamExt};
use futures_util::stream::SplitSink;
use tokio::{runtime::Runtime, time::sleep};
use tokio::sync::{mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel}, Mutex, RwLock};
use tokio_tungstenite::{connect_async, WebSocketStream};
use tokio_tungstenite::tungstenite::Message;

use tray_icon::{Icon, menu::{Menu, MenuItem, MenuEvent}, TrayIcon, TrayIconBuilder};
use winit::event::{Event, StartCause};
use winit::event_loop::{EventLoop, EventLoopBuilder, EventLoopProxy};

use serde::Serialize;
use serde_json::{json, Value};

use image::{ImageFormat, DynamicImage};
use single_instance::SingleInstance;
use base64::Engine;
use warp::Filter;

// Keybind polling statics
#[cfg(windows)]
use std::sync::atomic::{AtomicU32, AtomicBool, Ordering};

#[cfg(windows)]
static CAPTURE_KEY_CODE: AtomicU32 = AtomicU32::new(0x77); // F8 default (VK_F8)
#[cfg(windows)]
static CLEAR_KEY_CODE: AtomicU32 = AtomicU32::new(0x78); // F9 default (VK_F9)
#[cfg(windows)]
static CAPTURE_MODIFIERS: AtomicU32 = AtomicU32::new(0); // No modifiers by default
#[cfg(windows)]
static CLEAR_MODIFIERS: AtomicU32 = AtomicU32::new(0);
#[cfg(windows)]
static CAPTURE_KEY_WAS_DOWN: AtomicBool = AtomicBool::new(false);

// Windows-specific imports for window detection and capture
#[cfg(windows)]
use windows::Win32::Foundation::{HWND, RECT, BOOL, LPARAM};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, GetWindowTextLengthW, IsWindowVisible,
    GetWindowRect, GetForegroundWindow,
};
#[cfg(windows)]
use windows::Win32::Graphics::Gdi::{
    GetDC, ReleaseDC, CreateCompatibleDC, CreateCompatibleBitmap, SelectObject,
    BitBlt, DeleteDC, DeleteObject, GetDIBits, SRCCOPY, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS,
};
#[cfg(windows)]
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

type WsWrite = SplitSink<WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, Message>;

const WS_URL: &str = "wss://serverapi-4rtc.onrender.com/agent";
const AGENT_VERSION: &str = "0.7.0";
const LOCAL_SERVER_PORT: u16 = 30123;
const TOKEN_REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);
const FORTNITE_PROCESS_NAME: &str = "FortniteClient-Win64-Shipping.exe";
const FORTNITE_WINDOW_TITLE: &str = "Fortnite";
const CAPTURE_COOLDOWN: Duration = Duration::from_secs(5);
const DISCORD_CLIENT_ID: &str = "1322787879454916679";

// NOTE: Update this path for your build environment
static TRAY_ICO: &[u8] =
    include_bytes!(r#"C:\Users\Wentzy\Desktop\Dropmazter\DropmazterApp\icons\tray.ico"#);

// Absolute path for shortcuts (used at runtime)
const TRAY_ICO_PATH: &str = r#"C:\Users\Wentzy\Desktop\Dropmazter\DropmazterApp\icons\tray.ico"#;

// --- Shared Token State with expiry tracking ---
#[derive(Clone, Debug)]
struct TokenInfo {
    token: String,
    expires_at: SystemTime,
}

type SharedToken = Arc<Mutex<Option<TokenInfo>>>;

// Shared Fortnite window state
#[derive(Clone, Debug, Default)]
struct FortniteWindowState {
    hwnd: Option<isize>,  // Window handle as isize for thread safety
    is_running: bool,
    width: u32,
    height: u32,
}

type SharedFortniteState = Arc<Mutex<FortniteWindowState>>;

// Keybind configuration
#[derive(Clone, Debug)]
struct KeybindConfig {
    capture_screen: String,
    clear_screen: String,
}

impl Default for KeybindConfig {
    fn default() -> Self {
        Self {
            capture_screen: "F8".to_string(),
            clear_screen: "F9".to_string(),
        }
    }
}

type SharedKeybinds = Arc<RwLock<KeybindConfig>>;
type SharedLastCapture = Arc<Mutex<Option<Instant>>>;

// Commands for hotkey re-registration (sent from WS to main thread)
#[derive(Debug, Clone)]
enum HotkeyCmd {
    Register { capture: String, clear: String },
}

// Virtual key codes for Windows
#[cfg(windows)]
mod vk {
    pub const VK_F1: u32 = 0x70;
    pub const VK_F2: u32 = 0x71;
    pub const VK_F3: u32 = 0x72;
    pub const VK_F4: u32 = 0x73;
    pub const VK_F5: u32 = 0x74;
    pub const VK_F6: u32 = 0x75;
    pub const VK_F7: u32 = 0x76;
    pub const VK_F8: u32 = 0x77;
    pub const VK_F9: u32 = 0x78;
    pub const VK_F10: u32 = 0x79;
    pub const VK_F11: u32 = 0x7A;
    pub const VK_F12: u32 = 0x7B;
    pub const VK_CONTROL: u32 = 0x11;
    pub const VK_MENU: u32 = 0x12; // Alt
    pub const VK_SHIFT: u32 = 0x10;
}

// Parse keybind string to virtual key code and modifiers
// Returns (vk_code, modifiers_mask)
// Modifiers: 1=Ctrl, 2=Alt, 4=Shift
#[cfg(windows)]
fn parse_keybind_to_vk(keybind_str: &str) -> Option<(u32, u32)> {
    let parts: Vec<&str> = keybind_str.split('+').map(|s| s.trim()).collect();
    
    let mut modifiers: u32 = 0;
    let mut key_code: Option<u32> = None;
    
    for part in parts {
        match part.to_uppercase().as_str() {
            "CTRL" | "CONTROL" => modifiers |= 1,
            "ALT" => modifiers |= 2,
            "SHIFT" => modifiers |= 4,
            // Function keys
            "F1" => key_code = Some(vk::VK_F1),
            "F2" => key_code = Some(vk::VK_F2),
            "F3" => key_code = Some(vk::VK_F3),
            "F4" => key_code = Some(vk::VK_F4),
            "F5" => key_code = Some(vk::VK_F5),
            "F6" => key_code = Some(vk::VK_F6),
            "F7" => key_code = Some(vk::VK_F7),
            "F8" => key_code = Some(vk::VK_F8),
            "F9" => key_code = Some(vk::VK_F9),
            "F10" => key_code = Some(vk::VK_F10),
            "F11" => key_code = Some(vk::VK_F11),
            "F12" => key_code = Some(vk::VK_F12),
            // Letter keys (A-Z are 0x41-0x5A)
            s if s.len() == 1 && s.chars().next().unwrap().is_ascii_alphabetic() => {
                key_code = Some(s.chars().next().unwrap().to_ascii_uppercase() as u32);
            }
            // Number keys (0-9 are 0x30-0x39)
            s if s.len() == 1 && s.chars().next().unwrap().is_ascii_digit() => {
                key_code = Some(s.chars().next().unwrap() as u32);
            }
            _ => {}
        }
    }
    
    key_code.map(|code| (code, modifiers))
}

#[cfg(not(windows))]
fn parse_keybind_to_vk(_keybind_str: &str) -> Option<(u32, u32)> {
    None
}

// Check if modifier keys are currently pressed
#[cfg(windows)]
fn check_modifiers() -> u32 {
    let mut mods: u32 = 0;
    unsafe {
        if (GetAsyncKeyState(vk::VK_CONTROL as i32) as u16 & 0x8000) != 0 {
            mods |= 1;
        }
        if (GetAsyncKeyState(vk::VK_MENU as i32) as u16 & 0x8000) != 0 {
            mods |= 2;
        }
        if (GetAsyncKeyState(vk::VK_SHIFT as i32) as u16 & 0x8000) != 0 {
            mods |= 4;
        }
    }
    mods
}

// Check if capture hotkey is currently pressed (polling-based, doesn't block keys)
#[cfg(windows)]
fn check_capture_hotkey() -> bool {
    let capture_key = CAPTURE_KEY_CODE.load(Ordering::Relaxed);
    let capture_mods = CAPTURE_MODIFIERS.load(Ordering::Relaxed);
    
    unsafe {
        // Check if key is currently down
        let key_down = (GetAsyncKeyState(capture_key as i32) as u16 & 0x8000) != 0;
        let current_mods = check_modifiers();
        
        let was_down = CAPTURE_KEY_WAS_DOWN.load(Ordering::Relaxed);
        
        if key_down && current_mods == capture_mods {
            if !was_down {
                // Key just pressed (edge detection)
                CAPTURE_KEY_WAS_DOWN.store(true, Ordering::Relaxed);
                return true;
            }
        } else {
            // Key released
            CAPTURE_KEY_WAS_DOWN.store(false, Ordering::Relaxed);
        }
    }
    false
}

// Update keybinds
#[cfg(windows)]
fn update_keybinds(capture: &str, clear: &str) {
    if let Some((vk, mods)) = parse_keybind_to_vk(capture) {
        CAPTURE_KEY_CODE.store(vk, Ordering::Relaxed);
        CAPTURE_MODIFIERS.store(mods, Ordering::Relaxed);
        info!("[hotkey] Capture keybind set to: '{}' (vk=0x{:02X}, mods={})", capture, vk, mods);
    } else {
        warn!("[hotkey] Failed to parse capture keybind: '{}'", capture);
    }
    if let Some((vk, mods)) = parse_keybind_to_vk(clear) {
        CLEAR_KEY_CODE.store(vk, Ordering::Relaxed);
        CLEAR_MODIFIERS.store(mods, Ordering::Relaxed);
        info!("[hotkey] Clear keybind set to: '{}' (vk=0x{:02X}, mods={})", clear, vk, mods);
    } else {
        warn!("[hotkey] Failed to parse clear keybind: '{}'", clear);
    }
}

// Request/Response structs
#[derive(Serialize)]
struct HealthResponse {
    status: String,
    version: String,
    fortnite_running: bool,
}

#[derive(Serialize)]
struct ScreenStatusResponse {
    fortnite_running: bool,
    width: u32,
    height: u32,
}

#[derive(Debug, Clone)]
enum UiEvent { CaptureFortnite, CheckFortnite, Quit, Reconnect, RestartApp }

#[derive(Debug)]
enum Cmd { CaptureFortnite, CheckFortnite, ForceReconnect }

// ---- Fortnite Window Detection ----

#[cfg(windows)]
fn find_fortnite_window() -> Option<(HWND, u32, u32)> {
    use std::sync::Mutex as StdMutex;
    
    // Store as isize to avoid Send issues with HWND
    static RESULT: std::sync::OnceLock<StdMutex<Option<(isize, u32, u32, String)>>> = std::sync::OnceLock::new();
    let result = RESULT.get_or_init(|| StdMutex::new(None));
    
    // Clear previous result
    {
        let mut guard = result.lock().unwrap();
        *guard = None;
    }
    
    unsafe extern "system" fn enum_callback(hwnd: HWND, _lparam: LPARAM) -> BOOL {
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        use windows::Win32::System::ProcessStatus::GetModuleBaseNameW;
        use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
        
        unsafe {
            // Check if window is visible
            if !IsWindowVisible(hwnd).as_bool() {
                return BOOL(1); // Continue enumeration
            }
            
            // Get window title first
            let title_len = GetWindowTextLengthW(hwnd);
            let title = if title_len > 0 {
                let mut title_buf: Vec<u16> = vec![0; (title_len + 1) as usize];
                GetWindowTextW(hwnd, &mut title_buf);
                String::from_utf16_lossy(&title_buf[..title_len as usize]).trim().to_string()
            } else {
                String::new()
            };
            
            // Skip windows with no title
            if title.is_empty() {
                return BOOL(1);
            }
            
            // Get the process ID for this window
            let mut process_id: u32 = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut process_id));
            
            if process_id == 0 {
                return BOOL(1);
            }
            
            // Try to get process name
            let mut is_fortnite = false;
            let mut process_name = String::new();
            
            if let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) {
                let mut name_buf: Vec<u16> = vec![0; 260];
                let len = GetModuleBaseNameW(handle, None, &mut name_buf);
                let _ = windows::Win32::Foundation::CloseHandle(handle);
                
                if len > 0 {
                    process_name = String::from_utf16_lossy(&name_buf[..len as usize]);
                    let process_lower = process_name.to_lowercase();
                    
                    // Check if this is Fortnite by process name
                    is_fortnite = process_lower.contains("fortnite") 
                        || process_lower.contains("fortniteclient");
                }
            }
            
            // Fallback: If we couldn't get process name, check if title is EXACTLY "Fortnite"
            // (This handles anti-cheat protected processes)
            if !is_fortnite && title == "Fortnite" {
                is_fortnite = true;
            }
            
            if is_fortnite {
                let mut rect = RECT::default();
                if GetWindowRect(hwnd, &mut rect).is_ok() {
                    let width = (rect.right - rect.left) as u32;
                    let height = (rect.bottom - rect.top) as u32;
                    
                    // Fortnite window should be reasonably large (game window, not launcher)
                    if width > 640 && height > 480 {
                        let result = RESULT.get().unwrap();
                        let mut guard = result.lock().unwrap();
                        let info = if process_name.is_empty() {
                            format!("{} (PID:{})", title, process_id)
                        } else {
                            format!("{} ({})", title, process_name)
                        };
                        *guard = Some((hwnd.0 as isize, width, height, info));
                        return BOOL(0); // Stop enumeration - found it!
                    }
                }
            }
            
            BOOL(1) // Continue enumeration
        }
    }
    
    unsafe {
        let _ = EnumWindows(Some(enum_callback), LPARAM(0));
    }
    
    let guard = result.lock().unwrap();
    if let Some((hwnd_val, w, h, info)) = guard.as_ref() {
        info!("[fortnite] Found window: {} ({}x{})", info, w, h);
        Some((HWND(*hwnd_val as *mut _), *w, *h))
    } else {
        None
    }
}

#[cfg(not(windows))]
fn find_fortnite_window() -> Option<(i32, u32, u32)> {
    // Non-Windows placeholder
    None
}

#[cfg(windows)]
fn capture_fortnite_window(hwnd: HWND, width: u32, height: u32) -> Option<DynamicImage> {
    unsafe {
        let w = width as i32;
        let h = height as i32;
        
        if w <= 0 || h <= 0 {
            error!("[capture] Invalid dimensions: {}x{}", w, h);
            return None;
        }
        
        // Get window position
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            error!("[capture] Failed to get window rect");
            return None;
        }
        
        info!("[capture] Capturing Fortnite window at: left={}, top={}, {}x{}", 
              rect.left, rect.top, w, h);
        
        // Capture from screen at the Fortnite window's position
        let hdc_screen = GetDC(HWND::default());
        if hdc_screen.is_invalid() {
            error!("[capture] Failed to get screen DC");
            return None;
        }
        
        let hdc_mem = CreateCompatibleDC(hdc_screen);
        if hdc_mem.is_invalid() {
            ReleaseDC(HWND::default(), hdc_screen);
            error!("[capture] Failed to create compatible DC");
            return None;
        }
        
        let hbitmap = CreateCompatibleBitmap(hdc_screen, w, h);
        if hbitmap.is_invalid() {
            let _ = DeleteDC(hdc_mem);
            ReleaseDC(HWND::default(), hdc_screen);
            error!("[capture] Failed to create bitmap");
            return None;
        }
        
        let old_obj = SelectObject(hdc_mem, hbitmap);
        
        // BitBlt from screen at the Fortnite window's exact position
        let blt_result = BitBlt(hdc_mem, 0, 0, w, h, hdc_screen, rect.left, rect.top, SRCCOPY);
        
        if blt_result.is_err() {
            SelectObject(hdc_mem, old_obj);
            let _ = DeleteObject(hbitmap);
            let _ = DeleteDC(hdc_mem);
            ReleaseDC(HWND::default(), hdc_screen);
            error!("[capture] BitBlt failed");
            return None;
        }
        
        // Prepare bitmap info
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // Negative for top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0 as u32,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [Default::default()],
        };
        
        let mut pixels: Vec<u8> = vec![0; (w * h * 4) as usize];
        
        let result = GetDIBits(
            hdc_mem,
            hbitmap,
            0,
            h as u32,
            Some(pixels.as_mut_ptr() as *mut _),
            &mut bi,
            DIB_RGB_COLORS,
        );
        
        // Cleanup
        SelectObject(hdc_mem, old_obj);
        let _ = DeleteObject(hbitmap);
        let _ = DeleteDC(hdc_mem);
        ReleaseDC(HWND::default(), hdc_screen);
        
        if result == 0 {
            error!("[capture] GetDIBits failed");
            return None;
        }
        
        // Convert BGRA to RGBA
        for chunk in pixels.chunks_exact_mut(4) {
            chunk.swap(0, 2);
        }
        
        // Create image
        let img = image::RgbaImage::from_raw(width, height, pixels)?;
        info!("[capture] ✓ Successfully captured Fortnite {}x{}", width, height);
        Some(DynamicImage::ImageRgba8(img))
    }
}

#[cfg(not(windows))]
fn capture_fortnite_window(_hwnd: i32, _width: u32, _height: u32) -> Option<DynamicImage> {
    None
}

// Debug: List visible windows with their process names
#[cfg(windows)]
fn debug_list_windows() {
    info!("[debug] Listing visible windows with process names:");
    
    unsafe extern "system" fn debug_callback(hwnd: HWND, _lparam: LPARAM) -> BOOL {
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        use windows::Win32::System::ProcessStatus::GetModuleBaseNameW;
        use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
        
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() {
                return BOOL(1);
            }
            
            let title_len = GetWindowTextLengthW(hwnd);
            if title_len == 0 {
                return BOOL(1);
            }
            
            let mut title_buf: Vec<u16> = vec![0; (title_len + 1) as usize];
            GetWindowTextW(hwnd, &mut title_buf);
            let title = String::from_utf16_lossy(&title_buf[..title_len as usize]);
            
            let mut process_id: u32 = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut process_id));
            
            let process_name = if process_id != 0 {
                if let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) {
                    let mut name_buf: Vec<u16> = vec![0; 260];
                    let len = GetModuleBaseNameW(handle, None, &mut name_buf);
                    let _ = windows::Win32::Foundation::CloseHandle(handle);
                    if len > 0 {
                        String::from_utf16_lossy(&name_buf[..len as usize])
                    } else {
                        format!("PID:{}", process_id)
                    }
                } else {
                    format!("PID:{}", process_id)
                }
            } else {
                "Unknown".to_string()
            };
            
            let mut rect = RECT::default();
            let size = if GetWindowRect(hwnd, &mut rect).is_ok() {
                format!("{}x{}", rect.right - rect.left, rect.bottom - rect.top)
            } else {
                "?x?".to_string()
            };
            
            // Only log windows that might be games (larger than 640x480)
            if rect.right - rect.left > 640 && rect.bottom - rect.top > 480 {
                info!("[debug]   {} | {} | {}", process_name, size, title);
            }
        }
        BOOL(1)
    }
    
    unsafe {
        let _ = EnumWindows(Some(debug_callback), LPARAM(0));
    }
}

#[cfg(not(windows))]
fn debug_list_windows() {}

// Check if Fortnite is the foreground (active) window
#[cfg(windows)]
fn is_fortnite_foreground() -> bool {
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows::Win32::System::ProcessStatus::GetModuleBaseNameW;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    
    unsafe {
        let fg_hwnd = GetForegroundWindow();
        if fg_hwnd.0.is_null() {
            return false;
        }
        
        // Get window title for fallback check
        let title_len = GetWindowTextLengthW(fg_hwnd);
        let title = if title_len > 0 {
            let mut title_buf: Vec<u16> = vec![0; (title_len + 1) as usize];
            GetWindowTextW(fg_hwnd, &mut title_buf);
            String::from_utf16_lossy(&title_buf[..title_len as usize]).trim().to_string()
        } else {
            String::new()
        };
        
        // Get process ID of foreground window
        let mut process_id: u32 = 0;
        GetWindowThreadProcessId(fg_hwnd, Some(&mut process_id));
        
        if process_id == 0 {
            return false;
        }
        
        // Try to get process name
        if let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) {
            let mut name_buf: Vec<u16> = vec![0; 260];
            let len = GetModuleBaseNameW(handle, None, &mut name_buf);
            let _ = windows::Win32::Foundation::CloseHandle(handle);
            
            if len > 0 {
                let process_name = String::from_utf16_lossy(&name_buf[..len as usize]);
                let process_lower = process_name.to_lowercase();
                
                if process_lower.contains("fortnite") || process_lower.contains("fortniteclient") {
                    return true;
                }
            }
        }
        
        // Fallback: Check if window title is EXACTLY "Fortnite"
        title == "Fortnite"
    }
}

#[cfg(not(windows))]
fn is_fortnite_foreground() -> bool {
    false
}

// ---- Discord Rich Presence ----
fn run_discord_presence() {
    match Client::new(DISCORD_CLIENT_ID) {
        Ok(mut conn) => {
            info!("[discord] Rich Presence connected");
            
            loop {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                
                let activity = Activity::new()
                    .set_details("Perfecting drop timings".to_string())
                    .set_state("dropmazter.com".to_string())
                    .set_assets(Assets::new()
                        .set_large_image("logo".to_string())
                        .set_large_text("Dropmazter".to_string()))
                    .set_timestamps(Timestamps::new().set_start(now))
                    .set_buttons(vec![
                        Button::new()
                            .set_label("Visit Website".to_string())
                            .set_url("https://dropmazter.com".to_string())
                    ]);
                
                if let Err(e) = conn.set_activity(activity) {
                    warn!("[discord] Failed to set activity: {:?}", e);
                }
                
                std::thread::sleep(Duration::from_secs(15));
            }
        }
        Err(e) => {
            warn!("[discord] Failed to create client: {:?}", e);
        }
    }
}

// ---- Auto-Update Check ----
fn check_for_update() {
    info!("[update] Checking for updates...");
    // Placeholder for auto-update logic
    // In production, this would check a server for new versions
}

// ---- Shortcut Creation ----
#[cfg(windows)]
fn create_desktop_shortcut() -> Result<()> {
    use std::process::Command;
    
    let exe_path = std::env::current_exe()?;
    let exe_name = "Dropmazter Agent";
    
    let desktop = if let Ok(userprofile) = std::env::var("USERPROFILE") {
        PathBuf::from(userprofile).join("Desktop")
    } else {
        return Ok(());
    };
    
    let shortcut_path = desktop.join(format!("{}.lnk", exe_name));
    
    if shortcut_path.exists() {
        info!("[shortcut] Desktop shortcut already exists");
        return Ok(());
    }
    
    // Use the hardcoded icon path
    let icon_location = format!("{},0", TRAY_ICO_PATH);
    
    let ps_script = format!(
        r#"$WshShell = New-Object -comObject WScript.Shell; $Shortcut = $WshShell.CreateShortcut('{}'); $Shortcut.TargetPath = '{}'; $Shortcut.WorkingDirectory = '{}'; $Shortcut.Description = 'Dropmazter Desktop Agent'; $Shortcut.IconLocation = '{}'; $Shortcut.Save()"#,
        shortcut_path.display(),
        exe_path.display(),
        exe_path.parent().unwrap_or(&exe_path).display(),
        icon_location
    );
    
    match Command::new("powershell").args(["-Command", &ps_script]).output() {
        Ok(out) if out.status.success() => {
            info!("[shortcut] Created desktop shortcut: {}", shortcut_path.display());
        }
        Ok(out) => {
            warn!("[shortcut] Failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        Err(e) => {
            warn!("[shortcut] PowerShell error: {}", e);
        }
    }
    
    Ok(())
}

#[cfg(windows)]
fn create_startup_shortcut() -> Result<()> {
    use std::process::Command;
    
    let exe_path = std::env::current_exe()?;
    let exe_name = "Dropmazter Agent";
    
    let startup = if let Ok(appdata) = std::env::var("APPDATA") {
        PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup")
    } else {
        return Ok(());
    };
    
    let shortcut_path = startup.join(format!("{}.lnk", exe_name));
    
    if shortcut_path.exists() {
        info!("[shortcut] Startup shortcut already exists");
        return Ok(());
    }
    
    // Use the hardcoded icon path
    let icon_location = format!("{},0", TRAY_ICO_PATH);
    
    let ps_script = format!(
        r#"$WshShell = New-Object -comObject WScript.Shell; $Shortcut = $WshShell.CreateShortcut('{}'); $Shortcut.TargetPath = '{}'; $Shortcut.WorkingDirectory = '{}'; $Shortcut.Description = 'Dropmazter Desktop Agent'; $Shortcut.IconLocation = '{}'; $Shortcut.Save()"#,
        shortcut_path.display(),
        exe_path.display(),
        exe_path.parent().unwrap_or(&exe_path).display(),
        icon_location
    );
    
    let _ = Command::new("powershell").args(["-Command", &ps_script]).output();
    info!("[shortcut] Created startup shortcut");
    
    Ok(())
}

#[cfg(not(windows))]
fn create_desktop_shortcut() -> Result<()> { Ok(()) }

#[cfg(not(windows))]
fn create_startup_shortcut() -> Result<()> { Ok(()) }

// ---- File Logging ----
fn get_log_path() -> PathBuf {
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let mut path = PathBuf::from(local_app_data);
        path.push("Dropmazter");
        std::fs::create_dir_all(&path).ok();
        path.push("agent.log");
        path
    } else {
        PathBuf::from("agent.log")
    }
}

fn init_console() {
    use std::fs::OpenOptions;
    use std::io::Write;
    
    let log_path = get_log_path();
    
    // Truncate log if over 5MB
    if let Ok(meta) = std::fs::metadata(&log_path) {
        if meta.len() > 5 * 1024 * 1024 {
            let _ = std::fs::remove_file(&log_path);
        }
    }
    
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok();
    
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format(move |buf, record| {
            let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
            let log_line = format!(
                "[{}] [{}] {}\n",
                timestamp,
                record.level(),
                record.args()
            );
            
            // Write to file
            if let Some(ref file) = log_file {
                if let Ok(mut f) = file.try_clone() {
                    let _ = f.write_all(log_line.as_bytes());
                }
            }
            
            // Write to console
            writeln!(buf, "[{}] [{}] {}", timestamp, record.level(), record.args())
        })
        .init();
    
    info!("=== Drop Agent starting… v{} (Fortnite Window Mode) ===", AGENT_VERSION);
    info!("Log file: {:?}", log_path);
}

// ---- Local Web Server Handlers ----

async fn handle_health(fortnite_state: SharedFortniteState) -> Result<impl warp::Reply, warp::Rejection> {
    let state = fortnite_state.lock().await;
    Ok(warp::reply::json(&HealthResponse {
        status: "ok".to_string(),
        version: AGENT_VERSION.to_string(),
        fortnite_running: state.is_running,
    }))
}

async fn handle_screen_status(fortnite_state: SharedFortniteState) -> Result<impl warp::Reply, warp::Rejection> {
    // Check for Fortnite window
    let (is_running, width, height) = if let Some((_hwnd, w, h)) = find_fortnite_window() {
        (true, w, h)
    } else {
        (false, 0, 0)
    };
    
    // Update shared state
    {
        let mut state = fortnite_state.lock().await;
        state.is_running = is_running;
        state.width = width;
        state.height = height;
    }
    
    info!("[screen] Fortnite status - running: {}, {}x{}", is_running, width, height);
    
    Ok(warp::reply::json(&ScreenStatusResponse {
        fortnite_running: is_running,
        width,
        height,
    }))
}

// ---- tray ----
fn build_tray(proxy: EventLoopProxy<UiEvent>) -> Result<TrayIcon> {
    let menu = Menu::new();
    let item_capture = MenuItem::new("Capture Fortnite", true, None);
    let item_check = MenuItem::new("Check Fortnite Status", true, None);
    let item_reconnect = MenuItem::new("Force Reconnect", true, None);
    let item_restart = MenuItem::new("Restart Agent", true, None);
    let item_quit = MenuItem::new("Quit", true, None);
    
    let id_capture = item_capture.id().clone();
    let id_check = item_check.id().clone();
    let id_reconnect = item_reconnect.id().clone();
    let id_restart = item_restart.id().clone();
    let id_quit = item_quit.id().clone();
    
    menu.append(&item_capture)?;
    menu.append(&item_check)?;
    menu.append(&item_reconnect)?;
    menu.append(&item_restart)?;
    menu.append(&item_quit)?;
    
    let img = image::load_from_memory(TRAY_ICO)?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let icon = Icon::from_rgba(rgba.into_raw(), w, h)?;
    
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(icon)
        .with_tooltip(format!("Dropmazter Agent v{}", AGENT_VERSION))
        .build()?;
    
    std::thread::spawn(move || {
        loop {
            if let Ok(event) = MenuEvent::receiver().recv() {
                if event.id == id_capture {
                    let _ = proxy.send_event(UiEvent::CaptureFortnite);
                } else if event.id == id_check {
                    let _ = proxy.send_event(UiEvent::CheckFortnite);
                } else if event.id == id_reconnect {
                    let _ = proxy.send_event(UiEvent::Reconnect);
                } else if event.id == id_restart {
                    let _ = proxy.send_event(UiEvent::RestartApp);
                } else if event.id == id_quit {
                    let _ = proxy.send_event(UiEvent::Quit);
                }
            }
        }
    });
    
    Ok(tray)
}

// ---- Local HTTP Server loop ----
async fn local_server_loop(token_state: SharedToken, fortnite_state: SharedFortniteState) {
    let fortnite_state_health = fortnite_state.clone();
    let fortnite_state_status = fortnite_state.clone();
    let token_state_receive = token_state.clone();

    let cors = warp::cors()
        .allow_any_origin()
        .allow_methods(vec!["GET", "POST", "OPTIONS"])
        .allow_headers(vec!["Content-Type"]);

    let health = warp::path("health")
        .and(warp::get())
        .and(warp::any().map(move || fortnite_state_health.clone()))
        .and_then(handle_health);
        
    let screen_status = warp::path!("screen" / "status")
        .and(warp::get())
        .and(warp::any().map(move || fortnite_state_status.clone()))
        .and_then(handle_screen_status);

    let receive_token = warp::path!("auth" / "token")
        .and(warp::post())
        .and(warp::body::json())
        .and(warp::any().map(move || token_state_receive.clone()))
        .and_then(|body: Value, token_state: SharedToken| async move {
            if let Some(token) = body.get("token").and_then(|t| t.as_str()) {
                let expires_at = SystemTime::now() + TOKEN_REFRESH_INTERVAL;
                let mut guard = token_state.lock().await;
                *guard = Some(TokenInfo { token: token.to_string(), expires_at });
                info!("[auth] Received token from page via local server (expires in 10min)");
                Ok::<_, warp::Rejection>(warp::reply::json(&json!({"status": "ok"})))
            } else {
                Ok(warp::reply::json(&json!({"error": "missing token"})))
            }
        });

    let routes = health
        .or(screen_status)
        .or(receive_token)
        .with(cors);

    info!("[server] local HTTP server starting on 127.0.0.1:{}", LOCAL_SERVER_PORT);
    warp::serve(routes).run(([127, 0, 0, 1], LOCAL_SERVER_PORT)).await;
}

// ---- WebSocket loop ----
async fn ws_loop(
    mut rx: UnboundedReceiver<Cmd>, 
    token_state: SharedToken, 
    fortnite_state: SharedFortniteState,
    keybind_state: SharedKeybinds,
    _last_capture_state: SharedLastCapture,
    hotkey_tx: UnboundedSender<HotkeyCmd>,
    url: String
) {
    let writer_slot: Arc<Mutex<Option<WsWrite>>> = Arc::new(Mutex::new(None));

    // Periodic ping
    {
        let writer = writer_slot.clone();
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(30)).await;
                let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
                let payload = json!({ "type":"ping", "ts": ts }).to_string();
                let mut g = writer.lock().await;
                if let Some(w) = g.as_mut() { 
                    if let Err(e) = w.send(Message::Text(payload.into())).await {
                        error!("[ws/ping] failed to send ping: {}", e);
                    }
                }
            }
        });
    }

    // Periodic Fortnite window check
    {
        let fortnite_state_clone = fortnite_state.clone();
        let writer = writer_slot.clone();
        tokio::spawn(async move {
            let mut check_count: u32 = 0;
            
            // Immediate first check
            let (is_running, width, height, hwnd) = if let Some((h, w, ht)) = find_fortnite_window() {
                (true, w, ht, Some(h.0 as isize))
            } else {
                (false, 0, 0, None)
            };
            
            {
                let mut state = fortnite_state_clone.lock().await;
                state.is_running = is_running;
                state.width = width;
                state.height = height;
                state.hwnd = hwnd;
            }
            
            if is_running {
                info!("[fortnite] Initial check: FOUND {}x{}", width, height);
            } else {
                info!("[fortnite] Initial check: not found");
            }
            
            loop {
                sleep(Duration::from_secs(3)).await; // Check every 3 seconds
                check_count += 1;
                
                let (is_running, width, height, hwnd) = if let Some((h, w, ht)) = find_fortnite_window() {
                    (true, w, ht, Some(h.0 as isize))
                } else {
                    (false, 0, 0, None)
                };
                
                let status_changed = {
                    let mut state = fortnite_state_clone.lock().await;
                    let changed = state.is_running != is_running;
                    state.is_running = is_running;
                    state.width = width;
                    state.height = height;
                    state.hwnd = hwnd;
                    changed
                };
                
                // Log every 10 checks (every 30 seconds) or when status changes
                if check_count % 10 == 0 {
                    info!("[fortnite] Periodic check #{}: running={}, {}x{}", 
                          check_count, is_running, width, height);
                }
                
                // Notify webpage if status changed
                if status_changed {
                    let payload = json!({
                        "type": "screen:status:result",
                        "fortnite_running": is_running,
                        "width": width,
                        "height": height
                    });
                    let mut g = writer.lock().await;
                    if let Some(w) = g.as_mut() {
                        let _ = w.send(Message::Text(payload.to_string().into())).await;
                    }
                    info!("[fortnite] ★ Status CHANGED: running={}, {}x{}", is_running, width, height);
                }
            }
        });
    }
    
    // Main connection loop
    loop {
        // Wait for token
        let token = loop {
            let guard = token_state.lock().await;
            if let Some(info) = &*guard {
                if info.expires_at > SystemTime::now() {
                    break info.token.clone();
                } else {
                    info!("[ws] Token expired, waiting for refresh...");
                }
            }
            drop(guard);
            sleep(Duration::from_secs(2)).await;
        };
        
        info!("[ws] Connecting to {}", url);
        
        let ws_result = connect_async(&url).await;
        let ws_stream = match ws_result {
            Ok((stream, _)) => stream,
            Err(e) => {
                error!("[ws] Connection failed: {}", e);
                sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        
        let (mut write, mut read) = ws_stream.split();
        
        // Get current Fortnite status for hello message
        let (fn_running, fn_width, fn_height) = {
            let state = fortnite_state.lock().await;
            (state.is_running, state.width, state.height)
        };
        
        // Send hello with fortnite status
        let hello = json!({
            "type": "hello",
            "token": token,
            "role": "agent",
            "version": AGENT_VERSION,
            "fortnite_running": fn_running,
            "width": fn_width,
            "height": fn_height
        });
        
        if let Err(e) = write.send(Message::Text(hello.to_string().into())).await {
            error!("[ws] Failed to send hello: {}", e);
            continue;
        }
        
        {
            let mut slot = writer_slot.lock().await;
            *slot = Some(write);
        }
        
        info!("[ws] Connected and authenticated as agent v{} (fortnite: {}, {}x{})", 
              AGENT_VERSION, fn_running, fn_width, fn_height);
        
        // Also send explicit status message in case page is already connected
        {
            let payload = json!({
                "type": "screen:status:result",
                "fortnite_running": fn_running,
                "width": fn_width,
                "height": fn_height
            });
            let mut g = writer_slot.lock().await;
            if let Some(w) = g.as_mut() {
                let _ = w.send(Message::Text(payload.to_string().into())).await;
                info!("[ws] Sent initial Fortnite status");
            }
        }
        
        // Message loop
        loop {
            tokio::select! {
                Some(cmd) = rx.recv() => {
                    match cmd {
                        Cmd::CaptureFortnite => {
                            let state = fortnite_state.lock().await;
                            if state.is_running {
                                if let Some(hwnd_val) = state.hwnd {
                                    let hwnd = HWND(hwnd_val as *mut _);
                                    let w = state.width;
                                    let h = state.height;
                                    drop(state);
                                    
                                    info!("[capture] Capturing Fortnite window {}x{}", w, h);
                                    if let Some(img) = capture_fortnite_window(hwnd, w, h) {
                                        let mut png_bytes = Cursor::new(Vec::new());
                                        if img.write_to(&mut png_bytes, ImageFormat::Png).is_ok() {
                                            let base64_png = base64::engine::general_purpose::STANDARD.encode(png_bytes.into_inner());
                                            let payload = json!({
                                                "type": "screen:capture:result",
                                                "pngBase64": base64_png,
                                                "width": w,
                                                "height": h
                                            });
                                            let mut g = writer_slot.lock().await;
                                            if let Some(wr) = g.as_mut() {
                                                if let Err(e) = wr.send(Message::Text(payload.to_string().into())).await {
                                                    error!("[capture] Failed to send: {}", e);
                                                } else {
                                                    info!("[capture] Screenshot sent ({}x{})", w, h);
                                                }
                                            }
                                        }
                                    } else {
                                        error!("[capture] Failed to capture window");
                                        let payload = json!({
                                            "type": "screen:capture:error",
                                            "error": "Failed to capture window"
                                        });
                                        let mut g = writer_slot.lock().await;
                                        if let Some(wr) = g.as_mut() {
                                            let _ = wr.send(Message::Text(payload.to_string().into())).await;
                                        }
                                    }
                                }
                            } else {
                                info!("[capture] Fortnite not running, ignoring capture");
                            }
                        }
                        Cmd::CheckFortnite => {
                            let (running, w, h) = if let Some((_, w, h)) = find_fortnite_window() {
                                (true, w, h)
                            } else {
                                (false, 0, 0)
                            };
                            info!("[fortnite] Manual check: running={}, {}x{}", running, w, h);
                        }
                        Cmd::ForceReconnect => {
                            info!("[ws] Force reconnect requested");
                            break;
                        }
                    }
                }
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                                match v.get("type").and_then(|t| t.as_str()) {
                                    Some("screen:capture") => {
                                        info!("[ws] Capture requested from server");
                                        let state = fortnite_state.lock().await;
                                        if state.is_running {
                                            if let Some(hwnd_val) = state.hwnd {
                                                let hwnd = HWND(hwnd_val as *mut _);
                                                let w = state.width;
                                                let h = state.height;
                                                drop(state);
                                                
                                                if let Some(img) = capture_fortnite_window(hwnd, w, h) {
                                                    let mut png_bytes = Cursor::new(Vec::new());
                                                    if img.write_to(&mut png_bytes, ImageFormat::Png).is_ok() {
                                                        let base64_png = base64::engine::general_purpose::STANDARD.encode(png_bytes.into_inner());
                                                        let payload = json!({
                                                            "type": "screen:capture:result",
                                                            "pngBase64": base64_png,
                                                            "width": w,
                                                            "height": h
                                                        });
                                                        let mut g = writer_slot.lock().await;
                                                        if let Some(wr) = g.as_mut() {
                                                            let _ = wr.send(Message::Text(payload.to_string().into())).await;
                                                        }
                                                        info!("[capture] Screenshot sent via WS request");
                                                    }
                                                } else {
                                                    error!("[capture] Failed to capture window");
                                                }
                                            }
                                        } else {
                                            warn!("[capture] Fortnite not running");
                                        }
                                    }
                                    Some("keybinds:update") => {
                                        if let Some(keybinds) = v.get("keybinds") {
                                            let capture = keybinds.get("captureScreen")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("F8")
                                                .to_string();
                                            let clear = keybinds.get("clearScreen")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("F9")
                                                .to_string();
                                            
                                            info!("[keybind] Received: capture={}, clear={}", capture, clear);
                                            
                                            // Update shared state
                                            {
                                                let mut config = keybind_state.write().await;
                                                config.capture_screen = capture.clone();
                                                config.clear_screen = clear.clone();
                                            }
                                            
                                            // Update atomics directly (thread-safe, immediate effect)
                                            #[cfg(windows)]
                                            {
                                                update_keybinds(&capture, &clear);
                                            }
                                            
                                            // Also send to event loop for any UI updates
                                            let _ = hotkey_tx.send(HotkeyCmd::Register {
                                                capture: capture.clone(),
                                                clear: clear.clone(),
                                            });
                                        }
                                    }
                                    Some("force-update") | Some("restart") => {
                                        info!("[restart] Restart requested by server");
                                        if let Ok(exe) = std::env::current_exe() {
                                            let _ = std::process::Command::new(exe).spawn();
                                            std::process::exit(0);
                                        }
                                    }
                                    Some("token:update/ok") => {
                                        info!("[auth] Token update acknowledged by server");
                                    }
                                    Some("error") if v.get("reason").and_then(|r| r.as_str()) == Some("auth_failed") => {
                                        warn!("[auth] Server rejected token. Clearing and waiting for new token...");
                                        {
                                            let mut token_guard = token_state.lock().await;
                                            *token_guard = None;
                                        }
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) => {
                            info!("[ws] Server closed connection");
                            break;
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => { 
                            error!("[ws] read error: {e}"); 
                            break; 
                        }
                        None => break,
                    }
                }
            }
        }
        
        {
            let mut slot = writer_slot.lock().await;
            *slot = None;
        }
        
        info!("[ws] Disconnected, reconnecting in 5s...");
        sleep(Duration::from_secs(5)).await;
    }
}

// ---- Main ----
fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install ring CryptoProvider");

    init_console();

    let instance = SingleInstance::new("dropmazter_agent")?;
    if !instance.is_single() { 
        warn!("Another instance is already running. Exiting.");
        return Ok(()); 
    }
    
    // Create shortcuts on first run
    let first_run_marker = get_log_path().parent().unwrap().join(".installed");
    if !first_run_marker.exists() {
        info!("[setup] First run detected - creating shortcuts...");
        create_desktop_shortcut()?;
        create_startup_shortcut()?;
        std::fs::write(&first_run_marker, "1").ok();
    }
    
    if !cfg!(debug_assertions) {
        check_for_update();
    }

    let rt = Runtime::new()?;

    rt.spawn(async {
        run_discord_presence();
    });
    
    let shared_token: SharedToken = Arc::new(Mutex::new(None));
    let shared_fortnite: SharedFortniteState = Arc::new(Mutex::new(FortniteWindowState::default()));
    let shared_keybinds: SharedKeybinds = Arc::new(RwLock::new(KeybindConfig::default()));
    let shared_last_capture: SharedLastCapture = Arc::new(Mutex::new(None));

    let (tx_cmd, rx_cmd) = unbounded_channel::<Cmd>();
    let (tx_hotkey, mut rx_hotkey) = unbounded_channel::<HotkeyCmd>();

    rt.spawn(ws_loop(
        rx_cmd, 
        shared_token.clone(), 
        shared_fortnite.clone(), 
        shared_keybinds.clone(),
        shared_last_capture.clone(),
        tx_hotkey.clone(),
        WS_URL.to_string()
    ));
    rt.spawn(local_server_loop(shared_token.clone(), shared_fortnite.clone()));

    // Initialize keybinds
    #[cfg(windows)]
    update_keybinds("F8", "F9"); // Set defaults
    
    let event_loop: EventLoop<UiEvent> = EventLoopBuilder::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let _tray = build_tray(proxy.clone())?;
    info!("[tray] icon created");

    // Spawn a separate thread for hotkey polling (to avoid 100% CPU from ControlFlow::Poll)
    #[cfg(windows)]
    {
        let tx_cmd_for_hotkey = tx_cmd.clone();
        let fortnite_state_for_hotkey = shared_fortnite.clone();
        let last_capture_for_hotkey = shared_last_capture.clone();
        
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let mut last_log = Instant::now();
            let mut hotkey_log_count = 0u32;
            
            info!("[hotkey] Polling thread started");
            
            loop {
                // Very fast polling for responsive hotkeys (~200 polls per second)
                std::thread::sleep(Duration::from_millis(5));
                
                // First check if hotkey is pressed (fast, no async)
                if !check_capture_hotkey() {
                    continue; // Skip everything if key not pressed
                }
                
                // Hotkey is pressed - now check if Fortnite is running and foreground
                let (is_running, is_fg) = {
                    let state = rt.block_on(async { fortnite_state_for_hotkey.lock().await });
                    (state.is_running, is_fortnite_foreground())
                };
                
                // Log periodically for debugging
                hotkey_log_count += 1;
                if hotkey_log_count % 10 == 1 || last_log.elapsed() > Duration::from_secs(5) {
                    info!("[hotkey] Key pressed! running={}, foreground={}", is_running, is_fg);
                    last_log = Instant::now();
                }
                
                if !is_running {
                    continue; // Fortnite not running
                }
                
                if !is_fg {
                    continue; // Fortnite not foreground
                }
                
                // Check cooldown
                let (can_capture, remaining_secs) = rt.block_on(async {
                    let last = last_capture_for_hotkey.lock().await;
                    match *last {
                        Some(instant) => {
                            let elapsed = instant.elapsed();
                            if elapsed >= CAPTURE_COOLDOWN {
                                (true, 0.0)
                            } else {
                                (false, (CAPTURE_COOLDOWN - elapsed).as_secs_f32())
                            }
                        }
                        None => (true, 0.0),
                    }
                });
                
                if can_capture {
                    info!("[hotkey] ✓ Capture triggered - Fortnite is foreground");
                    let _ = tx_cmd_for_hotkey.send(Cmd::CaptureFortnite);
                    
                    // Update last capture time
                    rt.block_on(async {
                        let mut last = last_capture_for_hotkey.lock().await;
                        *last = Some(Instant::now());
                    });
                } else {
                    info!("[hotkey] Capture on cooldown ({:.1}s remaining)", remaining_secs);
                }
            }
        });
    }

    event_loop.run(move |event, _elwt| {
        // Process hotkey re-registration commands from WS thread
        while let Ok(cmd) = rx_hotkey.try_recv() {
            match cmd {
                HotkeyCmd::Register { capture, clear } => {
                    info!("[hotkey] Updating keybinds: capture={}, clear={}", capture, clear);
                    #[cfg(windows)]
                    update_keybinds(&capture, &clear);
                }
            }
        }
        
        match event {
            Event::NewEvents(StartCause::Init) => info!("[app] started"),
            Event::UserEvent(UiEvent::CaptureFortnite) => { 
                let _ = tx_cmd.send(Cmd::CaptureFortnite); 
            }
            Event::UserEvent(UiEvent::CheckFortnite) => { 
                let _ = tx_cmd.send(Cmd::CheckFortnite); 
            }
            Event::UserEvent(UiEvent::Reconnect) => {
                let _ = tx_cmd.send(Cmd::ForceReconnect);
            }
            Event::UserEvent(UiEvent::RestartApp) => {
                info!("[app] Restart requested");
                if let Ok(exe) = std::env::current_exe() {
                    let _ = std::process::Command::new(exe).spawn();
                }
                std::process::exit(0);
            }
            Event::UserEvent(UiEvent::Quit) => {
                info!("[tray] Quit requested");
                std::process::exit(0);
            }
            _ => {}
        }
    })?;

    #[allow(unreachable_code)] 
    Ok(())
}