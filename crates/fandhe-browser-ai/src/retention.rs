//! 表・一覧の圧縮で「どの項目を保持するか」を決める優先保持の選択層
//! （`AISNAP-12`・`TASK-16`・`TASK-16.1`・`TASK-16.2`・`TASK-16.3`・`TASK-16.4`・`MS-2`・Issue #104・#105・#106・#107）。
//!
//! 役割: 固定件数キャップの単純な `take(cap)` に代わり、キャップ境界付近の
//! 軽微なページ変化で重要要素が簡約表現から消える問題（PoC-4）を避けるための
//! 予算モデルを提供する。選択層（本ファイル）は項目数（`len`）・方針
//! （[`RetentionPolicy`]）・優先候補（[`PriorityCandidate`]）から保持する index
//! だけを返し、`select_retained*` は DOM に依存しない（表の行・`li` などの並びへ
//! [`Retention::apply`] で写像する）。[`priority_candidates`] は DOM 検出子
//! モジュールを束ねる薄い合成関数である。DOM 上の優先項目の検出は子モジュール
//! （ページネーションは [`pagination`]・TASK-16.2、フォーム送信ボタンは
//! [`submit_button`]・TASK-16.3）が担う。
//!
//! # 予算モデル
//!
//! 総予算 `cap`・先頭確保数 `head_keep` について、次の順で選ぶ。
//!
//! 1. 先頭から `min(head_keep, cap, len)` 件を [`RetentionReason::Head`] で予約する。
//!    後続の段がこの予約を追い出すことはない
//! 2. 優先項目の段（ページネーション・フォーム送信ボタンとも実装済み・
//!    TASK-16.2・TASK-16.3）。呼び出し元が渡す優先候補を、残り予算
//!    `cap - 確保済み` の範囲で index 昇順に採用する。先頭確保の範囲内・範囲外の
//!    候補は無視し、同一 index の重複は最初の理由を採る
//! 3. 残り予算で未選択の項目を文書順に [`RetentionReason::Fill`] で充填する
//! 4. 結果は文書順（index 昇順）で返し、省略件数は [`Retention::omitted`] に入れる
//!
//! `cap == 0` は保持の明示的な無効化で何も保持しない。「先頭件が必ず含まれる」
//! 保証は `cap >= 1`・`head_keep >= 1` が前提である。
//!
//! # 現状
//!
//! 統合済み（TASK-16.4・Issue #107）。`compress_table::compress_rows` が
//! [`priority_candidates`] → [`select_retained_with_priority`] → [`Retention::apply`]
//! で表示対象行を選ぶ。操作要素を含む表・一覧は `can_compress` により圧縮されないため、
//! `build_snapshot` は `compress_table::omitted_rows_preserving` で同じ予算モデルを
//! 展開側へ適用する（操作要素を持つ行は ref・state を保つため上限を超えても残す）。
//! `<select>` の件数キャップは Rust 実装に存在しないため本モジュールの対象外で、
//! 将来 `<select>` を圧縮するときは本モジュールを使う。

use fandhe_browser_core::dom::{Document, NodeId};

pub mod pagination;
pub mod submit_button;

pub use pagination::{
    PaginationKind, classify_pagination_link, find_pagination_link, pagination_candidates,
};
pub use submit_button::{
    SubmitButtonKind, classify_submit_button, find_submit_button, submit_button_candidates,
};

/// 保持の予算方針（`AISNAP-12`）。構築は [`RetentionPolicy::new`] 経由に限る。
///
/// 既定値は持たない。具体的なキャップ値・先頭確保数は統合側（TASK-16.4）が決める。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RetentionPolicy {
    /// 総予算（保持する最大件数）。
    pub cap: usize,
    /// 先頭から必ず確保する件数（構築時に `cap` 以下へ正規化済み）。
    pub head_keep: usize,
}

impl RetentionPolicy {
    /// 方針を構築する。`head_keep` は `cap` 以下へクランプする（`AISNAP-12`）。
    pub fn new(cap: usize, head_keep: usize) -> Self {
        Self {
            cap,
            head_keep: head_keep.min(cap),
        }
    }
}

