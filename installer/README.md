# MemOpt インストーラー（MSI）

`installer\build.ps1` がリリースビルドから MSI までを作ります。

## 必要なもの
| 何 | 用途 | 入手 |
|---|---|---|
| Rust (MSVC) | リリースビルド | rustup |
| WiX Toolset v3.14 | MSI の作成（`candle`/`light`、`WixUtilExtension`） | 公式: <https://wixtoolset.org/docs/wix3/> のインストーラー、または `choco install wixtoolset` |
| Windows SDK | `signtool.exe`（署名する場合） | Visual Studio Installer / Windows SDK |
| コード署名証明書 | 署名（**未決事項: 調達**） | 下記 |

`WixBin` は環境変数 `WIX`（WiX が設定する）か `-WixBin` で指定します。

## 使い方
```powershell
# 未署名（開発・社内検証用。警告が出ます）
.\installer\build.ps1

# 署名あり（ストア内の証明書）
.\installer\build.ps1 -CertThumbprint "<拇印>" -Version 0.1.0

# 署名あり（PFX。パスワードは環境変数から）
$env:MEMOPT_PFX_PASSWORD = "..."
.\installer\build.ps1 -PfxPath C:\keys\memopt.pfx
```
成果物は `dist\MemOpt-<version>-x64.msi`。署名した場合は、3 つの exe と MSI の署名を
`Get-AuthenticodeSignature` で検証し、SHA256 を表示します。

## コード署名について
未署名の常駐・管理者権限ツールは SmartScreen や他のウイルス対策製品で警告されやすいため、
**配布するビルドには必ず署名してください**（設計書6章）。

- 証明書: 一般的な OV/EV コード署名証明書が使えます。EV は SmartScreen の評判が付きやすい一方、
  ハードウェアトークンが必要で CI では扱いにくい点に注意してください。
- タイムスタンプ: 既定は `http://timestamp.digicert.com`（RFC 3161）。証明書の期限後も署名が有効になります。
- 署名順序: exe → MSI の順（MSI に入る前の exe に署名する）。スクリプトが自動で行います。
- 鍵は **リポジトリに置かない**でください。CI では Azure Key Vault / HSM 等の署名サービスを推奨します。

## インストールされるもの
- `C:\Program Files\MemOpt\` に `memopt-service.exe` / `memopt-tray.exe` / `memopt-cli.exe`
- Windows サービス `MemOptService`（LocalSystem・自動起動、回復設定: 1 回目の失敗で 60 秒後に再起動、2 回目以降は停止のまま）
- ログオン時のトレイ自動起動（HKLM の Run、全ユーザー、一般権限で動作）
- データ: `%ProgramData%\MemOpt\`（config.toml / history.db / journal.jsonl / logs）。書き込みは SYSTEM と Administrators のみ。

## アンインストール
- 対話モード: 未復元の永続変更（スタートアップ無効化・サービス種別変更など）があれば一覧を示し、
  元に戻すか確認します（`memopt-cli uninstall-prompt`）。
- サイレント（`msiexec /x ... /qn`）: 何も変更せず、後から `memopt-cli undo --all` で戻せます。
- `%ProgramData%\MemOpt\` は削除しません（ジャーナルは復元に必要なため）。

## 検証チェックリスト（VM で）
Windows 10 と 11 の VM（Hyper-V のチェックポイントから毎回復元）で:
1. インストール → サービスが起動し、トレイがログオン後に出る。
2. `sc qfailure MemOptService` が「1 回目: 再起動 60 秒 / 2 回目以降: なし」になっている。
3. 停止（スタートアップ無効化など）→ 再起動 → `memopt-cli undo --all` で元に戻る。
4. アップグレード（同じ UpgradeCode の新版）で設定・履歴・ジャーナルが残る。
5. アンインストール（対話）で復元の確認が出る。サイレントでは何も変更されない。

## 開発環境の WiX（管理者権限なしで用意する方法）
通常のインストーラー（`wix314.exe`）は管理者権限と .NET 3.5 を要求します。管理者が使えない環境では、
次の手順でツールだけを取り出して使えます（この開発環境ではこの方法で MSI をビルドしました）。

1. `winget download --id WiXToolset.WiXToolset -e --download-directory <dir>` — マニフェストの SHA256 で検証されます。
   さらに `Get-AuthenticodeSignature` で `.NET Foundation` の署名が Valid であることを確認します。
2. 7-Zip でバンドル内の CAB（`7z x -t# WiX314.exe`、続いて最大の `4.cab`）を展開し、
   `WiX Toolset v3.14 Core` の MSI を `msiexec /a <msi> /qn TARGETDIR=<dir>`（管理者インストール＝展開のみ）で展開します。
3. `...\WiX Toolset v3.14\bin` を `%LOCALAPPDATA%\MemOptTools\wix314\bin` に置き、`build.ps1 -WixBin` に渡します。

**GitHub の `wix314-binaries.zip` は署名されておらず、改ざんの有無を確認できないため使いません。**

## ビルドの検証（実施済み）
- `candle`/`light`（ICE 検証つき）が成功し、`WixUtilExtension` も解決される。
- MSI のデータベースを直接確認: サービス（LocalSystem・自動起動・Own process）、回復設定
  （1 回目 restart 60 秒 / 2 回目・3 回目 none / リセット 1 日）、HKLM Run、アンインストール前カスタムアクション
  （`StopServices` より前・対話時のみ）、アップグレード表（同一 UpgradeCode）、起動条件（64 ビット Windows）。
- MSI 内の 3 つの exe のサイズが `target\release` と一致する。
- 署名パイプライン（`signtool` + RFC3161 タイムスタンプ + 検証）を、使い捨ての自己署名証明書で通した
  （`-AllowSelfSigned`。**開発用であり配布用ではない**。証明書は削除済み）。

## 実機検証スクリプト
| スクリプト | 内容 | 権限 |
|---|---|---|
| `verify-admin.ps1` | `--console` 起動のサービスで L2/L3・停止手段・復元・保護を確認 | 管理者 |
| `verify-msi.ps1` | 本物の MSI をインストール → サービス/回復設定/ACL/自動起動 → 回復動作 → アップグレード → アンインストール | 管理者（**VM 推奨**） |

```powershell
# 0.1.1 を作ってアップグレードも確認する例
.\installer\build.ps1 -Version 0.1.0 -WixBin <bin>
.\installer\build.ps1 -Version 0.1.1 -WixBin <bin>
powershell -NoProfile -ExecutionPolicy Bypass -File .\installer\verify-msi.ps1 -UpgradeMsi .\dist\MemOpt-0.1.1-x64.msi
```
`verify-msi.ps1` は既に MemOptService が入っていると中止します。回復動作の確認（強制終了 2 回）に約 3 分かかります（`-SkipRecovery` で省略可）。

## 注意
- **MSI の実機インストールは、まだ実行していません**（ビルドと静的な検証のみ）。`verify-msi.ps1` を VM で実行してください。
- 署名証明書は未調達です。配布するビルドは署名してください（上記）。
- UpgradeCode（`8D7C1F34-…`）は変更しないでください（アップグレードの同一性）。
- `build.ps1` は exe を `dist\stage` にコピーして署名します（cargo の成果物は書き換えません）。
