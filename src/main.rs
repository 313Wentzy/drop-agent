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

use serde::{Serialize, Deserialize};
use serde_json::{json, Value};

use image::{ImageFormat, DynamicImage};
use single_instance::SingleInstance;
use base64::Engine;
use warp::Filter;

// Keybind polling statics
#[cfg(windows)]
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicBool, Ordering};

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
// Atomics so the hotkey thread never needs block_on
#[cfg(windows)]
static FORTNITE_IS_RUNNING: AtomicBool = AtomicBool::new(false);
#[cfg(windows)]
static LAST_CAPTURE_EPOCH_MS: AtomicU64 = AtomicU64::new(0);

// Windows-specific imports for window detection and capture
#[cfg(windows)]
use windows::Win32::Foundation::{HWND, RECT, BOOL, LPARAM};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, GetWindowTextLengthW, IsWindowVisible,
    GetWindowRect, GetForegroundWindow, IsWindow,
};
#[cfg(windows)]
use windows::Win32::Graphics::Gdi::{
    GetDC, ReleaseDC, CreateCompatibleDC, CreateCompatibleBitmap, SelectObject,
    BitBlt, DeleteDC, DeleteObject, GetDIBits, SRCCOPY, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS,
};
#[cfg(windows)]
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

// Direct FFI for PrintWindow (not exposed in windows crate)
#[cfg(windows)]
#[link(name = "user32")]
unsafe extern "system" {
    fn PrintWindow(hwnd: isize, hdc: isize, flags: u32) -> i32;
}

type WsWrite = SplitSink<WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, Message>;

const WS_URL: &str = "wss://api.dropmazter.com:8443/pluto/agent";
const AGENT_VERSION: &str = "2.3.0";
const LOCAL_SERVER_PORT: u16 = 30123;
const TOKEN_REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);
const FORTNITE_PROCESS_NAME: &str = "FortniteClient-Win64-Shipping.exe";
const FORTNITE_WINDOW_TITLE: &str = "Fortnite";
const CAPTURE_COOLDOWN: Duration = Duration::from_secs(5);
const DISCORD_CLIENT_ID: &str = "1244587407320551447";

// Embedded icons (relative paths so any machine can build)
static TRAY_ICO: &[u8] = include_bytes!("../../icons/tray.ico");
// High-res PNG for the system tray (the .ico loads at 16x16 which is blurry)
static TRAY_PNG: &[u8] = include_bytes!("../../icons/tray.png");

/// Returns the path to the on-disk tray icon, extracting it from the
/// embedded bytes if it doesn't already exist.  Available as a utility
/// (shortcut icons now come from the exe's embedded resource instead).
#[allow(dead_code)]
fn get_icon_path() -> PathBuf {
    let dir = if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let mut p = PathBuf::from(local_app_data);
        p.push("Dropmazter");
        p
    } else {
        PathBuf::from(".")
    };
    std::fs::create_dir_all(&dir).ok();
    let ico_path = dir.join("tray.ico");
    if !ico_path.exists() {
        std::fs::write(&ico_path, TRAY_ICO).ok();
    }
    ico_path
}

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
    pub const VK_BACK: u32 = 0x08;     // Backspace
    pub const VK_TAB: u32 = 0x09;
    pub const VK_RETURN: u32 = 0x0D;
    pub const VK_ESCAPE: u32 = 0x1B;
    pub const VK_SPACE: u32 = 0x20;
    pub const VK_PRIOR: u32 = 0x21;    // Page Up
    pub const VK_NEXT: u32 = 0x22;     // Page Down
    pub const VK_END: u32 = 0x23;
    pub const VK_HOME: u32 = 0x24;
    pub const VK_INSERT: u32 = 0x2D;
    pub const VK_DELETE: u32 = 0x2E;
    pub const VK_NUMPAD0: u32 = 0x60;
    pub const VK_NUMPAD1: u32 = 0x61;
    pub const VK_NUMPAD2: u32 = 0x62;
    pub const VK_NUMPAD3: u32 = 0x63;
    pub const VK_NUMPAD4: u32 = 0x64;
    pub const VK_NUMPAD5: u32 = 0x65;
    pub const VK_NUMPAD6: u32 = 0x66;
    pub const VK_NUMPAD7: u32 = 0x67;
    pub const VK_NUMPAD8: u32 = 0x68;
    pub const VK_NUMPAD9: u32 = 0x69;
    pub const VK_MULTIPLY: u32 = 0x6A;
    pub const VK_ADD: u32 = 0x6B;
    pub const VK_SUBTRACT: u32 = 0x6D;
    pub const VK_DECIMAL: u32 = 0x6E;
    pub const VK_DIVIDE: u32 = 0x6F;
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
    pub const VK_SHIFT: u32 = 0x10;
    pub const VK_CONTROL: u32 = 0x11;
    pub const VK_MENU: u32 = 0x12;     // Alt
    pub const VK_CAPITAL: u32 = 0x14;
    pub const VK_LWIN: u32 = 0x5B;
    pub const VK_RWIN: u32 = 0x5C;
    pub const VK_APPS: u32 = 0x5D;
    pub const VK_OEM_1: u32 = 0xBA;    // ;:
    pub const VK_OEM_PLUS: u32 = 0xBB; // =+
    pub const VK_OEM_COMMA: u32 = 0xBC;// ,<
    pub const VK_OEM_MINUS: u32 = 0xBD;// -_
    pub const VK_OEM_PERIOD: u32 = 0xBE;// .>
    pub const VK_OEM_2: u32 = 0xBF;    // /?
    pub const VK_OEM_3: u32 = 0xC0;    // `~
    pub const VK_OEM_4: u32 = 0xDB;    // [{
    pub const VK_OEM_5: u32 = 0xDC;    // \|
    pub const VK_OEM_6: u32 = 0xDD;    // ]}
    pub const VK_OEM_7: u32 = 0xDE;    // '"
}

