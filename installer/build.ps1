<#
.SYNOPSIS
  MemOpt の MSI を作る（リリースビルド → 実行ファイルに署名 → MSI 作成 → MSI に署名 → 検証）。

.DESCRIPTION
  署名には証明書が必要です（未決事項: コード署名証明書の調達）。証明書を指定しない場合は
  「未署名」のビルドを作り、警告を出します。未署名の常駐・管理者権限ツールは SmartScreen や
  他のウイルス対策製品で警告されやすいため、配布には署名が必要です。

.EXAMPLE
  # 未署名（開発・社内検証用）
  .\installer\build.ps1

.EXAMPLE
  # ストア内の証明書（拇印）で署名
  .\installer\build.ps1 -CertThumbprint "AB12...EF" -Version 0.1.0

.EXAMPLE
  # PFX ファイルで署名（パスワードは環境変数 MEMOPT_PFX_PASSWORD から読む）
  .\installer\build.ps1 -PfxPath C:\keys\memopt.pfx
#>
[CmdletBinding()]
param(
    [string]$Version = "0.1.0",
    [string]$WixBin = $(if ($env:WIX) { Join-Path $env:WIX "bin" } else { "C:\Program Files (x86)\WiX Toolset v3.14\bin" }),
    [string]$CertThumbprint,
    [string]$PfxPath,
    [string]$TimestampUrl = "http://timestamp.digicert.com",
    [string]$OutDir,
    [switch]$SkipBuild,
    # 自己署名の開発用証明書でパイプラインを試すとき用: 信頼されていない署名も「署名あり・改ざんなし」なら受け入れる。
    # 配布用ビルドでは指定しないこと（信頼された証明書での Valid を必須とする）。
    [switch]$AllowSelfSigned
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
if (-not $OutDir) { $OutDir = Join-Path $root "dist" }
$obj = Join-Path $OutDir "obj"
$bin = Join-Path $root "target\release"
$stage = Join-Path $OutDir "stage"   # 署名・梱包用のコピー（cargo の成果物は書き換えない）

function Find-SignTool {
    $c = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($c) { return $c.Source }
    $kits = "${env:ProgramFiles(x86)}\Windows Kits\10\bin"
    $f = Get-ChildItem $kits -Recurse -Filter signtool.exe -ErrorAction SilentlyContinue |
         Where-Object { $_.FullName -match "\\x64\\" } | Sort-Object FullName -Descending | Select-Object -First 1
    if (-not $f) { throw "signtool.exe が見つかりません（Windows SDK をインストールしてください）" }
    return $f.FullName
}

$signing = [bool]($CertThumbprint -or $PfxPath)
$signtool = $null
if ($signing) { $signtool = Find-SignTool }

function Sign-File([string]$path) {
    if (-not $signing) { return }
    $args = @("sign", "/fd", "sha256", "/tr", $TimestampUrl, "/td", "sha256", "/d", "MemOpt")
    if ($CertThumbprint) { $args += @("/sha1", $CertThumbprint) }
    else {
        $args += @("/f", $PfxPath)
        if ($env:MEMOPT_PFX_PASSWORD) { $args += @("/p", $env:MEMOPT_PFX_PASSWORD) }
    }
    & $signtool @args $path
    if ($LASTEXITCODE -ne 0) { throw "署名に失敗しました: $path" }
}

function Assert-Signed([string]$path) {
    $s = Get-AuthenticodeSignature $path
    $okStatus = if ($AllowSelfSigned) { @("Valid", "UnknownError", "NotTrusted") } else { @("Valid") }
    if ($s.Status -notin $okStatus -or -not $s.SignerCertificate) { throw "署名の検証に失敗しました ($($s.Status)): $path" }
    if ($s.Status -ne "Valid") { Write-Warning "信頼されていない署名です（開発用の自己署名）: $(Split-Path $path -Leaf)" }
    Write-Host ("  署名OK: {0} ({1})" -f (Split-Path $path -Leaf), $s.SignerCertificate.Subject)
}

# 1. リリースビルド
if (-not $SkipBuild) {
    Push-Location $root
    try { cargo build --release --workspace; if ($LASTEXITCODE -ne 0) { throw "cargo build に失敗しました" } }
    finally { Pop-Location }
}
$exes = "memopt-service.exe", "memopt-tray.exe", "memopt-cli.exe"
foreach ($e in $exes) { if (-not (Test-Path (Join-Path $bin $e))) { throw "$e がありません: $bin" } }

# 2. 実行ファイルをステージングへコピーし、コピーに署名する（MSI に入る前に署名する）。
#    cargo の成果物は hard link で共有されるため、target\release を直接署名すると以後のビルド結果を汚す。
Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $stage | Out-Null
foreach ($e in $exes) { Copy-Item (Join-Path $bin $e) (Join-Path $stage $e) -Force }
if ($signing) { foreach ($e in $exes) { Sign-File (Join-Path $stage $e) } }
else { Write-Warning "証明書が指定されていないため未署名でビルドします（配布には署名が必要です）" }

# 3. MSI の作成
$candle = Join-Path $WixBin "candle.exe"
$light = Join-Path $WixBin "light.exe"
foreach ($t in $candle, $light) { if (-not (Test-Path $t)) { throw "$t が見つかりません。WiX Toolset v3.14 をインストールするか -WixBin を指定してください" } }
New-Item -ItemType Directory -Force $obj | Out-Null
$msi = Join-Path $OutDir "MemOpt-$Version-x64.msi"
& $candle -nologo -arch x64 -ext WixUtilExtension "-dVersion=$Version" "-dBinDir=$stage" -out "$obj\" (Join-Path $PSScriptRoot "memopt.wxs")
if ($LASTEXITCODE -ne 0) { throw "candle に失敗しました" }
& $light -nologo -ext WixUtilExtension -out $msi (Join-Path $obj "memopt.wixobj")
if ($LASTEXITCODE -ne 0) { throw "light に失敗しました" }

# 4. MSI への署名と検証
if ($signing) {
    Sign-File $msi
    foreach ($f in ($exes | ForEach-Object { Join-Path $stage $_ }) + $msi) { Assert-Signed $f }
}
Write-Host "作成しました: $msi"
Get-FileHash $msi -Algorithm SHA256 | ForEach-Object { Write-Host ("SHA256: " + $_.Hash) }
