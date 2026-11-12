<#
.SYNOPSIS
  MSI の実機検証（管理者権限が必要）。インストール → サービス/回復設定/自動起動/ACL → 回復動作 →
  （任意）アップグレード → アンインストール（データが残ること）まで確認し、必ず後始末します。

.DESCRIPTION
  ⚠ このスクリプトは本物のインストールを行います（LocalSystem のサービスを登録）。
    VM（Hyper-V のチェックポイントから復元できる環境）での実行を推奨します。
    実行前に MemOptService が入っている場合は中止します（既存環境を壊さないため）。

  確認する内容（installer\README.md のチェックリストに対応）:
   1. インストール: サービス登録（LocalSystem・自動起動）・起動・ファイル配置・HKLM Run・データフォルダーの ACL・パイプ応答
   2. 回復設定: sc qfailure が「1 回目: 再起動 60 秒 / 2 回目以降: なし」
   3. 回復動作: プロセスを強制終了 → 約 60 秒後に再起動 / もう一度終了 → 停止のまま
   4. アップグレード（-UpgradeMsi を指定した場合）: 同じ UpgradeCode の新版で上書きされ、データが残る
   5. アンインストール（サイレント）: サービス/ファイル/Run が消え、%ProgramData%\MemOpt（設定・ジャーナル）は残る

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File .\installer\verify-msi.ps1
  powershell -NoProfile -ExecutionPolicy Bypass -File .\installer\verify-msi.ps1 -UpgradeMsi .\dist\MemOpt-0.1.1-x64.msi
  powershell -NoProfile -ExecutionPolicy Bypass -File .\installer\verify-msi.ps1 -SkipRecovery   # 約 3 分の回復待ちを省く
#>
[CmdletBinding()]
param(
    [string]$Msi,
    [string]$UpgradeMsi,
    [switch]$SkipRecovery,
    [switch]$KeepInstalled
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
if (-not $Msi) { $Msi = Join-Path $root "dist\MemOpt-0.1.0-x64.msi" }
$svcName = "MemOptService"
$pipeName = "memopt-v1"
$installDir = Join-Path $env:ProgramFiles "MemOpt"
$dataDir = Join-Path $env:ProgramData "MemOpt"
$logDir = Join-Path $root "dist\msi-logs"
$results = New-Object System.Collections.Generic.List[object]

function Record([string]$name, [string]$status, [string]$detail = "") {
    $results.Add([pscustomobject]@{ Check = $name; Result = $status; Detail = $detail }) | Out-Null
    $color = switch ($status) { "PASS" { "Green" } "FAIL" { "Red" } default { "Yellow" } }
    Write-Host ("[{0}] {1} {2}" -f $status, $name, $detail) -ForegroundColor $color
}
function Check([string]$name, [scriptblock]$body) {
    try { $d = & $body; Record $name "PASS" ([string]$d) } catch { Record $name "FAIL" $_.Exception.Message }
}
function Assert([bool]$cond, [string]$msg) { if (-not $cond) { throw $msg } }

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) { throw "管理者として実行した PowerShell が必要です" }
if (-not (Test-Path $Msi)) { throw "MSI がありません: $Msi（先に installer\build.ps1 を実行してください）" }
if (Get-Service $svcName -ErrorAction SilentlyContinue) { throw "$svcName が既にインストールされています。既存環境を壊さないため中止します（検証は VM で行ってください）" }
if (Test-Path "\\.\pipe\$pipeName") { throw "パイプ $pipeName が既に存在します（memopt-service が動作中？）。停止してから実行してください" }
New-Item -ItemType Directory -Force $logDir | Out-Null

