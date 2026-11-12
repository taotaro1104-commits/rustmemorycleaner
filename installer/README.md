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

## 注意
- この MSI の **ビルドと実機インストールは、開発環境ではまだ実行していません**（WiX 未導入のため）。
  `memopt.wxs` は上記チェックリストで検証してください。
- UpgradeCode（`8D7C1F34-…`）は変更しないでください（アップグレードの同一性）。
