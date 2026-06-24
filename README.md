# UsageBar

Codex CLIとClaude Codeの5時間・週間の使用量をmacOSメニューバーに表示するTauri v2アプリです。
APIキーは使わず、それぞれのCLIに保存された既存のサブスクリプションログインを利用します。
Dockには表示されない常駐型（メニューバー）アプリです。

## 機能

- メニューバーに5時間枠の残量を表示（`Codex 94% · Claude 96%` の数字表示、またはサービスごとの円形ゲージ表示を切替可能）
- クリックで週間残量、リセット時刻、プラン、取得状態を確認
- 残量がしきい値以下になったときのmacOS通知（Codex・Claudeごとに個別設定、0で無効）
- 自動更新間隔の設定（1分・5分・10分・60分）
- 手動更新
- GitHub Releasesからの署名検証付き自動更新

## インストール

[Releases](https://github.com/OsakanaForge/usage-bar/releases) の `.dmg` を開き、`UsageBar.app` を Applications にドラッグします（Apple Silicon向け）。

署名・公証はしていないため、初回起動時はAppを右クリック →「開く」で実行してください。

### 「"UsageBar.app" は壊れているため開けません」と表示される場合

署名・公証していないアプリには、ダウンロード時にmacOSの隔離属性（quarantine）が付与され、Gatekeeperが「壊れている」と誤表示してブロックすることがあります（実際にファイルが壊れているわけではありません）。この場合は、Appを Applications にコピーしたうえで、ターミナルで隔離属性を外してください。

```bash
xattr -dr com.apple.quarantine /Applications/UsageBar.app
```

実行後、改めて `UsageBar.app` を起動できます。

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

## リリース

自動更新の成果物は `v*` タグをpushするとGitHub ActionsがReleasesへ公開します。
リリース前に、Tauri updaterの秘密鍵とパスワードをGitHub ActionsのSecretsへ登録してください。

- `TAURI_SIGNING_PRIVATE_KEY`: `.tauri/usage-bar.key` の内容
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`: 鍵生成時に設定したパスワード（今回生成した鍵では不要）

秘密鍵を紛失すると既存ユーザーへ更新を配信できなくなるため、安全な場所にもバックアップしてください。公開鍵のみ `tauri.conf.json` に含まれます。
