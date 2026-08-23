<#
.SYNOPSIS
  release ビルドの RemoteAudio.exe を MSIX パッケージ化する。

.DESCRIPTION
  target\release\RemoteAudio.exe と assets\msix\ のロゴをステージングし、
  MakeAppx で dist\msix\audioremote-<X.Y.Z.0>.msix を作る。既定は「パックのみ」。

    -Sign     AppxManifest の Publisher と同じ Subject の自己署名証明書で署名する
    -Install  ローカルへ sideload する（-Sign を含意する）

  Store へ提出するパッケージは Microsoft が認定通過後に再署名するので、
  提出時に自己署名は要らない。署名はあくまでローカル検証のため。

  exe 自体はこのスクリプトではビルドしない（`cargo build --release` は
  リポジトリの規約で明示指示があるときだけ回す）。無ければ止まる。

.EXAMPLE
  pwsh -NoProfile -File packaging\msix\build-msix.ps1
  pwsh -NoProfile -File packaging\msix\build-msix.ps1 -Install
#>
param(
  [switch]$Sign,
  [switch]$Install
)

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = Split-Path -Parent (Split-Path -Parent $ScriptDir)

$ExePath = Join-Path $RepoRoot "target\release\RemoteAudio.exe"
$LogoDir = Join-Path $RepoRoot "assets\msix"
$CargoToml = Join-Path $RepoRoot "Cargo.toml"
$ManifestTemplate = Join-Path $ScriptDir "AppxManifest.xml"

$StageDir = Join-Path $RepoRoot "dist\msix\package"
$OutDir = Join-Path $RepoRoot "dist\msix"

# ── Windows SDK のツールを解決 ────────────────────────────────────────────────
# 決め打ちのバージョン番号は SDK 更新で腐るので、置き場だけ決めて中を探す。
$SdkBin = 'C:\Program Files (x86)\Windows Kits\10\bin'
$MakeAppx = Get-ChildItem $SdkBin -Recurse -Filter makeappx.exe -ErrorAction SilentlyContinue |
  Where-Object { $_.FullName -match '\\x64\\' } |
  Sort-Object FullName -Descending | Select-Object -First 1 -ExpandProperty FullName
$SignTool = Get-ChildItem $SdkBin -Recurse -Filter signtool.exe -ErrorAction SilentlyContinue |
  Where-Object { $_.FullName -match '\\x64\\' } |
  Sort-Object FullName -Descending | Select-Object -First 1 -ExpandProperty FullName

if (-not $MakeAppx) { throw "makeappx.exe が見つかりません（Windows SDK 未導入?）: $SdkBin" }
if (-not (Test-Path $ExePath)) {
  throw ("release exe がありません: {0}{1}  先に 'cargo build --release' を実行してください。" -f $ExePath, [Environment]::NewLine)
}

# ── バージョン（X.Y.Z → X.Y.Z.0） ────────────────────────────────────────────
# 4 桁目は Store 予約なので常に 0。
#
# 先頭セグメントの 0 は問題ない。子 plan には「先頭は 0 不可」と書かれているが、
# 同じ Partner Center アカウントの 1 番手 offline-md-editor-viewer が
# `0.3.1.0` のまま Packages セクションを Validated にして認定・公開まで通っている
# （offline-md-editor-viewer の plan_ms-store-submission.md L752 / L883）。
# 実績が上なので 0.2.0.0 をそのまま提出してよい。ここで止める必要はない。
$CargoVersion = (Get-Content $CargoToml | Select-String '^version\s*=\s*"(.+)"').Matches[0].Groups[1].Value
$PkgVersion = "$CargoVersion.0"
# 逆に本当に効くのは「既存提出と同じ版は弾かれる」方。Cargo.toml を上げ忘れたまま
# 2 回目を出そうとする事故はこれで気づける。
Write-Host "Package version: $PkgVersion (Cargo.toml: $CargoVersion)"
Write-Host "  ※ 提出済みの版と同じ番号は Partner Center が受け付けません。"

# ── ステージング ──────────────────────────────────────────────────────────────
if (Test-Path $StageDir) { Remove-Item $StageDir -Recurse -Force }
New-Item -ItemType Directory -Force $StageDir | Out-Null
New-Item -ItemType Directory -Force (Join-Path $StageDir "Assets") | Out-Null

Copy-Item $ExePath (Join-Path $StageDir "RemoteAudio.exe") -Force

