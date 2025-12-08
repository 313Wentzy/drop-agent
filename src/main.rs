// Console Toggle:
// This will SHOW the console in dev mode (cargo run)
// and HIDE it in release mode (cargo build --release)
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use anyhow::Result;
use log::{error, info, warn};
use std::{sync::Arc, time::{Duration, SystemTime, UNIX_EPOCH}};
use discord_presence_rs::activities::{Activity, ActivityType, Assets, Button, Party, Timestamps, StatusDisplayType};
use discord_presence_rs::discord_connection::Client;
use std::io::Cursor;

use futures_util::{SinkExt, StreamExt};
use futures_util::stream::SplitSink;
use tokio::{runtime::Runtime, time::sleep};
use tokio::sync::{mpsc::{UnboundedReceiver, unbounded_channel}, Mutex};
use tokio_tungstenite::{connect_async, WebSocketStream};
use tokio_tungstenite::tungstenite::Message;

use tray_icon::{Icon, menu::{Menu, MenuItem, MenuEvent}, TrayIcon, TrayIconBuilder};
use winit::event::{Event, StartCause};
use winit::event_loop::{EventLoop, EventLoopBuilder, EventLoopProxy};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use image::{ImageFormat, DynamicImage, ImageOutputFormat};
use screenshots::Screen;
use single_instance::SingleInstance;
use base64::Engine;
use warp::Filter; // For the local web server


type WsWrite = SplitSink<WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, Message>;

const WS_URL: &str = "wss://serverapi-4rtc.onrender.com/agent";
const AGENT_VERSION: &str = "0.4.0-jwt-refresh";
const LOCAL_SERVER_PORT: u16 = 30123;
const TOKEN_REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60); // 10 minutes

// NOTE: This path needs to be updated to a valid location in your final build environment
static TRAY_PNG: &[u8] =
    include_bytes!(r#"C:\Users\Wentzy\Desktop\Dropmazter\DropmazterApp\icons\tray.png"#);

// --- Shared Token State with expiry tracking ---
#[derive(Clone, Debug)]
struct TokenInfo {
    token: String,
    expires_at: SystemTime,
}

type SharedToken = Arc<Mutex<Option<TokenInfo>>>;

// This struct is what we expect to receive from the webpage's POST request
#[derive(Deserialize, Debug)]
struct AuthRequest {
    token: String,
}

#[derive(Serialize)]
struct AuthResponse {
    success: bool,
    message: String,
}

#[derive(Deserialize, Debug)]
struct SettingsRequest {
    monitor: Option<String>,
    keybind: Option<Vec<String>>,
}

#[derive(Serialize)]
struct SettingsResponse {
    success: bool,
    message: String,
}

#[derive(Serialize)]
struct HealthResponse {
    status: String,
    version: String,
}

#[derive(Serialize)]
struct MonitorWithScreenshot {
    id: String,
    name: String,
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    screenshot: String,  // Base64 data URL
}

fn load_tray_icon() -> Icon {
    let img = image::load_from_memory_with_format(TRAY_PNG, ImageFormat::Png)
        .expect("decode tray.png")
        .into_rgba8();
    let (w, h) = img.dimensions();
    Icon::from_rgba(img.into_raw(), w, h).expect("tray icon")
}

#[derive(Debug, Clone)]
enum UiEvent { SendMonitors, CaptureSelected, Quit, Reconnect }

#[derive(Debug)]
enum Cmd { SendMonitors, CaptureSelected, ForceReconnect }

#[derive(Debug, Clone, Serialize)]
struct MonitorDto {
    id: u32,
    label: String,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    scale: f32,
    primary: bool,
}

// ---- Onboarding Helpers  ----
/// Health check endpoint for onboarding to detect if agent is running
async fn handle_health() -> Result<impl warp::Reply, warp::Rejection> {
    Ok(warp::reply::json(&HealthResponse {
        status: "ok".to_string(),
        version: AGENT_VERSION.to_string(),
    }))
}

/// Get all monitors with screenshots for onboarding monitor selection
async fn handle_monitors_screenshots() -> Result<impl warp::Reply, warp::Rejection> {
    let mut results: Vec<MonitorWithScreenshot> = Vec::new();
    
    match Screen::all() {
        Ok(screens) => {
            for screen in screens {
                let info = screen.display_info;
                let id = info.id.to_string();
                let name = format!("Monitor {}", info.id);
                let width = info.width as u32;
                let height = info.height as u32;
                let x = info.x;
                let y = info.y;
                
                // Capture screenshot
                let screenshot = match screen.capture() {
                    Ok(frame) => {
                        let mut cursor = Cursor::new(Vec::new());
                        if DynamicImage::ImageRgba8(frame)
                            .write_to(&mut cursor, ImageOutputFormat::Png)
                            .is_ok()
                        {
                            let buf = cursor.into_inner();
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&buf);
                            format!("data:image/png;base64,{}", b64)
                        } else {
                            String::new()
                        }
                    }
                    Err(e) => {
                        error!("[monitors] Failed to capture screen {}: {}", id, e);
                        String::new()
                    }
                };
                
                results.push(MonitorWithScreenshot {
                    id,
                    name,
                    width,
                    height,
                    x,
                    y,
                    screenshot,
                });
            }
        }
        Err(e) => {
            error!("[monitors] Failed to enumerate screens: {}", e);
        }
    }
    
    info!("[local_server] Returning {} monitors with screenshots", results.len());
    Ok(warp::reply::json(&results))
}