/// 項目が保持された理由（`AISNAP-12`）。
///
/// ページネーション用・送信ボタン用 variant はそれぞれ TASK-16.2（#105）・
/// TASK-16.3（#106）で追加済み。将来の追加に備え `non_exhaustive` とする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RetentionReason {
    /// 先頭確保により保持された。
    Head,
    /// 残り予算による文書順の充填で保持された。
    Fill,
    /// ページネーションリンク（次へ・前へ・ページ番号・もっと見る等）を含む
    /// ため優先して保持された（TASK-16.2・Issue #105）。種別の詳細は
    /// [`PaginationKind`]。
    Pagination,
    /// フォームの送信ボタンを含むため優先して保持された（TASK-16.3・
    /// Issue #106）。種別の詳細は [`SubmitButtonKind`]。
    SubmitButton,
}

/// 優先して保持したい項目の候補（`AISNAP-12`・`TASK-16.3`）。
///
/// DOM 検出側（[`submit_button::submit_button_candidates`] 等）が生成し、選択層
/// [`select_retained_with_priority`] が予算内で採用する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PriorityCandidate {
    /// 元の並びにおける 0 始まりの位置。
    pub index: usize,
    /// 優先する理由（保持結果の [`RetainedItem::reason`] になる）。
    pub reason: RetentionReason,
    /// 予算不足時の採用順位。小さいほど先に採用する（既定 0。同順位は index 昇順）。
    pub rank: u8,
}

impl PriorityCandidate {
    /// 候補を構築する（採用順位は 0。`AISNAP-12`）。
    pub fn new(index: usize, reason: RetentionReason) -> Self {
        Self {
            index,
            reason,
            rank: 0,
        }
    }

    /// 採用順位を指定した候補を返す（小さいほど予算不足時に先に採用される。`AISNAP-12`）。
    pub fn with_rank(mut self, rank: u8) -> Self {
        self.rank = rank;
        self
    }
}

/// 保持された 1 項目（`AISNAP-12`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RetainedItem {
    /// 元の並びにおける 0 始まりの位置。
    pub index: usize,
    /// 保持された理由。
    pub reason: RetentionReason,
}

/// 選択結果（`AISNAP-12`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Retention {
    /// 保持した項目（文書順）。
    pub kept: Vec<RetainedItem>,
    /// 省略した件数（`len - kept.len()`。「他N行」の元値に使える）。
    pub omitted: usize,
}

impl Retention {
    /// 保持 index を `items` へ写像して文書順に返す。
    ///
    /// 範囲外の index は読み飛ばす（`[]` を使わず panic しない）。`AISNAP-12`。
    /// `compress_rows` が表示対象行へ適用する（`AISNAP-12`・TASK-16.4）。
    pub fn apply<'a, T>(&self, items: &'a [T]) -> Vec<&'a T> {
        self.kept
            .iter()
            .filter_map(|item| items.get(item.index))
            .collect()
    }
}

/// 一覧・表の項目から優先保持候補を合成する（`AISNAP-12`・`TASK-16.4`・Issue #107）。
///
/// `compress_rows` から呼ばれ、ページネーション（[`pagination_candidates`]）→
/// フォーム送信ボタン（[`submit_button_candidates`]）の順で連結して返す。両方を含む
/// 項目は同一 index が重複するが、[`select_retained_with_priority`] は与えられた順で
/// 先の理由（`Pagination`）を残し rank は最小値に統合する。新たな検出器はここへ追加する。
/// 確保量は `2 * items.len()` 以下、走査量は各検出器の上限（`items.len() * 256` ずつと、
/// `FormIdIndex` の高々 1 回の文書走査）の和。
pub fn priority_candidates(doc: &Document, items: &[NodeId]) -> Vec<PriorityCandidate> {
    let mut candidates = pagination_candidates(doc, items);
    candidates.extend(submit_button_candidates(doc, items));
    candidates
}

/// `len` 件の並びから `policy` に従い保持する項目を選ぶ（`AISNAP-12`）。
///
/// 優先候補なしの [`select_retained_with_priority`]。確保量は `min(len, cap)` で
/// 上限され、巨大な `len` でも `len` に比例した確保・走査はしない。
pub fn select_retained(len: usize, policy: &RetentionPolicy) -> Retention {
    select_retained_with_priority(len, policy, &[])
}

