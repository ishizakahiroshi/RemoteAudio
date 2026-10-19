# RDP 音声遅延の観測

停止マーカー、ホストの出力レベル、RemoteAudio の操作、任意のゲスト側 RDP 観測を照合するための opt-in capture です。この記録だけで遅延の原因は断定できません。

## ホストで記録する

1. 通常どおり Windows ホストの物理コンソールセッションで RemoteAudio を起動し、ブラウザ UI の診断欄を開きます。起動時の記録は無効です。再起動でも自動再開しません。
2. 「記録開始」を選び、capture ID を控えます。UTF-8 JSONL が `%APPDATA%\audioremote\diagnostics\` に保存されます。録音やアップロードはしません。
3. 必要なら同じ ID で下記のゲスト collector を開始します。
4. 再生時に「再生開始」、プレーヤーの停止操作に合わせて「停止ボタンを押した」、実際に音が止まったと感じたら「音が止まった」の固定マーカーを押します。
5. 「記録停止」を選び、履歴からファイルをダウンロードします。ホストは 30 分／16 MiB の上限でも自動停止し、終端イベントに理由を記録します。ゲストのファイルは別に保管してください。
6. 必要なファイルを保存してから「全消去」を選び、表示件数を確認して承認します。対象はアプリ管理下のホスト JSONL のみです。ダウンロード済みコピーやゲストのファイルは残ります。履歴の自動削除はありません。

マーカー時刻はリクエストがホストに到着した時刻です。プレーヤーで停止を押した正確な瞬間や、耳で無音を認識した正確な時刻ではありません。ブラウザ操作・通信時間が加わります。YouTube との自動連携はありません。

キャプチャ開始時は一意な `.pending` 一時ファイルへ完全なヘッダーを書き込み、同期してから既存ファイルを上書きせず JSONL を公開します。ハードリンク非対応のファイルシステムでは開始を安全に失敗させます。開始途中のプロセスクラッシュで `.pending` が残る場合があります。所有を検証できない一時ファイルは保持し、履歴・ダウンロード・一括削除の対象にしません。削除の要否は手動で内容を確認して判断してください。公開後の開始失敗では、完全なヘッダーを持つ JSONL が中断キャプチャとして履歴に残ります。


## 観測できること・できないこと

約 100 ms ごとに現在の既定 render endpoint の全体 peak、master volume、mute を記録します。endpoint は `endpoint_1` 等の capture 内限定の匿名 slot です。記録中の RemoteAudio のデバイス・音量・mute 操作と test sound の開始／完了／失敗も残します。メーターの失敗は状態遷移として記録し、COM エラーなら数値 HRESULT を添えます。同じエラーを毎 tick 出しません。

peak は音声録音ではなく、プロセス別の測定でも、スピーカーから聞こえた証明でもありません。別アプリの音が混ざります。mute、出力先の変更、曲の静かな区間にも注意してください。欠測やメーター利用不可は出力不明であり、0 として扱いません。token、request header、IP、ホスト名、ユーザー名、raw endpoint ID／名前、任意のマーカー文、イベント本文は記録しません。共有前にファイルを確認してください。外部で変更された入力には別の情報が含まれる可能性があります。

## ゲスト collector（任意）

ソース checkout の `scripts` にある PowerShell を Windows の RDP ゲストで実行します。ゲスト用アプリのインストールは不要です。Windows PowerShell 5.1 または Windows のイベント・counter cmdlet が使える PowerShell 7 を利用します。依存追加、管理者向け設定、RDP 設定変更、無効なイベントチャネルの有効化は行いません。実行ポリシーを尊重し、診断のためだけに回避しないでください。

下の架空 ID をホストの capture ID に置き換え、既存のローカルフォルダーを指定します。

```powershell
.\scripts\collect-rdp-audio-diagnostics.ps1 `
  -CaptureId '0123456789abcdef0123456789abcdef' `
  -OutputPath '.\guest-capture.jsonl' `
  -DurationSeconds 300 -IntervalSeconds 1 -MaxBytes 4194304
```

既存ファイルは上書きしません。時間またはサイズ上限で停止します（指定可能な最大値は 1,800 秒、16 MiB）。終端イベントの書き込み領域をあらかじめ確保します。cmdlet の処理により時間上限にスケジューリング遅延が加わる場合があります。Ctrl+C では通常 cleanup が動きますが、強制終了、ディスク障害、電源断では最終行の途中切れや終端イベント欠落がありえます。そのファイルも解析用に残してください。

RdpCoreTS Operational の新しいイベントのみを読み、XML を一時的に調べて `RDPSND`、`AUDIO_PLAYBACK_DVC`、`AUDIO_PLAYBACK_LOSSY_DVC` と完全一致する音声チャネルを検出します。保存するのは固定ラベル、数値 event ID、元イベントの UTC 時刻のみです。本文や任意 EventData は保存しません。event ID の意味は Windows の provider／版に依存するため、接続状態の断定に変換しません。該当イベントがないことは正常・未接続の証明ではありません。名称は [Microsoft の音声 transport 仕様](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpea/40527932-258d-4664-bf5a-e569222e23ed) に基づきます。