/// Save settings from onboarding (monitor + keybind)
async fn handle_settings_save(
    req: SettingsRequest,
) -> Result<impl warp::Reply, warp::Rejection> {
    info!("[local_server] Received settings - monitor: {:?}, keybind: {:?}", 
          req.monitor, req.keybind);
    
    // TODO: Store these settings persistently
    // For now, just log them
    // You could save to a config file, registry, or SQLite
    
    if let Some(monitor_id) = &req.monitor {
        info!("[settings] Selected monitor: {}", monitor_id);
        // You could update selected_monitor here if you make it globally accessible
    }
    
    if let Some(keys) = &req.keybind {
        info!("[settings] Keybind set: {:?}", keys);
        // You could register a global hotkey here
    }
    
    Ok(warp::reply::json(&SettingsResponse {
        success: true,
        message: "Settings saved".to_string(),
    }))
}

// ---- monitor helpers ----
fn list_monitors() -> anyhow::Result<Vec<MonitorDto>> {
    let mut out = Vec::new();
    for s in Screen::all()? {
        let info = s.display_info;
        let id = info.id as u32;
        let x = info.x;
        let y = info.y;
        let w = info.width as u32;
        let h = info.height as u32;
        let scale = info.scale_factor as f32;
        let label = format!("Monitor {} — {}x{} @ ({},{})", id, w, h, x, y);
        let primary = x == 0 && y == 0;
        out.push(MonitorDto{ id, label, x, y, width: w, height: h, scale, primary });
    }
    out.sort_by_key(|m| (m.y, m.x));
    Ok(out)
}

fn monitors_json() -> anyhow::Result<Value> {
    Ok(json!({ "type": "monitor:list", "monitors": list_monitors()? }))
}

fn capture_png_data_url(monitor_id: u32) -> anyhow::Result<String> {
    let mut target = None;
    for s in Screen::all()? {
        if (s.display_info.id as u32) == monitor_id {
            target = Some(s);
            break;
        }
    }
    let screen = target.ok_or_else(|| anyhow::anyhow!("monitor {} not found", monitor_id))?;
    let frame = screen.capture()?;
    let mut cur = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(frame).write_to(&mut cur, ImageOutputFormat::Png)?;
    let buf = cur.into_inner();
    let b64 = base64::engine::general_purpose::STANDARD.encode(buf);
    Ok(format!("data:image/png;base64,{}", b64))
}

// ---- JWT helpers ----
fn parse_jwt_exp(token: &str) -> Option<SystemTime> {
    // Simple JWT parsing to get expiry time
    // This is a basic implementation - you might want to use a proper JWT library
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    
    // Decode the payload (second part)
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .ok()?;
    
    let payload_str = String::from_utf8(payload).ok()?;
    let payload_json: Value = serde_json::from_str(&payload_str).ok()?;
    
    // Get the exp claim
    let exp = payload_json.get("exp")?.as_u64()?;
    Some(UNIX_EPOCH + Duration::from_secs(exp))
}

