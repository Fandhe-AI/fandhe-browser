# プロファイル分離テストの 3 OS 実機実施手順

対応 `TASK-63` / `MS-5` / ビヘイビア `XOS-10`・`MEAS-7`・`PROF-2`（Issue #295）。
実機作業（担当: 人間）の手順書であり、**測定結果ではない**。実施結果は #295 に記録する。

## 目的と合格基準

`PROF-2`（異なるアカウントでの同一サイトアクセスで、プロファイル間のデータ漏洩が 0 件）を、
Linux ext4・macOS APFS・Windows NTFS の実機で確認する（`XOS-10`・`MEAS-7`）。

- 合格: 3 OS それぞれで対象テストが全件成功し、漏洩件数が 0 件
- 成果物: OS・ファイルシステム別の測定結果（本書「結果記録テンプレート」）

## 現状の実施可否

| OS / FS | 実施可否 | 備考 |
| ------- | -------- | ---- |
| Linux ext4 | 可 | 対象テストは unix で実行される |
| macOS APFS | 可 | 同上 |
| Windows NTFS | 可（Issue #954 以降） | 下記の限界あり |

### Windows NTFS での実行と限界

- Windows の `Profile::open` は、ルートが既知フォルダ（`FOLDERID_LocalAppData`）配下のときだけ成功する
  （Issue #928・#951）。新規ルートには保護付き DACL を設定し（#952）、既存ルートは所有者と DACL を検証する
  （#953）。配下外は `ProfileError::Unsupported` でディレクトリを作らない（fail-closed）
- 分離テスト（`tests/isolation.rs`・`tests/concurrent_isolation.rs`）は Issue #954 で Windows でも
  実行されるようになった。テストのルートは既知フォルダ配下の `fandhe-test-tmp`
  （`fandhe_browser_profile::test_base_dir()`）で、`TMPDIR`・`TEMP` は効かない
- **限界 1**: Windows の `Profile::create_file_in`（ハンドル相対の書き込み）は #948 配下の Issue の完了まで
  未実装のため、Windows のテストは `tests/support/mod.rs` のアダプタ経由（検証済みパスを std で開く）で
  書き込む。確かめているのは `Profile::open` が作るレイアウト・ロック・DACL・ルートのハンドル保持と、
  ディスク上の分離であり、製品の書き込み API そのものではない（`REPAIR-3`）。#948 の完了後に
  アダプタを製品 API へ置き換える
- **限界 2**: `prof_3_dangling_symlink_is_reported_as_missing` は unix のみ（Windows のファイル symlink
  作成には特権が要り、ジャンクションでは「リンク先の無いファイル symlink」を模擬できない）
- Windows 専用に、DACL の継承（`prof_2_prof_6_windows_roots_have_protected_dacl_and_data_inherits_it`）、
  ルートのハンドル保持（`prof_2_windows_open_profile_root_cannot_be_swapped`）を検証する。
  二重 open の拒否（`prof_1_prof_2_lock_is_per_profile_and_rejects_double_open`）は全 OS で検証する

## 対象テスト

`PROF-2` の分離テストは `crates/fandhe-browser-profile/tests/isolation.rs`（#180・TASK-51）にある。

- 入力パターン: 12 ケース（`P2-01`〜`P2-12`。受入基準の最低 10 ケースを上回る）。Cookie・Storage・
  Cache・History の 4 データ種別、複数サイト、サブドメイン差、片側のみ書き込み、上書き、
  大文字小文字だけ異なる名前、境界サイズ、Barrier 同期の並行書き込み（各 200 回）を含む
- 本体テスト: `prof_2_all_cases_have_zero_leaks`（全ケースで漏洩 0 件・最終状態の完全一致・共通パスの
  内容がアカウント間で異なることをアサート）
- 陰性対照: `prof_2_on_disk_contamination_is_detected`（実 FS 上で汚染を検出できること）
- メタテスト: `prof_2_case_table_defines_at_least_10_cases` ほか（ケース表・漏洩判定自体の妥当性）

関連テスト（`PROF-3` の 5 プロファイル並行アクセス。`tests/concurrent_isolation.rs`）も同じ
プロファイル分離の信頼性に関わるため、実施時に併せて実行して結果を記録するとよい。

## 事前準備（共通）

1. Rust stable（`rust-toolchain.toml`）と、本リポジトリの通常のビルド環境を用意する
2. リポジトリを取得する。`docs/spec` は不要（ビルド・テストは spec 抜きで成立する）
3. テスト用ディレクトリは `std::env::temp_dir()` 配下に作られる（`TMPDIR` で変更可能）。
   **対象ファイルシステム上で実行するため、`TMPDIR` を明示して `df` 等で確認する**。
   既定の一時領域が tmpfs の環境（Linux の `/tmp` が tmpfs の場合など）では ext4 を測定したことにならない
4. 実行コミットを控える: `git rev-parse --short HEAD`

## Linux ext4

```bash
# ext4 上の作業ディレクトリを用意し、ファイルシステムを確認する
mkdir -p "$HOME/profile-iso-tmp"
export TMPDIR="$HOME/profile-iso-tmp"
df -T "$TMPDIR"        # Type 列が ext4 であること
uname -srm

# 本体（PROF-2）
cargo test -p fandhe-browser-profile --test isolation -- --nocapture

# 併せて（PROF-3）
cargo test -p fandhe-browser-profile --test concurrent_isolation -- --nocapture
```

`df -T` が ext4 以外（tmpfs・overlay・btrfs など）を示した場合は、ext4 の領域に `TMPDIR` を
取り直す。

