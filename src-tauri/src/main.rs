#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    env,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::{
    AppHandle, Manager,
    image::Image,
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
};

const TRAY_ID: &str = "codex-usage";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitWindow {
    used_percent: f64,
    window_duration_mins: u64,
    resets_at: u64,
}

impl RateLimitWindow {
    fn remaining_percent(&self) -> u8 {
        (100.0 - self.used_percent).round().clamp(0.0, 100.0) as u8
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RateLimits {
    primary: Option<RateLimitWindow>,
    secondary: Option<RateLimitWindow>,
    plan_type: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitsResult {
    rate_limits: RateLimits,
}

#[derive(Debug, Deserialize)]
struct RpcResponse {
    id: Option<u64>,
    result: Option<Value>,
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageSnapshot {
    rate_limits: RateLimits,
    fetched_at: u64,
}

#[derive(Default)]
struct MonitorState {
    latest: Option<UsageSnapshot>,
    last_error: Option<String>,
    refreshing: bool,
    display_mode: DisplayMode,
}

type SharedState = Arc<Mutex<MonitorState>>;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
enum DisplayMode {
    #[default]
    Number,
    Circle,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Settings {
    display_mode: DisplayMode,
}

fn main() {
    if env::args().any(|argument| argument == "--probe") {
        let result = locate_codex().and_then(|path| fetch_usage(&path));
        match result {
            Ok(snapshot) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&snapshot).expect("snapshot serialization failed")
                );
                return;
            }
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }

    tauri::Builder::default()
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let state = Arc::new(Mutex::new(MonitorState {
                latest: load_cache(),
                display_mode: load_settings().display_mode,
                ..MonitorState::default()
            }));
            app.manage(state.clone());

            let initial_menu = build_menu(app.handle(), &state)?;
            TrayIconBuilder::with_id(TRAY_ID)
                .tooltip("UsageBar")
                .title("Codex ...")
                .menu(&initial_menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "refresh" => refresh(app.clone()),
                    "display-number" => set_display_mode(app, DisplayMode::Number),
                    "display-circle" => set_display_mode(app, DisplayMode::Circle),
                    "open-usage" => {
                        let _ = Command::new("/usr/bin/open")
                            .arg("https://chatgpt.com/codex/settings/usage")
                            .spawn();
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            refresh(app.handle().clone());
            start_periodic_refresh(app.handle().clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to run UsageBar");
}

fn refresh(app: AppHandle) {
    let state = app.state::<SharedState>().inner().clone();
    {
        let mut current = state.lock().expect("monitor state lock poisoned");
        if current.refreshing {
            return;
        }
        current.refreshing = true;
        current.last_error = None;
    }
    update_tray(&app, &state);

    tauri::async_runtime::spawn_blocking(move || {
        let result = locate_codex().and_then(|path| fetch_usage(&path));
        {
            let mut current = state.lock().expect("monitor state lock poisoned");
            current.refreshing = false;
            match result {
                Ok(snapshot) => {
                    save_cache(&snapshot);
                    current.latest = Some(snapshot);
                    current.last_error = None;
                }
                Err(error) => current.last_error = Some(error),
            }
        }
        update_tray(&app, &state);
    });
}

fn start_periodic_refresh(app: AppHandle) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(300));
            refresh(app.clone());
        }
    });
}

fn fetch_usage(codex: &Path) -> Result<UsageSnapshot, String> {
    let mut child = Command::new(codex)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("Codexを起動できません: {error}"))?;

