#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    env,
    fs::OpenOptions,
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
use tauri_plugin_updater::UpdaterExt;

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

    /// 枠の名前は位置（primary/secondary）ではなく実際の長さから決める。
    /// Codexは枠の構成を変えることがあり、primary＝5時間とは限らない。
    fn window_label(&self) -> String {
        const DAY_MINS: u64 = 24 * 60;
        match self.window_duration_mins {
            0 => "枠".to_string(),
            mins if mins % (7 * DAY_MINS) == 0 => match mins / (7 * DAY_MINS) {
                1 => "週間".to_string(),
                weeks => format!("{weeks}週間"),
            },
            mins if mins % DAY_MINS == 0 => format!("{}日", mins / DAY_MINS),
            mins if mins % 60 == 0 => format!("{}時間", mins / 60),
            mins => format!("{mins}分"),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RateLimits {
    primary: Option<RateLimitWindow>,
    secondary: Option<RateLimitWindow>,
    plan_type: Option<String>,
}

impl RateLimits {
    fn windows(&self) -> impl Iterator<Item = &RateLimitWindow> {
        [self.primary.as_ref(), self.secondary.as_ref()]
            .into_iter()
            .flatten()
    }

    /// メニューバーとしきい値通知に使う枠。
    /// 一番短い枠＝いま枯れると困る枠なので、それを代表値にする。
    fn headline(&self) -> Option<&RateLimitWindow> {
        self.windows()
            .min_by_key(|window| window.window_duration_mins)
    }
}

/// `/usage` はリセット時刻を "1:10am" / "Aug 5 at 6pm" のような表示文字列でしか出さない。
/// 解析できなかったときはこの値を入れ、リセット判定の材料から除外する。
const UNKNOWN_RESET_LABEL: &str = "不明";

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
    refresh_interval_seconds: u64,
    codex_threshold: u8,
    claude_threshold: u8,
    remaining_notifications_enabled: bool,
    reset_notifications_enabled: bool,
    codex_notified: bool,
    claude_notified: bool,
    codex_enabled: bool,
    claude_enabled: bool,
    update_frequency: UpdateFrequency,
    five_hour_reset_label: Option<String>,
    seven_day_reset_label: Option<String>,
}

type SharedState = Arc<Mutex<MonitorState>>;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
enum DisplayMode {
    #[default]
    Number,
    Circle,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
enum UpdateFrequency {
    #[default]
    Daily,
    Weekly,
    Monthly,
}

impl UpdateFrequency {
    fn seconds(self) -> u64 {
        match self {
            UpdateFrequency::Daily => 24 * 60 * 60,
            UpdateFrequency::Weekly => 7 * 24 * 60 * 60,
            UpdateFrequency::Monthly => 30 * 24 * 60 * 60,
        }
    }
}

const ALLOWED_REFRESH_INTERVALS: [u64; 5] = [5 * 60, 10 * 60, 15 * 60, 20 * 60, 30 * 60];

fn normalize_refresh_interval(seconds: u64) -> u64 {
    ALLOWED_REFRESH_INTERVALS
        .into_iter()
        .min_by_key(|allowed| allowed.abs_diff(seconds))
        .unwrap_or(5 * 60)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
#[serde(rename_all = "camelCase")]
struct Settings {
    display_mode: DisplayMode,
    refresh_interval_seconds: u64,
    codex_threshold: u8,
    claude_threshold: u8,
    remaining_notifications_enabled: bool,
    reset_notifications_enabled: bool,
    codex_enabled: bool,
    claude_enabled: bool,
    launch_at_login: bool,
    update_frequency: UpdateFrequency,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateCheckResult {
    current_version: String,
    update_version: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            display_mode: DisplayMode::Number,
            refresh_interval_seconds: 5 * 60,
            codex_threshold: 0,
            claude_threshold: 0,
            remaining_notifications_enabled: true,
            reset_notifications_enabled: true,
            codex_enabled: true,
            claude_enabled: true,
            launch_at_login: true,
            update_frequency: UpdateFrequency::Daily,
        }
    }
}

fn main() {
    let show_settings_on_launch = env::args().any(|argument| argument == "--settings");
    if env::args().any(|argument| argument == "--probe") {
        let result = fetch_all_usage(true, true);
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
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            get_settings,
            set_settings,
            get_app_version,
            check_for_update_now
        ])
        .on_window_event(|window, event| {
            if window.label() == "settings"
                && let tauri::WindowEvent::CloseRequested { api, .. } = event
            {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let settings = load_settings();
            // ログイン時起動の実状態を設定値に合わせる（初回はデフォルトONで有効化される）。
            apply_launch_at_login(app.handle(), settings.launch_at_login);
            // v0.1.5以前が登録したUsageBar所有のstatusLineだけを一度解除する。
            // 既存の他ツール・ユーザー設定は変更しない。
            let _ = remove_owned_statusline_registration();
            let state = Arc::new(Mutex::new(MonitorState {
                latest: load_cache(),
                display_mode: settings.display_mode,
                refresh_interval_seconds: settings.refresh_interval_seconds,
                codex_threshold: settings.codex_threshold,
                claude_threshold: settings.claude_threshold,
                remaining_notifications_enabled: settings.remaining_notifications_enabled,
                reset_notifications_enabled: settings.reset_notifications_enabled,
                codex_enabled: settings.codex_enabled,
                claude_enabled: settings.claude_enabled,
                update_frequency: settings.update_frequency,
                ..MonitorState::default()
            }));
            app.manage(state.clone());

            use tauri_plugin_notification::NotificationExt;
            let _ = app.notification().request_permission();

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
            // 設定の確認頻度に達していれば自動更新チェックを実行。
            if now_epoch().saturating_sub(read_last_update_check())
                >= settings.update_frequency.seconds()
            {
                write_last_update_check(now_epoch());
                check_for_update(app.handle().clone());
            }
            if show_settings_on_launch {
                show_settings_window(app.handle());
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to run UsageBar");
}

fn check_for_update(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        match download_available_update(&app).await {
            Ok(Some(version)) => {
                send_notification(
                    &app,
                    "UsageBarを更新しました",
                    &format!("バージョン {version} を適用して再起動します。"),
                );
                app.restart();
            }
            Ok(None) => {}
            Err(error) => eprintln!("update check failed: {error}"),
        }
    });
}

async fn download_available_update(
    app: &AppHandle,
) -> Result<Option<String>, tauri_plugin_updater::Error> {
    let Some(update) = app.updater()?.check().await? else {
        return Ok(None);
    };

    let version = update.version.clone();
    update.download_and_install(|_, _| {}, || {}).await?;
    Ok(Some(version))
}

fn refresh(app: AppHandle) {
    let state = app.state::<SharedState>().inner().clone();
    let (codex_enabled, claude_enabled) = {
        let mut current = state.lock().expect("monitor state lock poisoned");
        if current.refreshing {
            return;
        }
        current.refreshing = true;
        current.last_error = None;
        (current.codex_enabled, current.claude_enabled)
    };
    update_tray(&app, &state);

    tauri::async_runtime::spawn_blocking(move || {
        let result = fetch_all_usage(codex_enabled, claude_enabled);
        let notifications = {
            let mut current = state.lock().expect("monitor state lock poisoned");
            current.refreshing = false;
            match result {
                Ok((mut snapshot, warning)) => {
                    // 今回取得できなかったサービスは前回値を引き継ぎ、メニューバーの%を維持する。
                    // ただし無効化されたサービスは前回値を引き継がない（表示から消す）。
                    if let Some(previous) = current.latest.as_ref() {
                        if claude_enabled && snapshot.claude_usage.is_none() {
                            snapshot.claude_usage = previous.claude_usage.clone();
                        }
                        if codex_enabled && snapshot.rate_limits.headline().is_none() {
                            snapshot.rate_limits = previous.rate_limits.clone();
                        }
                    }
                    save_cache(&snapshot);
                    current.latest = Some(snapshot);
                    current.last_error = warning;
                    let mut notifications = remaining_notifications(&mut current);
                    notifications.extend(reset_notifications(&mut current));
                    notifications
                }
                Err(error) => {
                    current.last_error = Some(error);
                    Vec::new()
                }
            }
        };
        for (title, body) in notifications {
            send_notification(&app, &title, &body);
        }
        update_tray(&app, &state);
    });
}

/// Claude のトークン枠（5時間/週間）がリセットされたら通知を返す。
/// `/usage` は次のリセット時刻を表示文字列でしか出さない（epochは取れない）ため、
/// **「リセット時刻の表示が別の値に変わった＝新しい枠が始まった」**をリセットとみなす。
/// 通知の発火は毎回 refresh の中なので、epochを持っても検知の粒度は変わらない。
fn reset_notifications(state: &mut MonitorState) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(usage) = state
        .latest
        .as_ref()
        .and_then(|snapshot| snapshot.claude_usage.as_ref())
    else {
        return out;
    };
    let five = usage.five_hour.resets_label.clone();
    let seven = usage.seven_day.resets_label.clone();
    check_reset(
        "Claudeの5時間枠",
        &five,
        state.reset_notifications_enabled,
        &mut state.five_hour_reset_label,
        &mut out,
    );
    check_reset(
        "Claudeの週間枠",
        &seven,
        state.reset_notifications_enabled,
        &mut state.seven_day_reset_label,
        &mut out,
    );
    out
}

fn check_reset(
    name: &str,
    label: &str,
    notifications_enabled: bool,
    tracked: &mut Option<String>,
    out: &mut Vec<(String, String)>,
) {
    // 解析できなかった枠は判定に使わない（誤通知と、追跡値の巻き戻りを防ぐ）。
    if label.is_empty() || label == UNKNOWN_RESET_LABEL {
        return;
    }
    match tracked {
        // 初回観測（起動直後・キャッシュ復帰）は記録するだけで通知しない。
        None => *tracked = Some(label.to_string()),
        Some(previous) if previous != label => {
            *tracked = Some(label.to_string());
            if notifications_enabled {
                out.push((
                    "UsageBar".to_string(),
                    format!("{name}がリセットされました（利用可能になりました）"),
                ));
            }
        }
        Some(_) => {}
    }
}

fn remaining_notifications(state: &mut MonitorState) -> Vec<(String, String)> {
    if !state.remaining_notifications_enabled {
        state.codex_notified = false;
        state.claude_notified = false;
        return Vec::new();
    }
    let Some(snapshot) = state.latest.as_ref() else {
        return Vec::new();
    };
    let codex_remaining = snapshot
        .rate_limits
        .headline()
        .map(RateLimitWindow::remaining_percent);
    let claude_remaining = snapshot
        .claude_usage
        .as_ref()
        .map(|usage| usage.five_hour.remaining_percent());

    let mut out = Vec::new();
    check_threshold(
        "Codex",
        codex_remaining,
        state.codex_threshold,
        &mut state.codex_notified,
        &mut out,
    );
    check_threshold(
        "Claude",
        claude_remaining,
        state.claude_threshold,
        &mut state.claude_notified,
        &mut out,
    );
    out
}

fn check_threshold(
    name: &str,
    remaining: Option<u8>,
    threshold: u8,
    notified: &mut bool,
    out: &mut Vec<(String, String)>,
) {
    if threshold == 0 {
        *notified = false;
        return;
    }
    let Some(remaining) = remaining else {
        return;
    };
    if remaining <= threshold {
        if !*notified {
            *notified = true;
            out.push((
                "UsageBar".to_string(),
                format!("{name}の残りが{remaining}%になりました（しきい値{threshold}%）"),
            ));
        }
    } else {
        *notified = false;
    }
}

fn send_notification(app: &AppHandle, title: &str, body: &str) {
    use tauri_plugin_notification::NotificationExt;
    let _ = app.notification().builder().title(title).body(body).show();
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
                .refresh_interval_seconds;
            let interval_seconds = normalize_refresh_interval(interval_seconds);
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
        let home = PathBuf::from(home);
        candidates.push(home.join(".local/bin/codex"));
        if let Ok(entries) = std::fs::read_dir(home.join(".vscode/extensions")) {
            candidates.extend(
                entries
                    .flatten()
                    .map(|entry| entry.path().join("bin/macos-aarch64/codex")),
            );
        }
    }
    if let Some(path) = env::var_os("PATH") {
        candidates.extend(env::split_paths(&path).map(|directory| directory.join("codex")));
    }

    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| "Codex CLIが見つかりません".into())
}