## macOS APFS

```bash
mkdir -p "$HOME/profile-iso-tmp"
export TMPDIR="$HOME/profile-iso-tmp"
df "$TMPDIR"                          # デバイスを控える
diskutil info "$TMPDIR" | grep -i "File System Personality"   # APFS であること
sw_vers

cargo test -p fandhe-browser-profile --test isolation -- --nocapture
cargo test -p fandhe-browser-profile --test concurrent_isolation -- --nocapture
```

macOS では `/var` → `/private/var` のような標準 symlink があるため、テスト側が一時領域を
`canonicalize` してから使う（`isolation.rs` の `TempDir`）。`TMPDIR` はそのままでよい。

## Windows NTFS

```powershell
# テストの基点は既知フォルダ（%LOCALAPPDATA%）配下の fandhe-test-tmp（TMPDIR・TEMP は効かない）
fsutil fsinfo volumeinfo C:     # File System Name が NTFS であること（LocalAppData のあるボリューム）
systeminfo | findstr /B /C:"OS Name" /C:"OS Version"

cargo test -p fandhe-browser-profile --test isolation -- --nocapture
cargo test -p fandhe-browser-profile --test concurrent_isolation -- --nocapture
```

DACL の検証は `powershell`（Get-Acl）と `icacls` を使う。いずれも標準で入っている。

## 判定の見方

- 各 `cargo test` が `test result: ok.` で終わり、`failed` / `ignored` が 0 であること
- テストを skip・ignore・弱体化して通さない（`coding-rust.md`）。失敗した場合は、失敗テスト名と
  出力を #295 に貼り、再現手順とあわせて報告する（原因の修正は別 Issue・別 PR）
- 漏洩件数は `prof_2_all_cases_have_zero_leaks` が全ケース 0 件を要求するため、成功＝漏洩 0 件。
  失敗時は assert メッセージに該当ケース ID（`P2-NN`）と漏洩内容が出る

## 結果記録テンプレート

Issue #295 へ貼る。OS ごとに 1 表を作る。

```markdown
## 実施環境

| 項目 | 値 |
| ---- | -- |
| 実施日 | YYYY-MM-DD |
| OS / バージョン | |
| ファイルシステム | ext4 / APFS / NTFS（`df -T` / `diskutil info` の出力要点） |
| TMPDIR | （パスは伏せてよい。FS 種別のみ） |
| Rust | `rustc --version` |
| コミット | `git rev-parse --short HEAD` |

## 結果

| テスト | 結果 | 漏洩件数 | 備考 |
| ------ | ---- | -------- | ---- |
| prof_2_all_cases_have_zero_leaks（P2-01〜P2-12） | ok / FAILED | 0 | |
| prof_2_on_disk_contamination_is_detected（陰性対照） | ok / FAILED | - | |
| tests/isolation.rs の残りのメタテスト | N 件 ok | - | |
| tests/concurrent_isolation.rs（PROF-3） | N 件 ok | 混線 0 | 任意 |
| prof_1_prof_2_lock_is_per_profile_and_rejects_double_open（全 OS） | ok / FAILED | - | |
| prof_2_prof_6_windows_roots_have_protected_dacl_and_data_inherits_it（Windows のみ） | ok / FAILED | - | |
| prof_2_windows_open_profile_root_cannot_be_swapped（Windows のみ） | ok / FAILED | - | |

## 判定

PROF-2 on <OS / FS>: 合格 / 不合格（理由）
```

### ケース別の記録（失敗時、または詳細を残す場合）

| ケース | 内容 | 結果 | 漏洩件数 |
| ------ | ---- | ---- | -------- |
| P2-01 | Cookie・同名 | | |
| P2-02 | Storage・同名 | | |
| P2-03 | Cache・同名 | | |
| P2-04 | History・同名 | | |
| P2-05 | 全 4 kind 同時 | | |
| P2-06 | 複数サイト | | |
| P2-07 | サブドメイン差 | | |
| P2-08 | 片側のみ書き込み | | |
| P2-09 | 上書き | | |
| P2-10 | 大文字小文字だけ異なる名前 | | |
| P2-11 | 境界サイズ | | |
| P2-12 | 並行書き込み | | |

`prof_2_all_cases_have_zero_leaks` は全ケースを 1 テスト内で順に実行する。ケース別の内訳が必要なのは
失敗時のみで、成功時は全ケース 0 件として一括で記録してよい。

## 部分報告の書式

一部の OS だけ実施した場合は、実施済みの結果を #295 に部分報告として貼る。

```markdown
## 部分報告（<実施済みの OS / FS>）

- 実施済み: <OS / FS>（合格 / 不合格）。詳細は各表のとおり
- 未実施: <OS / FS>（理由）
- 本 Issue の受け入れ条件（3 OS 分の測定レポート）は未達のため、close しない
```

## Windows 実施時の申し送り

- Windows の分離テストは Issue #954 で `cfg` の方針が決まった（共通の検証は 3 OS で実行し、unix 専用の
  検証は `cfg(unix)` に残し、Windows には DACL・ハンドル保持の対応する検証を足した）。限界は上記
  「Windows NTFS での実行と限界」のとおりで、結果を記録するときも書き込みがアダプタ経由であることを併記する
- #295 の実機実施は担当: 人間の別作業で、本書は手順書であって結果ではない
- #295 のコメントにある、Windows での Playwright トレース収集経路（`CDP-3` 系・
  `make trace-playwright` が Windows で失敗する件）の有効化確認も、実機実施の機会に行う
