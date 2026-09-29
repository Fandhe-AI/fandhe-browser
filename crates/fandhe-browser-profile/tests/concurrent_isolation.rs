//! 並行アクセステスト（`PROF-3`・TASK-52（52.1・52.2）・#184・#185、MS-3）。
//!
//! `PROF-3` は「5 つの `Profile` インスタンスを `Barrier` で同時に開始させ、
//! それぞれ 100 回ずつ並行に書き込んだとき、データ混線 0 件・競合による
//! panic/クラッシュ 0 件であること」を定める（PoC-7
//! `test_five_profiles_concurrent_no_crash_no_mixup` が土台）。
//!
//! - 52.1（#184）: ハーネス機構（複数プロファイルの生成・`Barrier` による
//!   同時開始・書き込みループ・結果の構造化収集・panic の捕捉）。
//! - 52.2（#185）: ハーネス実行後にディスクを読み戻して混線 0 件を確認する
//!   検出器（`find_mixups`）と、panic・open 失敗・書き込み失敗を数える
//!   集計器（`summarize_failures`）、およびそれらが実際に異常を検出できる
//!   ことを示す陰性対照テスト。
//!
//! 「混線」は次のいずれかとする。ディスクを真とし、期待値はレポートでなく
//! 定数から独立に導出する。
//! - 内容が期待ペイロードと一致しない（`ContentMismatch`）
//! - 内容に他プロファイルのマーカー `profile-{k}:` が含まれる（`ForeignMarker`）
//! - 想定外のエントリがある／想定エントリが欠けている
//! - 通常ファイルでない（symlink 等）・読み込みに失敗した
//!   （0 件へ丸めず fail-closed にする）
//!
//! プロセス全体の abort・segfault はテストバイナリごと落ちて CI が fail する
//! ため、ここでは thread 単位の panic・open 失敗・書き込み失敗を数える。
//! 別プロセスからの同時アクセスは対象外（`PROF-6` の将来拡張）。
//!
//! 本体（`src/`）は変更しない。新規外部依存も追加しない
//! （dependency-policy.md）。
//!
//! Windows では ACL 隔離（`XOS-7`〜`XOS-10`）が未実装のため `Profile::open`
//! は常に `ProfileError::Unsupported` を返す（`tests/profile_open.rs` と同じ
//! 契約。この契約は `src/profile.rs` の `#[cfg(windows)]` テストが確認して
//! いる）。本ファイルはディレクトリが実際に作られ書き込まれることを前提と
//! するため Unix 専用とする。

#![cfg(unix)]

use fandhe_browser_profile::{DataKind, Profile};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Barrier;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

/// `PROF-3` が定める最低並行数（「5 並行以上で複数 Profile へ同時に書き込める
/// こと」）。
const MIN_CONCURRENCY: usize = 5;
/// `PROF-3` が定める検証対象のプロファイル数。
const PROFILE_COUNT: usize = 5;
/// `PROF-3` が定める、1 プロファイルあたりの書き込み回数。
const WRITES_PER_PROFILE: usize = 100;
/// 全プロファイル・全 `DataKind` で共通のファイル名。どのワーカーも同じ
/// 相対パス（`<kind のディレクトリ>/concurrent.dat`）へ書き込むため、52.2 で
/// 他プロファイルのペイロードが混入していないかを検出できる。
const TARGET_FILE_NAME: &str = "concurrent.dat";

/// テスト用の一時ディレクトリ。drop 時に再帰削除する（`tests/profile_open.rs`
/// と同じパターン。tempfile 等の外部依存は追加しない方針。
/// dependency-policy.md）。
struct TempDir {
    path: PathBuf,
}

static TEMP_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

