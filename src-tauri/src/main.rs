#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use notify::Watcher;
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_tungstenite::tungstenite::Message;

#[derive(Serialize, Deserialize, Clone)]
struct ScriptEntry {
    name: String,
    path: String,
}

#[derive(Serialize, Clone)]
struct ClientInfo {
    pid: i32,
    label: String,
}

#[derive(Serialize, Clone)]
struct AttachResult {
    pid: i32,
    ok: bool,
    msg: String,
    label: String,
}

#[derive(Serialize, Clone)]
struct OutputEvent {
    pid: i32,
    status: i32,
    msg: String,
}

#[derive(Serialize, Clone)]
struct FileContent {
    path: String,
    content: String,
}

struct AppState {
    clients: Arc<RwLock<HashMap<i32, mpsc::UnboundedSender<String>>>>,
    usernames: Arc<RwLock<HashMap<i32, String>>>,
    auto_inject: Arc<std::sync::atomic::AtomicBool>,
    injecting: Arc<Mutex<bool>>,
    injected_pids: Arc<RwLock<HashSet<i32>>>,
    credentials: Arc<RwLock<Option<(String, String)>>>,
}

impl AppState {
    fn new() -> Self {
        let creds = read_saved_credentials();
        Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
            usernames: Arc::new(RwLock::new(HashMap::new())),
            auto_inject: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            injecting: Arc::new(Mutex::new(false)),
            injected_pids: Arc::new(RwLock::new(HashSet::new())),
            credentials: Arc::new(RwLock::new(creds)),
        }
    }
}

fn cosmic_dir() -> PathBuf {
    PathBuf::from(r"C:\Cosmic")
}

fn scripts_dir() -> PathBuf {
    std::env::current_exe()
        .unwrap_or_default()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
        .join("workspace")
}

fn settings_path() -> PathBuf {
    cosmic_dir().join("ui_settings.json")
}

fn credentials_path() -> PathBuf {
    cosmic_dir().join("Credentials.dat")
}

fn write_credentials(username: &str, password: &str) -> bool {
    let dir = cosmic_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    let content = format!(
        "{{\n  \"username\": \"{}\",\n  \"password\": \"{}\"\n}}",
        username.replace('\\', "\\\\").replace('"', "\\\""),
        password.replace('\\', "\\\\").replace('"', "\\\"")
    );
    std::fs::write(credentials_path(), content).is_ok()
}

fn read_saved_credentials() -> Option<(String, String)> {
    let content = std::fs::read_to_string(credentials_path()).ok()?;
    let v: Value = serde_json::from_str(&content).ok()?;
    let user = v["username"].as_str()?.to_string();
    let pass = v["password"].as_str()?.to_string();
    if user.is_empty() {
        return None;
    }
    Some((user, pass))
}

fn get_roblox_pids() -> Vec<u32> {
    use sysinfo::{ProcessExt, System, SystemExt};
    let mut sys = System::new_all();
    sys.refresh_all();
    sys.processes()
        .iter()
        .filter(|(_, p)| {
            let n = p.name().to_lowercase();
            n == "robloxplayerbeta.exe" || n == "windows10universal.exe"
        })
        .map(|(pid, _)| {
            use sysinfo::PidExt;
            pid.as_u32()
        })
        .collect()
}

fn get_process_uptime_secs(pid: u32) -> u64 {
    use sysinfo::{PidExt, ProcessExt, System, SystemExt};
    let mut sys = System::new_all();
    sys.refresh_all();
    let spid = sysinfo::Pid::from_u32(pid);
    if let Some(proc) = sys.process(spid) {
        let start = proc.start_time();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        return now.saturating_sub(start);
    }
    u64::MAX
}

fn ensure_resources_extracted(_app_handle: &tauri::AppHandle) {
    let _ = std::fs::create_dir_all(cosmic_dir());
}

fn run_injector(app_handle: &tauri::AppHandle, pid: u32) -> i32 {
    ensure_resources_extracted(app_handle);
    let dir = cosmic_dir();
    let exe = dir.join("Cosmic-Injector.exe");
    if !exe.exists() {
        return -2;
    }
    let dll = dir.join("Cosmic-Module.dll");
    if !dll.exists() {
        return -2;
    }
    match std::process::Command::new(&exe)
        .arg(pid.to_string())
        .current_dir(&dir)
        .output()
    {
        Ok(o) => o.status.code().unwrap_or(-1),
        Err(_) => -1,
    }
}