    let mut stdin = child.stdin.take().ok_or("Codexの標準入力を開けません")?;
    let stdout = child.stdout.take().ok_or("Codexの標準出力を開けません")?;
    write_rpc(
        &mut stdin,
        &json!({
            "method": "initialize",
            "id": 0,
            "params": {
                "clientInfo": {
                    "name": "usage_bar",
                    "title": "UsageBar",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        }),
    )?;

    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        let line = line.map_err(|error| format!("Codex応答の読み取りに失敗しました: {error}"))?;
        let response: RpcResponse = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };

        if let Some(error) = response.error {
            let _ = child.kill();
            return Err(format!("Codexエラー: {}", error.message));
        }

        match response.id {
            Some(0) => {
                write_rpc(
                    &mut stdin,
                    &json!({ "method": "initialized", "params": {} }),
                )?;
                write_rpc(
                    &mut stdin,
                    &json!({ "method": "account/rateLimits/read", "id": 1 }),
                )?;
            }
            Some(1) => {
                let result = response.result.ok_or("Codexの使用量応答が空です")?;
                let result: RateLimitsResult = serde_json::from_value(result)
                    .map_err(|error| format!("Codexの使用量応答を解析できません: {error}"))?;
                let _ = child.kill();
                let _ = child.wait();
                return Ok(UsageSnapshot {
                    rate_limits: result.rate_limits,
                    fetched_at: now_epoch(),
                });
            }
            _ => {}
        }
    }

    let _ = child.kill();
    Err("Codexから使用量を取得できませんでした".into())
}

fn write_rpc(stdin: &mut impl Write, message: &Value) -> Result<(), String> {
    serde_json::to_writer(&mut *stdin, message)
        .map_err(|error| format!("Codexリクエストを作成できません: {error}"))?;
    stdin
        .write_all(b"\n")
        .and_then(|_| stdin.flush())
        .map_err(|error| format!("Codexリクエストを送信できません: {error}"))
}

fn locate_codex() -> Result<PathBuf, String> {
    let mut candidates = vec![
        PathBuf::from("/Applications/Codex.app/Contents/Resources/codex"),
        PathBuf::from("/opt/homebrew/bin/codex"),
        PathBuf::from("/usr/local/bin/codex"),
    ];

    if let Some(home) = env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".local/bin/codex"));
    }
    if let Some(path) = env::var_os("PATH") {
        candidates.extend(env::split_paths(&path).map(|directory| directory.join("codex")));
    }

    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| "Codex CLIが見つかりません".into())
}

fn update_tray(app: &AppHandle, state: &SharedState) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let snapshot = state.lock().expect("monitor state lock poisoned");
    let title = if snapshot.refreshing && snapshot.latest.is_none() {
        "Codex ...".to_string()
    } else if let Some(primary) = snapshot
        .latest
        .as_ref()
        .and_then(|value| value.rate_limits.primary.as_ref())
    {
        format!("Codex {}%", primary.remaining_percent())
    } else {
        "Codex ?".to_string()
    };
    let remaining = snapshot
        .latest
        .as_ref()
        .and_then(|value| value.rate_limits.primary.as_ref())
        .map(RateLimitWindow::remaining_percent);
    match snapshot.display_mode {
        DisplayMode::Number => {
            let _ = tray.set_icon(None);
            let _ = tray.set_title(Some(&title));
        }
        DisplayMode::Circle => {
            let _ = tray.set_title(Some(""));
            let _ = tray.set_icon(Some(circle_icon(remaining.unwrap_or(0))));
            let _ = tray.set_icon_as_template(true);
        }
    }
    drop(snapshot);

    if let Ok(menu) = build_menu(app, state) {
        let _ = tray.set_menu(Some(menu));
    }
}