RemoteFX Network／Graphics は固定の英語 counter 名を許可リストとして使い、読み取り専用で Perflib の `009`／`CurrentLanguage` の数値 index を対応付け、`Get-Counter -ListSet` で実行環境のローカライズ名を確認して収集します。翻訳の推測はしません。対応表や counter がない、ログが無効／権限不足、cmdlet がない場合は `unavailable` を記録して継続します。ローカライズ名、パス、instance 名は保存しません。instance は件数・最小・最大・平均に集約し、セッション名や instance 名を保存しません。他セッションの値が混ざる可能性があります。値は Windows counter の単位をそのまま使い、collector が単位を検証していないため `unknown_unit` と明示します。パケット収集や音声固有の伝送遅延測定ではありません。イベント backlog、検出したログ reset、最古の保存レコードが未読 cursor を追い越す rollover は記録します。欠落したイベントは復元できません。既知の `ChannelName` field がないイベントでは観測の欠落がありえます。

## 元ログを変更せず解析する

ホストからダウンロードしたファイルとゲストのファイルをローカルに置いて実行します。

```powershell
.\scripts\analyze-rdp-audio-diagnostics.ps1 `
  -InputPath '.\host-capture.jsonl', '.\guest-capture.jsonl' `
  -CaptureId '0123456789abcdef0123456789abcdef' `
  -OutputPath '.\capture-report.json' -Threshold 0.01
```

ゲスト記録がない場合はホストのパスだけを指定できます。入力は最大 2 ファイル、合計 32 MiB、report は最大 128 MiB です。`-MaxInputBytes`／`-MaxReportBytes` で上限を下げられます。上限超過は出力ファイルを作る前に拒否し、読取中に長さが変わる入力も拒否します。出力先は新しいファイルにしてください。元ファイルは上書きしません。report は可読な生行と元バイト列の base64 も保持するため、入力より大きくなります。report も入力と同じプライバシー上の注意が必要です。

- `timeline`: ID が一致し envelope が有効なレコードを UTC 順に並べます。入力番号と行番号も保持します。ゲストの envelope 時刻は収集時刻、`event_utc_unix_ms` は元イベント時刻です。
- `stop_observations`: 各 `stop_pressed` の到着から、その後に取得した有効な peak が閾値を厳密に下回るまでの monotonic 時刻差です。閾値と等しい値は含めず、次の再生開始の到着時刻で探索を打ち切ります。再生開始レコードの追記順が停止より前でも到着順を使います。欠測を 0 ms に置き換えません。
- `warnings`: 不正／途中切れの行、host／guest 不在、不完全 capture、重複／逆順のレコード等を示します。ホスト順序が曖昧、またはホスト sequence に欠番があれば保守的に差分を抑制し、生データは保持します。guest の欠番は警告しますが、ホスト内の測定を変更しません。
- `raw_lines`／`raw_inputs`: 不正行、途中切れ、別 capture ID も含む全入力行と元バイト列を保持します。timeline から除外された行も消しません。

elapsed 時刻の精度はミリ秒です。停止到着と同一ミリ秒の sample は前後関係が不明なので、追記順にかかわらず除外します。到着より厳密に後の取得だけを測定に使います。1 回閾値を下回ったことは、継続した無音の証明ではありません。前後の sample、endpoint slot、meter state、操作も確認してください。host と guest の UTC 時計は同期が必要です。解析ツールは時刻差を推測補正しません。envelope の `elapsed_ms`／`utc_unix_ms` は追記時刻です。marker の `received_elapsed_ms`／`received_utc_unix_ms` は handler 到着時刻、meter の `acquired_elapsed_ms`／`acquired_utc_unix_ms` は peak 読み取り直後の取得時刻です。差分は sample の追記時刻でなく取得時刻を使い、取得時刻がない sample は警告して除外します。到着 field がない旧形式の marker は警告付きで envelope 時刻を使います。不正な timing field（monotonic 到着／取得時刻が追記時刻より後など）は測定から除外し、生データを保持します。UTC は時計変更がありうるため、整合性の前後比較には monotonic 時刻だけを使います。ホスト内の差分は monotonic 時刻で求めます。終端イベントがなければ中断または不完全な資料であり、正常停止と見なせません。

## Windows で残る確認

合成テストは `node --test test/rdp-audio-diagnostics.test.mjs` または `powershell -NoProfile -File test/rdp-audio-diagnostics.synthetic.ps1` です。PowerShell がない場合、Node は実行テストを明示的に skip します。fixture 検査だけでは PowerShell の実行確認になりません。

物理ホストで起動時 opt-in、実メーター、マーカー、デバイス／音量／mute／test sound 操作、デバイス不在からの復帰、時間／サイズ上限、download、認証と same-origin 拒否、件数確認付き全消去を確認してください。ゲストでは counter 有無、イベントログ権限、ID の一致も確認します。実際の再生を再現し、原因を先に決めず観測を比較します。合成テストで実機の受入完了とはしません。

操作イベントは最大 256 件のキューを通し、保存処理が AudioGate を保持しないようにしています。保存が追いつかなければ `queue_overflow` で capture を停止します。この場合は一部の操作イベントが欠落した不完全な証跡です。サンプル取得・マーカー到着・操作発生の時刻は、JSONL 追記順の時刻とは別に保持します。