/// 優先候補つきで保持する項目を選ぶ（`AISNAP-12`・`TASK-16.2`・`TASK-16.3`）。
///
/// 予算モデル（先頭確保 → 優先項目 → 文書順の充填）はモジュール doc を参照。
/// `candidates` のうち範囲外（`index >= len`）と先頭確保範囲内のものは無視し、
/// 残りを残り予算まで採用する（候補過多時は `rank` の小さい順、同順位は index 昇順）。
/// 同一 index が複数あれば与えられた順で先の理由を採る。確保量は
/// `min(len, cap) + candidates.len()` 以下、走査量は `cap + candidates.len()` 程度で、
/// 巨大な `len` に比例しない。
pub fn select_retained_with_priority(
    len: usize,
    policy: &RetentionPolicy,
    candidates: &[PriorityCandidate],
) -> Retention {
    // 正規化: 総予算は cap と len の小さい方。head_keep も予算以下へ抑える。
    let budget = policy.cap.min(len);
    let head = policy.head_keep.min(budget);
    let mut kept = Vec::with_capacity(budget);

    // 先頭確保: 後続の段に追い出されない。
    kept.extend((0..head).map(|index| RetainedItem {
        index,
        reason: RetentionReason::Head,
    }));

    // 優先項目の段: 範囲外・先頭確保済みを除き、index 昇順（安定整列で重複は
    // 与えられた順の先頭が残る）に並べて残り予算まで採用する。
    let mut prioritized: Vec<PriorityCandidate> = candidates
        .iter()
        .copied()
        .filter(|c| c.index >= head && c.index < len)
        .collect();
    prioritized.sort_by_key(|c| c.index);
    // 同一 index は先着の理由を残しつつ、採用順位は重複の中の最小 rank にする。
    prioritized.dedup_by(|dup, kept| {
        let same = dup.index == kept.index;
        if same {
            kept.rank = kept.rank.min(dup.rank);
        }
        same
    });
    // 予算不足時は rank の小さい候補（移動リンク等）を優先し、同順位は index 昇順。
    // 採用後は index 昇順へ戻す（充填段の skip が昇順を前提とするため）。
    let room = budget.saturating_sub(head);
    if prioritized.len() > room {
        prioritized.sort_by_key(|c| (c.rank, c.index));
        prioritized.truncate(room);
        prioritized.sort_by_key(|c| c.index);
    }
    kept.extend(prioritized.iter().map(|c| RetainedItem {
        index: c.index,
        reason: c.reason,
    }));

    // 文書順の充填: 優先段で採用済みの index（`prioritized` は昇順）を飛ばし、
    // 予算に達するまで埋める。
    let mut skip = prioritized.iter().map(|c| c.index).peekable();
    let mut index = head;
    while kept.len() < budget && index < len {
        while skip.next_if(|&s| s < index).is_some() {}
        if skip.next_if(|&s| s == index).is_none() {
            kept.push(RetainedItem {
                index,
                reason: RetentionReason::Fill,
            });
        }
        index += 1;
    }

    // 優先項目が Fill の間に入るため、文書順（index 昇順）へ整列する。
    kept.sort_by_key(|item| item.index);
    let omitted = len.saturating_sub(kept.len());
    Retention { kept, omitted }
}

#[cfg(test)]
mod tests {
    use super::{
        PriorityCandidate, RetentionPolicy, RetentionReason, select_retained,
        select_retained_with_priority,
    };
    use crate::compress_table::detect_regular_structure;
    use fandhe_browser_core::dom::{Document, NodeId};
    use fandhe_browser_core::parse::{ParseOptions, parse_document};
    use fandhe_browser_core::query::query_selector_str;

    fn indexes(r: &super::Retention, reason: RetentionReason) -> Vec<usize> {
        r.kept
            .iter()
            .filter(|i| i.reason == reason)
            .map(|i| i.index)
            .collect()
    }

