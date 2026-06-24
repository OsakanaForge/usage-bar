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
    #[serde(default)]
    resets_at: u64,
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
    codex_notified: bool,
    claude_notified: bool,
    codex_enabled: bool,
    claude_enabled: bool,
    update_frequency: UpdateFrequency,
    five_hour_reset_at: u64,
    five_hour_reset_notified: bool,
    seven_day_reset_at: u64,
    seven_day_reset_notified: bool,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
#[serde(rename_all = "camelCase")]
struct Settings {
    display_mode: DisplayMode,
    refresh_interval_seconds: u64,
    codex_threshold: u8,
    claude_threshold: u8,
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
            refresh_interval_seconds: 60,
            codex_threshold: 0,
            claude_threshold: 0,
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
        let result = fetch_all_usage(true, true, true);
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

    // Claude Code の statusLine から渡されるJSONを受け取り、rate_limits をキャッシュへ保存する。
    // （Claude Code が呼び出す: usage-bar --statusline ）
    if env::args().any(|argument| argument == "--statusline") {
        run_statusline_capture();
        return;
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
            // Claude監視がONなら statusLine を自動登録（デフォルトON・設定画面には出さない）。
            let _ = set_statusline_registered(settings.claude_enabled);
            let state = Arc::new(Mutex::new(MonitorState {
                latest: load_cache(),
                display_mode: settings.display_mode,
                refresh_interval_seconds: settings.refresh_interval_seconds,
                codex_threshold: settings.codex_threshold,
                claude_threshold: settings.claude_threshold,
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
                    "refresh" => refresh(app.clone(), true),
                    "settings" => show_settings_window(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            refresh(app.handle().clone(), false);
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

fn refresh(app: AppHandle, manual: bool) {
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
        let result = fetch_all_usage(codex_enabled, claude_enabled, manual);
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
                        if codex_enabled && snapshot.rate_limits.primary.is_none() {
                            snapshot.rate_limits = previous.rate_limits.clone();
                        }
                    }
                    save_cache(&snapshot);
                    current.latest = Some(snapshot);
                    current.last_error = warning;
                    let mut notifications = pending_notifications(&mut current);
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
/// resets_at(epoch) を追跡し、その時刻を過ぎたら一度だけ通知する。
fn reset_notifications(state: &mut MonitorState) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(usage) = state
        .latest
        .as_ref()
        .and_then(|snapshot| snapshot.claude_usage.as_ref())
    else {
        return out;
    };
    let five = usage.five_hour.resets_at;
    let seven = usage.seven_day.resets_at;
    let now = now_epoch();
    check_reset(
        "Claudeの5時間枠",
        five,
        now,
        &mut state.five_hour_reset_at,
        &mut state.five_hour_reset_notified,
        &mut out,
    );
    check_reset(
        "Claudeの週間枠",
        seven,
        now,
        &mut state.seven_day_reset_at,
        &mut state.seven_day_reset_notified,
        &mut out,
    );
    out
}

fn check_reset(
    name: &str,
    resets_at: u64,
    now: u64,
    tracked: &mut u64,
    notified: &mut bool,
    out: &mut Vec<(String, String)>,
) {
    if resets_at == 0 {
        return; // epoch不明（usageスクレイプ等）は対象外。
    }
    // 新しい枠を観測したら再アーム。既に過ぎている枠なら通知済み扱い（起動時の誤通知防止）。
    if resets_at > *tracked {
        *tracked = resets_at;
        *notified = now >= resets_at;
    }
    if *tracked != 0 && now >= *tracked && !*notified {
        *notified = true;
        out.push((
            "UsageBar".to_string(),
            format!("{name}がリセットされました（利用可能になりました）"),
        ));
    }
}

fn pending_notifications(state: &mut MonitorState) -> Vec<(String, String)> {
    let Some(snapshot) = state.latest.as_ref() else {
        return Vec::new();
    };
    let codex_remaining = snapshot
        .rate_limits
        .primary
        .as_ref()
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
                .refresh_interval_seconds
                .clamp(60, 3600);
            if elapsed_seconds >= interval_seconds {
                elapsed_seconds = 0;
                refresh(app.clone(), false);
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

fn fetch_all_usage(
    codex_enabled: bool,
    claude_enabled: bool,
    manual: bool,
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
        match fetch_claude(manual) {
            Ok(usage) => snapshot.claude_usage = Some(usage),
            Err(error) => errors.push(error),
        }
    }

    // 有効なサービスがすべて取得失敗したときだけエラーにする（無効なら静かに空で返す）。
    if !errors.is_empty()
        && snapshot.rate_limits.primary.is_none()
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

/// Claude 使用量を取得する。データ源は 2 つ:
///   1. StatusLine（Claude Code がターミナルでステータス行を描画したとき更新）
///   2. /usage バックフィルキャッシュ（StatusLine が古いときだけ叩いて保存）
/// どちらも UsageBar 側からは更新タイミングを制御できないため、ファイルの鮮度
/// （mtime）で新しい方を採用する。両方が鮮度切れ（または初回/手動）のときだけ
/// /usage を throttle 付きで叩き、結果をバックフィルキャッシュへ保存して、次回以降は
/// それを fresh として扱う（古い StatusLine 値で上書きされるのを防ぐ）。
fn fetch_claude(manual: bool) -> Result<ClaudeUsage, String> {
    const STALE_SECS: u64 = 30 * 60;
    const BACKFILL_THROTTLE_SECS: u64 = 10 * 60;

    // StatusLine と バックフィルキャッシュ のうち、鮮度の新しい方を best とする。
    let statusline = read_claude_statusline()
        .ok()
        .map(|usage| (usage, claude_status_age_secs().unwrap_or(u64::MAX)));
    let best = fresher(statusline, read_claude_backfill_cache());

    // ファイルが新しくても、ウィンドウのリセット時刻が既に過去なら使用率は無意味
    // （リセット済みなのに古い値を表示し続ける症状の原因）。その場合は鮮度切れ扱い。
    if let Some((usage, age)) = &best
        && *age < STALE_SECS
        && !claude_usage_expired(usage)
    {
        return Ok(usage.clone());
    }

    // 両方が鮮度切れ / 初回 / 手動 のときだけ /usage でバックフィル（throttleで頻度抑制）。
    let throttle_ok = now_epoch().saturating_sub(read_last_backfill()) >= BACKFILL_THROTTLE_SECS;
    if manual || throttle_ok {
        write_last_backfill(now_epoch());
        if let Ok(usage) = locate_claude().and_then(|path| fetch_claude_usage(&path)) {
            // 取得できたら保存。次回以降は fresh なバックフィルとして読まれ、古い
            // StatusLine 値に上書きされない。
            write_claude_backfill_cache(&usage);
            return Ok(usage);
        }
    }

    // バックフィル不可・失敗時は手元にある最も新しい値（古くても）を返す。
    best.map(|(usage, _)| usage).ok_or_else(|| {
        "StatusLineデータがまだありません（Claude Codeでセッションを開くと取得されます）"
            .to_string()
    })
}

/// いずれかのウィンドウのリセット時刻が既に過去なら true（使用率が更新待ちで信用できない）。
/// resets_at == 0 はエポック不明（/usage 由来など）なので過去判定の対象外。
fn claude_usage_expired(usage: &ClaudeUsage) -> bool {
    let now = now_epoch();
    let passed = |window: &ClaudeWindow| window.resets_at > 0 && window.resets_at <= now;
    passed(&usage.five_hour) || passed(&usage.seven_day)
}

/// 2 つの (使用量, 経過秒数) 候補から経過秒数の小さい（＝新しい）方を選ぶ。
fn fresher(
    a: Option<(ClaudeUsage, u64)>,
    b: Option<(ClaudeUsage, u64)>,
) -> Option<(ClaudeUsage, u64)> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if a.1 <= b.1 { a } else { b }),
        (some, None) | (None, some) => some,
    }
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
    // ポーリング・リトライはしない（/usage のエンドポイントを叩きすぎるとレート制限が出るため）。
    // 1回だけ描画を待ち、取得できなければ今回はあきらめる（呼び出し側で前回値を保持する）。
    std::thread::sleep(Duration::from_secs(5));
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
            resets_at: 0,
        },
        seven_day: ClaudeWindow {
            used_percent: percent_before_used(week)?,
            resets_label: extract_week_reset(week).unwrap_or_else(|| "不明".into()),
            resets_at: 0,
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
    if !state.codex_enabled {
        items.push(Box::new(disabled_item(app, "オフ")?));
    } else if state.refreshing {
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
        .inner_size(470.0, 660.0)
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
    if !(60..=3600).contains(&settings.refresh_interval_seconds) {
        return Err("更新間隔は60秒以上で指定してください".into());
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
    // Claude監視のON/OFFに連動して statusLine を登録/解除（~/.claude/settings.json を更新）。
    let _ = set_statusline_registered(settings.claude_enabled);
    persist_settings(&settings);
    update_tray(&app, state.inner());
    // 有効に戻したサービスをすぐ取得しにいく。
    refresh(app.clone(), false);
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

fn claude_status_path() -> Option<PathBuf> {
    env::var_os("HOME").map(|home| {
        PathBuf::from(home).join("Library/Application Support/UsageBar/claude-status.json")
    })
}

fn claude_settings_path() -> Option<PathBuf> {
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude/settings.json"))
}

/// StatusLine キャッシュの最終更新からの経過秒数（無ければ None）。
fn claude_status_age_secs() -> Option<u64> {
    let path = claude_status_path()?;
    let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
    let modified_epoch = modified.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(now_epoch().saturating_sub(modified_epoch))
}

fn last_backfill_path() -> Option<PathBuf> {
    env::var_os("HOME").map(|home| {
        PathBuf::from(home).join("Library/Application Support/UsageBar/last-claude-backfill")
    })
}

fn read_last_backfill() -> u64 {
    last_backfill_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

fn write_last_backfill(timestamp: u64) {
    let Some(path) = last_backfill_path() else {
        return;
    };
    if let Some(directory) = path.parent() {
        let _ = std::fs::create_dir_all(directory);
    }
    let _ = std::fs::write(path, timestamp.to_string());
}

/// /usage バックフィルで取得した ClaudeUsage を保存する先（ClaudeUsage をそのまま JSON 化）。
/// StatusLine の生 JSON とは別ファイルにし、mtime で鮮度を判定する。
fn claude_backfill_cache_path() -> Option<PathBuf> {
    env::var_os("HOME").map(|home| {
        PathBuf::from(home).join("Library/Application Support/UsageBar/claude-usage-backfill.json")
    })
}

/// バックフィルキャッシュを (使用量, 経過秒数) で読む。無ければ None。
fn read_claude_backfill_cache() -> Option<(ClaudeUsage, u64)> {
    let path = claude_backfill_cache_path()?;
    let data = std::fs::read(&path).ok()?;
    let usage: ClaudeUsage = serde_json::from_slice(&data).ok()?;
    let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
    let age = now_epoch().saturating_sub(modified.duration_since(UNIX_EPOCH).ok()?.as_secs());
    Some((usage, age))
}

/// バックフィル結果を保存する（書き込みで mtime が更新され、次回以降 fresh 扱いになる）。
fn write_claude_backfill_cache(usage: &ClaudeUsage) {
    let Some(path) = claude_backfill_cache_path() else {
        return;
    };
    if let Some(directory) = path.parent() {
        let _ = std::fs::create_dir_all(directory);
    }
    if let Ok(data) = serde_json::to_vec(usage) {
        let _ = std::fs::write(path, data);
    }
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

/// このアプリ自身を呼ぶ statusLine コマンド文字列（インストール先に追従）。
fn statusline_command_string() -> String {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|path| path.to_str().map(str::to_string))
        .unwrap_or_else(|| "usage-bar".to_string());
    format!("{exe} --statusline")
}

/// ~/.claude/settings.json に UsageBar の statusLine が登録済みか。
fn is_statusline_registered() -> bool {
    let Some(path) = claude_settings_path() else {
        return false;
    };
    let Ok(data) = std::fs::read(&path) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&data) else {
        return false;
    };
    value
        .get("statusLine")
        .and_then(|line| line.get("command"))
        .and_then(Value::as_str)
        .map(|command| command.contains("--statusline"))
        .unwrap_or(false)
}

/// ~/.claude/settings.json の statusLine を登録/解除する。既存のキーは保持する。
fn set_statusline_registered(enabled: bool) -> Result<(), String> {
    if enabled == is_statusline_registered() {
        return Ok(());
    }
    let path = claude_settings_path().ok_or("~/.claude/settings.json のパスを決定できません")?;
    let mut value: Value = if path.exists() {
        let data =
            std::fs::read(&path).map_err(|error| format!("settings.json を読めません: {error}"))?;
        serde_json::from_slice(&data)
            .map_err(|error| format!("settings.json を解析できません: {error}"))?
    } else {
        json!({})
    };
    let object = value
        .as_object_mut()
        .ok_or("settings.json の形式が不正です")?;

    // 変更前にバックアップを残す。
    if path.exists() {
        let _ = std::fs::copy(&path, path.with_extension("json.usagebar-bak"));
    }

    if enabled {
        object.insert(
            "statusLine".to_string(),
            json!({
                "type": "command",
                "command": statusline_command_string(),
            }),
        );
    } else {
        // UsageBar が登録した statusLine のときだけ削除する。
        let ours = object
            .get("statusLine")
            .and_then(|line| line.get("command"))
            .and_then(Value::as_str)
            .map(|command| command.contains("--statusline"))
            .unwrap_or(false);
        if ours {
            object.remove("statusLine");
        }
    }

    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)
            .map_err(|error| format!("~/.claude を作成できません: {error}"))?;
    }
    let data = serde_json::to_vec_pretty(&value)
        .map_err(|error| format!("settings.json を生成できません: {error}"))?;
    std::fs::write(&path, data)
        .map_err(|error| format!("settings.json を書き込めません: {error}"))?;
    Ok(())
}

/// `usage-bar --statusline` 実行時の処理。Claude Code の statusLine から渡される
/// JSON を stdin で受け取り、そのまま保存して、ステータス行を stdout に出力する。
fn run_statusline_capture() {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    if let Some(path) = claude_status_path() {
        if let Some(directory) = path.parent() {
            let _ = std::fs::create_dir_all(directory);
        }
        let _ = std::fs::write(&path, input.as_bytes());
    }
    // Claude Code 上のステータス行表示（任意）。残量を簡潔に出す。
    let remaining = |value: &Value, key: &str| -> Option<u8> {
        let used = value
            .get("rate_limits")?
            .get(key)?
            .get("used_percentage")?
            .as_f64()?;
        Some((100.0 - used).round().clamp(0.0, 100.0) as u8)
    };
    if let Ok(value) = serde_json::from_str::<Value>(&input)
        && let (Some(five), Some(seven)) = (
            remaining(&value, "five_hour"),
            remaining(&value, "seven_day"),
        )
    {
        println!("Claude 5h {five}% · 7d {seven}%");
    } else {
        println!("UsageBar");
    }
}

/// StatusLine が保存した JSON から Claude 使用量を読み取る（/usage は叩かない）。
fn read_claude_statusline() -> Result<ClaudeUsage, String> {
    let path = claude_status_path().ok_or("StatusLine保存先を決定できません")?;
    let data = std::fs::read(&path).map_err(|_| {
        "StatusLineデータがまだありません（Claude Codeでセッションを開くと取得されます）"
            .to_string()
    })?;
    let value: Value = serde_json::from_slice(&data)
        .map_err(|error| format!("StatusLineデータを解析できません: {error}"))?;
    let rate_limits = value
        .get("rate_limits")
        .ok_or("StatusLineにrate_limitsがありません（対象プラン/初回応答後に付与されます）")?;

    let window = |key: &str| -> Result<ClaudeWindow, String> {
        let window = rate_limits
            .get(key)
            .ok_or_else(|| format!("StatusLineに{key}がありません"))?;
        let used = window
            .get("used_percentage")
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("StatusLineの{key}使用率を解析できません"))?;
        let resets_at = window.get("resets_at").and_then(Value::as_u64).unwrap_or(0);
        Ok(ClaudeWindow {
            used_percent: used.round().clamp(0.0, 100.0) as u8,
            resets_label: if resets_at > 0 {
                format_reset_time(resets_at)
            } else {
                "不明".to_string()
            },
            resets_at,
        })
    };

    Ok(ClaudeUsage {
        five_hour: window("five_hour")?,
        seven_day: window("seven_day")?,
        plan_type: None,
    })
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
    fn fresher_prefers_newer_source() {
        let window = |percent: u8| ClaudeWindow {
            used_percent: percent,
            resets_label: "x".into(),
            resets_at: 0,
        };
        let usage = |percent: u8| ClaudeUsage {
            five_hour: window(percent),
            seven_day: window(percent),
            plan_type: None,
        };
        // 新しい（経過秒数が小さい）バックフィルが、古い StatusLine に勝つ。
        let picked = fresher(Some((usage(99), 80_000)), Some((usage(15), 60))).unwrap();
        assert_eq!(picked.0.five_hour.used_percent, 15);
        assert_eq!(picked.1, 60);
        // 片方しか無ければそれを返す。
        assert_eq!(
            fresher(None, Some((usage(7), 10)))
                .unwrap()
                .0
                .five_hour
                .used_percent,
            7
        );
        assert!(fresher(None, None).is_none());
    }

    #[test]
    fn usage_with_passed_reset_is_expired() {
        let window = |resets_at: u64| ClaudeWindow {
            used_percent: 50,
            resets_label: "x".into(),
            resets_at,
        };
        // 5h ウィンドウのリセットが過去（1）なら期限切れ。
        let expired = ClaudeUsage {
            five_hour: window(1),
            seven_day: window(u64::MAX),
            plan_type: None,
        };
        assert!(claude_usage_expired(&expired));
        // 両ウィンドウとも未来なら期限切れではない。
        let live = ClaudeUsage {
            five_hour: window(u64::MAX),
            seven_day: window(u64::MAX),
            plan_type: None,
        };
        assert!(!claude_usage_expired(&live));
        // resets_at == 0（エポック不明）は過去扱いしない。
        let unknown = ClaudeUsage {
            five_hour: window(0),
            seven_day: window(0),
            plan_type: None,
        };
        assert!(!claude_usage_expired(&unknown));
    }

    #[test]
    fn old_settings_receive_default_refresh_interval() {
        let settings: Settings = serde_json::from_str(r#"{"displayMode":"circle"}"#).unwrap();
        assert_eq!(settings.display_mode, DisplayMode::Circle);
        assert_eq!(settings.refresh_interval_seconds, 60);
        assert_eq!(settings.codex_threshold, 0);
        assert_eq!(settings.claude_threshold, 0);
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
    fn threshold_zero_disables_notifications() {
        let mut out = Vec::new();
        let mut notified = false;
        check_threshold("Claude", Some(0), 0, &mut notified, &mut out);
        assert!(out.is_empty());
        assert!(!notified);
    }
}