// Parse keybind string to virtual key code and modifiers
// Returns (vk_code, modifiers_mask)
// Modifiers: 1=Ctrl, 2=Alt, 4=Shift, 8=Win
#[cfg(windows)]
fn parse_keybind_to_vk(keybind_str: &str) -> Option<(u32, u32)> {
    let parts: Vec<&str> = keybind_str.split('+').map(|s| s.trim()).collect();
    
    let mut modifiers: u32 = 0;
    let mut key_code: Option<u32> = None;
    let mut fallback_modifier_key: Option<u32> = None;
    
    for part in parts {
        match part.to_uppercase().as_str() {
            "CTRL" | "CONTROL" | "CRTL" | "[CTRL]" | "[CONTROL]" | "[CRTL]" => {
                modifiers |= 1;
                fallback_modifier_key = Some(vk::VK_CONTROL);
            }
            "ALT" | "[ALT]" => {
                modifiers |= 2;
                fallback_modifier_key = Some(vk::VK_MENU);
            }
            "SHIFT" | "[SHIFT]" => {
                modifiers |= 4;
                fallback_modifier_key = Some(vk::VK_SHIFT);
            }
            "WIN" | "META" | "WINDOWS" | "[WIN]" | "[META]" | "[WINDOWS]" => {
                modifiers |= 8;
                fallback_modifier_key = Some(vk::VK_LWIN);
            }
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
            "TAB" | "[TAB]" => key_code = Some(vk::VK_TAB),
            "CAPSLOCK" | "CAPS LOCK" | "[CAPSLOCK]" | "[CAPS LOCK]" => key_code = Some(vk::VK_CAPITAL),
            "SPACE" | "SPACEBAR" | "[SPACE]" => key_code = Some(vk::VK_SPACE),
            "ENTER" | "RETURN" | "[ENTER]" | "[RETURN]" => key_code = Some(vk::VK_RETURN),
            "BACKSPACE" | "BACK" | "[BACKSPACE]" => key_code = Some(vk::VK_BACK),
            "ESC" | "ESCAPE" | "[ESC]" | "[ESCAPE]" => key_code = Some(vk::VK_ESCAPE),
            "INSERT" | "INS" | "[INSERT]" | "[INS]" => key_code = Some(vk::VK_INSERT),
            "DELETE" | "DEL" | "[DELETE]" | "[DEL]" => key_code = Some(vk::VK_DELETE),
            "HOME" | "[HOME]" => key_code = Some(vk::VK_HOME),
            "END" | "[END]" => key_code = Some(vk::VK_END),
            "PAGEUP" | "PAGE UP" | "PGUP" | "[PAGEUP]" | "[PGUP]" => key_code = Some(vk::VK_PRIOR),
            "PAGEDOWN" | "PAGE DOWN" | "PGDN" | "[PAGEDOWN]" | "[PGDN]" => key_code = Some(vk::VK_NEXT),
            "CONTEXTMENU" | "CONTEXT MENU" | "APPS" | "[CONTEXTMENU]" | "[APPS]" => key_code = Some(vk::VK_APPS),
            // Numpad
            "NUM0" | "NUMPAD0" | "[NUM0]" => key_code = Some(vk::VK_NUMPAD0),
            "NUM1" | "NUMPAD1" | "[NUM1]" => key_code = Some(vk::VK_NUMPAD1),
            "NUM2" | "NUMPAD2" | "[NUM2]" => key_code = Some(vk::VK_NUMPAD2),
            "NUM3" | "NUMPAD3" | "[NUM3]" => key_code = Some(vk::VK_NUMPAD3),
            "NUM4" | "NUMPAD4" | "[NUM4]" => key_code = Some(vk::VK_NUMPAD4),
            "NUM5" | "NUMPAD5" | "[NUM5]" => key_code = Some(vk::VK_NUMPAD5),
            "NUM6" | "NUMPAD6" | "[NUM6]" => key_code = Some(vk::VK_NUMPAD6),
            "NUM7" | "NUMPAD7" | "[NUM7]" => key_code = Some(vk::VK_NUMPAD7),
            "NUM8" | "NUMPAD8" | "[NUM8]" => key_code = Some(vk::VK_NUMPAD8),
            "NUM9" | "NUMPAD9" | "[NUM9]" => key_code = Some(vk::VK_NUMPAD9),
            "NUM*" | "NUMPAD*" | "[NUM*]" => key_code = Some(vk::VK_MULTIPLY),
            "NUM+" | "NUMPAD+" | "[NUM+]" => key_code = Some(vk::VK_ADD),
            "NUM-" | "NUMPAD-" | "[NUM-]" => key_code = Some(vk::VK_SUBTRACT),
            "NUM." | "NUMPAD." | "[NUM.]" => key_code = Some(vk::VK_DECIMAL),
            "NUM/" | "NUMPAD/" | "[NUM/]" => key_code = Some(vk::VK_DIVIDE),
            // Symbols
            "=" | "[=]" => key_code = Some(vk::VK_OEM_PLUS),
            "-" | "MINUS" | "[-]" | "[MINUS]" => key_code = Some(vk::VK_OEM_MINUS),
            "," | "[,]" => key_code = Some(vk::VK_OEM_COMMA),
            "." | "[.]" => key_code = Some(vk::VK_OEM_PERIOD),
            "/" | "[/]" => key_code = Some(vk::VK_OEM_2),
            ";" | "[;]" => key_code = Some(vk::VK_OEM_1),
            "'" | "['']" | "[']" => key_code = Some(vk::VK_OEM_7),
            "`" | "TILDE" | "[`]" | "[TILDE]" => key_code = Some(vk::VK_OEM_3),
            "\\" | "[\\]" => key_code = Some(vk::VK_OEM_5),
            "[" | "[[]]" | "[[" => key_code = Some(vk::VK_OEM_4),
            "]" | "[]]" | "]]" => key_code = Some(vk::VK_OEM_6),
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
    
    if key_code.is_none() {
        key_code = fallback_modifier_key;
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
        if (GetAsyncKeyState(vk::VK_LWIN as i32) as u16 & 0x8000) != 0
            || (GetAsyncKeyState(vk::VK_RWIN as i32) as u16 & 0x8000) != 0
        {
            mods |= 8;
        }
    }
    mods
}

#[cfg(windows)]
fn is_key_down(vk_code: u32) -> bool {
    unsafe {
        match vk_code {
            // Treat left/right Win as equivalent for standalone Win bindings.
            vk::VK_LWIN | vk::VK_RWIN => {
                (GetAsyncKeyState(vk::VK_LWIN as i32) as u16 & 0x8000) != 0
                    || (GetAsyncKeyState(vk::VK_RWIN as i32) as u16 & 0x8000) != 0
            }
            _ => (GetAsyncKeyState(vk_code as i32) as u16 & 0x8000) != 0,
        }
    }
}

// Check if capture hotkey is currently pressed (polling-based, doesn't block keys)
#[cfg(windows)]
fn check_capture_hotkey() -> bool {
    let capture_key = CAPTURE_KEY_CODE.load(Ordering::Relaxed);
    let capture_mods = CAPTURE_MODIFIERS.load(Ordering::Relaxed);
    
    {
        // Check if key is currently down
        let key_down = is_key_down(capture_key);
        let current_mods = check_modifiers();

        let was_down = CAPTURE_KEY_WAS_DOWN.load(Ordering::Relaxed);

        // For standalone keys (no modifiers required), ignore extra modifiers
        // so the hotkey works even while holding Shift/Ctrl in-game.
        // For modifier combos (e.g. Alt+K), require at least those modifiers.
        let mods_ok = if capture_mods == 0 {
            true // standalone key — fire regardless of held modifiers
        } else {
            (current_mods & capture_mods) == capture_mods
        };

        if key_down && mods_ok {
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
enum UiEvent { CaptureFortnite, CheckFortnite, Quit, Reconnect, RestartApp, CreateShortcut, OpenLogs }

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
fn capture_fortnite_window(hwnd: HWND, _width: u32, _height: u32) -> Option<DynamicImage> {
    unsafe {
        // Validate the window handle is still valid
        if !IsWindow(hwnd).as_bool() {
            error!("[capture] Invalid window handle - Fortnite window may have been closed/recreated");
            return None;
        }
        
        // Check if window is still visible
        if !IsWindowVisible(hwnd).as_bool() {
            error!("[capture] Window is not visible");
            return None;
        }
        
        // Always get fresh window rect - don't rely on cached dimensions
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            error!("[capture] Failed to get window rect");
            return None;
        }
        
        let w = rect.right - rect.left;
        let h = rect.bottom - rect.top;
        
        if w <= 0 || h <= 0 {
            error!("[capture] Invalid dimensions: {}x{}", w, h);
            return None;
        }
        
        info!("[capture] Capturing Fortnite window directly using PrintWindow ({}x{})", w, h);
        
        // Get DC for the window
        let hdc_window = GetDC(hwnd);
        if hdc_window.is_invalid() {
            error!("[capture] Failed to get window DC");
            return None;
        }
        
        let hdc_mem = CreateCompatibleDC(hdc_window);
        if hdc_mem.is_invalid() {
            ReleaseDC(hwnd, hdc_window);
            error!("[capture] Failed to create compatible DC");
            return None;
        }
        
        let hbitmap = CreateCompatibleBitmap(hdc_window, w, h);
        if hbitmap.is_invalid() {
            let _ = DeleteDC(hdc_mem);
            ReleaseDC(hwnd, hdc_window);
            error!("[capture] Failed to create bitmap");
            return None;
        }
        
        let old_obj = SelectObject(hdc_mem, hbitmap);
        
        // Use PrintWindow to capture the window content directly (ignores overlapping windows)
        // Flag 0x2 = PW_RENDERFULLCONTENT (better for DirectX/hardware accelerated windows)
        const PW_RENDERFULLCONTENT: u32 = 0x2;
        
        let print_result = PrintWindow(hwnd.0 as isize, hdc_mem.0 as isize, PW_RENDERFULLCONTENT);
        
        if print_result == 0 {
            // Try without flags as fallback
            info!("[capture] PW_RENDERFULLCONTENT failed, trying standard PrintWindow");
            let print_result2 = PrintWindow(hwnd.0 as isize, hdc_mem.0 as isize, 0);
            
            if print_result2 == 0 {
                // Final fallback: BitBlt from screen (will include overlapping windows)
                info!("[capture] PrintWindow failed, falling back to screen capture");
                
                let hdc_screen = GetDC(HWND::default());
                if !hdc_screen.is_invalid() {
                    let blt_result = BitBlt(hdc_mem, 0, 0, w, h, hdc_screen, rect.left, rect.top, SRCCOPY);
                    ReleaseDC(HWND::default(), hdc_screen);
                    
                    if blt_result.is_err() {
                        SelectObject(hdc_mem, old_obj);
                        let _ = DeleteObject(hbitmap);
                        let _ = DeleteDC(hdc_mem);
                        ReleaseDC(hwnd, hdc_window);
                        error!("[capture] All capture methods failed");
                        return None;
                    }
                } else {
                    SelectObject(hdc_mem, old_obj);
                    let _ = DeleteObject(hbitmap);
                    let _ = DeleteDC(hdc_mem);
                    ReleaseDC(hwnd, hdc_window);
                    error!("[capture] Failed to get screen DC for fallback");
                    return None;
                }
            }
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
        ReleaseDC(hwnd, hdc_window);
        
        if result == 0 {
            error!("[capture] GetDIBits failed");
            return None;
        }
        
        // Check if PrintWindow returned a black image (common with DirectX games)
        let non_black_pixels = pixels.chunks(4).filter(|p| p[0] != 0 || p[1] != 0 || p[2] != 0).count();
        let total_pixels = (w * h) as usize;
        let black_percentage = 100.0 - (non_black_pixels as f64 / total_pixels as f64 * 100.0);
        
        if black_percentage > 99.0 {
            info!("[capture] PrintWindow returned mostly black ({}%), falling back to screen capture", black_percentage);
            
            // Redo capture with screen BitBlt
            let hdc_screen = GetDC(HWND::default());
            if hdc_screen.is_invalid() {
                error!("[capture] Failed to get screen DC for black image fallback");
                return None;
            }
            
            let hdc_mem2 = CreateCompatibleDC(hdc_screen);
            if hdc_mem2.is_invalid() {
                ReleaseDC(HWND::default(), hdc_screen);
                error!("[capture] Failed to create compatible DC for fallback");
                return None;
            }
            
            let hbitmap2 = CreateCompatibleBitmap(hdc_screen, w, h);
            if hbitmap2.is_invalid() {
                let _ = DeleteDC(hdc_mem2);
                ReleaseDC(HWND::default(), hdc_screen);
                error!("[capture] Failed to create bitmap for fallback");
                return None;
            }
            
            let old_obj2 = SelectObject(hdc_mem2, hbitmap2);
            
            let blt_result = BitBlt(hdc_mem2, 0, 0, w, h, hdc_screen, rect.left, rect.top, SRCCOPY);
            
            if blt_result.is_err() {
                SelectObject(hdc_mem2, old_obj2);
                let _ = DeleteObject(hbitmap2);
                let _ = DeleteDC(hdc_mem2);
                ReleaseDC(HWND::default(), hdc_screen);
                error!("[capture] Screen capture fallback failed");
                return None;
            }
            
            let mut bi2 = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
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
            
            pixels = vec![0; (w * h * 4) as usize];
            
            let result2 = GetDIBits(
                hdc_mem2,
                hbitmap2,
                0,
                h as u32,
                Some(pixels.as_mut_ptr() as *mut _),
                &mut bi2,
                DIB_RGB_COLORS,
            );
            
            SelectObject(hdc_mem2, old_obj2);
            let _ = DeleteObject(hbitmap2);
            let _ = DeleteDC(hdc_mem2);
            ReleaseDC(HWND::default(), hdc_screen);
            
            if result2 == 0 {
                error!("[capture] GetDIBits failed for fallback");
                return None;
            }
            
            info!("[capture] Using screen capture fallback (note: may include overlapping windows)");
        }
        
        // Convert BGRA to RGBA
        for chunk in pixels.chunks_exact_mut(4) {
            chunk.swap(0, 2);
        }
        
        // Create image
        let img = image::RgbaImage::from_raw(w as u32, h as u32, pixels)?;
        info!("[capture] Successfully captured Fortnite {}x{}", w, h);
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
    loop {
        match Client::new(DISCORD_CLIENT_ID) {
            Ok(mut conn) => {
                info!("[discord] Rich Presence connected");

                // Capture timestamp ONCE so Discord shows total session time
                let session_start = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();

                loop {
                    let activity = Activity::new()
                        .set_details("Never get outdropped again!".to_string())
                        .set_state("dropmazter.com".to_string())
                        .set_assets(Assets::new()
                            .set_large_image("logo".to_string())
                            .set_large_text("Dropmazter - Drop Smarter. Land Faster.".to_string()))
                        .set_timestamps(Timestamps::new().set_start(session_start))
                        .set_buttons(vec![
                            Button::new()
                                .set_label("Try Drop Calculator".to_string())
                                .set_url("https://dropmazter.com/".to_string()),
                            Button::new()
                                .set_label("Shop Dropmaps".to_string())
                                .set_url("https://dropmazter.com/map".to_string()),
                        ]);

                    if let Err(e) = conn.set_activity(activity) {
                        warn!("[discord] Connection lost: {:?} - will reconnect", e);
                        break; // Break inner loop to reconnect
                    }

                    std::thread::sleep(Duration::from_secs(15));
                }
            }
            Err(e) => {
                warn!("[discord] Discord not available: {:?} - retrying in 30s", e);
            }
        }
        // Wait before (re)attempting connection
        std::thread::sleep(Duration::from_secs(30));
    }
}

// ---- Auto-Update Check ----
// Disabled for Microsoft Store builds (Store manages its own updates)
#[cfg(not(feature = "msstore"))]
fn check_for_update() {
    use self_update::backends::github::Update;

    let current_version = AGENT_VERSION;

    info!("[update] Checking for updates (current: v{})", current_version);

    // Configure GitHub repo details
    let result = Update::configure()
        .repo_owner("313Wentzy")
        .repo_name("drop-agent")
        .bin_name("Dropmazter-agent")
        .target("")                
        .no_confirm(true)          
        .show_download_progress(true)
        .current_version(current_version)
        .build();

    let updater = match result {
        Ok(u) => u,
        Err(e) => {
            warn!("[update] Failed to build updater: {}", e);
            return;
        }
    };

    match updater.update() {
        Ok(status) => {
            if status.updated() {
                info!(
                    "[update] Updated to version {} - restarting...",
                    status.version()
                );

                // Relaunch the new binary
                if let Ok(exe) = std::env::current_exe() {
                    match std::process::Command::new(&exe).spawn() {
                        Ok(_) => info!("[update] Spawned new version at {:?}", exe),
                        Err(e) => error!("[update] Failed to restart: {} - manual restart required", e),
                    }
                } else {
                    error!("[update] Could not determine exe path - manual restart required");
                }

                std::process::exit(0);
            } else {
                info!("[update] Already up to date (v{})", current_version);
            }
        }
        Err(e) => {
            warn!("[update] Update check failed: {}", e);
        }
    }
}

// ---- Periodic Update Check (every 24 hours) ----
#[cfg(not(feature = "msstore"))]
fn spawn_update_loop() {
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(Duration::from_secs(24 * 60 * 60));
            info!("[update] Periodic update check (24h interval)");
            check_for_update();
        }
    });
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

    // Icon is embedded in the exe via build.rs, so point the shortcut at the exe itself
    let icon_location = format!("{},0", exe_path.display());

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

    // Icon is embedded in the exe via build.rs
    let icon_location = format!("{},0", exe_path.display());

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

// Creates (or re-creates) a desktop shortcut — called from tray menu.
// Unlike create_desktop_shortcut() this always overwrites an existing shortcut.
#[cfg(windows)]
fn create_desktop_shortcut_force() -> Result<()> {
    use std::process::Command;

    let exe_path = std::env::current_exe()?;
    let exe_name = "Dropmazter Agent";

    let desktop = if let Ok(userprofile) = std::env::var("USERPROFILE") {
        PathBuf::from(userprofile).join("Desktop")
    } else {
        anyhow::bail!("Could not find Desktop folder");
    };

    let shortcut_path = desktop.join(format!("{}.lnk", exe_name));

    // Icon is embedded in the exe via build.rs
    let icon_location = format!("{},0", exe_path.display());

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
            Ok(())
        }
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr).to_string();
            warn!("[shortcut] Failed: {}", err);
            anyhow::bail!("PowerShell failed: {}", err)
        }
        Err(e) => {
            warn!("[shortcut] PowerShell error: {}", e);
            anyhow::bail!("PowerShell error: {}", e)
        }
    }
}