fn is_token_expired(token_info: &TokenInfo) -> bool {
    SystemTime::now() >= token_info.expires_at - Duration::from_secs(60) // Refresh 1 minute before expiry
}

// ---- logging / init ----
fn init_console() {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info")
    ).init();
    info!("=== Drop Agent starting… v{} ===", AGENT_VERSION);
}

// ---- tray ----
fn build_tray(proxy: EventLoopProxy<UiEvent>) -> Result<TrayIcon> {
    let menu = Menu::new();
    let item_send = MenuItem::new("Send Monitors Now", true, None);
    let item_cap  = MenuItem::new("Capture Selected", true, None);
    let item_reconnect = MenuItem::new("Force Reconnect", true, None);
    let item_quit = MenuItem::new("Quit", true, None);
    let id_send = item_send.id().clone();
    let id_cap  = item_cap.id().clone();
    let id_reconnect = item_reconnect.id().clone();
    let id_quit = item_quit.id().clone();
    menu.append(&item_send)?;
    menu.append(&item_cap)?;
    menu.append(&item_reconnect)?;
    menu.append(&item_quit)?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Drop Agent v0.4.0")
        .with_icon(load_tray_icon())
        .build()?;

    let proxy_clone = proxy.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if e.id == id_send { let _ = proxy_clone.send_event(UiEvent::SendMonitors); }
        if e.id == id_cap  { let _ = proxy_clone.send_event(UiEvent::CaptureSelected); }
        if e.id == id_reconnect { let _ = proxy_clone.send_event(UiEvent::Reconnect); }
        if e.id == id_quit { let _ = proxy_clone.send_event(UiEvent::Quit); }
    }));

    Ok(tray)
}

fn run_discord_presence() {
    let client_id = "1348039616476876861";

    let mut client = match Client::new(client_id) {
        Ok(client) => {
            info!("[discord] Connected to Discord!");
            client
        }
        Err(e) => {
            warn!("[discord] Failed to connect to Discord IPC: {e}");
            return;
        }
    };

    let start_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let activity = Activity::new()
        .set_details("Drop Mazter Agent".to_string())
        .set_state("Try out our agent now! Included with Calculator +!".to_string())
        .set_activity_type(ActivityType::Playing)
        .set_assets(
            Assets::new()
                .set_large_image("dropmazter_logo_final".to_string())
                .set_large_text("dropmazter.com".to_string()),
        )
        .set_party(Party::new().set_id("party".to_string()).set_size(1, 10))
        .set_buttons(vec![
            Button::new()
                .set_label("Open Drop Mazter".to_string())
                .set_url("https://dropmazter.com".to_string()),
        ])
        .set_status_display_type(StatusDisplayType::Details)
        .set_timestamps(Timestamps::new().set_start(start_time));

    if let Err(e) = client.set_activity(activity) {
        warn!("[discord] Error setting activity: {e}");
    } else {
        info!("[discord] Activity set successfully");
    }
}
// --- Local Web Server Loop ---
async fn local_server_loop(token_state: SharedToken) {
    info!("[local_server] Starting on http://127.0.0.1:{}", LOCAL_SERVER_PORT);
    
    // Filter to extract the shared state
    let state_filter = with_state(token_state);
    
    // === EXISTING: POST /auth route ===
    let auth_route = warp::post()
        .and(warp::path("auth"))
        .and(warp::body::json::<AuthRequest>())
        .and(state_filter)
        .and_then(handle_auth_request);
    
    // === NEW: GET /health route (for onboarding detection) ===
    let health_route = warp::get()
        .and(warp::path("health"))
        .and_then(handle_health);
    
    // === NEW: GET /monitors/screenshots route (for monitor selection) ===
    let monitors_route = warp::get()
        .and(warp::path("monitors"))
        .and(warp::path("screenshots"))
        .and_then(handle_monitors_screenshots);
    
    // === NEW: POST /settings route (for saving onboarding settings) ===
    let settings_route = warp::post()
        .and(warp::path("settings"))
        .and(warp::body::json::<SettingsRequest>())
        .and_then(handle_settings_save);
    
    // Combine all routes
    let routes = auth_route
        .or(health_route)
        .or(monitors_route)
        .or(settings_route);
    
    // Add CORS headers to allow requests from the designated webpage
    let cors = warp::cors()
        .allow_any_origin()  // For development - restrict this in production!
        .allow_methods(vec!["GET", "POST", "OPTIONS"])
        .allow_headers(vec!["Content-Type"]);
    
    let routes_with_cors = routes.with(cors);
    
    // Start serving the routes
    warp::serve(routes_with_cors)
        .run(([127, 0, 0, 1], LOCAL_SERVER_PORT))
        .await;
}

