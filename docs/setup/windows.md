# Windows セットアップ手順

対応 `TASK-56`（56.2）/ `MS-3` / ビヘイビア `XOS-6`（[spec-reference](../../.claude/rules/spec-reference.md)）。

## 目的・適用範囲

Linux・macOS・Windows の 3 OS 一級対応のうち、Windows でソースからビルド・テストするための手順です。対象は 64bit（`x86_64-pc-windows-msvc`）で、32bit は対象外です。

- 手順は「既定ビルド」と「Servo 組込ビルド」の 2 段に分けます
- 本手順書は Linux 上で作成しており、実機の Windows での再現は未実施です。既定ビルドについては、現行の `.github/workflows/ci.yml` の `rust-ci` と `rust-ci-default-features` が `windows-latest` を含む 3 OS matrix で稼働しています。一方、Servo 組込ビルドに固有の前提（v143 ツールセット・ATL・uv 等）を検証する Windows CI は未導入です（TASK-56.1・#195）。導入され次第、本手順書の確認コマンドをその判定と突き合わせます

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
4. GNU Make（[Chocolatey](https://community.chocolatey.org/packages/make) または [Scoop](https://scoop.sh/)）。`Makefile` は `SHELL := /bin/bash` を前提とするため、`make` は Git Bash など Windows 側で動く bash から実行します（WSL は別方式のため、後述の「WSL について」を参照）。
5. Node.js（`npx` を含む LTS）。`winget install OpenJS.NodeJS.LTS` または [公式インストーラ](https://nodejs.org/) で導入します。`make setup` の hooks は `lefthook` 未導入かつ `brew` 不在の環境では `npx` で lefthook を導入するため、Windows では実質必須です。`make ci` の `lint-md`・`lint-editorconfig`・`lint-commits` も `npx` を使います。
6. `jq`（`winget install jqlang.jq` または `choco install jq` / `scoop install jq`）。`make ci` の `check-compat-regression`・`check-publish-private` 等が使い、未導入だと fail-closed で止まります。
7. `yamllint` または `uv`（`uvx`）のどちらか。`make ci` の `lint-yaml` は `yamllint` があればそれを、なければ `uvx` の固定版を使い、どちらも無ければ失敗します。`pip install yamllint` か、`uv` の導入（[Python と uv](#python-と-uv) を参照）のいずれかを済ませてください。
8. lefthook（任意）。導入済みなら `make hooks` はそれを使います。未導入でも上記 5 の `npx` があれば自動で固定版を使うため、単独での導入は不要です。

`make setup` と `make ci` は、上記 1〜7 がすべて PATH 上にある状態で実行してください。その後は README「クイックスタート」の手順（`make setup` → `cargo build --workspace` → `make ci`）に従います。

### WSL について

WSL 上で rustup や Make を実行すると、ホストは Linux（`x86_64-unknown-linux-gnu`）になり、本手順書の対象である `x86_64-pc-windows-msvc` のビルドにはなりません。WSL は Linux ビルドを Windows 上で行う別の方式であり、Linux 側の手順（README の開発環境構築）に従ってください。本手順では Git Bash など Windows 側で動く環境を使います。

## Servo 組込ビルドの追加前提（XOS-6）

Servo を組み込む Windows ビルド（feature `rendering`）は、3 OS のうち最も前提が重くなります（PoC-12）。次をすべて満たしてください。

| 前提 | 下限・条件 |
| ---- | ---------- |
| Windows SDK | 10.0.19041.0 以上 |
| MSVC | v143 ツールセット（MSVC 14.30 以上 14.50 未満） |
| C++ ATL | v143 向け（x86 と x64） |
| Python | 3.x |
| `uv` | バージョンを固定して導入（Servo 組込ビルドでは必須。既定ビルドでは `yamllint` を別途導入すれば不要） |

### Visual Studio のバージョンによる違い

- Visual Studio 2022: 「Visual Studio Installer」の「個別のコンポーネント」で、次を選択します。
  - 「MSVC v143 - VS 2022 C++ x64/x86 ビルドツール」
  - 「最新の v143 ビルドツール用 C++ ATL (x86 & x64)」
  - 「Windows 11 SDK」または「Windows 10 SDK」（10.0.19041.0 以上）
- Visual Studio 2026: 既定のツールセットは v145 です。v143 ツールセットとその ATL は、サイドバイサイドのコンポーネントとして追加で導入します。CI の実測では ATL のコンポーネント ID は `Microsoft.VisualStudio.Component.VC.14.44.17.14.ATL`、対応する v143 ツールセット（MSVC 14.44）のコンポーネント ID は `Microsoft.VisualStudio.Component.VC.14.44.17.14.x86.x64` です（ATL の ID と同じ `14.44.17.14` 系列で揃えます）。他のマイナー版の ID は Microsoft Learn のコンポーネント一覧で確認してください（本手順書では未検証）。
- Servo 側が VS 2026 に対応しているかは未検証です。Servo Book「Building on Windows」の最新記述を確認してください。

### コマンドラインでの導入

管理者権限の PowerShell で、既存の Visual Studio へコンポーネントを追加します。`<VS のインストールパス>` は次節の `vswhere.exe` で取得できます。

```powershell
& "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\setup.exe" modify `
  --installPath "<VS のインストールパス>" `
  --add Microsoft.VisualStudio.Component.VC.14.44.17.14.x86.x64 `
  --add Microsoft.VisualStudio.Component.VC.14.44.17.14.ATL `
  --quiet --norestart
```

VS 2026 では v143 ツールセット（`...x86.x64`）と ATL（`...ATL`）の両方を必ず追加します。ATL だけでは v143 のコンパイラが入らず、v145 が既定のままになります。導入後は [前提の確認コマンド](#前提の確認コマンド) で、v143 のバージョン（14.30 以上 14.50 未満）と `atlbase.h` の存在の両方を確認してください。VS 2022 の場合は、上記の「個別のコンポーネント」の選択で同じ 2 つが入ります。

新規導入は `winget install Microsoft.VisualStudio.2022.BuildTools --override "--add <コンポーネント ID> --passive"` の形式でも行えます。コンポーネント ID は Microsoft Learn の一覧で確認してください。

### v143 ツールセットをビルドで選択する

VS 2026 では、v143 を導入しても既定のツールセットは v145 のままです。Servo 組込ビルドが v143 を使うよう、ビルドを実行するシェルで v143 の開発者環境を明示的に選択します（本手順は Linux 上で作成しており、実機の Windows では未検証です）。

1. 「x64 Native Tools Command Prompt」ではなく、通常の `cmd.exe` から `vcvarsall.bat` を `-vcvars_ver` 付きで呼び出します。`-vcvars_ver` には導入した v143 の版（前提の確認コマンドで表示される `14.44` 等）を `14.4` のように上位 2 桁で、または完全な版番号で指定します。

   ```bat
   rem <VS のインストール先> は vswhere で得た installationPath に置き換える
   "<VS のインストール先>\VC\Auxiliary\Build\vcvarsall.bat" x64 -vcvars_ver=14.4
   ```

2. 同じシェルから Git Bash（`bash`）を起動して `make` を実行します。`vcvarsall.bat` が設定した環境変数（`PATH`・`INCLUDE`・`LIB`・`VCToolsVersion`）が引き継がれ、Rust の `cc` クレートや `cmake` は `PATH` 上の `cl.exe` を使います。
3. ビルド前に、実際に使われるコンパイラが v143 であることを確認します。

   ```bat
   echo %VCToolsVersion%
   cl
   ```

   `VCToolsVersion` が 14.30 以上 14.50 未満であり、`cl` の出力の `Version 19.4x`（14.4x に対応。v143 は 19.3x〜19.4x）であれば v143 が選択されています。`VCToolsVersion` が 14.50 以上（v145）、または `cl` が `19.5x` 以降を示す場合は、`-vcvars_ver` の指定を見直し、`vcvarsall.bat` を新しいシェルで再実行してください。

### Python と uv

- Python 3 は `winget` の公式パッケージ、[python.org](https://www.python.org/downloads/windows/) の公式インストーラ、または `choco install python` で導入します。
- `uv` は取得したスクリプトをそのまま実行する方式（`irm ... | iex` 等）を避け、バージョンとハッシュを固定して導入します。現行の `.github/workflows/ci.yml` には、Servo 向けの固定値（`uv` のバージョン・wheel の sha256）はまだ存在しません（既定ビルドの CI は `uv` を必要としません。ローカルの `make ci` の `lint-yaml` は `yamllint` の代替として `uvx` を使えます）。Servo 固有の Windows CI 導入（TASK-56.1・#195）までは、次の手順で自分で確認した値を固定してください。
  1. [PyPI の uv](https://pypi.org/project/uv/#files) で採用するバージョンを選び、Windows x86_64 向け wheel（`win_amd64`）の SHA256 を控える
  2. 控えた値で次のように導入する（`pip` の `--require-hashes` がハッシュ不一致を拒否する）
  3. Servo 固有の Windows CI に固定値が導入された後は、その値へ合わせる

  ```powershell
  # <版> と <sha256> は PyPI で確認した値に置き換える
  "uv==<版> --hash=sha256:<sha256>" | Out-File uv-requirements.txt -Encoding ascii
  python -m pip install --require-hashes --only-binary=:all: --no-deps -r uv-requirements.txt
  ```

### 注意点

- Servo の `target/` は 10〜15GB に達すると公称されています（PoC-12）。空き容量を確認し、長パスを避けるため `CARGO_TARGET_DIR` は短いパス（例: `C:\t`）に置くことを推奨します。
- `cmake` が必要になる可能性があります（PoC-6 の macOS 実測での知見。Windows では未検証）。

## 前提の確認コマンド

PowerShell で実行します。Servo 固有の Windows CI（TASK-56.1・#195）が導入された後は、その判定と同じ内容になる想定です。

```powershell
# インストール済みの全 Visual Studio を列挙し、MSVC v143（14.30 以上 14.50 未満）を探す
# （-latest は最新の 1 件のみ返すため、VS 2026 と VS 2022 が併存する環境で v143 を見逃す）
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vsPaths = & $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
$v143 = $vsPaths | ForEach-Object { Join-Path $_ 'VC\Tools\MSVC' } |
  Where-Object { Test-Path $_ } | ForEach-Object { Get-ChildItem $_ -Directory } |
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
| ビルドで v145（14.50 以上）のコンパイラが使われる | v143 を導入しても既定は v145 のままです。[v143 ツールセットをビルドで選択する](#v143-ツールセットをビルドで選択する) の手順で `vcvarsall.bat -vcvars_ver=14.4` を指定し、`cl` の版を確認してください |
| `atlbase.h` が見つからない | v143 向けではなく v145 向け ATL を入れている可能性があります。v143 向けのコンポーネントを追加してください |
| パスが長すぎるエラー | `core.longpaths` を有効化し、`CARGO_TARGET_DIR` を短いパスにします。リポジトリも浅いパスに clone します |
| `make` が `/bin/bash` を見つけられない | PowerShell や cmd からではなく、Git Bash など Windows 側の bash から実行してください（WSL は Linux ビルドになります） |

## 参考

- Servo Book「Building on Windows」
- Microsoft Learn「Visual Studio Build Tools component directory」
- uv 公式ドキュメントのインストール手順
- spec ID: `XOS-5`・`XOS-6`・`TASK-56`・`TASK-58`
