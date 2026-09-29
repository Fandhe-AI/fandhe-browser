//! `worker_memory_footprint` ベンチ（TASK-29・Issue #557・`CORE-3`・
//! `PERF-6`・`PERF-7`。親 issue #519）が読む `/proc/<pid>/smaps_rollup`
//! （Linux 固有。カーネルが単一プロセスの全マッピングを合算して返す
//! サマリ）のパーサと、集計用の要約統計を提供する純関数モジュール。
//!
//! ベンチ本体（`worker_memory_footprint.rs`）が `#[path]` でこのファイルを
//! 取り込んで実測値の集計に使う。ユニットテストは同居させない:
//! `worker_spawn_latency/stats.rs` と同じ理由（bench ターゲットは
//! `--cfg test` は付くが `--test`（libtest 起動）は付かないため、
//! `#[test]` 関数を同居させると内部の `use` が「未使用」と誤検出される）
//! で、兄弟ファイル `smaps_tests.rs`（独立した
//! `[[test]] worker_memory_footprint_smaps` ターゲット）に分離する。
//!
//! v8 crate・子プロセス起動など外部依存を一切持たない（`required-features`
//! 無しでどのビルドでも実行できる）。OS 依存の `/proc` 読み込み自体は
//! ベンチ本体側（`#[cfg(target_os = "linux")]` 配下）が行い、このファイルは
//! 文字列 → 構造体の変換だけを担う。

/// `/proc/<pid>/smaps_rollup` の 1 回分のパース結果（単位はすべて KiB）。
///
/// `Rss`・`Pss` はカーネルが必ず出す行なので必須フィールドとするが、
/// その他は環境（カーネルバージョン）により有無が変わるため
/// `Option<u64>` にする。存在しない値を `0` で埋めると「計測できな
/// かった」ことが「実際に 0 だった」ことと区別できなくなり、実装済み
/// を装う結果になる（REPAIR-3・security.md「偽装・回避機能の禁止」と
/// 同じ考え方を計測データにも適用する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmapsRollup {
    pub rss_kib: u64,
    pub pss_kib: u64,
    pub pss_anon_kib: Option<u64>,
    pub pss_file_kib: Option<u64>,
    pub shared_clean_kib: Option<u64>,
    pub shared_dirty_kib: Option<u64>,
    pub private_clean_kib: Option<u64>,
    pub private_dirty_kib: Option<u64>,
    pub swap_kib: Option<u64>,
}

/// `text`（`/proc/<pid>/smaps_rollup` の内容）から [`SmapsRollup`] を
/// 求める。
///
/// 先頭のアドレス範囲の行（例: `12345-67890 rw-p 00000000 00:00 0`）は
/// `Key: <n> kB` の形と一致しないため自然に読み飛ばされる。各行は
/// `strip_suffix` / `split_whitespace` / `parse::<u64>()` だけで解釈し、
/// 添字アクセス（`[]`）は使わない（coding-rust.md「外部入力」節。
/// `/proc` はカーネルが生成する値だが、パーサ自身は「壊れた・想定外の
/// 形式」を panic ではなく `None` として扱うべき外部入力として扱う）。
///
/// `Rss` と `Pss` のどちらかが欠けている、数値として解釈できない、
/// または同じキーが重複している場合は `None` を返す（fail-closed。
/// 0 で埋めて「計測できたふり」をしない。REPAIR-3）。
pub fn parse_smaps_rollup(text: &str) -> Option<SmapsRollup> {
    let mut rss_kib: Option<u64> = None;
    let mut pss_kib: Option<u64> = None;
    let mut pss_anon_kib: Option<u64> = None;
    let mut pss_file_kib: Option<u64> = None;
    let mut shared_clean_kib: Option<u64> = None;
    let mut shared_dirty_kib: Option<u64> = None;
    let mut private_clean_kib: Option<u64> = None;
    let mut private_dirty_kib: Option<u64> = None;
    let mut swap_kib: Option<u64> = None;

    for line in text.lines() {
        let Some((key, value)) = parse_kib_line(line) else {
            continue;
        };
        // 重複キーは fail-closed（`Some` の上に無条件で上書きしない）。
        // 想定外の入力形式に対して「最初/最後の値を採用する」という
        // 暗黙のルールを持たせないための措置。
        let slot: &mut Option<u64> = match key {
            "Rss" => &mut rss_kib,
            "Pss" => &mut pss_kib,
            "Pss_Anon" => &mut pss_anon_kib,
            "Pss_File" => &mut pss_file_kib,
            "Shared_Clean" => &mut shared_clean_kib,
            "Shared_Dirty" => &mut shared_dirty_kib,
            "Private_Clean" => &mut private_clean_kib,
            "Private_Dirty" => &mut private_dirty_kib,
            "Swap" => &mut swap_kib,
            _ => continue,
        };
        if slot.is_some() {
            return None;
        }
        *slot = Some(value);
    }

    Some(SmapsRollup {
        rss_kib: rss_kib?,
        pss_kib: pss_kib?,
        pss_anon_kib,
        pss_file_kib,
        shared_clean_kib,
        shared_dirty_kib,
        private_clean_kib,
        private_dirty_kib,
        swap_kib,
    })
}

