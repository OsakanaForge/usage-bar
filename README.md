# UsageBar

Codex CLIとClaude Codeの5時間・週間の使用量をmacOSメニューバーに表示するTauri v2アプリです。
APIキーは使わず、それぞれのCLIに保存された既存のサブスクリプションログインを利用します。

## 起動

```bash
npm install
npm run tauri dev
```

メニューバーに `Codex 94% · Claude 96%` の形式で5時間枠の残量が表示されます。
クリックすると週間残量、リセット時刻、プラン、取得状態を確認できます。

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

## テストとビルド

```bash
cargo test --manifest-path src-tauri/Cargo.toml
npm run tauri build
```