# AppxManifest が参照する 4 枚。欠けたまま pack すると MakeAppx が落ちるが、
# 「どれが無いのか」は出ないのでここで名指しして止める。
# 描き直したら packaging\msix\gen-logos.ps1 で焼き直す。
$LogoNames = @(
  'Square44x44Logo.png', 'Square71x71Logo.png',
  'Square150x150Logo.png', 'StoreLogo.png'
)
foreach ($n in $LogoNames) {
  $src = Join-Path $LogoDir $n
  if (-not (Test-Path $src)) { throw "ロゴがありません: $src`n  pwsh -NoProfile -File packaging\msix\gen-logos.ps1 で生成できます。" }
  Copy-Item $src (Join-Path $StageDir "Assets\$n") -Force
}

$manifestRaw = Get-Content $ManifestTemplate -Raw -Encoding UTF8
$manifest = $manifestRaw -replace '\{\{VERSION\}\}', $PkgVersion
Set-Content (Join-Path $StageDir "AppxManifest.xml") -Value $manifest -Encoding UTF8 -NoNewline

# 提出前の目視用。Partner Center の「製品管理 → 製品 ID」と 1 文字ずつ突き合わせる。
[xml]$mx = $manifest
$IdName = $mx.Package.Identity.Name
$IdPublisher = $mx.Package.Identity.Publisher
$IdPublisherDisplay = $mx.Package.Properties.PublisherDisplayName
Write-Host ""
Write-Host "== Identity（Partner Center の表示と完全一致していること） =="
Write-Host "  Name                 : $IdName"
Write-Host "  Publisher            : $IdPublisher"
Write-Host "  PublisherDisplayName : $IdPublisherDisplay"
Write-Host ""
Write-Host "Staged: $StageDir"

# ── パック ────────────────────────────────────────────────────────────────────
$MsixPath = Join-Path $OutDir "audioremote-$PkgVersion.msix"
if (Test-Path $MsixPath) { Remove-Item $MsixPath -Force }

& $MakeAppx pack /d $StageDir /p $MsixPath /o
if ($LASTEXITCODE -ne 0) { throw "MakeAppx pack が失敗しました (exit $LASTEXITCODE)" }
Write-Host ""
Write-Host "Packed: $MsixPath"

# ── 署名（ローカル検証用） ────────────────────────────────────────────────────
# 証明書の Subject は manifest の Publisher と完全一致していないと
# Add-AppxPackage が拒否する。ハードコードせず manifest から取る。
$CerPath = Join-Path $OutDir "audioremote-local-test.cer"
if ($Sign -or $Install) {
  if (-not $SignTool) { throw "signtool.exe が見つかりません（Windows SDK 未導入?）" }
  $cert = Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Subject -eq $IdPublisher } | Select-Object -First 1
  if (-not $cert) {
    Write-Host "自己署名証明書を作成します: $IdPublisher"
    $cert = New-SelfSignedCertificate -Type Custom -Subject $IdPublisher `
      -KeyUsage DigitalSignature -FriendlyName "audioremote MSIX (local test)" `
      -CertStoreLocation "Cert:\CurrentUser\My" `
      -TextExtension @("2.5.29.37={text}1.3.6.1.5.5.7.3.3", "2.5.29.19={text}Subject Type:End Entity")
  }
  & $SignTool sign /fd SHA256 /sha1 $cert.Thumbprint $MsixPath
  if ($LASTEXITCODE -ne 0) { throw "signtool sign が失敗しました (exit $LASTEXITCODE)" }

  Export-Certificate -Cert $cert -FilePath $CerPath | Out-Null
  Write-Host "Signed. 証明書をエクスポート: $CerPath"
}

# ── ローカル sideload ─────────────────────────────────────────────────────────
if ($Install) {
  Write-Host ""
  Write-Host "ローカルへ sideload します..."
  try {
    Add-AppxPackage -Path $MsixPath -ErrorAction Stop
    Write-Host "Installed."
  }
  catch {
    Write-Warning "インストールに失敗しました: $_"
    Write-Host ""
    Write-Host "自己署名証明書がまだ信頼されていない場合はこれが出ます。"
    Write-Host "**管理者** PowerShell で 1 回だけ次を実行してから、もう一度このスクリプトを回してください:"
    Write-Host "  Import-Certificate -FilePath '$CerPath' -CertStoreLocation Cert:\LocalMachine\TrustedPeople"
    throw
  }
}

Write-Host ""
Write-Host "== 次の確認 =="
Write-Host "  起動            : スタートメニューの Remote Audio（コンソール窓が出ずトレイに常駐すること）"
Write-Host "  スタートアップ  : タスクマネージャー → スタートアップ アプリ に Remote Audio が出ること"
Write-Host "  PackageFullName : (Get-AppxPackage *audioremote*).PackageFullName"
Write-Host "  アンインストール: Remove-AppxPackage (Get-AppxPackage *audioremote*).PackageFullName"
Write-Host "  WACK            : 管理者 PowerShell で packaging\msix\run-wack.ps1"