fn attach_msg(code: i32) -> &'static str {
    match code {
        6  => "Success",
        -2 => "Injector or module not found",
        -1 => "Initialization failure",
        0  => "Failed to open Roblox process",
        1  => "Roblox version mismatch",
        2  => "Module DLL not found or corrupt",
        3  => "Memory operation failed",
        4  => "PDB download failed",
        5  => "Injection timeout",
        _  => "Unknown error",
    }
}

async fn fetch_username(user_id: i64) -> Option<String> {
    let url = format!("https://users.roblox.com/v1/users/{}", user_id);
    let resp = reqwest::get(&url).await.ok()?;
    let json: Value = resp.json().await.ok()?;
    json["name"].as_str().map(|s| s.to_string())
}

async fn get_label(state: &AppState, pid: i32) -> String {
    state
        .usernames
        .read()
        .await
        .get(&pid)
        .cloned()
        .unwrap_or_else(|| format!("PID {}", pid))
}

fn build_script_list() -> Vec<ScriptEntry> {
    let dir = scripts_dir();
    if !dir.exists() {
        return vec![];
    }
    let valid = ["lua", "luau", "txt"];
    let mut list: Vec<ScriptEntry> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| {
                let p = e.path();
                if !p.is_file() {
                    return false;
                }
                p.extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| valid.contains(&ext.to_lowercase().as_str()))
                    .unwrap_or(false)
            })
            .map(|e| {
                let p = e.path();
                ScriptEntry {
                    name: p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                    path: p.to_string_lossy().to_string(),
                }
            })
            .collect(),
        Err(_) => vec![],
    };
    list.sort_by(|a, b| a.name.cmp(&b.name));
    list
}

async fn broadcast(state: &AppState, msg: &str) {
    let clients = state.clients.read().await;
    for tx in clients.values() {
        let _ = tx.send(msg.to_string());
    }
}

async fn send_to(state: &AppState, pid: i32, msg: &str) {
    if let Some(tx) = state.clients.read().await.get(&pid) {
        let _ = tx.send(msg.to_string());
    }
}

async fn emit_clients(ah: &tauri::AppHandle, state: &AppState) {
    let clients = state.clients.read().await;
    let usernames = state.usernames.read().await;
    let list: Vec<ClientInfo> = clients
        .keys()
        .map(|&pid| ClientInfo {
            pid,
            label: usernames.get(&pid).cloned().unwrap_or_else(|| format!("PID {}", pid)),
        })
        .collect();
    let _ = ah.emit("clients", list);
}

async fn on_ws_message(ah: &tauri::AppHandle, state: &AppState, pid: i32, text: &str) {
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return,
    };
    if v.get("Message").is_some() {
        let status = v["Status"].as_i64().unwrap_or(0) as i32;
        let msg = v["Message"].as_str().unwrap_or("").to_string();
        let _ = ah.emit("output", OutputEvent { pid, status, msg });
        return;
    }
    if let Some(uid) = v.get("UserId").and_then(|u| u.as_i64()) {
        let game_id = v["GameId"].as_i64().unwrap_or(0);
        if let Some(name) = fetch_username(uid).await {
            state.usernames.write().await.insert(pid, name.clone());
            let _ = ah.emit("user_info", serde_json::json!({"pid": pid, "label": name, "userId": uid, "gameId": game_id}));
            emit_clients(ah, state).await;
        }
    }
}