    /// AISNAP-12（TASK-16.4・Issue #107）: ページネーションと送信ボタンの候補を合成し、
    /// 両方を含む項目は選択後に `Pagination` が残る。
    #[test]
    fn aisnap_12_priority_candidates_merge_pagination_and_submit() {
        let doc = parse(
            "<form><ul>\
             <li>a\
             <li><a href=\"/p2\" rel=\"next\">next</a>\
             <li><button>send</button>\
             <li><a href=\"/p3\" rel=\"next\">next</a><button>send</button>\
             </ul></form>",
        );
        let ul = select(&doc, "ul");
        let items: Vec<NodeId> = doc.children(ul).collect();
        let c = super::priority_candidates(&doc, &items);
        let pairs: Vec<(usize, RetentionReason)> = c.iter().map(|c| (c.index, c.reason)).collect();
        assert_eq!(
            pairs,
            vec![
                (1, RetentionReason::Pagination),
                (3, RetentionReason::Pagination),
                (2, RetentionReason::SubmitButton),
                (3, RetentionReason::SubmitButton),
            ]
        );
        let r = select_retained_with_priority(4, &RetentionPolicy::new(3, 1), &c);
        let kept: Vec<(usize, RetentionReason)> =
            r.kept.iter().map(|i| (i.index, i.reason)).collect();
        assert_eq!(kept[0], (0, RetentionReason::Head));
        assert!(kept.contains(&(1, RetentionReason::Pagination)));
    }

    /// AISNAP-12（TASK-16.1・Issue #104）: キャップ超過でも先頭件が保持される。
    #[test]
    fn aisnap_12_head_kept_when_exceeding_cap() {
        let r = select_retained(30, &RetentionPolicy::new(20, 5));
        let all: Vec<usize> = r.kept.iter().map(|i| i.index).collect();
        assert_eq!(all, (0..20).collect::<Vec<_>>());
        assert_eq!(indexes(&r, RetentionReason::Head), vec![0, 1, 2, 3, 4]);
        assert_eq!(
            indexes(&r, RetentionReason::Fill),
            (5..20).collect::<Vec<_>>()
        );
        assert_eq!(r.omitted, 10);
    }

    /// AISNAP-12: 末尾側の増加で先頭確保の結果は変わらない。
    #[test]
    fn aisnap_12_head_selection_stable_when_len_grows() {
        let p = RetentionPolicy::new(20, 5);
        let a = select_retained(30, &p);
        let b = select_retained(31, &p);
        assert_eq!(indexes(&a, RetentionReason::Head), vec![0, 1, 2, 3, 4]);
        assert_eq!(indexes(&b, RetentionReason::Head), vec![0, 1, 2, 3, 4]);
        assert_eq!((a.omitted, b.omitted), (10, 11));
    }

    /// AISNAP-12: キャップ以内なら全件保持。
    #[test]
    fn aisnap_12_all_kept_when_within_cap() {
        let r = select_retained(7, &RetentionPolicy::new(20, 3));
        assert_eq!(r.kept.len(), 7);
        assert_eq!(indexes(&r, RetentionReason::Head), vec![0, 1, 2]);
        assert_eq!(indexes(&r, RetentionReason::Fill), vec![3, 4, 5, 6]);
        assert_eq!(r.omitted, 0);
    }

    /// AISNAP-12: 空入力。
    #[test]
    fn aisnap_12_empty_input() {
        let r = select_retained(0, &RetentionPolicy::new(20, 3));
        assert!(r.kept.is_empty());
        assert_eq!(r.omitted, 0);
    }

    /// AISNAP-12: cap 0 は何も保持しない。
    #[test]
    fn aisnap_12_cap_zero_keeps_nothing() {
        let p = RetentionPolicy::new(0, 5);
        assert_eq!(p.head_keep, 0);
        let r = select_retained(10, &p);
        assert!(r.kept.is_empty());
        assert_eq!(r.omitted, 10);
    }

    /// AISNAP-12: head_keep 0 は充填のみ。
    #[test]
    fn aisnap_12_head_keep_zero_fills_in_document_order() {
        let r = select_retained(10, &RetentionPolicy::new(4, 0));
        assert_eq!(indexes(&r, RetentionReason::Fill), vec![0, 1, 2, 3]);
        assert!(indexes(&r, RetentionReason::Head).is_empty());
        assert_eq!(r.omitted, 6);
    }

    /// AISNAP-12: head_keep は cap にクランプされる。
    #[test]
    fn aisnap_12_head_keep_clamped_to_cap() {
        let p = RetentionPolicy::new(3, 10);
        assert_eq!(p.head_keep, 3);
        let r = select_retained(10, &p);
        assert_eq!(indexes(&r, RetentionReason::Head), vec![0, 1, 2]);
        assert_eq!(r.omitted, 7);
    }

