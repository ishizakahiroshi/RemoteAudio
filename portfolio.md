---
schemaVersion: 1
color: "#ff7a3d"
initials: "ar"
cat:
  ja: "Windows ツール / Rust · ローカル Web UI"
  en: "Windows Tool / Rust · Local Web UI"
tagline:
  ja: "隣の Windows 11 の音を、机の別 PC から切り替える"
  en: "Switch the audio device of the Windows 11 next to your desk — from any browser on your LAN."
short:
  ja: "Windows 11 ホストの既定音声出力を、同じ LAN 上のブラウザから切り替える単一 exe のローカルサービス。ホストの前に戻らずゲスト Win11・スマホ・VM から出力先を変えられる。"
  en: "A single-exe local service that switches your Windows 11 host's default audio output device from any browser on the LAN. No walking back to the host — control it from your guest Win11, phone, or VM."
tech: ["Rust", "axum", "Windows Core Audio", "vanilla JS", "Web UI"]
store: null
live: null
guide: null
featured: false
features:
  - icon: "▶"
    title: { ja: "3 役割まとめて切替", en: "Switches all three roles at once" }
    desc:  { ja: "Console / Multimedia / Communications の 3 役割を毎回まとめて切替。会議アプリだけ Comms 既定に取り残される事故を構造的に防ぐ。", en: "Console / Multimedia / Communications are switched together every time, so meeting apps can't get stranded on the old Comms endpoint." }
  - icon: "◇"
    title: { ja: "単一 exe に Web UI 同梱", en: "Web UI embedded in a single exe" }
    desc:  { ja: "Rust バイナリに Web UI 資産（HTML / CSS / JS / SVG アイコン）を rust-embed で焼き込み。1 ファイル配置でホスト常駐、npm 追加依存ゼロ。", en: "Web UI assets (HTML / CSS / JS / SVG icons) are baked into the Rust binary via rust-embed. One file to drop on the host, zero npm runtime dependencies." }
  - icon: "▤"
    title: { ja: "ゲストは URL を開くだけ", en: "Guests just open a URL" }
    desc:  { ja: "トレイまたはようこそ画面から明示的に共有 URL を渡す。ゲスト側は貼って開くだけで自動ログイン。ホスト自身のブラウザは loopback 素通しで認証不要。", en: "The tray or welcome window explicitly hands out a share URL. Guests paste-and-go, auto-signed-in. The host's own browser sails through via loopback bypass — no token required." }
---
## ja

「机の隣の Windows 11（ホスト）にスピーカーや Nest Hub Max がぶら下がっていて、そこで音を鳴らしたい。でも普段作業してるのは別の Win11（ゲスト）。会議のたびに立ち上がってホストの音声出力を切り替えるのがだるい」— それを解決するために作っている、Windows 11 専用の**ホスト常駐 HTTP サーバー + 同梱 Web UI**。

Windows Core Audio の非公開 COM インターフェース `IPolicyConfig::SetDefaultEndpoint` を Rust の `windows` crate から叩き、Console / Multimedia / Communications の 3 役割を毎回まとめて切り替える。**表示名ではなくデバイス ID で切替**するので、Bluetooth 機器の再接続で表示名が変わっても壊れない。

Web UI 側は**フレームワーク非依存**（vanilla JS + HTML + CSS）。npm も bundler も使わない。フロントエンドの依存が構造的にゼロなので、JS サプライチェーン攻撃の経路が存在しない。バックエンドは Rust + axum + tokio、単一 exe で完結する。

**運用モデル**: ホストで exe をダブルクリック → トレイに常駐し、ようこそ画面から初期設定・共有 URL コピー・デスクトップショートカット作成を選ぶ → ゲスト側の Chrome に共有 URL を貼るだけで即使える。接続用トークンを含む URL は明示操作でだけ渡す。config は `%APPDATA%\audioremote\config.toml` に自動生成、`audioremote setup` の対話ウィザードで LAN 開放・トークン再発行・並び順（state / name / recent）・自動復帰を切替できる。

**Remote Audio v0.1.0 リリース済み**（2026-07-31・npm / crates.io / GitHub Releases の 3 チャネル）。Rust 学習を兼ねた個人 OSS。続く v0.2 では、ホスト側のタスクトレイ常駐と autostart 完成版、共有 URL の明示的な配布導線、winget / Microsoft Store への配布拡張を進めている。Microsoft Store の表示名は予約済みで、初回提出は準備中。アプリ別音量は v0.3 以降へ保留し、Scoop は公開対象にしない。ゲスト側は今後もブラウザだけで完結させる（インストールするものを増やさない）。

## en

I have a Windows 11 host on my desk with real speakers and a Nest Hub Max wired up. But my daily driver is a *different* Win11 guest sitting next to it. Walking over to the host every time a meeting starts to switch the default output device — that's the friction this tool exists to remove.

`Remote Audio` is a Windows 11-only agent: a Rust HTTP server with an embedded Web UI. It calls the undocumented `IPolicyConfig::SetDefaultEndpoint` COM interface (via the `windows` crate) to switch the Console / Multimedia / Communications default endpoints together, every time. Devices are addressed by **device ID, not display name** — Bluetooth reconnects don't break switching.

The Web UI has **zero framework dependencies** (vanilla JS + HTML + CSS). No npm, no bundler. This means the JS supply-chain attack surface is structurally empty. The backend is Rust + axum + tokio, delivered as a single self-contained executable.

**Operating model**: double-click the exe on the host → it stays in the tray and the welcome window offers setup, share-URL copy and a desktop shortcut → paste the explicitly shared URL into the guest's Chrome to sign in and switch. The connection token is only handed out after an explicit action. Config is auto-generated at `%APPDATA%\audioremote\config.toml`; `audioremote setup` gives you an interactive wizard to toggle LAN mode, reissue the token, choose device sort order (state / name / recent), and enable or disable automatic recovery.

v0.1.0 shipped on 2026-07-31 across three channels: npm, crates.io, and GitHub Releases. It's a personal OSS project doubling as a Rust learning exercise. v0.2 adds the host-side tray resident, full autostart and explicit share-URL handoff, with winget / Microsoft Store publication in progress. Per-app volume is deferred to v0.3+ and Scoop is not part of the supported distribution; guests will keep needing nothing but a browser — shipping a native guest client is a deliberate non-goal.