function Msi-Run([string[]]$argList, [string]$logName) {
    $log = Join-Path $logDir $logName
    $p = Start-Process msiexec.exe -ArgumentList ($argList + @("/qn", "/l*v", "`"$log`"")) -Wait -PassThru
    return $p.ExitCode
}
function Wait-State([string]$want, [int]$timeoutSec) {
    $end = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $end) {
        $s = Get-Service $svcName -ErrorAction SilentlyContinue
        if ($s -and $s.Status.ToString() -eq $want) { return $true }
        Start-Sleep -Milliseconds 700
    }
    return $false
}
function Svc-Pid { (Get-CimInstance Win32_Service -Filter "Name='$svcName'").ProcessId }
function Pipe-Snapshot {
    $p = New-Object System.IO.Pipes.NamedPipeClientStream(".", $pipeName, [System.IO.Pipes.PipeDirection]::InOut)
    try {
        $p.Connect(5000)
        $w = New-Object System.IO.StreamWriter($p); $w.AutoFlush = $true; $r = New-Object System.IO.StreamReader($p)
        $w.WriteLine('{"protocol_version":1,"id":1,"type":"GetSnapshot"}')
        $t = $r.ReadLineAsync(); Assert ($t.Wait(8000)) "パイプの応答がありません"
        return ($t.Result | ConvertFrom-Json)
    } finally { $p.Dispose() }
}

$installed = $false
try {
    # ================= 1. インストール =================
    Check "msiexec /i が成功する（終了コード 0）" {
        $code = Msi-Run @("/i", "`"$Msi`"") "install.log"
        Assert ($code -eq 0) "終了コード $code（ログ: $logDir\install.log）"
        $script:installed = $true
        "exit 0"
    }
    $installed = $true
    Check "ファイルが Program Files\MemOpt に配置された" {
        foreach ($f in "memopt-service.exe", "memopt-tray.exe", "memopt-cli.exe") { Assert (Test-Path (Join-Path $installDir $f)) "$f が無い" }
        $installDir
    }
    Check "サービスが LocalSystem・自動起動で登録され、Running になる" {
        $s = Get-CimInstance Win32_Service -Filter "Name='$svcName'"
        Assert ($null -ne $s) "サービスが無い"
        Assert ($s.StartName -eq "LocalSystem") "アカウント: $($s.StartName)"
        Assert ($s.StartMode -eq "Auto") "開始種別: $($s.StartMode)"
        Assert (Wait-State "Running" 30) "Running にならない（状態: $((Get-Service $svcName).Status)）"
        "PID $(Svc-Pid)"
    }
    Check "パイプで GetSnapshot に応答する（サービスが実際に動いている）" {
        $s = Pipe-Snapshot
        Assert ($s.type -eq "Snapshot") "Snapshot が返らない: $($s | ConvertTo-Json -Compress)"
        "物理メモリ $([math]::Round($s.mem.total_phys/1MB)) MB"
    }
    Check "ログオン時のトレイ自動起動（HKLM Run）が引用符付きで登録された" {
        $v = (Get-ItemProperty "HKLM:\Software\Microsoft\Windows\CurrentVersion\Run" -Name MemOpt).MemOpt
        Assert ($v -eq ('"{0}"' -f (Join-Path $installDir "memopt-tray.exe"))) "値: $v"
        $v
    }
    Check "データフォルダーの ACL: SYSTEM/Administrators は全権、Users は読み取りのみ・継承なし" {
        $deadline = (Get-Date).AddSeconds(15)
        while (-not (Test-Path $dataDir) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
        Assert (Test-Path $dataDir) "データフォルダーが作られていない"
        $acl = Get-Acl $dataDir
        Assert ($acl.AreAccessRulesProtected) "継承が切られていない"
        $rules = $acl.Access | ForEach-Object { "{0}:{1}" -f $_.IdentityReference.Value, $_.FileSystemRights }
        Assert (($acl.Access | Where-Object { $_.IdentityReference.Value -match "SYSTEM" -and $_.FileSystemRights -match "FullControl" })) "SYSTEM が全権でない: $rules"
        Assert (($acl.Access | Where-Object { $_.IdentityReference.Value -match "Administrators" -and $_.FileSystemRights -match "FullControl" })) "Administrators が全権でない: $rules"
        $users = $acl.Access | Where-Object { $_.IdentityReference.Value -match "\\Users$" }
        Assert ($users) "Users の規則が無い: $rules"
        Assert (-not ($users | Where-Object { $_.FileSystemRights -match "Write|Modify|FullControl|CreateFiles|AppendData" })) "Users に書き込み権限がある: $($users.FileSystemRights)"
        "ok"
    }

    # ================= 2. 回復設定 =================
    Check "sc qfailure: 1 回目=再起動(60 秒) / 2 回目以降=なし / リセット 24 時間" {
        $q = (sc.exe qfailure $svcName | Out-String)
        Assert ($q -match "RESET_PERIOD.*:\s*86400") "リセット期間が 86400 秒でない`n$q"
        Assert ($q -match "RESTART -- Delay = 60000") "1 回目が 60 秒後の再起動でない`n$q"
        $actions = ([regex]::Matches($q, "(RESTART|REBOOT|RUN PROCESS)\s*--")).Count
        Assert ($actions -eq 1) "RESTART 以外の動作が設定されている（$actions 件）`n$q"
        "ok"
    }

    # ================= 3. 回復動作（約 3 分） =================
    if ($SkipRecovery) { Record "回復動作（強制終了 → 再起動 / 2 回目は停止のまま）" "SKIP" "-SkipRecovery" }
    else {
        Check "1 回目の異常終了: 約 60 秒後に自動で再起動される" {
            $old = Svc-Pid; Assert ($old -gt 0) "PID が取れない"
            Stop-Process -Id $old -Force
            Assert (Wait-State "Running" 120) "120 秒以内に再起動しなかった（状態: $((Get-Service $svcName).Status)）"
            $new = Svc-Pid; Assert ($new -gt 0 -and $new -ne $old) "PID が変わっていない"
            "PID $old → $new"
        }
        Check "2 回目の異常終了: 再起動されず停止のまま" {
            $old = Svc-Pid; Stop-Process -Id $old -Force
            Start-Sleep -Seconds 75
            $st = (Get-Service $svcName).Status.ToString()
            Assert ($st -eq "Stopped") "停止のままでない（状態: $st）"
            "Stopped"
        }
        Start-Service $svcName -ErrorAction SilentlyContinue   # 以降の確認のために手動で起動
    }

    # ================= 4. アップグレード =================
    if ($UpgradeMsi) {
        Check "アップグレード（同じ UpgradeCode の新版）: 上書きされ、データとサービスが残る" {
            Assert (Test-Path $UpgradeMsi) "MSI がありません: $UpgradeMsi"
            Set-Content -Path (Join-Path $dataDir "upgrade-marker.txt") -Value "keep" -Encoding ASCII
            $before = (Get-ItemProperty "HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*" | Where-Object { $_.DisplayName -eq "MemOpt" }).DisplayVersion
            $code = Msi-Run @("/i", "`"$UpgradeMsi`"") "upgrade.log"
            Assert ($code -eq 0) "終了コード $code（ログ: $logDir\upgrade.log）"
            $after = (Get-ItemProperty "HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*" | Where-Object { $_.DisplayName -eq "MemOpt" }).DisplayVersion
            Assert (@($after).Count -eq 1) "MemOpt のエントリが $(@($after).Count) 個ある（二重インストール）"
            Assert ($after -ne $before) "バージョンが変わっていない: $before"
            Assert (Wait-State "Running" 30) "アップグレード後に Running でない"
            Assert (Test-Path (Join-Path $dataDir "upgrade-marker.txt")) "データが消えた"
            "$before → $after"
        }
    } else { Record "アップグレード" "SKIP" "-UpgradeMsi 未指定（例: build.ps1 -Version 0.1.1 で別版を作る）" }

    # ================= 5. アンインストール（サイレント） =================
    # 未復元の永続変更が「ある」状態を作る（サイレントでは何も変更されず、ジャーナルが残ることを確認する）。
    $journal = Join-Path $dataDir "journal.jsonl"
    $entry = '{"ev":"entry","entry":{"id":1,"ts_unix":0,"feature_id":"f","feature_name":"MemOptMsiTest","method":"DisableTask","target":"\\MemOptMsiTest\\None","change":{"kind":"TaskEnabled","path":"\\MemOptMsiTest\\None","before_enabled":true},"outcome":"Ok","avail_before":null,"avail_after":null,"undone_ts":null}}'
    Add-Content -Path $journal -Value $entry -Encoding UTF8
    $journalBefore = (Get-FileHash $journal -Algorithm SHA256).Hash
    Check "msiexec /x（サイレント）が成功する" {
        $code = Msi-Run @("/x", "`"$Msi`"") "uninstall.log"
        if ($code -ne 0 -and $UpgradeMsi) { $code = Msi-Run @("/x", "`"$UpgradeMsi`"") "uninstall2.log" }
        Assert ($code -eq 0) "終了コード $code（ログ: $logDir\uninstall.log）"
        $script:installed = $false
        "exit 0"
    }
    $installed = $false
    Check "サービス・ファイル・Run が削除された" {
        Assert (-not (Get-Service $svcName -ErrorAction SilentlyContinue)) "サービスが残っている"
        Assert (-not (Test-Path (Join-Path $installDir "memopt-service.exe"))) "exe が残っている"
        Assert ($null -eq (Get-ItemProperty "HKLM:\Software\Microsoft\Windows\CurrentVersion\Run" -Name MemOpt -ErrorAction SilentlyContinue)) "Run が残っている"
        "removed"
    }
    Check "データ（設定・ジャーナル）は残り、サイレントでは何も復元されない（ジャーナル不変）" {
        Assert (Test-Path $dataDir) "データフォルダーが消えた"
        Assert (Test-Path $journal) "ジャーナルが消えた"
        Assert ((Get-FileHash $journal -Algorithm SHA256).Hash -eq $journalBefore) "ジャーナルが変更された"
        "kept"
    }
}
finally {
    # 後始末: 途中で失敗してもインストール済みなら外す（-KeepInstalled の場合を除く）
    if ($installed -and -not $KeepInstalled) {
        Write-Host "後始末: アンインストールします…" -ForegroundColor Cyan
        Get-Service $svcName -ErrorAction SilentlyContinue | Stop-Service -Force -ErrorAction SilentlyContinue
        [void](Msi-Run @("/x", "`"$Msi`"") "cleanup.log")
    }
    # 検証用に作った一時ファイル/ジャーナルを片付ける（実データは作られていないはず）
    Remove-Item (Join-Path $dataDir "upgrade-marker.txt") -Force -ErrorAction SilentlyContinue
    if (Test-Path (Join-Path $dataDir "journal.jsonl")) {
        $txt = Get-Content (Join-Path $dataDir "journal.jsonl") -Raw -ErrorAction SilentlyContinue
        if ($txt -match "MemOptMsiTest") { Remove-Item (Join-Path $dataDir "journal.jsonl") -Force -ErrorAction SilentlyContinue }
    }
}

Write-Host ""
$results | Format-Table -AutoSize -Wrap | Out-String | Write-Host
$fail = @($results | Where-Object { $_.Result -eq "FAIL" }).Count
$report = Join-Path $root ("dist\verify-msi-{0:yyyyMMdd-HHmmss}.txt" -f (Get-Date))
($results | Format-Table -AutoSize -Wrap | Out-String) | Set-Content -Encoding UTF8 $report
Write-Host ("結果: PASS {0} / FAIL {1} / SKIP {2}  （レポート: {3}、MSI ログ: {4}）" -f @($results | Where-Object { $_.Result -eq "PASS" }).Count, $fail, @($results | Where-Object { $_.Result -eq "SKIP" }).Count, $report, $logDir)
if ($fail -gt 0) { exit 1 }