// --- Helper for warp state ---
fn with_state(
    state: SharedToken,
) -> impl Filter<Extract = (SharedToken,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || state.clone())
}

// --- Async handler for auth request ---
async fn handle_auth_request(
    req: AuthRequest,
    token_state: SharedToken,
) -> Result<impl warp::Reply, warp::Rejection> {
    info!("[local_server] Received new token from webpage.");
    
    // Parse token expiry
    let expires_at = parse_jwt_exp(&req.token)
        .unwrap_or_else(|| SystemTime::now() + TOKEN_REFRESH_INTERVAL);
    
    let token_info = TokenInfo {
        token: req.token,
        expires_at,
    };
    
    // Lock the shared state and update the token
    let mut token_guard = token_state.lock().await; 
    *token_guard = Some(token_info);
    
    // Respond successfully
    Ok(warp::reply::json(
        &AuthResponse { 
            success: true, 
            message: "Token stored successfully".to_string() 
        },
    ))
}

// ---- websocket loop with token refresh ----
async fn ws_loop(mut rx: UnboundedReceiver<Cmd>, token_state: SharedToken, url: String) {
    let writer_slot: Arc<Mutex<Option<WsWrite>>> = Arc::new(Mutex::new(None));
    let selected_monitor: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));

    // periodic ping
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

    // Token refresh task
    {
        let token_state_clone = token_state.clone();
        let _writer_clone = writer_slot.clone();
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(60)).await; // Check every minute
                
                let should_refresh = {
                    let token_guard = token_state_clone.lock().await;
                    token_guard.as_ref().map_or(false, |info| is_token_expired(info))
                };
                
                if should_refresh {
                    warn!("[auth] Token is expiring soon, waiting for refresh from webpage...");
                    // In a production system, you might want to request a new token
                    // from the webpage or implement a refresh mechanism
                }
            }
        });
    }

    // command pump
    {
        let writer = writer_slot.clone();
        let sel = selected_monitor.clone();
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    Cmd::SendMonitors => {
                        match monitors_json() {
                            Ok(v) => {
                                let mut g = writer.lock().await;
                                if let Some(w) = g.as_mut() {
                                    let _ = w.send(Message::Text(v.to_string().into())).await;
                                }
                            }
                            Err(e) => error!("[monitors] list failed: {e}"),
                        }
                    }
                    Cmd::CaptureSelected => {
                        let mid = *sel.lock().await;
                        match mid {
                            Some(id) => {
                                match capture_png_data_url(id) {
                                    Ok(data_url) => {
                                        let payload = json!({ "type":"agent:screenshot", "pngBase64": data_url });
                                        let mut g = writer.lock().await;
                                        if let Some(w) = g.as_mut() { 
                                            let _ = w.send(Message::Text(payload.to_string().into())).await; 
                                        }
                                    }
                                    Err(e) => error!("[capture] {}", e),
                                }
                            }
                            None => info!("[capture] no monitor selected"),
                        }
                    }
                    Cmd::ForceReconnect => {
                        info!("[ws] Force reconnect requested");
                        let mut g = writer.lock().await;
                        if let Some(w) = g.as_mut() {
                            let _ = w.close().await;
                        }
                        *g = None;
                    }
                }
            }
        });
    }

    // reconnect forever
    loop {
        // Wait for token before connecting
        let token_info = loop {
            let token_guard = token_state.lock().await;
            if let Some(info) = token_guard.clone() {
                // Check if token is not expired
                if !is_token_expired(&info) {
                    info!("[ws] Token acquired and valid for connection.");
                    break info;
                } else {
                    warn!("[ws] Token is expired, waiting for refresh...");
                }
            }
            
            drop(token_guard); 
            warn!("[ws] No valid token provided. Waiting for webpage to send one...");
            sleep(Duration::from_secs(5)).await;
        };

        info!("[ws] Connecting to {}", url);
        match connect_async(&url).await {
            Ok((ws_stream, _)) => {
                info!("[ws] connected");
                let (write, mut read) = ws_stream.split();
                { 
                    let mut g = writer_slot.lock().await; 
                    *g = Some(write); 
                }

                // Send authenticated hello
                { 
                    let mut g = writer_slot.lock().await;
                    if let Some(w) = g.as_mut() {
                        let hello_msg = json!({
                            "type":"hello",
                            "role":"agent",
                            "version": AGENT_VERSION,
                            "token": token_info.token
                        }).to_string();
                        let _ = w.send(Message::Text(hello_msg.into())).await; 
                        info!("[ws] Sent authentication hello");
                    }
                }

                // Handle incoming messages
                while let Some(msg) = read.next().await {
                    match msg {
                        Ok(Message::Text(txt)) => {
                            if !txt.contains("pong") { // Don't log pong messages
                                info!("[ws] recv: {}", txt);
                            }
                            
                            if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                                match v.get("type").and_then(|t| t.as_str()) {
                                    Some("hello/ok") => {
                                        info!("[auth] Successfully authenticated with server");
                                    }
                                    Some("monitor:list") => { 
                                        match monitors_json() {
                                            Ok(mv) => {
                                                let mut g = writer_slot.lock().await;
                                                if let Some(w) = g.as_mut() {
                                                    let _ = w.send(Message::Text(mv.to_string().into())).await;
                                                }
                                            }
                                            Err(e) => error!("[monitors] list failed: {e}"),
                                        }
                                    }
                                    Some("monitor:set") => {
                                        if let Some(mid) = v.get("monitorId").and_then(|x| x.as_u64()) {
                                            *selected_monitor.lock().await = Some(mid as u32);
                                            info!("[mon] selected {}", mid);
                                        }
                                    }
                                    Some("capture:selected") => {
                                        let mid = *selected_monitor.lock().await;
                                        match mid {
                                            Some(id) => {
                                                match capture_png_data_url(id) {
                                                    Ok(data_url) => {
                                                        let payload = json!({ "type":"capture:image", "pngBase64": data_url });
                                                        let mut g = writer_slot.lock().await;
                                                        if let Some(w) = g.as_mut() { 
                                                            let _ = w.send(Message::Text(payload.to_string().into())).await; 
                                                        }
                                                    }
                                                    Err(e) => error!("[capture] {}", e),
                                                }
                                            }
                                            None => info!("[capture] no monitor selected"),
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
                                        break; // Break from read loop to force reconnect
                                    }
                                    Some("monitors:screenshots") => {
                                        // Capture all monitors with screenshots
                                        let mut results: Vec<Value> = Vec::new();
                                        
                                        if let Ok(screens) = Screen::all() {
                                            for screen in screens {
                                                let info = screen.display_info;
                                                let id = info.id.to_string();
                                                let name = format!("Monitor {}", info.id);
                                                let width = info.width as u32;
                                                let height = info.height as u32;
                                                let x = info.x;
                                                let y = info.y;
                                                
                                                let screenshot = match screen.capture() {
                                                    Ok(frame) => {
                                                        let mut cursor = Cursor::new(Vec::new());
                                                        if DynamicImage::ImageRgba8(frame)
                                                            .write_to(&mut cursor, ImageOutputFormat::Png)
                                                            .is_ok()
                                                        {
                                                            let buf = cursor.into_inner();
                                                            let b64 = base64::engine::general_purpose::STANDARD.encode(&buf);
                                                            format!("data:image/png;base64,{}", b64)
                                                        } else {
                                                            String::new()
                                                        }
                                                    }
                                                    Err(_) => String::new(),
                                                };
                                                
                                                results.push(json!({
                                                    "id": id,
                                                    "name": name,
                                                    "width": width,
                                                    "height": height,
                                                    "x": x,
                                                    "y": y,
                                                    "screenshot": screenshot
                                                }));
                                            }
                                        }
                                        
                                        let payload = json!({
                                            "type": "monitors:screenshots:result",
                                            "monitors": results
                                        });
                                        
                                        let mut g = writer_slot.lock().await;
                                        if let Some(w) = g.as_mut() {
                                            let _ = w.send(Message::Text(payload.to_string().into())).await;
                                        }
                                        
                                        info!("[ws] Sent {} monitor screenshots", results.len());
                                    }
                                    Some("settings:save") => {
                                        // Handle settings save via WebSocket
                                        let monitor = v.get("monitor").and_then(|m| m.as_str()).map(String::from);
                                        let keybind = v.get("keybind").and_then(|k| {
                                            k.as_array().map(|arr| {
                                                arr.iter()
                                                    .filter_map(|v| v.as_str().map(String::from))
                                                    .collect::<Vec<String>>()
                                            })
                                        });
                                        
                                        info!("[ws] Settings received - monitor: {:?}, keybind: {:?}", monitor, keybind);
                                        
                                        // Store settings...
                                        
                                        let response = json!({ "type": "settings:save:ok" });
                                        let mut g = writer_slot.lock().await;
                                        if let Some(w) = g.as_mut() {
                                            let _ = w.send(Message::Text(response.to_string().into())).await;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Ok(Message::Close(_)) => {
                            info!("[ws] Server closed connection");
                            break;
                        }
                        Ok(_) => {}
                        Err(e) => { 
                            error!("[ws] read error: {e}"); 
                            break; 
                        }
                    }
                }
                
                { 
                    let mut g = writer_slot.lock().await; 
                    *g = None; 
                }
                info!("[ws] disconnected, will retry...");
            }
            Err(e) => { 
                error!("[ws] connect error: {e}"); 
            }
        }
        
        // Wait before reconnecting
        sleep(Duration::from_secs(5)).await;
    }
}


// --- main function ---
fn main() -> Result<()> {
    // Required for rustls to work correctly
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install ring CryptoProvider");

    init_console();

    // Ensure only a single instance of the application is running
    let instance = SingleInstance::new("drop_agent_singleton")?;
    if !instance.is_single() { 
        warn!("Another instance is already running. Exiting.");
        return Ok(()); 
    }
    

    // Create the Tokio runtime for async tasks
    let rt = Runtime::new()?;

    rt.spawn(async {
    run_discord_presence(); // runs quickly, returns
});
    
    // This will hold the JWT token, shared between the server and WS loop
    let shared_token: SharedToken = Arc::new(Mutex::new(None));

    // Channel for sending commands from the tray/winit loop to the ws_loop
    let (tx_cmd, rx_cmd) = unbounded_channel::<Cmd>();

    // Spawn the WebSocket connection/reconnection loop in the background
    rt.spawn(ws_loop(rx_cmd, shared_token.clone(), WS_URL.to_string()));

    // Spawn the local HTTP server loop in the background
    rt.spawn(local_server_loop(shared_token.clone()));

    // The rest of the code handles the system tray icon and UI events
    let event_loop: EventLoop<UiEvent> = EventLoopBuilder::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let _tray = build_tray(proxy.clone())?;
    info!("[tray] icon created");
 

    // The winit event loop *must* run on the main thread
    event_loop.run(move |event, _elwt| {
        match event {
            Event::NewEvents(StartCause::Init) => info!("[app] started"),
            Event::UserEvent(UiEvent::SendMonitors) => { 
                let _ = tx_cmd.send(Cmd::SendMonitors); 
            }
            Event::UserEvent(UiEvent::CaptureSelected) => { 
                let _ = tx_cmd.send(Cmd::CaptureSelected); 
            }
            Event::UserEvent(UiEvent::Reconnect) => {
                let _ = tx_cmd.send(Cmd::ForceReconnect);
            }
            Event::UserEvent(UiEvent::Quit) => {
                info!("[tray] Quit requested");
                // Optional: log / flush here if you care
                std::process::exit(0);
            }

            _ => {}
        }
    })?;


    #[allow(unreachable_code)] 
    Ok(())
}