#[cfg(not(windows))]
fn create_desktop_shortcut_force() -> Result<()> { Ok(()) }

#[cfg(not(windows))]
fn create_desktop_shortcut() -> Result<()> { Ok(()) }

#[cfg(not(windows))]
fn create_startup_shortcut() -> Result<()> { Ok(()) }

// ---- Add/Remove Programs Registration ----
// Writes an "Uninstall" registry key so the app appears in
// Settings > Apps > Installed Apps (Add or Remove Programs).
#[cfg(windows)]
fn register_in_add_remove_programs() -> Result<()> {
    use winreg::enums::*;
    use winreg::RegKey;

    let exe_path = std::env::current_exe()?;
    let exe_dir = exe_path.parent().unwrap_or(&exe_path);
    let install_location = exe_dir.to_string_lossy().to_string();

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let uninstall_path = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\DropmazterAgent";
    let (key, _disposition) = hkcu.create_subkey(uninstall_path)?;

    key.set_value("DisplayName", &"Dropmazter Agent")?;
    key.set_value("DisplayVersion", &AGENT_VERSION)?;
    key.set_value("Publisher", &"Dropmazter")?;
    key.set_value("InstallLocation", &install_location)?;
    key.set_value("DisplayIcon", &format!("{},0", exe_path.display()))?;
    key.set_value("UninstallString", &format!("\"{}\" --uninstall", exe_path.display()))?;
    key.set_value("QuietUninstallString", &format!("\"{}\" --uninstall --quiet", exe_path.display()))?;
    key.set_value("NoModify", &1u32)?;
    key.set_value("NoRepair", &1u32)?;

    // Estimate installed size in KB (exe size + overhead)
    if let Ok(meta) = std::fs::metadata(&exe_path) {
        let size_kb = (meta.len() / 1024) as u32;
        key.set_value("EstimatedSize", &size_kb)?;
    }

    // URL for "Support" link in Add/Remove Programs
    key.set_value("URLInfoAbout", &"https://dropmazter.com")?;
    key.set_value("HelpLink", &"https://dropmazter.com")?;

    info!("[setup] Registered in Add/Remove Programs");
    Ok(())
}