impl TempDir {
    fn new() -> Self {
        let n = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        // macOS の `/var` -> `/private/var` のような OS 標準 symlink を
        // 経由すると `Profile::open` の厳格な symlink 検証（PR #437
        // レビュー指摘）に弾かれるため、基点を事前に正規化する
        // （`tests/profile_open.rs` と同じ理由）。
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        let path = base.join(format!(
            "fandhe-profile-concurrent-test-{}-{n}",
            std::process::id()
        ));
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// ハーネス実行時の並行度設定。
struct HarnessConfig {
    profiles: usize,
    writes_per_profile: usize,
}

/// 1 ワーカー（1 プロファイル）の実行結果（`PROF-3`・TASK-52（52.1））。
///
/// 52.2（#185）の検出器・集計器が読み戻す情報の発生源。
struct WorkerReport {
    index: usize,
    /// このワーカーが開いたプロファイルのルート（表示・52.2 の読み戻し用）。
    root: PathBuf,
    writes_attempted: usize,
    writes_succeeded: usize,
    /// 書き込み失敗（`ProfileError`・`io::Error` の `Display`）。成功に
    /// 丸めず、件数を正確に数えるため記録する（security.md「偽装の禁止」）。
    write_errors: Vec<String>,
    /// 各 `DataKind` について最後に書き込みに成功したペイロード。
    /// 52.2 での期待値の発生源になる。
    last_payloads: Vec<(DataKind, String)>,
    /// 各書き込みの直後に行った混線検査（`check_after_write`）の指摘。
    /// 最終状態だけを見る `find_mixups` では、途中の混線が後続の正しい
    /// 書き込みで上書きされて消えるため、書き込みごとに記録して残す
    /// （PROF-3・#185 レビュー指摘）。
    intermediate_mixups: Vec<String>,
}

/// ワーカーの終わり方。panic・open 失敗を `Completed` に丸めない
/// （security.md「偽装の禁止」: 検出回避として作用する成功の一律返却を
/// 避ける方針と同様、ハーネス自身も失敗を成功として報告しない）。
enum WorkerOutcome {
    Completed(WorkerReport),
    OpenFailed { index: usize, error: String },
    Panicked { index: usize, message: String },
}

/// `config` の並行数を検証する。`PROF-3` は「5 並行以上」を要求するため、
/// これを下回る設定でハーネスを呼ぶことはテストコードの誤りとみなし、
/// テストヘルパーとして `panic!` で打ち切る。
fn validate_config(config: &HarnessConfig) {
    assert!(
        config.profiles >= MIN_CONCURRENCY,
        "PROF-3 は並行プロファイル数を {MIN_CONCURRENCY} 以上要求する（got: {}）",
        config.profiles
    );
    assert!(
        config.writes_per_profile > 0,
        "writes_per_profile は 1 以上である必要がある"
    );
}

/// `base` 配下に `config.profiles` 個のプロファイルを作り、`Barrier` で
/// 同時に開始させて、それぞれ `config.writes_per_profile` 回ずつ並行に
/// 書き込む（`PROF-3`・TASK-52（52.1））。
///
/// 処理の流れ（不変条件を含む）:
/// 1. `std::thread::scope` の中で `config.profiles` 本のワーカーを spawn
///    する。`Barrier::new(config.profiles)` はスコープ内で参照を共有する
///    （`Arc` は不要）。
/// 2. 各ワーカー `i` は `base.join("profile-{i}")` に `Profile::open` する。
///    **open の成否・panic の有無に関わらず `barrier.wait()` をちょうど 1 回
///    呼ぶ**。open が失敗（`Err`）したワーカーが wait せずに return すると、
///    残りのワーカーが永久にブロックする（libtest にはテスト単位の
///    タイムアウトがないため、ここが deadlock の最大の危険箇所になる）。
///    `Profile::open` は `Result` を返し panic しない契約
///    （coding-rust.md「ライブラリコードでは `Result` を返し、panic
///    させない」）だが、ハーネスはその契約破りを検出対象そのものとして
///    扱う（PROF-3 は「競合による panic/クラッシュ 0 件」を要求するため、
///    `open` 側の契約違反もハーネスが `Panicked` として報告できなければ
///    ならない）。そのため `Profile::open` の呼び出しは
///    `std::panic::catch_unwind` で包み、panic した場合も
///    `barrier.wait()` を必ず経由してから `Panicked` を返す
///    （#184 レビュー指摘。この `catch_unwind` はハーネス〔テストコード〕
///    による panic 捕捉であり、release ビルドで `panic = "abort"` になる
///    ライブラリコード〔`src/`〕には適用しない。coding-rust.md の
///    「release は `panic = "abort"` 前提のため `catch_unwind` に頼らない」
///    はライブラリコードの話で、本ファイルは test プロファイル〔unwind〕
///    でのみビルドされるため矛盾しない）。open が失敗（`Err`）したワーカーは
///    wait の後で `OpenFailed` を返す。
/// 3. 書き込みは `DataKind::ALL` を順に巡回しながら `writes_per_profile` 回
///    行う。1 回の書き込みは
///    `create_file_in` → `set_len(0)`（過去の内容の残留を防ぐ。
///    `create_file_in` は `RDWR|CREATE` で開くため切り詰めない） →
///    `seek(Start(0))` → `write_all` の順に行う。52.2 が読み戻すのは
///    同一プロセス内（同じページキャッシュ）のため `sync_data` によるディスク
///    同期は不要と判断し、行わない（3 OS CI・特に macOS での所要時間を
///    抑えるため。#184 レビュー指摘）。失敗しても打ち切らず
///    `write_errors` に記録し、件数を正確に数える。
/// 4. 全ハンドルを `join()` する。パニックは伝播させず `Panicked` として記録
///    する。テストは dev/test プロファイル（unwind）でビルドされるため、
///    `[profile.release]` の `panic = "abort"` はここには効かない。スレッドの
///    join 結果で panic を観測しているだけで `catch_unwind` は使わない
///    （coding-rust.md と矛盾しない）。
/// 5. 戻り値は `index` の昇順に並べて返す。
fn run_concurrent_writes(base: &Path, config: &HarnessConfig) -> Vec<WorkerOutcome> {
    validate_config(config);

    let barrier = Barrier::new(config.profiles);
    let profile_count = config.profiles;

    thread::scope(|scope| {
        let handles: Vec<_> = (0..config.profiles)
            .map(|index| {
                let barrier = &barrier;
                let writes_per_profile = config.writes_per_profile;
                let root = base.join(format!("profile-{index}"));
                scope.spawn(move || -> WorkerOutcome {
                    // `Profile::open` の panic（契約違反）を捕捉する。
                    // 捕捉せずに panic させると、このワーカーは
                    // `barrier.wait()` へ到達できず、残りのワーカーが
                    // 永久にブロックする（#184 レビュー指摘）。
                    let open_result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            Profile::open(&root)
                        }));

                    let profile = match open_result {
                        Ok(Ok(profile)) => profile,
                        Ok(Err(err)) => {
                            // open に失敗しても、他のワーカーを deadlock
                            // させないため必ず wait する。
                            barrier.wait();
                            return WorkerOutcome::OpenFailed {
                                index,
                                error: err.to_string(),
                            };
                        }
                        Err(payload) => {
                            // open 自体が panic した場合も、他のワーカーを
                            // deadlock させないため必ず wait してから
                            // `Panicked` として報告する。
                            barrier.wait();
                            return WorkerOutcome::Panicked {
                                index,
                                message: panic_message(&payload),
                            };
                        }
                    };

                    barrier.wait();

                    let mut writes_succeeded = 0usize;
                    let mut write_errors = Vec::new();
                    let mut last_payloads: Vec<(DataKind, String)> = Vec::new();
                    let mut intermediate_mixups: Vec<String> = Vec::new();

                    for j in 0..writes_per_profile {
                        let Some(&kind) = DataKind::ALL.get(j % DataKind::ALL.len()) else {
                            // `DataKind::ALL` は空になり得ないが、外部入力に
                            // 準じた扱いとして `.get()` で明示的に処理する
                            // （coding-rust.md「エラーハンドリング」）。
                            write_errors.push("DataKind::ALL is unexpectedly empty".to_string());
                            continue;
                        };
                        let payload = format!("profile-{index}:write-{j:04}\n");
                        match write_once(&profile, kind, payload.as_bytes()) {
                            Ok(()) => {
                                writes_succeeded += 1;
                                // 書き込み直後に所有先・内容を検査し、後続の
                                // 書き込みで上書きされても途中の混線を残す。
                                intermediate_mixups.extend(check_after_write(
                                    base,
                                    index,
                                    kind,
                                    &payload,
                                    profile_count,
                                ));
                                if let Some(slot) =
                                    last_payloads.iter_mut().find(|(k, _)| *k == kind)
                                {
                                    slot.1 = payload;
                                } else {
                                    last_payloads.push((kind, payload));
                                }
                            }
                            Err(message) => write_errors.push(message),
                        }
                    }

                    // `profile` を明示的に drop してロックを解放してから
                    // 結果を返す（他ワーカーの join・後続の再 open に影響
                    // させないため）。
                    drop(profile);

                    WorkerOutcome::Completed(WorkerReport {
                        index,
                        root,
                        writes_attempted: writes_per_profile,
                        writes_succeeded,
                        write_errors,
                        last_payloads,
                        intermediate_mixups,
                    })
                })
            })
            .collect();

        let mut outcomes: Vec<WorkerOutcome> = handles
            .into_iter()
            .enumerate()
            .map(|(index, handle)| match handle.join() {
                Ok(outcome) => outcome,
                Err(payload) => WorkerOutcome::Panicked {
                    index,
                    message: panic_message(&payload),
                },
            })
            .collect();

        outcomes.sort_by_key(|outcome| match outcome {
            WorkerOutcome::Completed(report) => report.index,
            WorkerOutcome::OpenFailed { index, .. } => *index,
            WorkerOutcome::Panicked { index, .. } => *index,
        });
        outcomes
    })
}