fn build_menu(app: &AppHandle, state: &SharedState) -> tauri::Result<Menu<tauri::Wry>> {
    let state = state.lock().expect("monitor state lock poisoned");
    let mut items: Vec<Box<dyn tauri::menu::IsMenuItem<tauri::Wry>>> = Vec::new();

    items.push(Box::new(disabled_item(app, "Codex CLI")?));
    if state.refreshing {
        items.push(Box::new(disabled_item(app, "更新中...")?));
    } else if let Some(snapshot) = &state.latest {
        if let Some(primary) = &snapshot.rate_limits.primary {
            add_window_items(app, &mut items, "5時間", primary)?;
        }
        if let Some(secondary) = &snapshot.rate_limits.secondary {
            add_window_items(app, &mut items, "週間", secondary)?;
        }
        items.push(Box::new(disabled_item(app, "状態: 正確")?));
        if let Some(plan) = &snapshot.rate_limits.plan_type {
            items.push(Box::new(disabled_item(
                app,
                &format!("プラン: {}", plan.to_uppercase()),
            )?));
        }
    } else {
        items.push(Box::new(disabled_item(app, "残量: 不明")?));
    }

    if let Some(error) = &state.last_error {
        items.push(Box::new(PredefinedMenuItem::separator(app)?));
        items.push(Box::new(disabled_item(app, &format!("更新失敗: {error}"))?));
    }

    items.push(Box::new(PredefinedMenuItem::separator(app)?));
    items.push(Box::new(disabled_item(app, "表示形式")?));
    items.push(Box::new(CheckMenuItem::with_id(
        app,
        "display-number",
        "数字",
        true,
        state.display_mode == DisplayMode::Number,
        None::<&str>,
    )?));
    items.push(Box::new(CheckMenuItem::with_id(
        app,
        "display-circle",
        "サークル",
        true,
        state.display_mode == DisplayMode::Circle,
        None::<&str>,
    )?));
    items.push(Box::new(PredefinedMenuItem::separator(app)?));
    items.push(Box::new(MenuItem::with_id(
        app,
        "refresh",
        "今すぐ更新",
        !state.refreshing,
        None::<&str>,
    )?));
    items.push(Box::new(MenuItem::with_id(
        app,
        "open-usage",
        "Codex使用状況を開く",
        true,
        None::<&str>,
    )?));
    items.push(Box::new(PredefinedMenuItem::separator(app)?));
    items.push(Box::new(MenuItem::with_id(
        app,
        "quit",
        "終了",
        true,
        None::<&str>,
    )?));

    let references = items.iter().map(|item| item.as_ref()).collect::<Vec<_>>();
    Menu::with_items(app, &references)
}

fn add_window_items(
    app: &AppHandle,
    items: &mut Vec<Box<dyn tauri::menu::IsMenuItem<tauri::Wry>>>,
    label: &str,
    window: &RateLimitWindow,
) -> tauri::Result<()> {
    items.push(Box::new(disabled_item(
        app,
        &format!("{label}残量: {}%", window.remaining_percent()),
    )?));
    items.push(Box::new(disabled_item(
        app,
        &format!("  使用: {:.0}%", window.used_percent),
    )?));
    items.push(Box::new(disabled_item(
        app,
        &format!("  リセット: {}", format_reset_time(window.resets_at)),
    )?));
    Ok(())
}

fn disabled_item(app: &AppHandle, label: &str) -> tauri::Result<MenuItem<tauri::Wry>> {
    MenuItem::new(app, label, false, None::<&str>)
}

fn set_display_mode(app: &AppHandle, display_mode: DisplayMode) {
    let state = app.state::<SharedState>().inner().clone();
    {
        let mut current = state.lock().expect("monitor state lock poisoned");
        current.display_mode = display_mode;
    }
    save_settings(&Settings { display_mode });
    update_tray(app, &state);
}