async fn handle_ws_connection(stream: tokio::net::TcpStream, ah: tauri::AppHandle, state: Arc<AppState>) {
    use std::sync::Mutex as StdMutex;
    let pid_store: Arc<StdMutex<i32>> = Arc::new(StdMutex::new(-1));
    let pid_clone = pid_store.clone();
    let ws = match tokio_tungstenite::accept_hdr_async(stream, |req: &tokio_tungstenite::tungstenite::handshake::server::Request, res: tokio_tungstenite::tungstenite::handshake::server::Response| {
        if let Some(v) = req.headers().get("process-id") {
            if let Ok(s) = v.to_str() {
                if let Ok(p) = s.parse::<i32>() {
                    *pid_clone.lock().unwrap() = p;
                }
            }
        }
        Ok(res)
    }).await {
        Ok(ws) => ws,
        Err(_) => return,
    };
    let pid = *pid_store.lock().unwrap();
    if pid < 0 { return; }
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    state.clients.write().await.insert(pid, tx);
    let label = get_label(&state, pid).await;
    let _ = ah.emit("client_connected", ClientInfo { pid, label });
    emit_clients(&ah, &state).await;
    let write_handle = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sink.send(Message::Text(msg)).await.is_err() { break; }
        }
    });
    while let Some(Ok(msg)) = stream.next().await {
        if let Message::Text(text) = msg {
            on_ws_message(&ah, &state, pid, &text).await;
        }
    }
    write_handle.abort();
    state.clients.write().await.remove(&pid);
    state.usernames.write().await.remove(&pid);
    state.injected_pids.write().await.remove(&pid);
    let _ = ah.emit("client_disconnected", pid);
    emit_clients(&ah, &state).await;
}

async fn run_ws_server(ah: tauri::AppHandle, state: Arc<AppState>) {
    let listener = match TcpListener::bind("127.0.0.1:24950").await {
        Ok(l) => l,
        Err(_) => return,
    };
    loop {
        if let Ok((stream, _)) = listener.accept().await {
            tauri::async_runtime::spawn(handle_ws_connection(stream, ah.clone(), state.clone()));
        }
    }
}

async fn do_inject(ah: &tauri::AppHandle, state: &AppState, pids: &[u32]) {
    {
        let mut inj = state.injecting.lock().await;
        if *inj { return; }
        *inj = true;
    }
    if let Some((user, pass)) = state.credentials.read().await.clone() {
        write_credentials(&user, &pass);
    }
    let _ = ah.emit("inject_start", ());
    for &pid in pids {
        let ah_clone = ah.clone();
        let code = tokio::task::spawn_blocking(move || run_injector(&ah_clone, pid)).await.unwrap_or(-1);
        let ok = code == 6;
        if ok {
            state.injected_pids.write().await.insert(pid as i32);
        } else {
            state.injected_pids.write().await.remove(&(pid as i32));
        }
        let label = get_label(state, pid as i32).await;
        let _ = ah.emit("attach_result", AttachResult { pid: pid as i32, ok, msg: attach_msg(code).to_string(), label });
    }
    emit_clients(ah, state).await;
    *state.injecting.lock().await = false;
    let _ = ah.emit("inject_end", ());
}

async fn auto_inject_tick(ah: &tauri::AppHandle, state: &AppState) {
    if state.credentials.read().await.is_none() { return; }
    let all_pids = get_roblox_pids();
    {
        let mut inj = state.injected_pids.write().await;
        inj.retain(|&p| all_pids.contains(&(p as u32)));
    }
    let already: HashSet<i32> = state.injected_pids.read().await.clone();
    let ready: Vec<u32> = all_pids.iter()
        .filter(|&&p| !already.contains(&(p as i32)))
        .copied()
        .filter(|&p| get_process_uptime_secs(p) >= 8)
        .collect();
    if ready.is_empty() { return; }
    for &p in &ready {
        state.injected_pids.write().await.insert(p as i32);
    }
    do_inject(ah, state, &ready).await;
}

async fn run_auto_inject_loop(ah: tauri::AppHandle, state: Arc<AppState>) {
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    loop {
        interval.tick().await;
        if !state.auto_inject.load(std::sync::atomic::Ordering::Relaxed) { continue; }
        if *state.injecting.lock().await { continue; }
        auto_inject_tick(&ah, &state).await;
    }
}