fn fetch_all_usage(
    codex_enabled: bool,
    claude_enabled: bool,
) -> Result<(UsageSnapshot, Option<String>), String> {
    let mut errors = Vec::new();
    let mut snapshot = UsageSnapshot {
        rate_limits: RateLimits::default(),
        claude_usage: None,
        fetched_at: now_epoch(),
    };

    if codex_enabled {
        match locate_codex().and_then(|path| fetch_usage(&path)) {
            Ok(codex) => {
                snapshot.rate_limits = codex.rate_limits;
                snapshot.fetched_at = codex.fetched_at;
            }
            Err(error) => errors.push(error),
        }
    }

    if claude_enabled {
        match fetch_claude() {
            Ok(usage) => snapshot.claude_usage = Some(usage),
            Err(error) => errors.push(error),
        }
    }

    // 有効なサービスがすべて取得失敗したときだけエラーにする（無効なら静かに空で返す）。
    if !errors.is_empty()
        && snapshot.rate_limits.headline().is_none()
        && snapshot.claude_usage.is_none()
    {
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
        if let Ok(entries) =
            std::fs::read_dir(home.join("Library/Application Support/Claude/claude-code"))
        {
            candidates.extend(
                entries
                    .flatten()
                    .map(|entry| entry.path().join("claude.app/Contents/MacOS/claude")),
            );
        }
        let node_versions = home.join(".nvm/versions/node");
        if let Ok(entries) = std::fs::read_dir(node_versions) {
            for entry in entries.flatten() {
                let node_version = entry.path();
                candidates.push(node_version.join("bin/claude"));
            }
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

/// Claude CodeをPTYで起動し、`/usage`の表示から使用量を取得する。
/// OAuth資格情報や`~/.claude/settings.json`は直接読み書きしない。
fn fetch_claude() -> Result<ClaudeUsage, String> {
    locate_claude().and_then(|path| fetch_claude_usage(&path))
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
    let session_id = new_probe_session_id();
    let mut child = Command::new("/usr/bin/script")
        .args(["-q", "/dev/null"])
        .arg(claude)
        .args(["--allowed-tools", ""])
        .args(["--settings", r#"{"disableAllHooks":true}"#])
        .args(["--session-id", &session_id])
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
    // /usage 自体は1回だけ送る。描画が遅いことがあるため、画面に使用量が出るまで待つ。
    let usage_deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut parsed_usage = None;
    while std::time::Instant::now() < usage_deadline {
        let screen = {
            let captured = captured.lock().expect("Claude output lock poisoned");
            strip_terminal_sequences(&String::from_utf8_lossy(&captured))
        };
        if let Ok(usage) = parse_claude_usage(&screen) {
            parsed_usage = Some(usage);
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
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
    let result = parsed_usage
        .or_else(|| parse_claude_usage(&screen).ok())
        .ok_or_else(|| claude_usage_error(&screen))
        .map(|mut usage| {
            usage.plan_type = fetch_claude_plan(claude);
            usage
        });
    cleanup_claude_probe_session(&session_id);
    result
}

fn claude_usage_error(screen: &str) -> String {
    if screen.contains("API Usage Billing")
        || (screen.contains("Total cost:") && screen.contains("Usage:"))
    {
        "Claudeの5時間・週間使用量を取得できません。Claude.aiのPro/Maxプランでログインしているか確認してください"
            .to_string()
    } else {
        "Claude Codeの/usage出力を時間内に解析できませんでした".to_string()
    }
}

fn new_probe_session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seed = nanos ^ (u128::from(std::process::id()) << 96);
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (seed >> 96) as u32,
        (seed >> 80) as u16,
        (seed >> 68) & 0x0fff,
        (seed >> 56) & 0x0fff,
        seed & 0xffff_ffff_ffff
    )
}

/// 監視用セッションだけをClaude Codeの履歴から除去する。
/// 対象はUsageBarが生成したUUIDと完全一致するファイル・ディレクトリに限定する。
fn cleanup_claude_probe_session(session_id: &str) {
    let mut config_roots = Vec::new();
    if let Some(config_dirs) = env::var_os("CLAUDE_CONFIG_DIR") {
        config_roots.extend(
            config_dirs
                .to_string_lossy()
                .split(',')
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
        );
    }
    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        config_roots.push(home.join(".claude"));
        config_roots.push(home.join(".config/claude"));
    }

    for projects_root in config_roots
        .into_iter()
        .map(|root| root.join("projects"))
        .filter(|root| root.is_dir())
    {
        let Ok(projects) = std::fs::read_dir(projects_root) else {
            continue;
        };
        for project in projects.flatten().filter(|entry| entry.path().is_dir()) {
            let project_path = project.path();
            let _ = std::fs::remove_file(project_path.join(format!("{session_id}.jsonl")));
            let session_directory = project_path.join(session_id);
            if session_directory.is_dir() {
                let _ = std::fs::remove_dir_all(session_directory);
            }
        }
    }
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
                .unwrap_or(UNKNOWN_RESET_LABEL)
                .to_string(),
        },
        seven_day: ClaudeWindow {
            used_percent: percent_before_used(week)?,
            resets_label: extract_week_reset(week).unwrap_or_else(|| UNKNOWN_RESET_LABEL.into()),
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
        .and_then(|value| value.rate_limits.headline())
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
    if !state.codex_enabled {
        items.push(Box::new(disabled_item(app, "オフ")?));
    } else if state.refreshing {
        items.push(Box::new(disabled_item(app, "更新中...")?));
    } else if let Some(snapshot) = &state.latest {
        for window in snapshot.rate_limits.windows() {
            add_window_items(app, &mut items, &window.window_label(), window)?;
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
    if !state.claude_enabled {
        items.push(Box::new(disabled_item(app, "オフ")?));
    } else if state.refreshing {
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

fn apply_launch_at_login(app: &AppHandle, enabled: bool) {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    let currently = manager.is_enabled().unwrap_or(false);
    if enabled && !currently {
        let _ = manager.enable();
    } else if !enabled && currently {
        let _ = manager.disable();
    }
}

fn launch_at_login_enabled(app: &AppHandle) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().unwrap_or(false)
}

fn show_settings_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("settings") {
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("index.html".into()))
        .title("UsageBar設定")
        .inner_size(470.0, 720.0)
        .resizable(false)
        .center()
        .build();
}

#[tauri::command]
fn get_app_version(app: AppHandle) -> String {
    app.package_info().version.to_string()
}

#[tauri::command]
async fn check_for_update_now(app: AppHandle) -> Result<UpdateCheckResult, String> {
    let current_version = app.package_info().version.to_string();
    let update_version = download_available_update(&app)
        .await
        .map_err(|error| error.to_string())?;

    if let Some(version) = update_version.as_ref() {
        send_notification(
            &app,
            "UsageBarを更新しました",
            &format!("バージョン {version} を適用して再起動します。"),
        );
        app.restart();
    }

    Ok(UpdateCheckResult {
        current_version,
        update_version,
    })
}

#[tauri::command]
fn get_settings(app: AppHandle, state: tauri::State<'_, SharedState>) -> Settings {
    let current = state.lock().expect("monitor state lock poisoned");
    Settings {
        display_mode: current.display_mode,
        refresh_interval_seconds: current.refresh_interval_seconds,
        codex_threshold: current.codex_threshold,
        claude_threshold: current.claude_threshold,
        remaining_notifications_enabled: current.remaining_notifications_enabled,
        reset_notifications_enabled: current.reset_notifications_enabled,
        codex_enabled: current.codex_enabled,
        claude_enabled: current.claude_enabled,
        launch_at_login: launch_at_login_enabled(&app),
        update_frequency: current.update_frequency,
    }
}

#[tauri::command]
fn set_settings(
    app: AppHandle,
    state: tauri::State<'_, SharedState>,
    settings: Settings,
) -> Result<(), String> {
    if !ALLOWED_REFRESH_INTERVALS.contains(&settings.refresh_interval_seconds) {
        return Err("更新間隔は5分・10分・15分・20分・30分から選択してください".into());
    }
    if settings.codex_threshold > 100 || settings.claude_threshold > 100 {
        return Err("しきい値は0〜100%で指定してください".into());
    }
    {
        let mut current = state.lock().expect("monitor state lock poisoned");
        current.display_mode = settings.display_mode;
        current.refresh_interval_seconds = settings.refresh_interval_seconds;
        if current.codex_threshold != settings.codex_threshold {
            current.codex_threshold = settings.codex_threshold;
            current.codex_notified = false;
        }
        if current.claude_threshold != settings.claude_threshold {
            current.claude_threshold = settings.claude_threshold;
            current.claude_notified = false;
        }
        if current.remaining_notifications_enabled != settings.remaining_notifications_enabled {
            current.codex_notified = false;
            current.claude_notified = false;
        }
        current.remaining_notifications_enabled = settings.remaining_notifications_enabled;
        current.reset_notifications_enabled = settings.reset_notifications_enabled;
        current.codex_enabled = settings.codex_enabled;
        current.claude_enabled = settings.claude_enabled;
        current.update_frequency = settings.update_frequency;
        apply_launch_at_login(&app, settings.launch_at_login);
        // 無効化されたサービスの表示値は即座にクリアする。
        if let Some(snapshot) = current.latest.as_mut() {
            if !settings.codex_enabled {
                snapshot.rate_limits = RateLimits::default();
            }
            if !settings.claude_enabled {
                snapshot.claude_usage = None;
            }
        }
    }
    persist_settings(&settings);
    update_tray(&app, state.inner());
    // 有効に戻したサービスをすぐ取得しにいく。
    refresh(app.clone());
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

fn claude_settings_path() -> Option<PathBuf> {
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude/settings.json"))
}

/// 同じディレクトリの一時ファイルを経由して、所有者だけが読める状態で置換する。
fn write_private_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let directory = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "保存先に親ディレクトリがありません",
        )
    })?;
    std::fs::create_dir_all(directory)?;

    let file_name = path
        .file_name()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "保存先にファイル名がありません",
            )
        })?
        .to_string_lossy();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = directory.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), nonce));

    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);

        let mut file = options.open(&temporary)?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn last_update_check_path() -> Option<PathBuf> {
    env::var_os("HOME").map(|home| {
        PathBuf::from(home).join("Library/Application Support/UsageBar/last-update-check")
    })
}