// Performs a clean uninstall: removes shortcuts, registry entries, and log files.
// Called when the exe is run with `--uninstall`.
#[cfg(windows)]
fn run_uninstall(quiet: bool) -> Result<()> {
    use winreg::enums::*;
    use winreg::RegKey;

    if !quiet {
        // Show a confirmation dialog via PowerShell
        let confirm = std::process::Command::new("powershell")
            .args(["-Command", r#"Add-Type -AssemblyName PresentationFramework; [System.Windows.MessageBox]::Show('Are you sure you want to uninstall Dropmazter Agent?','Uninstall Dropmazter Agent','YesNo','Question')"#])
            .output()?;
        let answer = String::from_utf8_lossy(&confirm.stdout).trim().to_string();
        if answer != "Yes" {
            info!("[uninstall] Cancelled by user");
            return Ok(());
        }
    }

    info!("[uninstall] Starting uninstall...");

    // 1. Remove desktop shortcut
    if let Ok(userprofile) = std::env::var("USERPROFILE") {
        let shortcut = PathBuf::from(&userprofile).join("Desktop").join("Dropmazter Agent.lnk");
        if shortcut.exists() {
            std::fs::remove_file(&shortcut).ok();
            info!("[uninstall] Removed desktop shortcut");
        }
    }

    // 2. Remove startup shortcut
    if let Ok(appdata) = std::env::var("APPDATA") {
        let startup_shortcut = PathBuf::from(&appdata)
            .join("Microsoft\\Windows\\Start Menu\\Programs\\Startup\\Dropmazter Agent.lnk");
        if startup_shortcut.exists() {
            std::fs::remove_file(&startup_shortcut).ok();
            info!("[uninstall] Removed startup shortcut");
        }
    }

    // 3. Remove registry entry (Add/Remove Programs)
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let _ = hkcu.delete_subkey_all(r"Software\Microsoft\Windows\CurrentVersion\Uninstall\DropmazterAgent");
    info!("[uninstall] Removed registry entry");

    // 4. Remove app data (logs, cached icon, marker files)
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let app_dir = PathBuf::from(local_app_data).join("Dropmazter");
        if app_dir.exists() {
            std::fs::remove_dir_all(&app_dir).ok();
            info!("[uninstall] Removed app data: {:?}", app_dir);
        }
    }

    if !quiet {
        let _ = std::process::Command::new("powershell")
            .args(["-Command", r#"Add-Type -AssemblyName PresentationFramework; [System.Windows.MessageBox]::Show('Dropmazter Agent has been uninstalled.','Uninstall Complete','OK','Information')"#])
            .output();
    }

    // 5. Schedule self-deletion (exe deletes itself after exit)
    let exe_path = std::env::current_exe()?;
    let _ = std::process::Command::new("cmd")
        .args(["/C", "timeout", "/t", "2", "/nobreak", ">nul", "&", "del", "/f", "/q",
               &exe_path.to_string_lossy()])
        .spawn();

    info!("[uninstall] Complete");
    Ok(())
}

#[cfg(not(windows))]
fn register_in_add_remove_programs() -> Result<()> { Ok(()) }

#[cfg(not(windows))]
fn run_uninstall(_quiet: bool) -> Result<()> { Ok(()) }

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

            // Only write to console if one is attached (not on startup in release)
            #[cfg(debug_assertions)]
            {
                let _ = writeln!(buf, "[{}] [{}] {}", timestamp, record.level(), record.args());
            }
            #[cfg(not(debug_assertions))]
            {
                let _ = buf; // suppress unused warning
            }
            Ok(())
        })
        .init();

    info!("=== Drop Agent starting… v{} (Fortnite Window Mode) ===", AGENT_VERSION);
    info!("Log file: {:?}", log_path);
}

