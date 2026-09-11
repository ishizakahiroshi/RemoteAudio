<!-- このファイルはプロジェクト固有ルールのみを書く。個人/グローバル AI ルール
（言語・確認スタイル・出力フォーマット等）は各 AI ツールのグローバル設定へ。
fresh public clone でも有効な内容に保つこと。 -->

# Remote Audio 開発ガイド

## プロジェクト概要

Windows 11 ホスト（ホスト）の**既定音声出力デバイス**を、同じ LAN 上の任意のブラウザ／クライアントから切り替える軽量ローカルサービス。ホストの前に戻らず、Hyper-V ゲスト・WSL2・スマホ・別 PC から出力先（例: Nest Hub Max ⇄ 有線イヤホン ⇄ ヘッドフォン）を変えられるようにする。

構成は「ホスト常駐 Rust サーバー ＋ HTTP API ＋ 薄いクライアント」。切替は Windows Core Audio の `IMMDeviceEnumerator` で列挙し、非公開 COM `IPolicyConfig::SetDefaultEndpoint` で Console/Multimedia/Communications の 3 役割をまとめて変更する（会議アプリの Communications 既定漏れを防ぐため常に 3 役割一括）。表示名: `Remote Audio`。ネイティブ exe のファイル名は `RemoteAudio.exe` に統一し、npm / crates / config / API の互換識別子は `audioremote` を維持する。

## やらないこと（スコープ外）

- macOS / Linux サーバー実装（Windows Core Audio 前提）
- Windows 10 の正式対応（v0.1 は Windows 11 のみ）
- コード署名（v0.1 は未署名。採否は v0.2 で Azure Trusted Signing として判断する。MS Store 経路は Store が再署名するので証明書不要）
- Console / Multimedia / Communications の**個別切替 UI**（常に 3 役割まとめて切り替える）
- 自己署名 / mkcert 等の本格 HTTPS（v0.1 は HTTP 既定 + 任意有効化のみ）
- **ゲスト側にインストールさせるネイティブアプリ（Tauri / iOS / Android 等）**（2026-08-03 却下・恒久）。ゲストはブラウザで完結させる。共有 URL を 1 回開けばトークンが `localStorage` に保存され以後はブックマークだけで繋がるので、exe を配ると導入手順がゼロから 1 に増えるだけで、スマホ / Linux / 別 PC が同じ URL で繋がる汎用性も失う。**ホスト側のトレイ常駐に Tauri は不要**（`tray-icon` クレートで足りる）
- **アプリ窓目的の PWA / Chrome アプリ化**（2026-08-05 却下）。用途がパソコンのブラウザに限られる以上、独立ウィンドウにしても呼び出しの手間が縮まらない（ブックマークバーで 1 クリック）。加えて LAN の `http://` は secure context ではないため、Chrome の「インストール」も service worker も使えない。**通知目的の PWA は却下範囲外**で v0.3 候補として存続する（**PWA は HTTPS が前提**なので HTTPS 自動セットアップとセットでしか成立しない）

## 技術スタック

| レイヤ | 採用 |
|---|---|
| 言語 | Rust (edition 2021, バイナリ crate) |
| ビルド | Cargo |
| 対象 OS（サーバー） | Windows 11 のみ |
| Windows API | `windows` crate（COM）／`IMMDeviceEnumerator` ／ 非公開 `IPolicyConfig` |
| HTTP サーバー | axum 等の軽量 crate（予定） |
| 静的アセット同梱 | `rust-embed` 等で単一バイナリに Web UI を埋め込み（予定） |
| 設定 | `%APPDATA%\audioremote\config.toml`（bind / allowed_networks / `[tray] auto_restart`）。保存は temp + rename の atomic 書き込み |
| 認証 | `Authorization: Bearer <token>`（初回生成・名前付き複数トークン・失効可。`RemoteAudio token add\|revoke\|list`）。**token だけは起動時スナップショットにせず `src/auth.rs` が mtime 監視で再読込**（失効を再起動なしで反映） |
| MSRV | `rust-version = "1.85"`（lock 済み依存の実効下限。`validate.yml` の MSRV job で固定検証） |
| 配布（主） | npm レジストリ（Rust exe を optionalDependencies でプラットフォーム別に同梱） |
| 配布（副） | crates.io（`cargo install audioremote`）／ GitHub Releases（exe + SHA256SUMS・未署名） |
| 配布（v0.2 で追加） | winget（`ishizakahiroshi.AudioRemote`）。GitHub Releases の zip を指すだけで、ビルド経路は増やさない。マニフェスト原本は `packaging/winget/` |
| 配布（v0.2 で追加） | Microsoft Store（表示名 `Remote Audio`、Product ID `9PC6L3B67FV9`、MSIX identity `ishizakahiroshi.RemoteAudio`。ホスト exe の MSIX、Store が再署名するので証明書不要。初回提出待ち） |