fn read_last_update_check() -> u64 {
    last_update_check_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

fn write_last_update_check(timestamp: u64) {
    let Some(path) = last_update_check_path() else {
        return;
    };
    if let Some(directory) = path.parent() {
        let _ = std::fs::create_dir_all(directory);
    }
    let _ = std::fs::write(path, timestamp.to_string());
}

/// 任意のUTF-8文字列をPOSIX shellの単一引数として安全に引用する。
fn shell_quote(argument: &str) -> String {
    format!("'{}'", argument.replace('\'', "'\\''"))
}

fn statusline_command_for_exe(exe: &Path) -> String {
    let exe = exe.to_str().unwrap_or("usage-bar");
    format!("{} --statusline", shell_quote(exe))
}

fn is_current_statusline_command(command: &str, exe: &Path) -> bool {
    command == statusline_command_for_exe(exe)
}

/// 現行の安全な形式と、旧版が保存した未引用の完全一致形式だけをUsageBar所有とみなす。
fn is_owned_statusline_command(command: &str, exe: &Path) -> bool {
    if is_current_statusline_command(command, exe) {
        return true;
    }
    exe.to_str()
        .map(|exe| command == format!("{exe} --statusline"))
        .unwrap_or(false)
}

/// v0.1.5以前が登録したUsageBar所有のstatusLineだけを解除する。
/// 他ツール・ユーザー自身のstatusLineは読み取るだけで変更しない。
fn remove_owned_statusline_registration() -> Result<(), String> {
    let current_exe = std::env::current_exe()
        .map_err(|error| format!("実行ファイルのパスを取得できません: {error}"))?;
    let path = claude_settings_path().ok_or("~/.claude/settings.json のパスを決定できません")?;
    if !path.exists() {
        return Ok(());
    }
    let data =
        std::fs::read(&path).map_err(|error| format!("settings.json を読めません: {error}"))?;
    let mut value: Value = serde_json::from_slice(&data)
        .map_err(|error| format!("settings.json を解析できません: {error}"))?;
    let object = value
        .as_object_mut()
        .ok_or("settings.json の形式が不正です")?;
    let ours = object
        .get("statusLine")
        .and_then(|line| line.get("command"))
        .and_then(Value::as_str)
        .map(|command| is_owned_statusline_command(command, &current_exe))
        .unwrap_or(false);
    if !ours {
        return Ok(());
    }

    // 変更前にバックアップを残す。
    let _ = std::fs::copy(&path, path.with_extension("json.usagebar-bak"));
    object.remove("statusLine");
    let data = serde_json::to_vec_pretty(&value)
        .map_err(|error| format!("settings.json を生成できません: {error}"))?;
    write_private_atomic(&path, &data)
        .map_err(|error| format!("settings.json を書き込めません: {error}"))?;
    Ok(())
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
    let mut settings: Settings = settings_path()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default();
    settings.refresh_interval_seconds =
        normalize_refresh_interval(settings.refresh_interval_seconds);
    settings
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

    fn window(used_percent: f64, window_duration_mins: u64) -> RateLimitWindow {
        RateLimitWindow {
            used_percent,
            window_duration_mins,
            resets_at: 0,
        }
    }

    #[test]
    fn window_label_comes_from_the_actual_duration() {
        assert_eq!(window(0.0, 300).window_label(), "5時間");
        assert_eq!(window(0.0, 10080).window_label(), "週間");
        assert_eq!(window(0.0, 1440).window_label(), "1日");
        assert_eq!(window(0.0, 20160).window_label(), "2週間");
        assert_eq!(window(0.0, 45).window_label(), "45分");
        assert_eq!(window(0.0, 0).window_label(), "枠");
    }

    #[test]
    fn headline_is_the_shortest_window_regardless_of_position() {
        // primaryが週間・secondaryが5時間でも、代表値は短い方（5時間）を使う。
        let limits = RateLimits {
            primary: Some(window(10.0, 10080)),
            secondary: Some(window(40.0, 300)),
            plan_type: None,
        };
        assert_eq!(limits.headline().unwrap().remaining_percent(), 60);

        // 週間枠しか返ってこない構成でも、それを代表値として拾う（primary固定だと取りこぼす）。
        let weekly_only = RateLimits {
            primary: None,
            secondary: Some(window(25.0, 10080)),
            plan_type: None,
        };
        assert_eq!(weekly_only.headline().unwrap().remaining_percent(), 75);
        assert_eq!(weekly_only.windows().count(), 1);

        assert!(RateLimits::default().headline().is_none());
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
    fn explains_when_claude_plan_usage_is_unavailable() {
        let error = claude_usage_error(
            "Opus 5 · API Usage Billing Session Total cost: $0.0000 Usage: 0 input",
        );
        assert!(error.contains("Pro/Maxプラン"));
        assert!(!error.contains("API Usage Billing"));
    }

    #[test]
    fn refresh_interval_is_limited_to_usage_safe_choices() {
        assert_eq!(normalize_refresh_interval(300), 300);
        assert_eq!(normalize_refresh_interval(600), 600);
        assert_eq!(normalize_refresh_interval(900), 900);
        assert_eq!(normalize_refresh_interval(1200), 1200);
        assert_eq!(normalize_refresh_interval(1800), 1800);
        assert_eq!(normalize_refresh_interval(60), 300);
        assert_eq!(normalize_refresh_interval(3600), 1800);
        assert_eq!(normalize_refresh_interval(5), 300);
    }

    #[test]
    fn old_settings_receive_default_refresh_interval() {
        let settings: Settings = serde_json::from_str(r#"{"displayMode":"circle"}"#).unwrap();
        assert_eq!(settings.display_mode, DisplayMode::Circle);
        assert_eq!(settings.refresh_interval_seconds, 300);
        assert_eq!(settings.codex_threshold, 0);
        assert_eq!(settings.claude_threshold, 0);
        assert!(settings.remaining_notifications_enabled);
        assert!(settings.reset_notifications_enabled);
    }

    #[test]
    fn probe_session_id_is_a_uuid() {
        let session_id = new_probe_session_id();
        assert_eq!(session_id.len(), 36);
        assert_eq!(session_id.as_bytes()[8], b'-');
        assert_eq!(session_id.as_bytes()[13], b'-');
        assert_eq!(session_id.as_bytes()[18], b'-');
        assert_eq!(session_id.as_bytes()[23], b'-');
        assert_eq!(session_id.as_bytes()[14], b'4');
        assert_eq!(session_id.as_bytes()[19], b'8');
    }

    #[test]
    fn posix_shell_quote_handles_spaces_and_single_quotes() {
        assert_eq!(shell_quote("/tmp/Usage Bar"), "'/tmp/Usage Bar'");
        assert_eq!(
            shell_quote("/tmp/Usage Bar's executable"),
            "'/tmp/Usage Bar'\\''s executable'"
        );
    }

    #[test]
    fn statusline_command_quotes_shell_metacharacters() {
        let exe = Path::new("/tmp/Usage Bar;$(touch pwned)");
        assert_eq!(
            statusline_command_for_exe(exe),
            "'/tmp/Usage Bar;$(touch pwned)' --statusline"
        );
    }

    #[test]
    fn statusline_ownership_requires_an_exact_usagebar_command() {
        let exe = Path::new("/Applications/Usage Bar.app/Contents/MacOS/usage-bar");
        let current = statusline_command_for_exe(exe);
        let legacy = format!("{} --statusline", exe.display());

        assert!(is_current_statusline_command(&current, exe));
        assert!(is_owned_statusline_command(&current, exe));
        assert!(
            !is_current_statusline_command(&legacy, exe),
            "legacy command should be rewritten to the quoted form when enabled"
        );
        assert!(
            is_owned_statusline_command(&legacy, exe),
            "legacy command remains removable during migration"
        );

        for foreign in [
            "printf --statusline",
            "other-tool --statusline",
            "'/Applications/Usage Bar.app/Contents/MacOS/usage-bar' --statusline; touch /tmp/pwned",
            "env DEBUG=1 '/Applications/Usage Bar.app/Contents/MacOS/usage-bar' --statusline",
        ] {
            assert!(!is_owned_statusline_command(foreign, exe), "{foreign}");
        }
    }

    #[test]
    fn threshold_notifies_once_until_recovery() {
        let mut out = Vec::new();
        let mut notified = false;

        check_threshold("Codex", Some(15), 20, &mut notified, &mut out);
        assert_eq!(out.len(), 1);
        assert!(notified);

        check_threshold("Codex", Some(12), 20, &mut notified, &mut out);
        assert_eq!(out.len(), 1, "should not re-notify while still below");

        check_threshold("Codex", Some(50), 20, &mut notified, &mut out);
        assert!(!notified, "recovering above threshold resets the flag");

        check_threshold("Codex", Some(10), 20, &mut notified, &mut out);
        assert_eq!(out.len(), 2, "notifies again after recovery");
    }

    #[test]
    fn reset_notifies_once_when_the_window_rolls_over() {
        let mut out = Vec::new();
        let mut tracked = None;

        // 初回観測は記録だけ（起動直後に鳴らさない）。
        check_reset("Claudeの5時間枠", "1:10am", true, &mut tracked, &mut out);
        assert!(out.is_empty());
        assert_eq!(tracked.as_deref(), Some("1:10am"));

        // 同じ枠を見ている間は鳴らない。
        check_reset("Claudeの5時間枠", "1:10am", true, &mut tracked, &mut out);
        assert!(out.is_empty());

        // 新しい枠に変わったら1回だけ鳴る。
        check_reset("Claudeの5時間枠", "6:10am", true, &mut tracked, &mut out);
        assert_eq!(out.len(), 1);
        assert!(out[0].1.contains("リセットされました"));
        check_reset("Claudeの5時間枠", "6:10am", true, &mut tracked, &mut out);
        assert_eq!(out.len(), 1, "same window must not re-notify");
    }

    #[test]
    fn reset_ignores_unparsed_labels() {
        let mut out = Vec::new();
        let mut tracked = Some("6:10am".to_string());

        // 解析失敗は判定に使わず、追跡値も巻き戻さない。
        check_reset(
            "Claudeの5時間枠",
            UNKNOWN_RESET_LABEL,
            true,
            &mut tracked,
            &mut out,
        );
        check_reset("Claudeの5時間枠", "", true, &mut tracked, &mut out);
        assert!(out.is_empty());
        assert_eq!(tracked.as_deref(), Some("6:10am"));

        // 解析に戻ったとき、同じ枠なら誤通知しない。
        check_reset("Claudeの5時間枠", "6:10am", true, &mut tracked, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn disabled_reset_notifications_still_track_the_current_window() {
        let mut out = Vec::new();
        let mut tracked = Some("1:10am".to_string());

        check_reset("Claudeの5時間枠", "6:10am", false, &mut tracked, &mut out);

        assert!(out.is_empty());
        assert_eq!(tracked.as_deref(), Some("6:10am"));
    }

    #[test]
    fn disabled_remaining_notifications_clear_notification_state() {
        let mut state = MonitorState {
            remaining_notifications_enabled: false,
            codex_notified: true,
            claude_notified: true,
            ..MonitorState::default()
        };

        assert!(remaining_notifications(&mut state).is_empty());
        assert!(!state.codex_notified);
        assert!(!state.claude_notified);
    }

    #[test]
    fn threshold_zero_disables_notifications() {
        let mut out = Vec::new();
        let mut notified = false;
        check_threshold("Claude", Some(0), 0, &mut notified, &mut out);
        assert!(out.is_empty());
        assert!(!notified);
    }
}
