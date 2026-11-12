<#
.SYNOPSIS
  管理者権限での通し確認（P2〜P5 の「管理者でしか確かめられない部分」）。

.DESCRIPTION
  リリースビルドの memopt-service.exe を --console で起動し（本物のサービスは登録しません）、
  名前付きパイプ経由で次を確認します。触るのは使い捨ての対象だけで、終了時に必ず元へ戻します。

   1. L2 / L3 の手動解放が管理者権限で成功し、履歴にスタンバイの変化が残る
   2. 使い捨てアプリ（PING.EXE のコピー）に対して:
        スタートアップ無効化（HKLM と HKCU）→ 復元 / スケジュールタスク無効化 → 復元 /
        メモリ優先度を下げる → 復元 / 強制終了（元に戻せない）
   3. 実サービス（既定: Fax → WSearch → SysMain のうち最初に使えるもの）:
        サービス停止 → 復元（再開）/ スタートアップ「手動」→ 復元
   4. 保護対象（EventLog）は選択肢が無く、停止要求がすべて拒否される
   5. 一括復元（UndoAll）と、memopt-cli undo --all の動作

  管理者でない場合は L2/L3・HKLM・実サービスの確認を SKIP し、ユーザー範囲の確認だけ行います。

.EXAMPLE
  # 管理者として実行した PowerShell で
  .\installer\verify-admin.ps1
  .\installer\verify-admin.ps1 -ServiceName WSearch
  .\installer\verify-admin.ps1 -SkipL3      # スタンバイ全解放（一時的に遅くなる）を省く
#>
[CmdletBinding()]
param(
    [string]$ServiceName,
    [switch]$SkipL3,
    [string]$BinDir
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
if (-not $BinDir) { $BinDir = Join-Path $root "target\release" }
$svcExe = Join-Path $BinDir "memopt-service.exe"
$cliExe = Join-Path $BinDir "memopt-cli.exe"
$pipeName = "memopt-v1"
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
$results = New-Object System.Collections.Generic.List[object]
$script:id = 0
$script:notes = New-Object System.Collections.Generic.List[string]

function Record([string]$name, [string]$status, [string]$detail = "") {
    $results.Add([pscustomobject]@{ Check = $name; Result = $status; Detail = $detail }) | Out-Null
    $color = switch ($status) { "PASS" { "Green" } "FAIL" { "Red" } default { "Yellow" } }
    Write-Host ("[{0}] {1} {2}" -f $status, $name, $detail) -ForegroundColor $color
}
function Check([string]$name, [scriptblock]$body) {
    try { $d = & $body; Record $name "PASS" ([string]$d) }
    catch { Record $name "FAIL" $_.Exception.Message }
}
function Assert([bool]$cond, [string]$msg) { if (-not $cond) { throw $msg } }

# ---- 事前確認 ----
foreach ($f in $svcExe, $cliExe) { if (-not (Test-Path $f)) { throw "$f がありません。先に cargo build --release --workspace を実行してください" } }
if (Test-Path "\\.\pipe\$pipeName") { throw "パイプ $pipeName が既に存在します（本物の MemOptService が動作中？）。停止してから実行してください" }
if (-not $isAdmin) { Write-Warning "管理者ではありません。L2/L3・HKLM・実サービスの確認は SKIP します" }

# ---- パイプ ----
function Connect-Pipe {
    $p = New-Object System.IO.Pipes.NamedPipeClientStream(".", $pipeName, [System.IO.Pipes.PipeDirection]::InOut)
    $p.Connect(5000)
    $script:pipe = $p
    $script:sw = New-Object System.IO.StreamWriter($p); $script:sw.AutoFlush = $true
    $script:sr = New-Object System.IO.StreamReader($p)
}
function Read-Line([int]$timeoutSec) {
    $t = $script:sr.ReadLineAsync()
    if (-not $t.Wait($timeoutSec * 1000)) { throw "応答がありません（${timeoutSec}秒）" }
    return $t.Result
}
# 要求を送り、同じ id の応答を返す。通知は $script:notes に積む。
function Call([string]$type, [hashtable]$fields = @{}, [int]$timeoutSec = 30) {
    $script:id++
    $msg = @{ protocol_version = 1; id = $script:id; type = $type } + $fields
    $script:sw.WriteLine(($msg | ConvertTo-Json -Compress -Depth 8))
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        $line = Read-Line $timeoutSec
        $o = $line | ConvertFrom-Json
        if ($o.id -eq $script:id) { return $o }
        $script:notes.Add($line) | Out-Null
    }
    throw "応答がありません: $type"
}
function Expect-Ok($r) { if ($r.type -eq "Error") { throw "$($r.code): $($r.message)" } return $r }
function Find-Feature([string]$id) {
    for ($i = 0; $i -lt 6; $i++) {
        $f = (Call "GetFeatures" @{} 40).features | Where-Object { $_.id -eq $id }
        if ($f) { return $f }
        Start-Sleep -Milliseconds 2300   # 一覧は 2 秒キャッシュ
    }
    throw "機能が一覧に見つかりません: $id"
}
function Stop-Op($featureId, [string]$method, $target = $null, [bool]$confirmed = $false) {
    $f = @{ feature_id = $featureId; method = $method; confirmed = $confirmed }
    if ($null -ne $target) { $f.target = $target } else { $f.target = $null }
    return Call "StopFeature" $f 60
}