    /// AISNAP-12: 巨大な len でも確保量は cap で上限され panic しない。
    #[test]
    fn aisnap_12_large_len_bounded_allocation() {
        let p = RetentionPolicy::new(20, 1);
        let r = select_retained(1_000_000, &p);
        assert_eq!(r.kept.len(), 20);
        assert_eq!(r.omitted, 999_980);
        let r = select_retained(usize::MAX, &p);
        assert_eq!(r.kept.len(), 20);
        assert_eq!(r.omitted, usize::MAX - 20);
    }

    /// AISNAP-12: apply は文書順に写像し、範囲外 index を読み飛ばす。
    #[test]
    fn aisnap_12_apply_maps_items_and_skips_out_of_range() {
        let r = select_retained(5, &RetentionPolicy::new(4, 2));
        let items = ["a", "b", "c", "d", "e"];
        assert_eq!(r.apply(&items), vec![&"a", &"b", &"c", &"d"]);
        assert_eq!(r.apply(&items[..2]), vec![&"a", &"b"]);
        assert!(r.apply::<&str>(&[]).is_empty());
    }

    fn sb(index: usize) -> PriorityCandidate {
        PriorityCandidate::new(index, RetentionReason::SubmitButton)
    }

    /// AISNAP-12（TASK-16.3・Issue #106）: キャップ外の優先候補が保持される。
    #[test]
    fn aisnap_12_priority_candidate_beyond_cap_is_kept() {
        let r = select_retained_with_priority(30, &RetentionPolicy::new(20, 5), &[sb(29)]);
        assert_eq!(indexes(&r, RetentionReason::Head), vec![0, 1, 2, 3, 4]);
        assert_eq!(indexes(&r, RetentionReason::SubmitButton), vec![29]);
        assert_eq!(
            indexes(&r, RetentionReason::Fill),
            (5..19).collect::<Vec<_>>()
        );
        assert_eq!(r.kept.len(), 20);
        assert_eq!(r.omitted, 10);
        let all: Vec<usize> = r.kept.iter().map(|i| i.index).collect();
        assert!(all.windows(2).all(|w| w[0] < w[1]));
    }

    /// AISNAP-12: 先頭確保範囲内の候補は二重計上されない。
    #[test]
    fn aisnap_12_priority_candidate_inside_head_is_not_double_counted() {
        let r = select_retained_with_priority(30, &RetentionPolicy::new(20, 5), &[sb(2)]);
        assert_eq!(indexes(&r, RetentionReason::Head), vec![0, 1, 2, 3, 4]);
        assert!(indexes(&r, RetentionReason::SubmitButton).is_empty());
        assert_eq!(
            indexes(&r, RetentionReason::Fill),
            (5..20).collect::<Vec<_>>()
        );
    }

    /// AISNAP-12: 範囲外・重複の候補は無視・統合される。
    #[test]
    fn aisnap_12_priority_out_of_range_and_duplicates() {
        let c = [sb(30), sb(usize::MAX), sb(25), sb(25)];
        let r = select_retained_with_priority(30, &RetentionPolicy::new(20, 5), &c);
        assert_eq!(indexes(&r, RetentionReason::SubmitButton), vec![25]);
        assert_eq!(r.kept.len(), 20);
        assert_eq!(r.omitted, 10);
    }

    /// AISNAP-12: 候補過多なら index 昇順で残り予算まで採用する。
    #[test]
    fn aisnap_12_priority_overflow_takes_lowest_indexes() {
        let c = [sb(29), sb(10), sb(20), sb(15), sb(12)];
        let r = select_retained_with_priority(30, &RetentionPolicy::new(8, 5), &c);
        assert_eq!(indexes(&r, RetentionReason::SubmitButton), vec![10, 12, 15]);
        assert!(indexes(&r, RetentionReason::Fill).is_empty());
        assert_eq!(r.kept.len(), 8);
    }

    /// AISNAP-12（TASK-16.2）: 候補過多時は rank の小さい候補が先に採用される。
    #[test]
    fn aisnap_12_priority_overflow_prefers_lower_rank() {
        let c = [
            sb(10).with_rank(1),
            sb(11).with_rank(1),
            sb(25).with_rank(0),
        ];
        let r = select_retained_with_priority(30, &RetentionPolicy::new(7, 5), &c);
        assert_eq!(indexes(&r, RetentionReason::SubmitButton), vec![10, 25]);
        let all: Vec<usize> = r.kept.iter().map(|i| i.index).collect();
        assert!(all.windows(2).all(|w| w[0] < w[1]));
    }