/// 1 回分の書き込みを行う。`profile.create_file_in` が `RDWR|CREATE` で開き
/// 切り詰めないため、短い内容で上書きしたときに古いバイトが残らないよう
/// `set_len(0)` で明示的に切り詰めてから書く（52.2 の読み戻しで最終内容を
/// 一意にするために必須）。
fn write_once(profile: &Profile, kind: DataKind, payload: &[u8]) -> Result<(), String> {
    let mut file = profile
        .create_file_in(kind, OsStr::new(TARGET_FILE_NAME))
        .map_err(|err| err.to_string())?;
    file.set_len(0).map_err(|err| err.to_string())?;
    file.seek(SeekFrom::Start(0))
        .map_err(|err| err.to_string())?;
    file.write_all(payload).map_err(|err| err.to_string())?;
    Ok(())
}

/// 書き込み直後の混線検査（`PROF-3`・#185）。自プロファイルの
/// `<kind>/concurrent.dat` は自ワーカーだけが書くため `payload` と完全一致する
/// はずで、他プロファイルのファイルは空（truncate 直後）か読めない（未作成）
/// 場合を除き、自身以外のマーカー `profile-{m}:` を含まないこと。
/// 未作成（`NotFound`）を許容するのは他プロファイルのファイルだけで、自
/// プロファイルの読み取り失敗やその他の I/O エラーは指摘として記録する。
fn check_after_write(
    base: &Path,
    index: usize,
    kind: DataKind,
    payload: &str,
    profile_count: usize,
) -> Vec<String> {
    let mut found = Vec::new();
    for owner in 0..profile_count {
        let path = base
            .join(format!("profile-{owner}"))
            .join(kind.dir_name())
            .join(TARGET_FILE_NAME);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            // 未作成の他プロファイルのファイルだけは正常として許容する。
            Err(err) if owner != index && err.kind() == std::io::ErrorKind::NotFound => {
                continue;
            }
            // 自プロファイルは書き込み直後で必ず存在するため、NotFound を含む
            // あらゆる読み取り失敗を違反として記録する（後続の書き込みで
            // 異常が消えても最終読み戻しで見逃さないため）。
            Err(err) => {
                found.push(format!(
                    "profile-{owner} {kind:?}: failed to read {}: {err}",
                    path.display()
                ));
                continue;
            }
        };
        if owner == index {
            if bytes != payload.as_bytes() {
                found.push(format!(
                    "profile-{index} {kind:?}: own file {:?} != just-written {payload:?}",
                    String::from_utf8_lossy(&bytes)
                ));
            }
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let mut marker_found = false;
        for other in (0..profile_count).filter(|&m| m != owner) {
            if text.contains(&format!("profile-{other}:")) {
                marker_found = true;
                found.push(format!(
                    "profile-{owner} {kind:?}: contains marker of profile-{other} ({text:?})"
                ));
            }
        }
        // 他プロファイルのファイルは空（truncate 直後）か、所有者が書き得る
        // ペイロード形式に完全一致する内容のみ許容する。途中まで書かれた・
        // 壊れた内容（他プロファイルのマーカーを含まないもの）も指摘にする。
        if !marker_found && !bytes.is_empty() && !is_owner_payload(owner, &text) {
            found.push(format!(
                "profile-{owner} {kind:?}: content is not a valid payload of profile-{owner} ({text:?})"
            ));
        }
    }
    found
}

/// `text` が `owner` のワーカーが書き得るペイロード
/// `profile-{owner}:write-{j:04}\n`（`j` は 4 桁以上の 10 進数）と完全一致するか。
fn is_owner_payload(owner: usize, text: &str) -> bool {
    let Some(rest) = text.strip_prefix(&format!("profile-{owner}:write-")) else {
        return false;
    };
    let Some(digits) = rest.strip_suffix('\n') else {
        return false;
    };
    digits.len() >= 4 && digits.bytes().all(|b| b.is_ascii_digit())
}

