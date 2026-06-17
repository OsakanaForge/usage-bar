#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    env,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::{
    AppHandle, Manager, WebviewUrl, WebviewWindowBuilder,
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
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

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RateLimits {
    primary: Option<RateLimitWindow>,
    secondary: Option<RateLimitWindow>,
    plan_type: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeWindow {
    used_percent: u8,
    resets_label: String,
}

impl ClaudeWindow {
    fn remaining_percent(&self) -> u8 {
        100u8.saturating_sub(self.used_percent.min(100))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeUsage {
    five_hour: ClaudeWindow,
    seven_day: ClaudeWindow,
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
    #[serde(default)]
    claude_usage: Option<ClaudeUsage>,
    fetched_at: u64,
}

#[derive(Default)]
struct MonitorState {
    latest: Option<UsageSnapshot>,
    last_error: Option<String>,
    refreshing: bool,
    display_mode: DisplayMode,
    refresh_interval_minutes: u64,
}

type SharedState = Arc<Mutex<MonitorState>>;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
enum DisplayMode {
    #[default]
    Number,
    Circle,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
#[serde(rename_all = "camelCase")]
struct Settings {
    display_mode: DisplayMode,
    refresh_interval_minutes: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            display_mode: DisplayMode::Number,
            refresh_interval_minutes: 5,
        }
    }
}

fn main() {
    let show_settings_on_launch = env::args().any(|argument| argument == "--settings");
    if env::args().any(|argument| argument == "--probe") {
        let result = fetch_all_usage();
        match result {
            Ok((snapshot, warning)) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&snapshot).expect("snapshot serialization failed")
                );
                if let Some(warning) = warning {
                    eprintln!("{warning}");
                }
                return;
            }
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![get_settings, set_settings])
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let settings = load_settings();
            let state = Arc::new(Mutex::new(MonitorState {
                latest: load_cache(),
                display_mode: settings.display_mode,
                refresh_interval_minutes: settings.refresh_interval_minutes,
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
                    "settings" => show_settings_window(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            refresh(app.handle().clone());
            start_periodic_refresh(app.handle().clone());
            if show_settings_on_launch {
                show_settings_window(app.handle());
            }
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
        let result = fetch_all_usage();
        {
            let mut current = state.lock().expect("monitor state lock poisoned");
            current.refreshing = false;
            match result {
                Ok((snapshot, warning)) => {
                    save_cache(&snapshot);
                    current.latest = Some(snapshot);
                    current.last_error = warning;
                }
                Err(error) => current.last_error = Some(error),
            }
        }
        update_tray(&app, &state);
    });
}

fn start_periodic_refresh(app: AppHandle) {
    std::thread::spawn(move || {
        let mut elapsed_seconds = 0u64;
        loop {
            std::thread::sleep(Duration::from_secs(1));
            elapsed_seconds += 1;
            let interval_seconds = app
                .state::<SharedState>()
                .lock()
                .expect("monitor state lock poisoned")
                .refresh_interval_minutes
                .clamp(1, 60)
                * 60;
            if elapsed_seconds >= interval_seconds {
                elapsed_seconds = 0;
                refresh(app.clone());
            }
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
                    claude_usage: None,
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

fn fetch_all_usage() -> Result<(UsageSnapshot, Option<String>), String> {
    let codex = locate_codex().and_then(|path| fetch_usage(&path));
    let claude = locate_claude().and_then(|path| fetch_claude_usage(&path));
    let mut errors = Vec::new();

    let mut snapshot = match codex {
        Ok(snapshot) => snapshot,
        Err(error) => {
            errors.push(error);
            UsageSnapshot {
                rate_limits: RateLimits::default(),
                claude_usage: None,
                fetched_at: now_epoch(),
            }
        }
    };

    match claude {
        Ok(usage) => snapshot.claude_usage = Some(usage),
        Err(error) => errors.push(error),
    }

    if snapshot.rate_limits.primary.is_none() && snapshot.claude_usage.is_none() {
        return Err(errors.join(" / "));
    }
    let warning = (!errors.is_empty()).then(|| errors.join(" / "));
    Ok((snapshot, warning))
}

fn locate_claude() -> Result<PathBuf, String> {
    let mut candidates = vec![
        PathBuf::from("/opt/homebrew/bin/claude"),
        PathBuf::from("/usr/local/bin/claude"),
    ];
    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        candidates.push(home.join(".local/bin/claude"));
        let node_versions = home.join(".nvm/versions/node");
        if let Ok(entries) = std::fs::read_dir(node_versions) {
            candidates.extend(
                entries
                    .flatten()
                    .map(|entry| entry.path().join("bin/claude")),
            );
        }
    }
    if let Some(path) = env::var_os("PATH") {
        candidates.extend(env::split_paths(&path).map(|directory| directory.join("claude")));
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| "Claude Code CLIが見つかりません".into())
}

fn fetch_claude_usage(claude: &Path) -> Result<ClaudeUsage, String> {
    let probe_directory = cache_path()
        .and_then(|path| {
            path.parent()
                .map(|directory| directory.join("claude-probe"))
        })
        .ok_or("Claude Code用ディレクトリを決定できません")?;
    std::fs::create_dir_all(&probe_directory)
        .map_err(|error| format!("Claude Code用ディレクトリを作成できません: {error}"))?;
    let mut child = Command::new("/usr/bin/script")
        .args(["-q", "/dev/null"])
        .arg(claude)
        .arg("--safe-mode")
        .current_dir(probe_directory)
        .env("TERM", "xterm-256color")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("Claude Codeを起動できません: {error}"))?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or("Claude Codeの標準出力を開けません")?;
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_capture = captured.clone();
    let reader = std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        while let Ok(length) = stdout.read(&mut chunk) {
            if length == 0 {
                break;
            }
            reader_capture
                .lock()
                .expect("Claude output lock poisoned")
                .extend_from_slice(&chunk[..length]);
        }
    });
    let mut stdin = child
        .stdin
        .take()
        .ok_or("Claude Codeの標準入力を開けません")?;
    for _ in 0..30 {
        let startup = {
            let captured = captured.lock().expect("Claude output lock poisoned");
            strip_terminal_sequences(&String::from_utf8_lossy(&captured))
        };
        if startup.contains("safety") && startup.contains("folder") {
            let _ = stdin.write_all(b"\r");
            let _ = stdin.flush();
            std::thread::sleep(Duration::from_secs(1));
            break;
        }
        if startup.contains("Tips") && startup.contains("getting") {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_secs(1));
    stdin
        .write_all(b"/usage\r")
        .and_then(|_| stdin.flush())
        .map_err(|error| format!("Claude Codeへ/usageを送信できません: {error}"))?;
    std::thread::sleep(Duration::from_secs(4));
    let _ = stdin.write_all(b"\x1b");
    let _ = stdin.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = stdin.write_all(b"/exit\r");
    let _ = stdin.flush();
    drop(stdin);

    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while std::time::Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }

    let _ = reader.join();
    let output = captured.lock().expect("Claude output lock poisoned");
    let screen = strip_terminal_sequences(&String::from_utf8_lossy(&output));
    let mut usage = parse_claude_usage(&screen)?;
    usage.plan_type = fetch_claude_plan(claude);
    Ok(usage)
}

fn fetch_claude_plan(claude: &Path) -> Option<String> {
    let output = Command::new(claude)
        .args(["auth", "status", "--json"])
        .output()
        .ok()?;
    let value: Value = serde_json::from_slice(&output.stdout).ok()?;
    value
        .get("subscriptionType")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn strip_terminal_sequences(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b {
            index += 1;
            if index < bytes.len() && bytes[index] == b'[' {
                index += 1;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
                output.push(' ');
            } else if index < bytes.len() && bytes[index] == b']' {
                index += 1;
                while index < bytes.len() && bytes[index] != 0x07 {
                    index += 1;
                }
                index += usize::from(index < bytes.len());
            } else {
                index += usize::from(index < bytes.len());
            }
            continue;
        }
        let byte = bytes[index];
        if byte == b'\r' || byte == b'\n' || byte == b'\t' {
            output.push(' ');
        } else if byte >= 0x20 {
            output.push(byte as char);
        }
        index += 1;
    }
    output.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_claude_usage(screen: &str) -> Result<ClaudeUsage, String> {
    let usage = screen
        .rfind("Current session")
        .map(|index| &screen[index..])
        .ok_or("Claude Codeの使用量画面を解析できません")?;
    let week_index = usage
        .find("Current week")
        .ok_or("Claude Codeの週間使用量が見つかりません")?;
    let session = &usage[..week_index];
    let week = &usage[week_index..];

    Ok(ClaudeUsage {
        five_hour: ClaudeWindow {
            used_percent: percent_before_used(session)?,
            resets_label: session
                .split_whitespace()
                .find(|word| is_clock_time(word))
                .unwrap_or("不明")
                .to_string(),
        },
        seven_day: ClaudeWindow {
            used_percent: percent_before_used(week)?,
            resets_label: extract_week_reset(week).unwrap_or_else(|| "不明".into()),
        },
        plan_type: None,
    })
}

fn percent_before_used(text: &str) -> Result<u8, String> {
    let percent = text
        .find('%')
        .ok_or("Claude Codeの使用率が見つかりません")?;
    let digits = text[..percent]
        .chars()
        .rev()
        .take_while(|character| character.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    digits
        .parse::<u8>()
        .map(|value| value.min(100))
        .map_err(|_| "Claude Codeの使用率を解析できません".into())
}

fn is_clock_time(word: &str) -> bool {
    let word =
        word.trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != ':');
    (word.ends_with("am") || word.ends_with("pm"))
        && word[..word.len().saturating_sub(2)].contains(':')
}

fn extract_week_reset(text: &str) -> Option<String> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let words = text.split_whitespace().collect::<Vec<_>>();
    let index = words.iter().position(|word| MONTHS.contains(word))?;
    Some(words.get(index..index + 4)?.join(" "))
}

fn update_tray(app: &AppHandle, state: &SharedState) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let snapshot = state.lock().expect("monitor state lock poisoned");
    let codex_remaining = snapshot
        .latest
        .as_ref()
        .and_then(|value| value.rate_limits.primary.as_ref())
        .map(RateLimitWindow::remaining_percent);
    let claude_remaining = snapshot
        .latest
        .as_ref()
        .and_then(|value| value.claude_usage.as_ref())
        .map(|usage| usage.five_hour.remaining_percent());
    let title = if snapshot.refreshing && snapshot.latest.is_none() {
        "Usage ...".to_string()
    } else {
        let mut parts = Vec::new();
        if let Some(remaining) = codex_remaining {
            parts.push(format!("Codex {remaining}%"));
        }
        if let Some(remaining) = claude_remaining {
            parts.push(format!("Claude {remaining}%"));
        }
        if parts.is_empty() {
            "Usage ?".to_string()
        } else {
            parts.join(" · ")
        }
    };
    match snapshot.display_mode {
        DisplayMode::Number => {
            let _ = tray.set_icon(None);
            let _ = tray.set_title(Some(&title));
        }
        DisplayMode::Circle => {
            let _ = tray.set_title(Some(""));
            let percentages = [codex_remaining, claude_remaining]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            let _ = tray.set_icon(Some(circle_icon(&percentages)));
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

    items.push(Box::new(PredefinedMenuItem::separator(app)?));
    items.push(Box::new(disabled_item(app, "Claude Code")?));
    if state.refreshing {
        items.push(Box::new(disabled_item(app, "更新中...")?));
    } else if let Some(usage) = state
        .latest
        .as_ref()
        .and_then(|snapshot| snapshot.claude_usage.as_ref())
    {
        add_claude_window_items(app, &mut items, "5時間", &usage.five_hour)?;
        add_claude_window_items(app, &mut items, "週間", &usage.seven_day)?;
        items.push(Box::new(disabled_item(app, "状態: 正確")?));
        if let Some(plan) = &usage.plan_type {
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
    items.push(Box::new(MenuItem::with_id(
        app,
        "refresh",
        "今すぐ更新",
        !state.refreshing,
        None::<&str>,
    )?));
    items.push(Box::new(MenuItem::with_id(
        app,
        "settings",
        "設定…",
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

fn add_claude_window_items(
    app: &AppHandle,
    items: &mut Vec<Box<dyn tauri::menu::IsMenuItem<tauri::Wry>>>,
    label: &str,
    window: &ClaudeWindow,
) -> tauri::Result<()> {
    items.push(Box::new(disabled_item(
        app,
        &format!("{label}残量: {}%", window.remaining_percent()),
    )?));
    items.push(Box::new(disabled_item(
        app,
        &format!("  使用: {}%", window.used_percent),
    )?));
    items.push(Box::new(disabled_item(
        app,
        &format!("  リセット: {}", window.resets_label),
    )?));
    Ok(())
}

fn disabled_item(app: &AppHandle, label: &str) -> tauri::Result<MenuItem<tauri::Wry>> {
    MenuItem::new(app, label, false, None::<&str>)
}

fn show_settings_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("settings") {
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("index.html".into()))
        .title("UsageBar設定")
        .inner_size(440.0, 390.0)
        .resizable(false)
        .center()
        .build();
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, SharedState>) -> Settings {
    let current = state.lock().expect("monitor state lock poisoned");
    Settings {
        display_mode: current.display_mode,
        refresh_interval_minutes: current.refresh_interval_minutes,
    }
}

#[tauri::command]
fn set_settings(
    app: AppHandle,
    state: tauri::State<'_, SharedState>,
    settings: Settings,
) -> Result<(), String> {
    if !(1..=60).contains(&settings.refresh_interval_minutes) {
        return Err("更新間隔は1〜60分で指定してください".into());
    }
    {
        let mut current = state.lock().expect("monitor state lock poisoned");
        current.display_mode = settings.display_mode;
        current.refresh_interval_minutes = settings.refresh_interval_minutes;
    }
    persist_settings(&settings);
    update_tray(&app, state.inner());
    Ok(())
}

fn circle_icon(remaining_percentages: &[u8]) -> Image<'static> {
    const SIZE: u32 = 32;
    const SAMPLES: u32 = 4;
    let count = remaining_percentages.len().max(1) as u32;
    let width = SIZE * count;
    let mut rgba = vec![0; (width * SIZE * 4) as usize];

    for (ring, remaining_percent) in remaining_percentages
        .iter()
        .copied()
        .chain((remaining_percentages.is_empty()).then_some(0))
        .enumerate()
    {
        let center_x = ring as f64 * f64::from(SIZE) + 16.0;
        let progress = f64::from(remaining_percent.clamp(0, 100)) / 100.0;
        for y in 0..SIZE {
            for x in ring as u32 * SIZE..(ring as u32 + 1) * SIZE {
                let mut alpha = 0u32;
                for sample_y in 0..SAMPLES {
                    for sample_x in 0..SAMPLES {
                        let px = f64::from(x) + (f64::from(sample_x) + 0.5) / f64::from(SAMPLES);
                        let py = f64::from(y) + (f64::from(sample_y) + 0.5) / f64::from(SAMPLES);
                        let dx = px - center_x;
                        let dy = py - 16.0;
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
                let index = ((y * width + x) * 4 + 3) as usize;
                rgba[index] = (alpha / (SAMPLES * SAMPLES)) as u8;
            }
        }
    }

    Image::new_owned(rgba, width, SIZE)
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

fn persist_settings(settings: &Settings) {
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
        let icon = circle_icon(&[50, 75]);
        assert_eq!(icon.width(), 64);
        assert_eq!(icon.height(), 32);
        assert_eq!(icon.rgba().len(), 64 * 32 * 4);
    }

    #[test]
    fn parses_claude_usage_screen() {
        let usage = parse_claude_usage(
            "Current session 15% used Resets 6:10am (Asia/Tokyo) Current week (all models) 28% used Resets Jun 24 at 9am (Asia/Tokyo)",
        )
        .unwrap();
        assert_eq!(usage.five_hour.remaining_percent(), 85);
        assert_eq!(usage.five_hour.resets_label, "6:10am");
        assert_eq!(usage.seven_day.remaining_percent(), 72);
        assert_eq!(usage.seven_day.resets_label, "Jun 24 at 9am");
    }

    #[test]
    fn old_settings_receive_default_refresh_interval() {
        let settings: Settings = serde_json::from_str(r#"{"displayMode":"circle"}"#).unwrap();
        assert_eq!(settings.display_mode, DisplayMode::Circle);
        assert_eq!(settings.refresh_interval_minutes, 5);
    }
}