    /// AISNAP-12（TASK-16.2・Codex P2）: 同一 index の重複は先着の理由を残し、
    /// 採用順位は最小の rank になる。
    #[test]
    fn aisnap_12_duplicate_index_keeps_first_reason_and_min_rank() {
        // 残り予算 1 枠。index 10 は重複の最小 rank（0）で、index 9（rank 1）に勝つ。
        // rank を統合しないと 10 の rank は 1 のままで、index 昇順の 9 が採用される。
        let c = [
            PriorityCandidate::new(10, RetentionReason::Pagination).with_rank(1),
            PriorityCandidate::new(9, RetentionReason::Pagination).with_rank(1),
            sb(10).with_rank(0),
        ];
        let r = select_retained_with_priority(30, &RetentionPolicy::new(6, 5), &c);
        assert_eq!(indexes(&r, RetentionReason::Pagination), vec![10]);
        assert!(indexes(&r, RetentionReason::SubmitButton).is_empty());
    }

    /// AISNAP-12: head_keep == cap では優先段は先頭確保を追い出さない。
    #[test]
    fn aisnap_12_priority_does_not_evict_head() {
        let r = select_retained_with_priority(30, &RetentionPolicy::new(5, 5), &[sb(29)]);
        assert_eq!(indexes(&r, RetentionReason::Head), vec![0, 1, 2, 3, 4]);
        assert!(indexes(&r, RetentionReason::SubmitButton).is_empty());
    }

    /// AISNAP-12: cap 0 は候補があっても何も保持しない。
    #[test]
    fn aisnap_12_priority_cap_zero_keeps_nothing() {
        let r = select_retained_with_priority(10, &RetentionPolicy::new(0, 0), &[sb(3)]);
        assert!(r.kept.is_empty());
        assert_eq!(r.omitted, 10);
    }

    /// AISNAP-12: 巨大 len でも優先候補つきで確保量は cap に収まる。
    #[test]
    fn aisnap_12_priority_large_len_bounded() {
        let r = select_retained_with_priority(
            usize::MAX,
            &RetentionPolicy::new(20, 1),
            &[sb(usize::MAX - 1)],
        );
        assert_eq!(r.kept.len(), 20);
        assert_eq!(
            indexes(&r, RetentionReason::SubmitButton),
            vec![usize::MAX - 1]
        );
        assert_eq!(r.omitted, usize::MAX - 20);
    }

    /// AISNAP-12: 候補なしなら select_retained と一致する。
    #[test]
    fn aisnap_12_select_retained_equals_empty_priority() {
        let p = RetentionPolicy::new(20, 5);
        assert_eq!(
            select_retained(30, &p),
            select_retained_with_priority(30, &p, &[])
        );
    }

    fn parse(html: &str) -> Document {
        parse_document(html, &ParseOptions::default())
            .expect("テスト入力は必ず成功する")
            .document
    }

    fn select(doc: &Document, selector: &str) -> NodeId {
        query_selector_str(doc, doc.root(), selector)
            .expect("セレクタは解釈できる")
            .expect("対象要素が見つかる")
    }

    /// AISNAP-12: 実 DOM の 30 件の `li` でも先頭の `li` が保持される。
    #[test]
    fn aisnap_12_first_li_retained_on_dom_fixture() {
        let mut html = String::from("<ul>");
        for i in 0..30 {
            html.push_str(&format!("<li>item{i}</li>"));
        }
        html.push_str("</ul>");
        let doc = parse(&html);
        let ul = select(&doc, "ul");
        let detection = detect_regular_structure(&doc, ul);
        let structure = detection.as_regular().expect("規則的な一覧");
        let r = select_retained(structure.body_rows.len(), &RetentionPolicy::new(20, 1));
        let rows = r.apply(&structure.body_rows);
        assert_eq!(rows.len(), 20);
        assert_eq!(*rows[0], select(&doc, "li"));
        assert_eq!(r.omitted, 10);
    }
}