/// `thread::Result` の `Err` ペイロード（`Box<dyn Any + Send>`）から panic
/// メッセージを取り出す。`&str`・`String` のいずれでもない場合は固定文言を
/// 返す（外部入力ではないが、想定外の型を弾く防御的な処理）。
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "worker thread panicked with a non-string payload".to_string()
    }
}

// ---------------------------------------------------------------------
// 52.1 のテスト: ハーネスの機構確認（具体値で assert する）。
//
// 注意: ここでの `last_payloads` の assert はハーネスが記録した値の正しさの
// 確認であり、ディスク上の分離の検証ではない。ディスクの読み戻しによる
// 混線確認は下の 52.2 のテスト（`prof_3_five_concurrent_profiles_*`）が行う。
// ---------------------------------------------------------------------

/// `PROF-3`・TASK-52（52.1）・#184: `PROFILE_COUNT` 個のプロファイルを
/// `WRITES_PER_PROFILE` 回ずつ並行に書き込ませると、5 件すべてが
/// `Completed` で返り、書き込み試行・成功回数、記録されたペイロードが
/// 期待どおりであること。
#[test]
fn prof_3_harness_runs_five_profiles_concurrently() {
    let tmp = TempDir::new();
    let config = HarnessConfig {
        profiles: PROFILE_COUNT,
        writes_per_profile: WRITES_PER_PROFILE,
    };

    let outcomes = run_concurrent_writes(tmp.path(), &config);

    assert_eq!(outcomes.len(), PROFILE_COUNT, "outcome の件数が想定と違う");

    let mut seen_roots = std::collections::BTreeSet::new();
    for (expected_index, outcome) in outcomes.into_iter().enumerate() {
        let report = match outcome {
            WorkerOutcome::Completed(report) => report,
            WorkerOutcome::OpenFailed { index, error } => {
                panic!("worker {index} open failed: {error}")
            }
            WorkerOutcome::Panicked { index, message } => {
                panic!("worker {index} panicked: {message}")
            }
        };

        assert_eq!(report.index, expected_index, "index が昇順に並んでいない");
        assert_eq!(
            report.root,
            tmp.path().join(format!("profile-{expected_index}")),
            "root が想定のパスと一致しない"
        );
        assert!(
            seen_roots.insert(report.root.clone()),
            "root が他のワーカーと重複している: {:?}",
            report.root
        );
        assert_eq!(
            report.writes_attempted, WRITES_PER_PROFILE,
            "worker {expected_index}: writes_attempted が想定と違う"
        );
        assert_eq!(
            report.writes_succeeded, WRITES_PER_PROFILE,
            "worker {expected_index}: writes_succeeded が想定と違う"
        );
        assert!(
            report.write_errors.is_empty(),
            "worker {expected_index}: write_errors が空でない: {:?}",
            report.write_errors
        );

        // `WRITES_PER_PROFILE`（100）回を `DataKind::ALL`（4 種）で巡回した
        // 場合、各 kind の最後の j は 96..99 のいずれかになる。
        assert_eq!(
            report.last_payloads.len(),
            DataKind::ALL.len(),
            "worker {expected_index}: last_payloads が DataKind::ALL の全種を含まない"
        );
        for (kind_index, &kind) in DataKind::ALL.iter().enumerate() {
            let last_j = last_write_index_for_kind(kind_index, DataKind::ALL.len());
            let expected_payload = format!("profile-{expected_index}:write-{last_j:04}\n");
            let actual_payload = report
                .last_payloads
                .iter()
                .find(|(k, _)| *k == kind)
                .map(|(_, payload)| payload.as_str());
            assert_eq!(
                actual_payload,
                Some(expected_payload.as_str()),
                "worker {expected_index}: {kind:?} の last_payloads が期待値と一致しない"
            );
        }
    }
}

/// `WRITES_PER_PROFILE` 回を `DataKind::ALL` で巡回したとき、`kind_index`
/// 番目の種別が最後に選ばれる `j` を計算する（`j % kind_count == kind_index`
/// を満たす最大の `j < WRITES_PER_PROFILE`）。テストの期待値をハードコード
/// せず、定数から導出することで `WRITES_PER_PROFILE`/`DataKind::ALL` の
/// 変更に追随させる。
fn last_write_index_for_kind(kind_index: usize, kind_count: usize) -> usize {
    let last_full_cycle_start = (WRITES_PER_PROFILE / kind_count) * kind_count;
    if last_full_cycle_start + kind_index < WRITES_PER_PROFILE {
        last_full_cycle_start + kind_index
    } else {
        last_full_cycle_start + kind_index - kind_count
    }
}

/// `PROF-3`・TASK-52（52.1）・#184: 最低並行数（5）を超える設定でも
/// ハーネスが動作すること（「5 以上」を満たすことの確認）。実行時間を
/// 抑えるため書き込み回数は小さい値にする。
#[test]
fn prof_3_harness_supports_more_than_minimum_concurrency() {
    let tmp = TempDir::new();
    let profiles = 8;
    let writes_per_profile = 20;
    let config = HarnessConfig {
        profiles,
        writes_per_profile,
    };

    let outcomes = run_concurrent_writes(tmp.path(), &config);

    assert_eq!(outcomes.len(), profiles, "outcome の件数が想定と違う");
    for (expected_index, outcome) in outcomes.into_iter().enumerate() {
        match outcome {
            WorkerOutcome::Completed(report) => {
                assert_eq!(report.index, expected_index);
                assert_eq!(report.writes_attempted, writes_per_profile);
                assert_eq!(report.writes_succeeded, writes_per_profile);
            }
            WorkerOutcome::OpenFailed { index, error } => {
                panic!("worker {index} open failed: {error}")
            }
            WorkerOutcome::Panicked { index, message } => {
                panic!("worker {index} panicked: {message}")
            }
        }
    }
}

