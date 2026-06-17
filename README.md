# UsageBar

Codex CLIとClaude Codeの5時間・週間の使用量をmacOSメニューバーに表示するTauri v2アプリです。
APIキーは使わず、それぞれのCLIに保存された既存のサブスクリプションログインを利用します。
Dockには表示されない常駐型（メニューバー）アプリです。

## 機能

- メニューバーに5時間枠の残量を表示（`Codex 94% · Claude 96%` の数字表示、またはサービスごとの円形ゲージ表示を切替可能）
- クリックで週間残量、リセット時刻、プラン、取得状態を確認
- 残量がしきい値以下になったときのmacOS通知（Codex・Claudeごとに個別設定、0で無効）
- 自動更新間隔の設定（5秒・10秒・30秒・60秒・5分）
- 手動更新

## インストール

[Releases](https://github.com/OsakanaForge/usage-bar/releases) の `.dmg` を開き、`UsageBar.app` を Applications にドラッグします（Apple Silicon向け）。

署名・公証はしていないため、初回起動時はAppを右クリック →「開く」で実行してください。

## 設定

トレイメニューの「設定…」から、メニューバーの表示形式・自動更新間隔・通知しきい値を変更できます。
設定は `~/Library/Application Support/UsageBar/settings.json` に保存されます。
直近の取得結果は `~/Library/Application Support/UsageBar/status.json` にキャッシュされます。

## 取得方法

アプリはローカルの `codex app-server` を起動し、JSON-RPCの
`account/rateLimits/read` を呼び出します。認証ファイルやアクセストークンは読みません。

Codex CLIが未ログインの場合は、先に次を実行してください。

```bash
codex login
```

Claude Codeは擬似端末内で `/usage` を開き、その表示から使用率とリセット時刻を取得します。
KeychainのOAuth資格情報は直接読みません。Claude Codeが未導入・未ログインの場合は、先に次を実行してください。

```bash
npm install -g @anthropic-ai/claude-code
claude auth login
```

## 開発

```bash
npm install
npm run tauri dev                                  # 開発起動
cargo test --manifest-path src-tauri/Cargo.toml    # テスト
npm run tauri build                                # .app と .dmg をビルド
src-tauri/target/debug/usage-bar --probe           # 取得結果をJSONで確認（診断用）
```

ビルド成果物:

```text
src-tauri/target/release/bundle/macos/UsageBar.app
src-tauri/target/release/bundle/dmg/UsageBar_<version>_aarch64.dmg
```