fn start_file_watcher(ah: tauri::AppHandle) {
    let dir = scripts_dir();
    let _ = std::fs::create_dir_all(&dir);
    let (tx, rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = match notify::recommended_watcher(tx) {
        Ok(w) => w,
        Err(_) => return,
    };
    if watcher.watch(&dir, notify::RecursiveMode::NonRecursive).is_err() { return; }
    std::thread::spawn(move || {
        let _watcher = watcher;
        for event in rx {
            if let Ok(ev) = event {
                let refresh = matches!(ev.kind,
                    notify::EventKind::Create(_) | notify::EventKind::Remove(_) |
                    notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
                );
                if refresh {
                    let scripts = build_script_list();
                    let _ = ah.emit("scripts", &scripts);
                }
            }
        }
    });
}

fn extract_resources_setup(_app: &tauri::App) {
    let _ = std::fs::create_dir_all(cosmic_dir());
}

#[tauri::command]
async fn set_credentials(state: tauri::State<'_, Arc<AppState>>, username: String, password: String) -> Result<bool, ()> {
    if username.is_empty() {
        let _ = std::fs::remove_file(credentials_path());
        *state.credentials.write().await = None;
        return Ok(true);
    }
    let ok = write_credentials(&username, &password);
    if ok {
        *state.credentials.write().await = Some((username, password));
    }
    Ok(ok)
}

#[tauri::command]
async fn get_saved_credentials() -> Result<Value, ()> {
    match read_saved_credentials() {
        Some((user, _)) => Ok(serde_json::json!({ "username": user, "hasCredentials": true })),
        None => Ok(serde_json::json!({ "username": "", "hasCredentials": false })),
    }
}

#[tauri::command]
async fn execute_script(state: tauri::State<'_, Arc<AppState>>, pid: i32, script: String) -> Result<(), ()> {
    if script.trim().is_empty() { return Ok(()); }
    if pid > 0 { send_to(&state, pid, &script).await; } else { broadcast(&state, &script).await; }
    Ok(())
}

#[tauri::command]
async fn kill_client(ah: tauri::AppHandle, state: tauri::State<'_, Arc<AppState>>, pid: i32) -> Result<(), ()> {
    state.clients.write().await.remove(&pid);
    state.usernames.write().await.remove(&pid);
    state.injected_pids.write().await.remove(&pid);
    let _ = ah.emit("client_disconnected", pid);
    emit_clients(&ah, &state).await;

    if pid > 0 {
        tokio::task::spawn_blocking(move || {
            use sysinfo::{PidExt, ProcessExt, System, SystemExt};
            let mut sys = System::new_all();
            sys.refresh_all();
            let spid = sysinfo::Pid::from_u32(pid as u32);
            if let Some(proc) = sys.process(spid) {
                proc.kill();
            }
        }).await.ok();
    }

    Ok(())
}

#[tauri::command]
async fn save_to_workspace(name: String, content: String) -> Result<String, String> {
    let dir = scripts_dir();
    let _ = std::fs::create_dir_all(&dir);
    let safe_name = name.trim().to_string();
    let safe_name = if safe_name.is_empty() { "script".to_string() } else { safe_name };
    let fname = if safe_name.ends_with(".lua") || safe_name.ends_with(".luau") {
        safe_name
    } else {
        format!("{}.lua", safe_name)
    };
    let path = dir.join(&fname);
    std::fs::write(&path, content).map_err(|e| e.to_string())?;
    Ok(fname)
}

#[tauri::command]
async fn attach_roblox(ah: tauri::AppHandle, state: tauri::State<'_, Arc<AppState>>) -> Result<(), ()> {
    if state.credentials.read().await.is_none() {
        if let Some(saved) = read_saved_credentials() {
            *state.credentials.write().await = Some(saved);
        } else {
            let _ = ah.emit("output", OutputEvent { pid: 0, status: 2, msg: "No credentials set.".into() });
            let _ = ah.emit("need_credentials", ());
            return Ok(());
        }
    }
    let pids = get_roblox_pids();
    if pids.is_empty() {
        let _ = ah.emit("output", OutputEvent { pid: 0, status: 2, msg: "No Roblox processes found.".into() });
        return Ok(());
    }
    let st = state.inner().clone();
    tauri::async_runtime::spawn(async move { do_inject(&ah, &st, &pids).await; });
    Ok(())
}

#[tauri::command]
async fn get_clients_list(state: tauri::State<'_, Arc<AppState>>) -> Result<Vec<ClientInfo>, ()> {
    let clients = state.clients.read().await;
    let usernames = state.usernames.read().await;
    Ok(clients.keys().map(|&pid| ClientInfo {
        pid,
        label: usernames.get(&pid).cloned().unwrap_or_else(|| format!("PID {}", pid)),
    }).collect())
}

#[tauri::command]
async fn get_scripts_list() -> Result<Vec<ScriptEntry>, ()> {
    Ok(build_script_list())
}

#[tauri::command]
async fn read_file(ah: tauri::AppHandle, path: String) -> Result<(), ()> {
    match std::fs::read_to_string(&path) {
        Ok(content) => { let _ = ah.emit("file_content", FileContent { path, content }); }
        Err(e) => { let _ = ah.emit("output", OutputEvent { pid: 0, status: 2, msg: format!("Read error: {}", e) }); }
    }
    Ok(())
}

#[tauri::command]
async fn save_settings_cmd(json: String) -> Result<(), ()> {
    let p = settings_path();
    let _ = std::fs::create_dir_all(p.parent().unwrap_or(&p));
    let _ = std::fs::write(&p, json);
    Ok(())
}

#[tauri::command]
async fn load_settings_cmd() -> Result<String, ()> {
    Ok(std::fs::read_to_string(settings_path()).unwrap_or_else(|_| "{}".into()))
}

#[tauri::command]
async fn set_auto_inject(state: tauri::State<'_, Arc<AppState>>, enabled: bool) -> Result<(), ()> {
    state.auto_inject.store(enabled, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

#[tauri::command]
async fn open_scripts_folder() -> Result<(), ()> {
    let dir = scripts_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::process::Command::new("explorer").arg(&dir).spawn();
    Ok(())
}

#[tauri::command]
async fn launch_roblox() -> Result<(), ()> {
    let _ = std::process::Command::new("cmd").args(["/c", "start", "", "roblox-player://"]).spawn();
    Ok(())
}

#[tauri::command]
async fn minimize_window(ah: tauri::AppHandle) -> Result<(), ()> {
    if let Some(w) = ah.get_webview_window("main") { let _ = w.minimize(); }
    Ok(())
}

#[tauri::command]
async fn maximize_window(ah: tauri::AppHandle) -> Result<(), ()> {
    if let Some(w) = ah.get_webview_window("main") {
        let maximized = w.is_maximized().unwrap_or(false);
        if maximized { let _ = w.unmaximize(); } else { let _ = w.maximize(); }
    }
    Ok(())
}

#[tauri::command]
async fn close_window(ah: tauri::AppHandle) -> Result<(), ()> {
    ah.exit(0);
    Ok(())
}

#[tauri::command]
async fn drag_window(ah: tauri::AppHandle) -> Result<(), ()> {
    if let Some(w) = ah.get_webview_window("main") { let _ = w.start_dragging(); }
    Ok(())
}

#[tauri::command]
async fn set_always_on_top(ah: tauri::AppHandle, enabled: bool) -> Result<(), ()> {
    if let Some(w) = ah.get_webview_window("main") { let _ = w.set_always_on_top(enabled); }
    Ok(())
}

fn main() {
    let state = Arc::new(AppState::new());
    tauri::Builder::default()
        .manage(state.clone())
        .setup(move |app| {
            extract_resources_setup(app);
            let sd = scripts_dir();
            let _ = std::fs::create_dir_all(&sd);
            let ah = app.handle().clone();
            let st = state.clone();
            tauri::async_runtime::spawn(run_ws_server(ah.clone(), st.clone()));
            tauri::async_runtime::spawn(run_auto_inject_loop(ah.clone(), st.clone()));
            let ah2 = ah.clone();
            let st2 = st.clone();
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(3));
                loop { interval.tick().await; emit_clients(&ah2, &st2).await; }
            });
            start_file_watcher(ah.clone());
            let ah3 = ah.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_millis(600)).await;
                let _ = ah3.emit("scripts", &build_script_list());
                let settings = std::fs::read_to_string(settings_path()).unwrap_or_else(|_| "{}".into());
                let _ = ah3.emit("load_settings", settings);
                let creds_info = match read_saved_credentials() {
                    Some((user, _)) => serde_json::json!({ "username": user, "hasCredentials": true }),
                    None => serde_json::json!({ "username": "", "hasCredentials": false }),
                };
                let _ = ah3.emit("credentials_status", creds_info);
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            execute_script, attach_roblox, kill_client, get_clients_list,
            get_scripts_list, read_file, save_settings_cmd, load_settings_cmd,
            set_auto_inject, open_scripts_folder, launch_roblox, minimize_window,
            maximize_window, close_window, drag_window, set_always_on_top,
            set_credentials, get_saved_credentials, save_to_workspace,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