/// `PROF-3`・TASK-52（52.1）・#184: `MIN_CONCURRENCY`（5）を下回る並行数は
/// ハーネスの前提条件違反として拒否する。テストヘルパーの誤用（テスト
/// コード自身の不変条件違反であり外部入力ではない）とみなし、`Result` で
/// 呼び出し元に判断を委ねるより安全側（より厳密な方）である `panic!` で
/// 即座に打ち切る設計を選んだ。
#[test]
#[should_panic(expected = "PROF-3 は並行プロファイル数を 5 以上要求する")]
fn prof_3_harness_rejects_concurrency_below_minimum() {
    let tmp = TempDir::new();
    let config = HarnessConfig {
        profiles: 4,
        writes_per_profile: 1,
    };

    // 並行数の検証は書き込みより前に行われるため、実際にワーカーを走らせる
    // 前に panic する。
    let _ = run_concurrent_writes(tmp.path(), &config);
}

/// `PROF-3`・TASK-52（52.1）・#184: `Profile::open` に失敗したワーカーが
/// `barrier.wait()` を経て `OpenFailed` を返し、他のワーカーを deadlock
/// させずに `Completed` として完走させられること。§T2 のドキュメント
/// コメントが「deadlock の最大の危険箇所」と明記する経路（open 失敗後の
/// `barrier.wait()`）を実際に踏む。`profile-0` の root パスをあらかじめ
/// 通常ファイルとして作っておくことで、`Profile::open` を
/// `Err(ProfileError::InvalidLayout)` にする
/// （`src/profile.rs` の `prof_1_open_fails_when_root_is_a_file` と同じ手法）。
#[test]
fn prof_3_harness_open_failure_does_not_deadlock_other_workers() {
    let tmp = TempDir::new();
    std::fs::create_dir_all(tmp.path()).expect("ベースディレクトリの作成");
    let failing_root = tmp.path().join("profile-0");
    std::fs::write(&failing_root, b"not a directory").expect("root 用ファイルの作成");

    let config = HarnessConfig {
        profiles: PROFILE_COUNT,
        writes_per_profile: WRITES_PER_PROFILE,
    };

    // このテストが（タイムアウトせず）完走すること自体が、open 失敗した
    // ワーカーが他のワーカーを deadlock させないことの証明になる。
    let outcomes = run_concurrent_writes(tmp.path(), &config);

    assert_eq!(outcomes.len(), PROFILE_COUNT, "outcome の件数が想定と違う");

    for (expected_index, outcome) in outcomes.into_iter().enumerate() {
        if expected_index == 0 {
            match outcome {
                WorkerOutcome::OpenFailed { index, error } => {
                    assert_eq!(index, 0, "open 失敗ワーカーの index が想定と違う");
                    assert!(!error.is_empty(), "エラーメッセージが空");
                }
                WorkerOutcome::Completed(report) => {
                    panic!(
                        "worker 0 は open 失敗するはずが Completed で返った（index={}）",
                        report.index
                    )
                }
                WorkerOutcome::Panicked { index, message } => {
                    panic!("worker {index} が想定外に panic した: {message}")
                }
            }
        } else {
            match outcome {
                WorkerOutcome::Completed(report) => {
                    assert_eq!(report.index, expected_index);
                    assert_eq!(report.writes_attempted, WRITES_PER_PROFILE);
                    assert_eq!(
                        report.writes_succeeded, WRITES_PER_PROFILE,
                        "worker {expected_index}: writes_succeeded が想定と違う"
                    );
                    assert!(
                        report.write_errors.is_empty(),
                        "worker {expected_index}: write_errors が空でない: {:?}",
                        report.write_errors
                    );
                }
                WorkerOutcome::OpenFailed { index, error } => {
                    panic!("worker {index} open failed: {error}")
                }
                WorkerOutcome::Panicked { index, message } => {
                    panic!("worker {index} panicked: {message}")
                }
            }
        }
    }
}
// ---------------------------------------------------------------------
// 52.2: ディスク読み戻しによる混線検出・クラッシュ集計（`PROF-3`・#185）。
// ---------------------------------------------------------------------

/// 混線の種別。全フィールドは `Display` で読む。
#[derive(Debug)]
enum MixupDetail {
    ContentMismatch { expected: String, actual: Vec<u8> },
    ForeignMarker { foreign_index: usize },
    UnexpectedEntry { dir: PathBuf, name: String },
    MissingEntry { dir: PathBuf, name: String },
    NotRegularFile { path: PathBuf },
    ReadFailed { path: PathBuf, error: String },
}

/// 検出された混線 1 件。`index` は所有者とされるプロファイル、`kind` は
/// 対象のデータ種別（配置検査など種別に依らないものは `None`）。
#[derive(Debug)]
struct Mixup {
    index: usize,
    kind: Option<DataKind>,
    detail: MixupDetail,
}

impl std::fmt::Display for Mixup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "profile-{} kind={:?}: ", self.index, self.kind)?;
        match &self.detail {
            MixupDetail::ContentMismatch { expected, actual } => write!(
                f,
                "content mismatch (expected {expected:?}, actual {:?})",
                String::from_utf8_lossy(actual)
            ),
            MixupDetail::ForeignMarker { foreign_index } => {
                write!(f, "contains marker of profile-{foreign_index}")
            }
            MixupDetail::UnexpectedEntry { dir, name } => {
                write!(f, "unexpected entry {name:?} in {}", dir.display())
            }
            MixupDetail::MissingEntry { dir, name } => {
                write!(f, "missing entry {name:?} in {}", dir.display())
            }
            MixupDetail::NotRegularFile { path } => {
                write!(f, "not a regular file: {}", path.display())
            }
            MixupDetail::ReadFailed { path, error } => {
                write!(f, "read failed: {} ({error})", path.display())
            }
        }
    }
}