/// `"Key:      1234 kB"` 形式（末尾の `kB` 単位は必須・前後の空白は
/// 任意個数）の 1 行を `(key, value_kib)` へ解釈する。形式に合わない行
/// （アドレス範囲の行・空行・`kB` 以外の単位・末尾に余分なトークンが
/// 残る行等）は `None` を返し、呼び出し側が読み飛ばす。
fn parse_kib_line(line: &str) -> Option<(&str, u64)> {
    let (key_part, rest) = line.split_once(':')?;
    let key = key_part.trim();
    if key.is_empty() {
        return None;
    }
    let value_part = rest.trim();
    // `kB` 以外の単位（`smaps_rollup` は通常 kB 固定だが、想定外の単位が
    // 混ざっていたら `strip_suffix` が失敗し `None` へ落ちる。fail-closed
    // のため、末尾に余分なトークンが残っている行（例: `"100 MB"`・
    // `"100 kB extra"`）も同様に `None` にする）。
    let digits = value_part.strip_suffix("kB")?.trim();
    let mut tokens = digits.split_whitespace();
    let value: u64 = tokens.next()?.parse().ok()?;
    if tokens.next().is_some() {
        return None;
    }
    Some((key, value))
}

/// KiB 単位のサンプル列の要約統計。フラットな数値ではなく構造体にする
/// ことで、将来の拡張（他パーセンタイル等）が呼び出し側を壊さない
/// （coding-rust.md「公開 API」節）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemorySummary {
    pub n: usize,
    pub min_kib: u64,
    pub median_kib: f64,
    pub max_kib: u64,
}

/// `samples` から [`MemorySummary`] を求める。空なら `None`（0 件を
/// 「min=0・max=0」のように偽装しない。REPAIR-3）。
///
/// 中央値は要素数が偶数なら中央 2 値の平均（`f64`）、奇数なら中央値
/// そのもの。ソートは整数の全順序（`Ord`）でよいため `sort_unstable`
/// を使う（`worker_spawn_latency/stats.rs` の `f64::total_cmp` と役割は
/// 同じだが、こちらは NaN を持たない整数入力なので通常のソートで足り
/// る）。
pub fn summarize_kib(samples: &[u64]) -> Option<MemorySummary> {
    let n = samples.len();
    if n == 0 {
        return None;
    }

    let mut sorted: Vec<u64> = samples.to_vec();
    sorted.sort_unstable();

    let min_kib = *sorted.first()?;
    let max_kib = *sorted.last()?;
    let median_kib = median_of_sorted(&sorted)?;

    Some(MemorySummary {
        n,
        min_kib,
        median_kib,
        max_kib,
    })
}

/// ソート済み整数配列の中央値（`f64`。偶数個は中央 2 値の平均）。
fn median_of_sorted(sorted: &[u64]) -> Option<f64> {
    let n = sorted.len();
    if n == 0 {
        return None;
    }
    if n % 2 == 1 {
        sorted.get(n / 2).copied().map(|v| v as f64)
    } else {
        let hi = *sorted.get(n / 2)?;
        let lo = *sorted.get(n / 2 - 1)?;
        Some((hi as f64 + lo as f64) / 2.0)
    }
}

/// `Option<u64>` のサンプル列を要約する。すべての試行でその値が取れて
/// いた場合にのみ `Some` を返す（一部の試行だけ `Some` を集計すると
/// `n` が実際の試行数より少なくなり、「試行数と要約の `n` が一致する」
/// という呼び出し側の前提が壊れるため。all-or-none のルールで
/// fail-closed にする）。
pub fn summarize_opt_kib(samples: &[Option<u64>]) -> Option<MemorySummary> {
    let mut resolved: Vec<u64> = Vec::with_capacity(samples.len());
    for sample in samples {
        resolved.push((*sample)?);
    }
    summarize_kib(&resolved)
}