# ---- 後始末用の記録 ----
$tmp = Join-Path $env:TEMP ("memopt_e2e_" + [guid]::NewGuid().ToString("N").Substring(0, 8))
New-Item -ItemType Directory -Force $tmp | Out-Null
$env:MEMOPT_DATA_DIR = Join-Path $tmp "data"
$victimExe = Join-Path $tmp "memopt_e2e_victim.exe"
Copy-Item "$env:SystemRoot\System32\PING.EXE" $victimExe
$victimId = "app:" + $victimExe.ToLowerInvariant()
$runName = "MemOptE2E" + $PID          # HKCU 側
$runNameM = "MemOptE2EM" + $PID         # HKLM 側
$taskName = "T$PID"; $taskPath = "\MemOptE2E\$taskName"
$hkcuRun = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
$hklmRun = "HKLM:\Software\Microsoft\Windows\CurrentVersion\Run"
$approvedRel = "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run"
$svcProc = $null; $victim = $null; $svcBefore = $null

function Cleanup {
    try { if ($script:pipe) { $script:pipe.Dispose() } } catch {}
    if ($svcProc -and -not $svcProc.HasExited) { Stop-Process -Id $svcProc.Id -Force -ErrorAction SilentlyContinue }
    Get-Process | Where-Object { $_.Path -eq $victimExe } | Stop-Process -Force -ErrorAction SilentlyContinue
    foreach ($n in @($runName, $runNameM)) {
        foreach ($k in @($hkcuRun, $hklmRun)) { Remove-ItemProperty -Path $k -Name $n -ErrorAction SilentlyContinue }
        foreach ($h in @("HKCU:\$approvedRel", "HKLM:\$approvedRel")) { Remove-ItemProperty -Path $h -Name $n -ErrorAction SilentlyContinue }
    }
    schtasks.exe /Delete /TN "MemOptE2E\$taskName" /F 2>$null | Out-Null
    # 実サービスは元の状態へ（復元が失敗していても元に戻す）
    if ($svcBefore) {
        try {
            Set-Service -Name $svcBefore.Name -StartupType $svcBefore.StartMode -ErrorAction SilentlyContinue
            if ($svcBefore.State -eq "Running") { Start-Service -Name $svcBefore.Name -ErrorAction SilentlyContinue }
        } catch {}
    }
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

try {
    # ---- サービス起動（--console。本物のサービスは登録しない）----
    $svcProc = Start-Process $svcExe -ArgumentList "--console" -PassThru -WindowStyle Hidden
    $ok = $false
    for ($i = 0; $i -lt 20 -and -not $ok; $i++) { Start-Sleep -Milliseconds 500; $ok = Test-Path "\\.\pipe\$pipeName" }
    Assert $ok "サービスが起動しませんでした"
    Connect-Pipe
    Check "サービスに接続し、管理者権限の有無が一致する" {
        $s = Expect-Ok (Call "GetSnapshot")
        Assert ($s.type -eq "Snapshot") "Snapshot が返らない"
        "admin=$isAdmin / 物理メモリ $([math]::Round($s.mem.total_phys/1MB)) MB"
    }

    # ================= 1. L2 / L3 =================
    foreach ($lv in @("L2", "L3")) {
        if (-not $isAdmin) { Record "$lv 手動解放（管理者権限）" "SKIP" "管理者ではない"; continue }
        if ($lv -eq "L3" -and $SkipL3) { Record "L3 手動解放" "SKIP" "-SkipL3"; continue }
        Check "$lv 手動解放が成功し履歴に残る" {
            $before = (Expect-Ok (Call "GetHistory" @{ limit = 50 })).runs.Count
            Expect-Ok (Call "RunClean" @{ level = $lv }) | Out-Null
            $deadline = (Get-Date).AddSeconds(30); $run = $null
            while ((Get-Date) -lt $deadline -and -not $run) {
                Start-Sleep -Seconds 1
                $h = (Expect-Ok (Call "GetHistory" @{ limit = 50 })).runs
                if ($h.Count -gt $before) { $run = $h[0] }
            }
            Assert ($null -ne $run) "履歴に記録されません"
            Assert ($run.note -notmatch "利用不可") "利用不可になっています: $($run.note)"
            $sb = if ($run.standby_before -and $run.standby_after) { "スタンバイ {0} → {1} MB" -f [int]($run.standby_before/1MB), [int]($run.standby_after/1MB) } else { "スタンバイ値なし" }
            "id=$($run.id) level=$($run.level) $sb"
        }
    }

    # ================= 2. 使い捨てアプリ =================
    $victim = Start-Process $victimExe -ArgumentList "-n", "600", "127.0.0.1" -PassThru -WindowStyle Hidden
    New-ItemProperty -Path $hkcuRun -Name $runName -Value ('"{0}" -n 600 127.0.0.1' -f $victimExe) -PropertyType String -Force | Out-Null
    if ($isAdmin) { New-ItemProperty -Path $hklmRun -Name $runNameM -Value ('"{0}" -n 600 127.0.0.1' -f $victimExe) -PropertyType String -Force | Out-Null }
    schtasks.exe /Create /TN "MemOptE2E\$taskName" /TR ('"{0}" -n 600 127.0.0.1' -f $victimExe) /SC ONCE /ST 23:59 /F | Out-Null
    Start-Sleep -Seconds 1
    $f = Find-Feature $victimId
    $plan = $null
    Check "計画に 正常終了/強制終了/優先度/スタートアップ/タスク が並ぶ" {
        $script:plan = (Expect-Ok (Call "PlanStop" @{ feature_id = $victimId } 90)).plan
        $m = $script:plan.options | ForEach-Object { $_.method }
        foreach ($want in "GracefulClose", "ForceKill", "LowerPriority", "DisableStartup", "DisableTask") { Assert ($m -contains $want) "$want が無い: $($m -join ',')" }
        "options=$($m.Count) safety=$($script:plan.safety)"
    }
    # --- スタートアップ（HKCU は常に、HKLM は管理者のとき）---
    Check "スタートアップ無効化: 確認なしは拒否される（2 段階確認の強制）" {
        $r = Stop-Op $victimId "DisableStartup" $runName $false
        Assert ($r.type -eq "Error" -and $r.code -eq "denied") "拒否されなかった: $($r | ConvertTo-Json -Compress)"
        Assert ($null -eq (Get-ItemProperty -Path "HKCU:\$approvedRel" -Name $runName -ErrorAction SilentlyContinue).$runName) "拒否されたのに書き換わっている"
        "denied"
    }
    $scopes = @(@{ Label = "HKCU"; Name = $runName; Hive = "HKCU:\$approvedRel"; Run = $hkcuRun })
    if ($isAdmin) { $scopes += @{ Label = "HKLM"; Name = $runNameM; Hive = "HKLM:\$approvedRel"; Run = $hklmRun } }
    else { Record "スタートアップ無効化（HKLM）" "SKIP" "管理者ではない" }
    foreach ($sc in $scopes) {
        $sc = $sc
        Check "スタートアップ無効化 [$($sc.Label)] → StartupApproved が無効値(3) → 復元で元に戻る" {
            $r = Expect-Ok (Stop-Op $victimId "DisableStartup" $sc.Name $true)
            Assert ($r.entry.outcome -eq "Ok") "失敗: $($r.entry.outcome | ConvertTo-Json -Compress)"
            $v = (Get-ItemProperty -Path $sc.Hive -Name $sc.Name -ErrorAction SilentlyContinue).($sc.Name)
            Assert ($v -and $v[0] -eq 3) "無効値(3)になっていない"
            Expect-Ok (Call "Undo" @{ journal_id = $r.entry.id } 60) | Out-Null
            $v2 = (Get-ItemProperty -Path $sc.Hive -Name $sc.Name -ErrorAction SilentlyContinue).($sc.Name)
            Assert (-not ($v2 -and $v2[0] -eq 3)) "復元後も無効のまま"
            Assert ($null -ne (Get-ItemProperty -Path $sc.Run -Name $sc.Name -ErrorAction SilentlyContinue).($sc.Name)) "Run の登録が消えている"
            "journal #$($r.entry.id)"
        }
    }

    # --- タスク ---
    Check "タスク無効化 → Disabled / 復元 → Ready" {
        $r = Expect-Ok (Stop-Op $victimId "DisableTask" $taskPath $true)
        Assert ($r.entry.outcome -eq "Ok") "失敗: $($r.entry.outcome | ConvertTo-Json -Compress)"
        Assert ((Get-ScheduledTask -TaskPath "\MemOptE2E\" -TaskName $taskName).State -eq "Disabled") "Disabled になっていない"
        Expect-Ok (Call "Undo" @{ journal_id = $r.entry.id } 60) | Out-Null
        Assert ((Get-ScheduledTask -TaskPath "\MemOptE2E\" -TaskName $taskName).State -ne "Disabled") "復元後も Disabled"
        "journal #$($r.entry.id)"
    }

    # --- 優先度 ---
    Check "メモリ優先度を下げる → 復元" {
        $r = Expect-Ok (Stop-Op $victimId "LowerPriority")
        Assert ($r.entry.outcome -eq "Ok") "失敗"
        Expect-Ok (Call "Undo" @{ journal_id = $r.entry.id } 60) | Out-Null
        "journal #$($r.entry.id)"
    }

    # --- 強制終了 ---
    Check "強制終了: プロセスが消え、元に戻せない操作として記録される" {
        $r = Expect-Ok (Stop-Op $victimId "ForceKill")
        Assert ($r.entry.outcome -eq "Ok") "失敗"
        Start-Sleep -Milliseconds 500
        Assert ($victim.HasExited) "プロセスが残っている"
        $u = Call "Undo" @{ journal_id = $r.entry.id }
        Assert ($u.type -eq "Error") "強制終了を Undo できてしまった"
        "avail $([int]($r.entry.avail_before/1MB)) → $([int]($r.entry.avail_after/1MB)) MB"
    }

    # ================= 3. 実サービス =================
    $cand = $null
    if (-not $isAdmin) { Record "実サービスの停止/復元" "SKIP" "管理者ではない" }
    else {
        $names = if ($ServiceName) { @($ServiceName) } else { @("Fax", "WSearch", "SysMain") }
        foreach ($n in $names) {
            $s = Get-CimInstance Win32_Service -Filter "Name='$n'" -ErrorAction SilentlyContinue
            if ($s -and $s.State -eq "Running" -and $s.AcceptStop -and $s.StartMode -in @("Auto", "Manual") -and -not (Get-Service $n).DependentServices) { $cand = $s; break }
        }
        if (-not $cand) { Record "実サービスの停止/復元" "SKIP" "使えるサービスが無い（-ServiceName で指定可）" }
        else {
            $svcBefore = [pscustomobject]@{ Name = $cand.Name; StartMode = $(if ($cand.StartMode -eq "Auto") { "Automatic" } else { $cand.StartMode }); State = $cand.State }
            $sid = "svc:" + $cand.Name
            Write-Host "  対象サービス: $($cand.Name)（$($cand.DisplayName)）" -ForegroundColor Cyan
            $null = Find-Feature $sid
            Check "サービス停止 → Stopped / 復元 → Running" {
                $r = Expect-Ok (Stop-Op $sid "ServiceStop" $cand.Name)
                Assert ($r.entry.outcome -eq "Ok") "失敗: $($r.entry.outcome | ConvertTo-Json -Compress)"
                Assert ((Get-Service $cand.Name).Status -eq "Stopped") "停止していない"
                Expect-Ok (Call "Undo" @{ journal_id = $r.entry.id } 90) | Out-Null
                Assert ((Get-Service $cand.Name).Status -eq "Running") "復元後に起動していない"
                "journal #$($r.entry.id)"
            }
            Check "スタートアップ種別「手動」→ 元の種別へ復元（遅延自動も保持）" {
                $before = (Get-CimInstance Win32_Service -Filter "Name='$($cand.Name)'")
                $r = Expect-Ok (Stop-Op $sid "ServiceStartupManual" $cand.Name $true)
                if ($cand.StartMode -ne "Auto") { Record "  (開始種別が自動でないため「手動」変更は NotOffered になり得る)" "SKIP" "" ; return "skipped" }
                Assert ($r.entry.outcome -eq "Ok") "失敗: $($r.entry.outcome | ConvertTo-Json -Compress)"
                Assert ((Get-CimInstance Win32_Service -Filter "Name='$($cand.Name)'").StartMode -eq "Manual") "手動になっていない"
                Expect-Ok (Call "Undo" @{ journal_id = $r.entry.id } 60) | Out-Null
                $after = (Get-CimInstance Win32_Service -Filter "Name='$($cand.Name)'")
                Assert ($after.StartMode -eq $before.StartMode) "種別が戻っていない: $($before.StartMode) → $($after.StartMode)"
                "$($before.StartMode)"
            }
        }
    }

    # ================= 4. 保護対象 =================
    Check "EventLog（OS 中核）は選択肢が無く、停止がすべて拒否される" {
        $null = Find-Feature "svc:EventLog"
        $p = (Expect-Ok (Call "PlanStop" @{ feature_id = "svc:EventLog" } 60)).plan
        Assert ($p.safety -eq "Protected") "Protected ではない: $($p.safety)"
        Assert (@($p.options).Count -eq 0) "選択肢がある"
        foreach ($m in "ServiceStop", "ServiceStartupDisabled", "ForceKill") {
            $r = Stop-Op "svc:EventLog" $m "EventLog" $true
            Assert ($r.type -eq "Error" -and $r.code -eq "denied") "$m が拒否されない"
        }
        "protected"
    }
    Check "本ツール自身（memopt-service）は停止できない" {
        $self = ((Call "GetFeatures" @{} 40).features | Where-Object { $_.name -like "memopt-service*" } | Select-Object -First 1)
        Assert ($null -ne $self) "一覧に自分自身が無い"
        Assert ($self.safety -eq "Protected") "Protected ではない: $($self.safety)"
        "protected"
    }

    # ================= 5. 一括復元 =================
    Check "一括復元（UndoAll）: 残っている復元可能な変更が無い / または全て戻る" {
        $r = Expect-Ok (Call "UndoAll" @{} 90)
        Assert (@($r.failed).Count -eq 0) "失敗あり: $($r.failed | ConvertTo-Json -Compress)"
        "restored=$(@($r.restored).Count)"
    }
    Check "ジャーナル: 追記のみで、復元済みは undone として残る" {
        $j = (Expect-Ok (Call "GetJournal" @{ limit = 100 })).entries
        Assert (@($j).Count -ge 4) "記録が少ない: $(@($j).Count)"
        $pending = @($j | Where-Object { $_.outcome -eq "Ok" -and $null -eq $_.undone_ts -and $_.change.kind -ne "None" })
        Assert ($pending.Count -eq 0) "未復元の変更が残っている: $($pending.Count)"
        "entries=$(@($j).Count)"
    }
    $script:pipe.Dispose(); $script:pipe = $null
    Check "CLI: journal 一覧と undo --all（変更が無ければ 0 件）" {
        $out = & $cliExe journal --journal (Join-Path $env:MEMOPT_DATA_DIR "journal.jsonl") 2>&1 | Out-String
        Assert ($out -match "#\d+") "journal 一覧が出ない: $out"
        $u = & $cliExe undo --all --journal (Join-Path $env:MEMOPT_DATA_DIR "journal.jsonl") 2>&1 | Out-String
        Assert ($u -match "復元 0 件") "復元 0 件になっていない: $u"
        "ok"
    }
}
finally {
    Cleanup
}

Write-Host ""
$results | Format-Table -AutoSize -Wrap | Out-String | Write-Host
$fail = @($results | Where-Object { $_.Result -eq "FAIL" }).Count
$skip = @($results | Where-Object { $_.Result -eq "SKIP" }).Count
$report = Join-Path $root ("dist\verify-admin-{0:yyyyMMdd-HHmmss}.txt" -f (Get-Date))
New-Item -ItemType Directory -Force (Split-Path $report) | Out-Null
($results | Format-Table -AutoSize -Wrap | Out-String) | Set-Content -Encoding UTF8 $report
Write-Host ("結果: PASS {0} / FAIL {1} / SKIP {2}  （レポート: {3}）" -f @($results | Where-Object { $_.Result -eq "PASS" }).Count, $fail, $skip, $report)
if ($fail -gt 0) { exit 1 }