/// `dir` 直下のエントリ名集合が `expected` とちょうど一致するか検査し、
/// 差分を `Mixup` として `out` へ積む。読めない場合も 0 件に丸めない。
fn check_entries(
    dir: &Path,
    expected: &BTreeSet<String>,
    index: usize,
    kind: Option<DataKind>,
    out: &mut Vec<Mixup>,
) {
    let read_failed = |error: String| Mixup {
        index,
        kind,
        detail: MixupDetail::ReadFailed {
            path: dir.to_path_buf(),
            error,
        },
    };
    let mut actual = BTreeSet::new();
    match std::fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(e) => {
                        actual.insert(e.file_name().to_string_lossy().into_owned());
                    }
                    Err(err) => out.push(read_failed(err.to_string())),
                }
            }
        }
        Err(err) => {
            out.push(read_failed(err.to_string()));
            return;
        }
    }
    for name in actual.difference(expected) {
        out.push(Mixup {
            index,
            kind,
            detail: MixupDetail::UnexpectedEntry {
                dir: dir.to_path_buf(),
                name: name.clone(),
            },
        });
    }
    for name in expected.difference(&actual) {
        out.push(Mixup {
            index,
            kind,
            detail: MixupDetail::MissingEntry {
                dir: dir.to_path_buf(),
                name: name.clone(),
            },
        });
    }
}

/// 1 個の `<kind>/concurrent.dat` を読み戻し、期待ペイロード・他プロファイルの
/// マーカーの有無を検査する。
fn check_payload_file(
    path: PathBuf,
    index: usize,
    kind: DataKind,
    kind_index: usize,
    profile_count: usize,
    out: &mut Vec<Mixup>,
) {
    let mut push = |detail: MixupDetail| {
        out.push(Mixup {
            index,
            kind: Some(kind),
            detail,
        })
    };
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_file() => {}
        // 壊れた symlink（リンク先が存在しない）は `read_dir` には名前が現れる
        // ため `check_entries` は欠損を報告できない。実体が無いので欠損として
        // 報告し、実体のある非通常ファイル（`NotRegularFile`）と区別する。
        Ok(meta)
            if meta.file_type().is_symlink()
                && matches!(
                    std::fs::metadata(&path),
                    Err(ref e) if e.kind() == std::io::ErrorKind::NotFound
                ) =>
        {
            let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
            return push(MixupDetail::MissingEntry {
                dir,
                name: TARGET_FILE_NAME.to_string(),
            });
        }
        Ok(_) => return push(MixupDetail::NotRegularFile { path }),
        // 欠落は check_entries が MissingEntry として報告済み。
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
        Err(err) => {
            return push(MixupDetail::ReadFailed {
                path,
                error: err.to_string(),
            });
        }
    }
    let actual = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) => {
            return push(MixupDetail::ReadFailed {
                path,
                error: err.to_string(),
            });
        }
    };

    let last_j = last_write_index_for_kind(kind_index, DataKind::ALL.len());
    let expected = format!("profile-{index}:write-{last_j:04}\n");
    let text = String::from_utf8_lossy(&actual).into_owned();
    if actual != expected.as_bytes() {
        push(MixupDetail::ContentMismatch { expected, actual });
    }
    // コロン付きで比較する（`profile-1` が `profile-10` に誤一致しない）。
    for foreign_index in (0..profile_count).filter(|&k| k != index) {
        if text.contains(&format!("profile-{foreign_index}:")) {
            push(MixupDetail::ForeignMarker { foreign_index });
        }
    }
}

/// `base` 配下の `profile_count` 個のプロファイルをディスクから読み戻し、
/// 混線を全件返す（`PROF-3`）。各 `<kind>/concurrent.dat` は最後の書き込み
/// ペイロード（定数から導出）と完全一致し、他プロファイルのマーカーを含まない
/// こと。配置（ディレクトリ構成）もちょうど期待どおりであること。
fn find_mixups(base: &Path, profile_count: usize) -> Vec<Mixup> {
    let mut out = Vec::new();

    let base_expected: BTreeSet<String> =
        (0..profile_count).map(|i| format!("profile-{i}")).collect();
    check_entries(base, &base_expected, 0, None, &mut out);

    let mut root_expected: BTreeSet<String> = DataKind::ALL
        .iter()
        .map(|k| k.dir_name().to_string())
        .collect();
    root_expected.insert("profile.lock".to_string());
    let file_expected: BTreeSet<String> = BTreeSet::from([TARGET_FILE_NAME.to_string()]);

    for index in 0..profile_count {
        let root = base.join(format!("profile-{index}"));
        check_entries(&root, &root_expected, index, None, &mut out);
        for (kind_index, &kind) in DataKind::ALL.iter().enumerate() {
            let dir = root.join(kind.dir_name());
            check_entries(&dir, &file_expected, index, Some(kind), &mut out);
            check_payload_file(
                dir.join(TARGET_FILE_NAME),
                index,
                kind,
                kind_index,
                profile_count,
                &mut out,
            );
        }
    }
    out
}

/// ワーカー結果の失敗集計。panic・open 失敗を `Completed` に丸めない。
#[derive(Debug, Default)]
struct FailureSummary {
    panicked: Vec<(usize, String)>,
    open_failed: Vec<(usize, String)>,
    write_failures: Vec<(usize, Vec<String>)>,
    short_writes: Vec<(usize, usize)>,
    /// 書き込み直後検査で見つかった途中の混線（`(worker index, 指摘)`）。
    intermediate_mixups: Vec<(usize, Vec<String>)>,
}

/// `outcomes` から panic・open 失敗・書き込み失敗・書き込み不足を数える。
/// プロセス全体の abort・segfault はここへ到達せずバイナリごと fail する。
fn summarize_failures(outcomes: &[WorkerOutcome], expected_writes: usize) -> FailureSummary {
    let mut summary = FailureSummary::default();
    for outcome in outcomes {
        match outcome {
            WorkerOutcome::Completed(report) => {
                if !report.write_errors.is_empty() {
                    summary
                        .write_failures
                        .push((report.index, report.write_errors.clone()));
                }
                if !report.intermediate_mixups.is_empty() {
                    summary
                        .intermediate_mixups
                        .push((report.index, report.intermediate_mixups.clone()));
                }
                if report.writes_succeeded != expected_writes {
                    summary
                        .short_writes
                        .push((report.index, report.writes_succeeded));
                }
            }
            WorkerOutcome::OpenFailed { index, error } => {
                summary.open_failed.push((*index, error.clone()));
            }
            WorkerOutcome::Panicked { index, message } => {
                summary.panicked.push((*index, message.clone()));
            }
        }
    }
    summary
}

