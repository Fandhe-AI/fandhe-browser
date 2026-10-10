# プロファイル分離テストの 3 OS 実機実施手順

対応 `TASK-63` / `MS-5` / ビヘイビア `XOS-10`・`MEAS-7`・`PROF-2`（Issue #295）。
実機作業（担当: 人間）の手順書であり、**測定結果ではない**。実施結果は #295 に記録する。

## 目的と合格基準

`PROF-2`（異なるアカウントでの同一サイトアクセスで、プロファイル間のデータ漏洩が 0 件）を、
Linux ext4・macOS APFS・Windows NTFS の実機で確認する（`XOS-10`・`MEAS-7`）。

- 合格: 3 OS それぞれで対象テストが全件成功し、漏洩件数が 0 件
- 成果物: OS・ファイルシステム別の測定結果（本書「結果記録テンプレート」）

## 現状の実施可否

| OS / FS | 実施可否 | 理由 |
| ------- | -------- | ---- |
| Linux ext4 | 可 | 対象テストは `#![cfg(unix)]` で実行される |
| macOS APFS | 可 | 同上 |
| Windows NTFS | **現状は実行不能** | 後述 |

### Windows NTFS が実行不能である根拠

- `crates/fandhe-browser-profile/src/profile.rs` の `Profile::open_impl` は、`#[cfg(not(unix))]` で
  ディレクトリを作成せず `ProfileError::Unsupported`（理由: ACL-based directory isolation is not
  implemented on this platform yet, tracked by XOS-7..XOS-10）を返す。ACL 隔離（`XOS-7`）が未実装のため、
  継承された無制限の権限でプロファイルを作らない（fail-closed）設計である
- `crates/fandhe-browser-profile/src/store.rs` のモジュール doc（「Windows の ACL 隔離が未実装のため、現状
  Windows の `Profile::open` は `Unsupported` を返し」）と `open_or_create` の注記（「現状は `Profile::open` が
  ACL 未実装（`XOS-7`）で先に `Unsupported` を返す」）も同じ事実を述べている
- `crates/fandhe-browser-profile/tests/isolation.rs` の冒頭 doc も、3 OS での分離テスト実行（`XOS-10`）は
  Windows ACL 実装後の課題と明記している。Windows 側は `src/profile.rs` の
  `prof_1_open_is_unsupported_on_windows` が `Unsupported` を返すことを確認するのみ
- Windows ACL 実装方式の判断は #813 が追跡している（本書作成時点で OPEN）。Windows の実施は #813 と
  後続の ACL 実装の完了待ちで、**実施時期・順序は未決定（オーナー判断待ち）**

したがって当面は Linux・macOS を先に実施して部分報告とし、Windows は ACL 実装後に追記する。
`Unsupported` を返すだけの Windows 実行を「分離テスト合格」として記録しない（`REPAIR-3`）。

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

## 事前準備（Linux・macOS 共通）

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

Windows が未実施の間は、Linux・macOS の結果を #295 に部分報告として貼る。

```markdown
## 部分報告（Linux ext4 / macOS APFS）

- 実施済み: Linux ext4（合格 / 不合格）、macOS APFS（合格 / 不合格）。詳細は各表のとおり
- 未実施: Windows NTFS
  - 理由: `Profile::open` が `Unsupported` を返す（`XOS-7` の ACL 隔離が未実装。#813 で方式を判断中）
  - 再開条件: Windows の ACL 実装完了後に本手順（`tests/isolation.rs` の `#![cfg(unix)]` の扱いを含む）を
    Windows 向けに見直して実施する
- 本 Issue の受け入れ条件（3 OS 分の測定レポート）は未達のため、close しない
```

## Windows 実施時の申し送り

- `tests/isolation.rs` と `tests/concurrent_isolation.rs` は `#![cfg(unix)]` のため、Windows で実行するには
  ACL 実装後にテスト側の cfg 方針と Windows 向けの隔離検証（ACL の確認）を別途設計する必要がある。
  この設計は本書では行わない（**未決定（オーナー判断待ち）**）
- #295 のコメントにある、Windows での Playwright トレース収集経路（`CDP-3` 系・
  `make trace-playwright` が Windows で失敗する件）の有効化確認も、ACL 実装後に同じ機会で行う
