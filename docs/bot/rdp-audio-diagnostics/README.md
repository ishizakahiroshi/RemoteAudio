# P-20261019-001 ishizakahiroshi/RemoteAudio: RDP 音声遅延の観測ログ

- GitHub Issue: 作成後に追記
- 作業対象: `ishizakahiroshi/RemoteAudio`
- 作業 branch: `dots/P-20261019-001-rdp-audio-diagnostics`
- PR base: `develop`
- 挙動 oracle: `1f74eaa2d17dd521e2472a3842267bb1d87fbccf`
- 指示保存先: `docs/bot/rdp-audio-diagnostics/`

## 目的

YouTube 等の再生を停止した後もしばらく音声が続く事象について、原因を先に決めつけず、次の再現時に「停止操作」「ホスト出力レベルが下がった時刻」「RemoteAudio の操作」「ゲスト側 RDP 音声チャネルと transport の状態」を同じ capture ID で照合できる記録を用意する。ユーザーがリリース版をホストで起動して収集し、後から保存・解析・消去できること。

## 実装要件

1. ホストの RemoteAudio に、既定では記録しない opt-in capture を追加する。capture ごとに UTF-8 JSONL ファイルを `%APPDATA%/audioremote/diagnostics/` へ永続保存し、UTC 時刻、capture 内単調増加 sequence、capture 開始からの monotonic elapsed milliseconds、capture ID、event type を持たせる。
2. 出力信号を 100 ms 程度の周期でサンプリングする。記録は現在の既定 render endpoint の peak level、master volume、mute とする。endpoint は raw ID / friendly name を記録せず、capture 内だけで使う `endpoint_1` 等の slot に置き換える。メーター取得失敗は数値 HRESULT を含む状態遷移として記録し、同一エラーを毎 tick 出力しない。
3. RemoteAudio UI に capture 開始・停止、`再生開始`、`停止ボタンを押した`、`音が止まった` の固定マーカー、capture の一覧・ダウンロード・全消去を追加する。消去は件数を示した明示確認を要求する。marker はリクエストがホストへ到着した時刻を記録し、ネットワーク/UI 操作時間があることを説明する。
4. capture 中は RemoteAudio 自身の default device 切替、volume/mute 変更、test sound の開始/完了/失敗も同じ JSONL に記録する。request headers、Bearer token、IP、host 名、ユーザー名、raw endpoint ID / 名前、任意入力の自由文は記録しない。
5. データを音声として録音しない。peak meter はホスト出力全体であり、プロセス別の音源を識別せず、実際にスピーカーから聞こえたことの証明でもない。UI とドキュメントでその限界を明記する。
6. JSONL はダウンロードでき、`scripts/` の PowerShell 解析ツールで host/guest capture を capture ID と時刻でまとめ、停止マーカー以降に peak が閾値を下回るまでの差分を示せるようにする。生イベント行は残し、集計で元ログを上書きしない。
7. 付属の guest-side PowerShell collector は capture ID を受け取り、RdpCoreTS Operational の音声チャネル状態イベントと存在する RemoteFX Network/Graphics counters を定期収集する。イベント本文と任意 EventData は保存しない。counter が無い環境では `unavailable` を記録して継続する。ローカル出力先は利用者が指定し、RemoteAudio ホストの capture ID と揃える。
8. ログサイズ/実行時間を制限し、上限時に capture をきれいに停止して理由を記録する。履歴は勝手に削除しない。全消去 API は diagnostics ディレクトリ内のアプリ管理下の `*.jsonl` のみを削除する。
9. 全 diagnostics API は既存の `/api` 認証と same-origin guard を通す。Audio COM は既存 `AudioGate` を通す。常駐サーバー起動時に capture を自動開始しない。
10. User guide に開始、マーカー、ダウンロード、guest collector、解析、全消去を記述する。

## 範囲と維持条件

- 変更範囲: Rust host server / Core Audio wrapper / Web UI (JA + EN) / PowerShell guest collector と analyzer / 必要な user guide / tests。
- 新規 Rust / JavaScript 依存、audio recording、送信・アップロード、RDP 設定変更、Windows event log の本文保存、CI workflow 変更は追加しない。
- Existing volume, device switching, test-sound API semantics は維持する。
- ローカル実機・音声デバイスが必要な手動確認は dots 環境で完了扱いにしない。ユーザーがホストで行う受入条件として残す。

## 検証

- `cargo fmt --check`
- `cargo test`
- `npm test`
- PowerShell collector/analyzer の合成 JSONL fixture 検査。fixture は架空の時刻・値のみ。
- `node scripts/secrets-scan.mjs --staged --block` は適用可能な変更で実行し、環境依存で不可なら理由と範囲を報告。
- CI が PR SHA で実行された場合、その run/commit と結果を報告する。

## 完了条件

- JSONL capture は opt-in、永続、個別 download、解析、確認付き purge ができ、単体 tests は temporary directory 以外へ触れない。
- host/guest records を同じ ID で並べ、手動 stop marker と host output peak の後続値を時系列で見られる。
- 秘密情報・音声データ・実デバイス固有 ID をログに保存しない。
- implementation commit、CI、独立レビュー対象 SHA、未検証の host playback acceptance を明示した Draft PR がある。
- merge、release、deploy はしない。

## 進め方

承認は依頼開始の 1 回のみ。実装、tests、PR、dots 内レビューまで順に進める。新規依存、範囲外変更、secret finding、危険な自動承認拒否、ユーザー操作が必要な停止条件が出たら、その状態を管理 Issue へ記録して止める。公開 repo の作業 branch / Draft PR / 管理 Issue コメントへのコード成果提出は許可。merge、release、deploy、base branch への push、非公開・実環境データの取得は許可しない。

dots からの受付・停止・完了報告は毎回、同案件の Slack thread のみで `#P-20261019-001 ishizakahiroshi/RemoteAudio：<報告>` の形にし、Issue にも同じ状態と証跡を記録する。看板 `PROGRESS.md` の更新は手元担当のみ。