fn open_logs() {
    let log_path = get_log_path();
    // Open a PowerShell window that tails the log file in real-time
    let _ = std::process::Command::new("cmd")
        .args([
            "/C", "start", "powershell", "-NoExit", "-Command",
            &format!("Get-Content '{}' -Tail 50 -Wait", log_path.display()),
        ])
        .spawn();
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
                    #[cfg(windows)]
                    FORTNITE_IS_RUNNING.store(is_running, Ordering::Relaxed);
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

// ---- dark mode for context menus ----
// Uses undocumented uxtheme API to enable dark context menus on Windows 10+.
// On Windows 11 native menus follow the theme automatically, but this ensures
// it works on Windows 10 as well.
#[cfg(windows)]
fn enable_dark_mode_menus() {
    // SetPreferredAppMode(AllowDark = 1) — uxtheme.dll ordinal 135
    unsafe {
        let lib = windows::Win32::System::LibraryLoader::LoadLibraryW(
            windows::core::w!("uxtheme.dll"),
        );
        if let Ok(hmod) = lib {
            let proc = windows::Win32::System::LibraryLoader::GetProcAddress(
                hmod,
                windows::core::PCSTR(135usize as *const u8),
            );
            if let Some(func) = proc {
                let set_preferred: unsafe extern "system" fn(i32) -> i32 =
                    std::mem::transmute(func);
                set_preferred(1); // 1 = AllowDark
                info!("[theme] Dark mode menus enabled");
            }
        }
    }
}

#[cfg(not(windows))]
fn enable_dark_mode_menus() {}

// ---- tray ----
fn build_tray(proxy: EventLoopProxy<UiEvent>) -> Result<TrayIcon> {
    use tray_icon::menu::PredefinedMenuItem;

    // Enable dark mode for native context menus (matches system theme)
    enable_dark_mode_menus();

    let menu = Menu::new();
    let item_capture = MenuItem::new("Capture Fortnite", true, None);
    let item_check = MenuItem::new("Check Fortnite Status", true, None);
    let item_shortcut = MenuItem::new("Create Desktop Shortcut", true, None);
    let item_eula = MenuItem::new("EULA", true, None);
    let item_logs = MenuItem::new("Open Logs", true, None);
    let item_reconnect = MenuItem::new("Force Reconnect", true, None);
    let item_restart = MenuItem::new("Restart Agent", true, None);
    let item_quit = MenuItem::new("Quit", true, None);

    let id_capture = item_capture.id().clone();
    let id_check = item_check.id().clone();
    let id_shortcut = item_shortcut.id().clone();
    let id_eula = item_eula.id().clone();
    let id_logs = item_logs.id().clone();
    let id_reconnect = item_reconnect.id().clone();
    let id_restart = item_restart.id().clone();
    let id_quit = item_quit.id().clone();

    menu.append(&item_capture)?;
    menu.append(&item_check)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&item_shortcut)?;
    menu.append(&item_eula)?;
    menu.append(&item_logs)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&item_reconnect)?;
    menu.append(&item_restart)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&item_quit)?;

    // Query the actual system-tray icon size (accounts for DPI scaling)
    // and resize our high-res PNG to match exactly, avoiding Windows'
    // low-quality built-in downscaling.
    let tray_size = {
        #[cfg(windows)]
        {
            use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSMICON};
            let sz = unsafe { GetSystemMetrics(SM_CXSMICON) } as u32;
            if sz > 0 { sz } else { 32 }
        }
        #[cfg(not(windows))]
        { 32u32 }
    };
    info!("[tray] System tray icon size: {}x{}", tray_size, tray_size);
    let img = image::load_from_memory(TRAY_PNG)?
        .resize_exact(tray_size, tray_size, image::imageops::FilterType::Lanczos3);
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
                } else if event.id == id_shortcut {
                    let _ = proxy.send_event(UiEvent::CreateShortcut);
                } else if event.id == id_eula {
                    // Open EULA in default browser
                    let _ = std::process::Command::new("cmd")
                        .args(["/C", "start", "https://dropmazter.com/eula/"])
                        .spawn();
                } else if event.id == id_logs {
                    let _ = proxy.send_event(UiEvent::OpenLogs);
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
                    #[cfg(windows)]
                    FORTNITE_IS_RUNNING.store(is_running, Ordering::Relaxed);
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
                    #[cfg(windows)]
                    FORTNITE_IS_RUNNING.store(is_running, Ordering::Relaxed);
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
                    info!("[fortnite] â˜… Status CHANGED: running={}, {}x{}", is_running, width, height);
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
                            // Always get fresh window handle before capture
                            let (hwnd_val, w, h) = if let Some((fresh_hwnd, fresh_w, fresh_h)) = find_fortnite_window() {
                                (fresh_hwnd.0 as isize, fresh_w, fresh_h)
                            } else {
                                // Fortnite not found
                                {
                                    let mut state = fortnite_state.lock().await;
                                    state.is_running = false;
                    #[cfg(windows)]
                    FORTNITE_IS_RUNNING.store(false, Ordering::Relaxed);
                                    state.hwnd = None;
                                }
                                info!("[capture] Fortnite window not found");
                                continue;
                            };
                            
                            // Update state with fresh info
                            {
                                let mut state = fortnite_state.lock().await;
                                state.is_running = true;
                    #[cfg(windows)]
                    FORTNITE_IS_RUNNING.store(true, Ordering::Relaxed);
                                state.hwnd = Some(hwnd_val);
                                state.width = w;
                                state.height = h;
                            }
                            
                            let hwnd = HWND(hwnd_val as *mut _);
                            info!("[capture] Capturing Fortnite window {}x{}", w, h);
                            if let Some(img) = capture_fortnite_window(hwnd, w, h) {
                                let captured_at = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_millis() as u64;
                                let mut jpg_bytes = Cursor::new(Vec::new());
                                let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpg_bytes, 85);
                                if img.write_with_encoder(encoder).is_ok() {
                                    let base64_jpg = base64::engine::general_purpose::STANDARD.encode(jpg_bytes.into_inner());
                                    let sent_at = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_millis() as u64;
                                    let payload = json!({
                                        "type": "screen:capture:result",
                                        "pngBase64": base64_jpg,
                                        "width": w,
                                        "height": h,
                                        "capturedAt": captured_at,
                                        "sentAt": sent_at
                                    });
                                    let mut g = writer_slot.lock().await;
                                    if let Some(wr) = g.as_mut() {
                                        if let Err(e) = wr.send(Message::Text(payload.to_string().into())).await {
                                            error!("[capture] Failed to send: {}", e);
                                        } else {
                                            info!("[capture] Screenshot sent ({}x{}, encode={}ms)", w, h, sent_at - captured_at);
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
                                    Some("screen:status") => {
                                        info!("[ws] Status check requested from server");
                                        let (is_running, width, height) = if let Some((_h, w, ht)) = find_fortnite_window() {
                                            (true, w, ht)
                                        } else {
                                            (false, 0, 0)
                                        };
                                        {
                                            let mut state = fortnite_state.lock().await;
                                            state.is_running = is_running;
                    #[cfg(windows)]
                    FORTNITE_IS_RUNNING.store(is_running, Ordering::Relaxed);
                                            state.width = width;
                                            state.height = height;
                                        }
                                        let payload = json!({
                                            "type": "screen:status:result",
                                            "fortnite_running": is_running,
                                            "width": width,
                                            "height": height
                                        });
                                        let mut g = writer_slot.lock().await;
                                        if let Some(wr) = g.as_mut() {
                                            let _ = wr.send(Message::Text(payload.to_string().into())).await;
                                        }
                                        info!("[ws] Status response sent: running={}, {}x{}", is_running, width, height);
                                    }
                                    Some("screen:capture") => {
                                        info!("[ws] Capture requested from server");
                                        // Always get fresh window handle before capture
                                        let window_info = find_fortnite_window().map(|(h, w, ht)| (h.0 as isize, w, ht));
                                        
                                        if let Some((hwnd_val, w, h)) = window_info {
                                            // Update state with fresh info
                                            {
                                                let mut state = fortnite_state.lock().await;
                                                state.is_running = true;
                    #[cfg(windows)]
                    FORTNITE_IS_RUNNING.store(true, Ordering::Relaxed);
                                                state.hwnd = Some(hwnd_val);
                                                state.width = w;
                                                state.height = h;
                                            }
                                            
                                            let hwnd = HWND(hwnd_val as *mut _);
                                            if let Some(img) = capture_fortnite_window(hwnd, w, h) {
                                                let captured_at = std::time::SystemTime::now()
                                                    .duration_since(std::time::UNIX_EPOCH)
                                                    .unwrap_or_default()
                                                    .as_millis() as u64;
                                                let mut jpg_bytes = Cursor::new(Vec::new());
                                                let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpg_bytes, 85);
                                                if img.write_with_encoder(encoder).is_ok() {
                                                    let base64_jpg = base64::engine::general_purpose::STANDARD.encode(jpg_bytes.into_inner());
                                                    let sent_at = std::time::SystemTime::now()
                                                        .duration_since(std::time::UNIX_EPOCH)
                                                        .unwrap_or_default()
                                                        .as_millis() as u64;
                                                    let payload = json!({
                                                        "type": "screen:capture:result",
                                                        "pngBase64": base64_jpg,
                                                        "width": w,
                                                        "height": h,
                                                        "capturedAt": captured_at,
                                                        "sentAt": sent_at
                                                    });
                                                    let mut g = writer_slot.lock().await;
                                                    if let Some(wr) = g.as_mut() {
                                                        let _ = wr.send(Message::Text(payload.to_string().into())).await;
                                                    }
                                                    info!("[capture] Screenshot sent via WS request (encode={}ms)", sent_at - captured_at);
                                                }
                                            } else {
                                                error!("[capture] Failed to capture window");
                                            }
                                        } else {
                                            warn!("[capture] Fortnite not found");
                                            let mut state = fortnite_state.lock().await;
                                            state.is_running = false;
                    #[cfg(windows)]
                    FORTNITE_IS_RUNNING.store(false, Ordering::Relaxed);
                                            state.hwnd = None;
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
    // Handle --uninstall before anything else (standalone builds only)
    #[cfg(not(feature = "msstore"))]
    {
        let args: Vec<String> = std::env::args().collect();
        if args.iter().any(|a| a == "--uninstall") {
            let quiet = args.iter().any(|a| a == "--quiet");
            return run_uninstall(quiet);
        }
    }

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install ring CryptoProvider");

    init_console();

    let instance = SingleInstance::new("dropmazter_agent")?;
    if !instance.is_single() {
        warn!("Another instance is already running. Exiting.");
        return Ok(());
    }

    // Create shortcuts + register in Add/Remove Programs on first run
    // (not needed for Store builds — Store handles all of this)
    #[cfg(not(feature = "msstore"))]
    {
        let first_run_marker = get_log_path().parent().unwrap().join(".installed");
        if !first_run_marker.exists() {
            info!("[setup] First run detected - creating shortcuts...");
            create_desktop_shortcut()?;
            create_startup_shortcut()?;
            register_in_add_remove_programs().ok();
            std::fs::write(&first_run_marker, "1").ok();
        } else {
            // Always re-register to keep version/path up to date after auto-updates
            register_in_add_remove_programs().ok();
        }
    }

    // Check for updates (not needed for Store builds — Store manages updates)
    #[cfg(not(feature = "msstore"))]
    check_for_update();

    // Re-check for updates every 24 hours while the app is running
    #[cfg(not(feature = "msstore"))]
    spawn_update_loop();

    let rt = Runtime::new()?;

    // Discord presence uses blocking sleep loop — run on a dedicated OS thread, not tokio
    std::thread::spawn(|| run_discord_presence());
    
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

        std::thread::spawn(move || {
            let mut last_log = Instant::now();
            let mut hotkey_log_count = 0u32;

            info!("[hotkey] Polling thread started");

            loop {
                // ~200 polls/sec for responsive hotkeys
                std::thread::sleep(Duration::from_millis(5));

                // 1. Check key press (pure atomic, no locks)
                if !check_capture_hotkey() {
                    continue;
                }

                // 2. Check Fortnite state via atomic (no mutex, no block_on)
                let is_running = FORTNITE_IS_RUNNING.load(Ordering::Relaxed);
                let is_fg = is_fortnite_foreground();

                hotkey_log_count += 1;
                if hotkey_log_count % 10 == 1 || last_log.elapsed() > Duration::from_secs(5) {
                    info!("[hotkey] Key pressed! running={}, foreground={}", is_running, is_fg);
                    last_log = Instant::now();
                }
                
                if !is_running || !is_fg {
                    continue;
                }

                // 3. Check cooldown via atomic (no mutex, no block_on)
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let last_ms = LAST_CAPTURE_EPOCH_MS.load(Ordering::Relaxed);
                let elapsed_ms = now_ms.saturating_sub(last_ms);
                let cooldown_ms = CAPTURE_COOLDOWN.as_millis() as u64;

                if elapsed_ms >= cooldown_ms {
                    info!("[hotkey] Capture triggered - Fortnite is foreground");
                    LAST_CAPTURE_EPOCH_MS.store(now_ms, Ordering::Relaxed);
                    let _ = tx_cmd_for_hotkey.send(Cmd::CaptureFortnite);
                } else {
                    let remaining = (cooldown_ms - elapsed_ms) as f32 / 1000.0;
                    info!("[hotkey] Capture on cooldown ({:.1}s remaining)", remaining);
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
            Event::UserEvent(UiEvent::CreateShortcut) => {
                info!("[tray] Create Desktop Shortcut requested");
                match create_desktop_shortcut_force() {
                    Ok(()) => info!("[tray] Desktop shortcut created successfully"),
                    Err(e) => warn!("[tray] Failed to create desktop shortcut: {}", e),
                }
            }
            Event::UserEvent(UiEvent::OpenLogs) => {
                info!("[tray] Open Logs requested");
                open_logs();
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
                // Use the event loop's exit instead of process::exit so
                // destructors run and the tray icon is cleaned up properly.
                _elwt.exit();
            }
            _ => {}
        }
    })?;

    #[allow(unreachable_code)] 
    Ok(())
}