/// 1 ラウンド分のハーネスを走らせ、`find_mixups` で混線を返す。
fn run_round(base: &Path) -> (Vec<WorkerOutcome>, Vec<Mixup>) {
    let config = HarnessConfig {
        profiles: PROFILE_COUNT,
        writes_per_profile: WRITES_PER_PROFILE,
    };
    let outcomes = run_concurrent_writes(base, &config);
    let mixups = find_mixups(base, PROFILE_COUNT);
    (outcomes, mixups)
}

/// `PROF-3`・TASK-52（52.2）・#185: 5 プロファイル × 100 回の並行書き込みで
/// panic・open 失敗・書き込み失敗が 0 件、かつディスク上の混線が 0 件で
/// あること。競合の検出機会を増やすため複数ラウンド繰り返す。
#[test]
fn prof_3_five_concurrent_profiles_have_zero_mixups_and_zero_crashes() {
    const ROUNDS: usize = 3;
    for round in 0..ROUNDS {
        let tmp = TempDir::new();
        let (outcomes, mixups) = run_round(tmp.path());

        assert_eq!(outcomes.len(), PROFILE_COUNT, "round {round}: outcome 件数");
        let summary = summarize_failures(&outcomes, WRITES_PER_PROFILE);
        assert_eq!(summary.panicked.len(), 0, "round {round}: {summary:?}");
        assert_eq!(summary.open_failed.len(), 0, "round {round}: {summary:?}");
        assert_eq!(
            summary.write_failures.len(),
            0,
            "round {round}: {summary:?}"
        );
        assert_eq!(summary.short_writes.len(), 0, "round {round}: {summary:?}");
        assert_eq!(
            summary.intermediate_mixups.len(),
            0,
            "round {round}: 書き込み途中の混線: {summary:?}"
        );

        let rendered: Vec<String> = mixups.iter().map(|m| m.to_string()).collect();
        assert_eq!(mixups.len(), 0, "round {round}: mixups: {rendered:#?}");

        // レポートの記録とディスクの内容が一致することの二重確認。
        for outcome in &outcomes {
            let WorkerOutcome::Completed(report) = outcome else {
                panic!("round {round}: 完走しないワーカーがある");
            };
            for (kind, payload) in &report.last_payloads {
                let path = report.root.join(kind.dir_name()).join(TARGET_FILE_NAME);
                let on_disk = std::fs::read(&path).expect("ディスク内容の読み戻し");
                assert_eq!(
                    on_disk,
                    payload.as_bytes(),
                    "round {round}: worker {} {kind:?}",
                    report.index
                );
            }
        }
    }
}

/// `PROF-3`・#185（陰性対照）: 他プロファイルのペイロードがディスクに
/// 混入した場合に `find_mixups` が検出すること。
#[test]
fn prof_3_on_disk_mixup_is_detected() {
    let tmp = TempDir::new();
    let (_, mixups) = run_round(tmp.path());
    assert_eq!(mixups.len(), 0, "汚染前は 0 件");

    let kind = DataKind::ALL[0];
    let path = tmp
        .path()
        .join("profile-0")
        .join(kind.dir_name())
        .join(TARGET_FILE_NAME);
    std::fs::write(&path, b"profile-1:write-0000\n").expect("汚染の書き込み");

    let mixups = find_mixups(tmp.path(), PROFILE_COUNT);
    assert_eq!(mixups.len(), 2, "{mixups:?}");
    assert!(
        mixups.iter().all(|m| m.index == 0 && m.kind == Some(kind)),
        "{mixups:?}"
    );
    assert!(
        mixups.iter().any(|m| matches!(
            &m.detail,
            MixupDetail::ContentMismatch { actual, .. } if actual == b"profile-1:write-0000\n"
        )),
        "{mixups:?}"
    );
    assert!(
        mixups
            .iter()
            .any(|m| matches!(m.detail, MixupDetail::ForeignMarker { foreign_index: 1 })),
        "{mixups:?}"
    );
}

/// `PROF-3`・#185（陰性対照）: 想定外のエントリを配置検査が 1 件だけ検出する。
#[test]
fn prof_3_unexpected_entry_is_detected() {
    let tmp = TempDir::new();
    let (_, mixups) = run_round(tmp.path());
    assert_eq!(mixups.len(), 0, "汚染前は 0 件");

    let kind = DataKind::ALL[0];
    let dir = tmp.path().join("profile-2").join(kind.dir_name());
    std::fs::write(dir.join("stray.dat"), b"x").expect("余分なファイルの作成");

    let mixups = find_mixups(tmp.path(), PROFILE_COUNT);
    assert_eq!(mixups.len(), 1, "{mixups:?}");
    let m = &mixups[0];
    assert_eq!((m.index, m.kind), (2, Some(kind)));
    assert!(
        matches!(&m.detail, MixupDetail::UnexpectedEntry { name, .. } if name == "stray.dat"),
        "{m}"
    );
}

/// `PROF-3`・#185（陰性対照）: 壊れた symlink を欠損（`MissingEntry`）として
/// 1 件だけ報告し、`NotRegularFile` とは区別する。
#[cfg(unix)]
#[test]
fn prof_3_dangling_symlink_is_reported_as_missing() {
    let tmp = TempDir::new();
    let (_, mixups) = run_round(tmp.path());
    assert_eq!(mixups.len(), 0, "汚染前は 0 件");

    let kind = DataKind::ALL[0];
    let path = tmp
        .path()
        .join("profile-3")
        .join(kind.dir_name())
        .join(TARGET_FILE_NAME);
    std::fs::remove_file(&path).expect("実体の削除");
    std::os::unix::fs::symlink(tmp.path().join("no-such-target"), &path)
        .expect("壊れた symlink の作成");

    let mixups = find_mixups(tmp.path(), PROFILE_COUNT);
    assert_eq!(mixups.len(), 1, "{mixups:?}");
    let m = &mixups[0];
    assert_eq!((m.index, m.kind), (3, Some(kind)));
    assert!(
        matches!(&m.detail, MixupDetail::MissingEntry { name, .. } if name == TARGET_FILE_NAME),
        "{m}"
    );
}