fn circle_icon(remaining_percent: u8) -> Image<'static> {
    const SIZE: u32 = 32;
    const SAMPLES: u32 = 4;
    const CENTER: f64 = 16.0;
    let progress = f64::from(remaining_percent.clamp(0, 100)) / 100.0;
    let mut rgba = vec![0; (SIZE * SIZE * 4) as usize];

    for y in 0..SIZE {
        for x in 0..SIZE {
            let mut alpha = 0u32;
            for sample_y in 0..SAMPLES {
                for sample_x in 0..SAMPLES {
                    let px = f64::from(x) + (f64::from(sample_x) + 0.5) / f64::from(SAMPLES);
                    let py = f64::from(y) + (f64::from(sample_y) + 0.5) / f64::from(SAMPLES);
                    let dx = px - CENTER;
                    let dy = py - CENTER;
                    let distance = (dx * dx + dy * dy).sqrt();
                    if (10.0..=14.0).contains(&distance) {
                        let angle = dx.atan2(-dy).rem_euclid(std::f64::consts::TAU);
                        alpha += if angle <= progress * std::f64::consts::TAU {
                            255
                        } else {
                            55
                        };
                    }
                }
            }
            let index = ((y * SIZE + x) * 4 + 3) as usize;
            rgba[index] = (alpha / (SAMPLES * SAMPLES)) as u8;
        }
    }

    Image::new_owned(rgba, SIZE, SIZE)
}

fn format_reset_time(timestamp: u64) -> String {
    let output = Command::new("/bin/date")
        .args(["-r", &timestamp.to_string(), "+%m/%d %H:%M"])
        .output();
    output
        .ok()
        .filter(|result| result.status.success())
        .map(|result| String::from_utf8_lossy(&result.stdout).trim().to_string())
        .unwrap_or_else(|| timestamp.to_string())
}

fn cache_path() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support/UsageBar/status.json"))
}

fn legacy_cache_path() -> Option<PathBuf> {
    env::var_os("HOME").map(|home| {
        PathBuf::from(home).join("Library/Application Support/CodexUsageMonitor/status.json")
    })
}

fn settings_path() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support/UsageBar/settings.json"))
}

fn save_settings(settings: &Settings) {
    let Some(path) = settings_path() else { return };
    let Some(directory) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(directory).is_ok()
        && let Ok(data) = serde_json::to_vec(settings)
    {
        let _ = std::fs::write(path, data);
    }
}

fn load_settings() -> Settings {
    settings_path()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

fn save_cache(snapshot: &UsageSnapshot) {
    let Some(path) = cache_path() else { return };
    let Some(directory) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(directory).is_ok() {
        if let Ok(data) = serde_json::to_vec(snapshot) {
            let _ = std::fs::write(path, data);
        }
    }
}

fn load_cache() -> Option<UsageSnapshot> {
    let data = cache_path()
        .and_then(|path| std::fs::read(path).ok())
        .or_else(|| legacy_cache_path().and_then(|path| std::fs::read(path).ok()))?;
    serde_json::from_slice(&data).ok()
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_percent_is_clamped() {
        let over = RateLimitWindow {
            used_percent: 120.0,
            window_duration_mins: 300,
            resets_at: 0,
        };
        let under = RateLimitWindow {
            used_percent: -5.0,
            window_duration_mins: 300,
            resets_at: 0,
        };
        assert_eq!(over.remaining_percent(), 0);
        assert_eq!(under.remaining_percent(), 100);
    }

    #[test]
    fn parses_rate_limit_response() {
        let response: RpcResponse = serde_json::from_value(json!({
            "id": 1,
            "result": {
                "rateLimits": {
                    "primary": { "usedPercent": 6, "windowDurationMins": 300, "resetsAt": 1781723293u64 },
                    "secondary": { "usedPercent": 1, "windowDurationMins": 10080, "resetsAt": 1782310093u64 },
                    "planType": "plus"
                }
            }
        })).unwrap();
        let limits: RateLimitsResult = serde_json::from_value(response.result.unwrap()).unwrap();
        let limits = limits.rate_limits;
        assert_eq!(limits.primary.unwrap().remaining_percent(), 94);
        assert_eq!(limits.secondary.unwrap().remaining_percent(), 99);
        assert_eq!(limits.plan_type.as_deref(), Some("plus"));
    }

    #[test]
    fn circle_icon_has_expected_dimensions() {
        let icon = circle_icon(50);
        assert_eq!(icon.width(), 32);
        assert_eq!(icon.height(), 32);
        assert_eq!(icon.rgba().len(), 32 * 32 * 4);
    }
}
