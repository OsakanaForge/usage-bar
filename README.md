# UsageBar

Codex CLIの5時間・週間の使用量をmacOSメニューバーに表示するTauri v2アプリです。
APIキーは使わず、Codex CLIの既存ChatGPTログインを利用します。

## 起動

```bash
npm install
npm run tauri dev
```

メニューバーに `Codex 94%` の形式で5時間枠の残量が表示されます。
クリックすると週間残量、リセット時刻、プラン、取得状態を確認できます。

## 取得方法

アプリはローカルの `codex app-server` を起動し、JSON-RPCの
`account/rateLimits/read` を呼び出します。認証ファイルやアクセストークンは読みません。

Codex CLIが未ログインの場合は、先に次を実行してください。

```bash
codex login
```

## テストとビルド

```bash
cargo test --manifest-path src-tauri/Cargo.toml
npm run tauri build
```