/// `PROF-3`・#185（陰性対照）: 集計器が panic・open 失敗を数えること。
/// 本体テストの「0 件」が空虚でないことを示す。
#[test]
fn prof_3_crash_summary_counts_panicked_and_open_failed() {
    let report = |index: usize| {
        WorkerOutcome::Completed(WorkerReport {
            index,
            root: PathBuf::from(format!("profile-{index}")),
            writes_attempted: WRITES_PER_PROFILE,
            writes_succeeded: WRITES_PER_PROFILE,
            write_errors: Vec::new(),
            last_payloads: Vec::new(),
            intermediate_mixups: Vec::new(),
        })
    };
    let outcomes = vec![
        report(0),
        report(1),
        WorkerOutcome::Panicked {
            index: 2,
            message: "boom".to_string(),
        },
        WorkerOutcome::OpenFailed {
            index: 3,
            error: "denied".to_string(),
        },
        report(4),
    ];
    let summary = summarize_failures(&outcomes, WRITES_PER_PROFILE);
    assert_eq!(summary.panicked, vec![(2, "boom".to_string())]);
    assert_eq!(summary.open_failed, vec![(3, "denied".to_string())]);
    assert_eq!(summary.write_failures.len(), 0);
    assert_eq!(summary.short_writes.len(), 0);
}

/// `PROF-3`・#185（陰性対照）: 書き込み直後検査が、後で上書きされて最終状態には
/// 残らない途中の混線（他プロファイルへの誤書き込み・自ファイルの内容不一致）を
/// 検出すること。
#[test]
fn prof_3_intermediate_mixup_is_detected_even_if_overwritten_later() {
    let tmp = TempDir::new();
    let kind = DataKind::ALL[0];
    let mut profiles = Vec::new();
    for i in 0..PROFILE_COUNT {
        profiles.push(Profile::open(tmp.path().join(format!("profile-{i}"))).expect("open"));
    }
    let own = "profile-0:write-0000\n";
    write_once(&profiles[0], kind, own.as_bytes()).expect("write");
    assert_eq!(
        check_after_write(tmp.path(), 0, kind, own, PROFILE_COUNT),
        Vec::<String>::new(),
        "正常時は 0 件"
    );

    // profile-1 のファイルへ profile-0 のペイロードが誤って書かれた状況。
    write_once(&profiles[1], kind, own.as_bytes()).expect("誤書き込み");
    let found = check_after_write(tmp.path(), 0, kind, own, PROFILE_COUNT);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("contains marker of profile-0"),
        "{found:?}"
    );

    // 後続の正しい書き込みで上書きされると最終状態の検出器は 0 件になるが、
    // 途中の検査結果（上の found）は残る。
    write_once(&profiles[1], kind, b"profile-1:write-0000\n").expect("正しい書き込み");
    assert_eq!(
        check_after_write(tmp.path(), 0, kind, own, PROFILE_COUNT).len(),
        0
    );
}

/// `PROF-3`・#185（陰性対照）: 自プロファイルのファイルが読めない場合は
/// 未作成の他プロファイルと違い違反として記録されること。
#[test]
fn prof_3_own_file_read_failure_is_reported() {
    let tmp = TempDir::new();
    let kind = DataKind::ALL[0];
    for i in 0..PROFILE_COUNT {
        Profile::open(tmp.path().join(format!("profile-{i}"))).expect("open");
    }
    // どのプロファイルにも未書き込み: 他プロファイルの NotFound は許容、
    // 自プロファイル（index 0）の NotFound だけが 1 件の指摘になる。
    let found = check_after_write(tmp.path(), 0, kind, "profile-0:write-0000\n", PROFILE_COUNT);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("failed to read"), "{found:?}");
}

/// `PROF-3`・#185（陰性対照）: 他プロファイルのファイルに、他プロファイルの
/// マーカーを含まない壊れた・途中までの内容があっても指摘されること。
/// 空・所有者の正規ペイロードは許容される。
#[test]
fn prof_3_corrupted_other_profile_content_is_detected() {
    let tmp = TempDir::new();
    let kind = DataKind::ALL[0];
    let mut profiles = Vec::new();
    for i in 0..PROFILE_COUNT {
        profiles.push(Profile::open(tmp.path().join(format!("profile-{i}"))).expect("open"));
    }
    let own = "profile-0:write-0000\n";
    write_once(&profiles[0], kind, own.as_bytes()).expect("write");

    // 空（truncate 直後）と正規ペイロードは正常。
    write_once(&profiles[1], kind, b"").expect("空書き込み");
    assert_eq!(
        check_after_write(tmp.path(), 0, kind, own, PROFILE_COUNT),
        Vec::<String>::new()
    );
    write_once(&profiles[1], kind, b"profile-1:write-0007\n").expect("正規");
    assert_eq!(
        check_after_write(tmp.path(), 0, kind, own, PROFILE_COUNT),
        Vec::<String>::new()
    );

    // 途中まで書かれた内容・ゴミ・改行欠落は指摘になる。
    for corrupted in [&b"profile-1:wri"[..], b"garbage\n", b"profile-1:write-0007"] {
        write_once(&profiles[1], kind, corrupted).expect("壊れた内容");
        let found = check_after_write(tmp.path(), 0, kind, own, PROFILE_COUNT);
        assert_eq!(found.len(), 1, "{corrupted:?}: {found:?}");
        assert!(found[0].contains("not a valid payload"), "{found:?}");
    }
}
