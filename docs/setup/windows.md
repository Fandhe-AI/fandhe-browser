# Windows セットアップ手順

対応 `TASK-56`（56.2）/ `MS-3` / ビヘイビア `XOS-6`（[spec-reference](../../.claude/rules/spec-reference.md)）。

## 目的・適用範囲

Linux・macOS・Windows の 3 OS 一級対応のうち、Windows でソースからビルド・テストするための手順です。対象は 64bit（`x86_64-pc-windows-msvc`）で、32bit は対象外です。

- 手順は「既定ビルド」と「Servo 組込ビルド」の 2 段に分けます
- 本手順書は Linux 上で作成しており、実機の Windows での再現は未実施です。CI 側の検証は `windows-servo-prereqs` ジョブ（TASK-56.1・#195・PR #583。マージ状況は PR で確認してください）が担います

## 現状

Servo は現時点でどの crate の依存にも入っていません（`fandhe-browser-render` は雛形です）。したがって次の区分になります。

| ビルド | 必要な前提 | 状態 |
| ------ | ---------- | ---- |
| 既定ビルド（Servo なし） | [既定ビルドの前提](#既定ビルドの前提) | 現在ビルド可能 |
| Servo 組込ビルド（feature `rendering`） | 上記に加えて [Servo 組込ビルドの追加前提](#servo-組込ビルドの追加前提xos-6) | 未実装。Windows での実ビルドの CI 化は TASK-58 以降で予定 |

## 既定ビルドの前提

管理者権限が必要な操作（Visual Studio Installer 等）はその旨を明記します。

1. [Git for Windows](https://git-scm.com/download/win)（Git Bash を含む）。長いパス対策として次を設定します。

   ```powershell
   git config --global core.longpaths true
   ```

2. [rustup](https://rustup.rs/)。`rust-toolchain.toml` が stable・rustfmt・clippy を自動選択します。ホストは `x86_64-pc-windows-msvc` を使います。
3. Visual Studio Build Tools の「C++ によるデスクトップ開発」ワークロード（MSVC と Windows SDK を含みます。管理者権限が必要）。C コードを含む依存のビルドとリンクに使います。
4. GNU Make（[Chocolatey](https://community.chocolatey.org/packages/make) または [Scoop](https://scoop.sh/)）。`Makefile` は `SHELL := /bin/bash` を前提とするため、`make` は Git Bash か WSL から実行します。

その後は README「クイックスタート」の手順（`make setup` → `cargo build --workspace` → `make ci`）に従います。

## Servo 組込ビルドの追加前提（XOS-6）

Servo を組み込む Windows ビルド（feature `rendering`）は、3 OS のうち最も前提が重くなります（PoC-12）。次をすべて満たしてください。

| 前提 | 下限・条件 |
| ---- | ---------- |
| Windows SDK | 10.0.19041.0 以上 |
| MSVC | v143 ツールセット（MSVC 14.30 以上 14.50 未満） |
| C++ ATL | v143 向け（x86 と x64） |
| Python | 3.x |
| `uv` | バージョンを固定して導入 |

### Visual Studio のバージョンによる違い

- Visual Studio 2022: 「Visual Studio Installer」の「個別のコンポーネント」で、次を選択します。
  - 「MSVC v143 - VS 2022 C++ x64/x86 ビルドツール」
  - 「最新の v143 ビルドツール用 C++ ATL (x86 & x64)」
  - 「Windows 11 SDK」または「Windows 10 SDK」（10.0.19041.0 以上）
- Visual Studio 2026: 既定のツールセットは v145 です。v143 ツールセットとその ATL は、サイドバイサイドのコンポーネントとして追加で導入します。CI の実測では ATL のコンポーネント ID は `Microsoft.VisualStudio.Component.VC.14.44.17.14.ATL` です。他のマイナー版の ID は Microsoft Learn のコンポーネント一覧で確認してください（本手順書では未検証）。
- Servo 側が VS 2026 に対応しているかは未検証です。Servo Book「Building on Windows」の最新記述を確認してください。

### コマンドラインでの導入

管理者権限の PowerShell で、既存の Visual Studio へコンポーネントを追加します。`<VS のインストールパス>` は次節の `vswhere.exe` で取得できます。

```powershell
& "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\setup.exe" modify `
  --installPath "<VS のインストールパス>" `
  --add Microsoft.VisualStudio.Component.VC.14.44.17.14.ATL `
  --quiet --norestart
```

新規導入は `winget install Microsoft.VisualStudio.2022.BuildTools --override "--add <コンポーネント ID> --passive"` の形式でも行えます。コンポーネント ID は Microsoft Learn の一覧で確認してください。

### Python と uv

- Python 3 は `winget` の公式パッケージ、[python.org](https://www.python.org/downloads/windows/) の公式インストーラ、または `choco install python` で導入します。
- `uv` は取得したスクリプトをそのまま実行する方式（`irm ... | iex` 等）を避け、バージョンとハッシュを固定して導入します。CI（`windows-servo-prereqs`）は wheel の sha256 を固定し、`pip` の `--require-hashes` で検証しています。固定するバージョンとハッシュは `.github/workflows/ci.yml` の `UV_VERSION`・`UV_WHEEL_SHA256` と同期してください。

  ```powershell
  # <版> と <sha256> は ci.yml の値に合わせる
  "uv==<版> --hash=sha256:<sha256>" | Out-File uv-requirements.txt -Encoding ascii
  python -m pip install --require-hashes --only-binary=:all: --no-deps -r uv-requirements.txt
  ```

### 注意点

- Servo の `target/` は 10〜15GB に達すると公称されています（PoC-12）。空き容量を確認し、長パスを避けるため `CARGO_TARGET_DIR` は短いパス（例: `C:\t`）に置くことを推奨します。
- `cmake` が必要になる可能性があります（PoC-6 の macOS 実測での知見。Windows では未検証）。

## 前提の確認コマンド

PowerShell で実行します。CI の `windows-servo-prereqs` ジョブと同じ判定です。

```powershell
# Visual Studio のインストールパスと MSVC v143（14.30 以上 14.50 未満）
$vs = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -products * -latest `
  -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
$v143 = Get-ChildItem (Join-Path $vs 'VC\Tools\MSVC') -Directory |
  Where-Object { [version]$_.Name -ge [version]'14.30' -and [version]$_.Name -lt [version]'14.50' } |
  Sort-Object { [version]$_.Name } -Descending | Select-Object -First 1
$v143.Name

# C++ ATL（v143 向け）。ファイルが存在すれば True
Test-Path (Join-Path $v143.FullName 'atlmfc\include\atlbase.h')

# Windows SDK（10.0.19041.0 以上が 1 件以上あること）
Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\Include" -Directory |
  Where-Object { $_.Name -match '^10\.\d+\.\d+\.\d+$' -and [version]$_.Name -ge [version]'10.0.19041.0' }

# Python 3.x と uv
python --version
python -m uv --version
```

## トラブルシューティング

| 症状 | 原因と対処 |
| ---- | ---------- |
| `atlbase.h` が見つからない | v143 向けではなく v145 向け ATL を入れている可能性があります。v143 向けのコンポーネントを追加してください |
| パスが長すぎるエラー | `core.longpaths` を有効化し、`CARGO_TARGET_DIR` を短いパスにします。リポジトリも浅いパスに clone します |
| `make` が `/bin/bash` を見つけられない | PowerShell や cmd からではなく、Git Bash か WSL から実行してください |

## 参考

- Servo Book「Building on Windows」
- Microsoft Learn「Visual Studio Build Tools component directory」
- uv 公式ドキュメントのインストール手順
- spec ID: `XOS-5`・`XOS-6`・`TASK-56`・`TASK-58`