## ディレクトリ構成

- `src/main.rs` — CLI エントリ（serve / setup / list / set / share / token / autostart）
- `src/server.rs` — HTTP。層順は外側から securityヘッダ → networkガード（CIDR / Host・static も含む全 request）→ API ミドルウェア（same-origin + bearer）→ handler。Core Audio 呼出しは全て `AudioGate`（単一ロック + `spawn_blocking`）経由
- `src/auth.rs` — 稼働中の token 集合（config.toml 変更を mtime 監視で再読込）
- `src/audio/` — Core Audio ラッパー。`GetId` の `PWSTR` は `CoTaskString` で必ず `CoTaskMemFree`
- `bin/audioremote.js` / `test/launcher.test.mjs` — npm ランチャーとその node:test
- `Cargo.toml` — パッケージ定義（`name = "audioremote"` / `version = "0.1.0"`。crates.io には予約用 `0.0.0` と本リリース `0.1.0` が併存）
- `docs/local/` — plan / recap / bugfix / pending（**gitignore・非公開**。"local" の名のとおり追跡しない。公開したい開発ドキュメントは `docs/` 直下へ置く）
  - `archive/v0.1.x/` — **v0.1 系（0.1.0 以降）のリリースサイクル全記録**をまとめる箱（release md / 実装計画 / 監査 2 件 / bugfix 3 件）。patch リリースの記録も同じ箱へ入れる
- `packaging/` — 配布用の原本。`audioremote.manifest`（exe に埋め込む Win32 マニフェスト。**欠けると exe が起動しない**）／ `winget/` ／ `msix/`。`packaging/scoop/` は公開対象外の学習・参照用原本で、提出経路には含めない
- `scripts/` — `secrets-scan.mjs` / `install-hooks.{sh,ps1}`
- `.githooks/` — layer 2 pre-commit（`core.hooksPath = .githooks` で有効化）
- `.github/workflows/` — layer 3 CI (`secrets-scan.yml`)

## 開発時のデプロイ構成（重要）

- **開発機（ゲスト）**: この repo で `cargo build --release` するマシン。音声デバイスは持たない or 使わない
- **稼働機（ホスト）**: 実際に audioremote を走らせる Windows 11。物理コンソールセッション必須（RDP 不可）。Nest Hub Max / 有線イヤホン等の実デバイスが繋がっている
- **配線**: 両者は同 LAN 上・物理隣接。ゲストは Hyper-V VM でホストがその Hyper-V ホスト
- **デプロイ**: RDP のドライブリダイレクト経由でホストのフォルダへ exe を `Copy-Item` するだけ（認証情報不要。実値と前提は `docs/local/setup_host-deploy-pipeline.md`）。**起動はホスト側で行う**。リモートから起動すると別セッションになり実デバイスを掴めないため、ここは自動化しない
- **検証**: ゲストのブラウザから `http://<host-lan-ip>:17650/` にアクセスして API と Web UI を検証（loopback から見えるのはゲスト自身の `リモート オーディオ` だけなので実 UX にならない）

## 主要コマンド

- ビルド: `cargo build`
- 実行: `cargo run`
- テスト: `cargo test` / npm ランチャーは `npm test`
- lint: `cargo clippy`
- 整形: `cargo fmt`
- 自動起動登録: `RemoteAudio.exe --install-autostart`
- 自動起動解除: `RemoteAudio.exe --uninstall-autostart`
- secrets-scan 手動実行: `node scripts/secrets-scan.mjs --staged --block`

## AI 作業共通ルール

ビルド・コミット禁止、secrets-scan 責務、plan/bugfix/pending md の作成ルール等の AI 作業共通ルールは、各利用者のグローバル AI 設定に従う（作者環境の例: `~/.claude/CLAUDE.md` および `~/.claude/guides/`）。

このリポジトリ固有:

- **v0.1 は 2026-07-31 に v0.1.0 としてリリース済み**（npm / crates.io / GitHub Releases の 3 チャネル）。当時の実装計画と全記録は `docs/local/archive/v0.1.x/`。**次の作業単位は `docs/local/plan_audioremote-v0.2-resident-ux.md`**（親 1 枚 + 子 4 枚・作業ブランチは `develop`）。版をまたぐ方針の正典は引き続き `docs/local/plan_audioremote-roadmap.md`
- **音声デバイス切替は必ず Windows のデバイス ID で行う**（表示名は再接続等で変わりうる）
- **切替 API は 3 役割（Console/Multimedia/Communications）まとめて変更する**（個別切替は v0.1 スコープ外）。`IPolicyConfig` は 1 役割ずつ設定するため、**切替後に 3 役割を再列挙して照合し、分裂していれば 409 を返す**。並行切替は `AudioGate` で直列化する（新しい音声操作を追加する時も必ず gate 経由にする）
- **loopback は token をバイパスするため、状態変更 API には same-origin 検証（`Sec-Fetch-Site` / `Origin`）が必須**。これを外すと任意の Web ページからホストの出力先を切り替えられる（CSRF）
- **token を stdout に出す既定を作らない**（起動バナー・`token list` はマスク。全表示は `RemoteAudio share` / `token list --show` の明示操作のみ）
- **サーバーはログオンユーザーのセッションで動かす**（SYSTEM サービスにしない。音声デバイスは対話ユーザー所属のため）
- **HTTP bind の既定は `0.0.0.0`（LAN 公開・2026-07-24 LAN-first 転換）**。非 loopback クライアントは Bearer トークン必須・loopback（ホスト自身）は素通し。`127.0.0.1` に閉じたい場合は `RemoteAudio setup`。Host ヘッダ許可リストで DNS rebinding を防ぎ、`allowed_networks`（CIDR）で送信元 IP を絞れる
- **`crates.io` の `0.0.0` は予約占有用**（本リリースは `0.1.0` 以降）。yank しても名前は永久占有される点に注意

## secrets-scan（このリポジトリの配線）

書く瞬間の責務（固有名詞の一般化・fixture は合成データ等）は上記「AI 作業共通ルール」の参照先に従う。このリポジトリ固有の配線は以下:

- scanner: `scripts/secrets-scan.mjs`（手動実行: `node scripts/secrets-scan.mjs --staged --block`）
- layer 2: `.githooks/pre-commit`（`core.hooksPath = .githooks` で有効化。第三者 clone 時は `bash scripts/install-hooks.sh` または `pwsh scripts/install-hooks.ps1`）
- layer 3: `.github/workflows/secrets-scan.yml`（server-side backstop）
- layer 4: release skill 側で kb 全文突き合わせ
- env（full coverage に必要・未設定なら構造 regex のみで継続）: `KB_ROOT` / `FAMILY_ROOT`。設定詳細は `scripts/secrets-scan.mjs` の冒頭コメント
- 参照実装・設計詳細: `worklog-bridge` リポの `docs/local/secrets-scan-design/`（gitignored・公開しない）

## 関連ドキュメント

| 項目 | パス |
|---|---|
| **どのファイルが何をして、どのテーブルを読み書きするか**（探す前にここ） | `.omitnix/index.json`。全ファイルの索引とテーブル逆引き。**解析できなかったファイルも名前と理由付きで載る**ので「索引に無い」と「読めなかった」を取り違えない。参照 0 件は「未使用」ではない。**`generated.commit` が HEAD と違えば索引はその commit 時点のもの**なので、古いまま断定せず `omitnix` で作り直すか、古いことを添えて答える |
| ユーザー向け README | `README.md` |
| Codex/他 AI 用入口 | `AGENTS.md` |
| ロードマップ（v0.1 → v0.2 → v0.3+ の正典） | `docs/local/plan_audioremote-roadmap.md` |
| v0.1 系のリリースサイクル全記録 | `docs/local/archive/v0.1.x/` |
| ホストへのデプロイ手順 | `docs/local/setup_host-deploy-pipeline.md` |
