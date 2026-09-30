//! 表・一覧の圧縮で「どの項目を保持するか」を決める優先保持の選択層
//! （`AISNAP-12`・`TASK-16`・`TASK-16.1`・`MS-2`・Issue #104）。
//!
//! 役割: 固定件数キャップの単純な `take(cap)` に代わり、キャップ境界付近の
//! 軽微なページ変化で重要要素が簡約表現から消える問題（PoC-4）を避けるための
//! 予算モデルを提供する。本モジュールは項目数（`len`）と方針
//! （[`RetentionPolicy`]）から保持する index だけを返し、DOM には依存しない
//! （表の行・`li` などの並びへ [`Retention::apply`] で写像する）。
//!
//! # 予算モデル
//!
//! 総予算 `cap`・先頭確保数 `head_keep` について、次の順で選ぶ。
//!
//! 1. 先頭から `min(head_keep, cap, len)` 件を [`RetentionReason::Head`] で予約する。
//!    後続の段がこの予約を追い出すことはない
//! 2. 優先項目の段（ページネーション・送信ボタン。TASK-16.2・16.3 で追加予定。
//!    現在は未実装で何も選ばない）。残り予算 `cap - 確保済み` の範囲で追加する
//! 3. 残り予算で未選択の項目を文書順に [`RetentionReason::Fill`] で充填する
//! 4. 結果は文書順（index 昇順）で返し、省略件数は [`Retention::omitted`] に入れる
//!
//! `cap == 0` は保持の明示的な無効化で何も保持しない。「先頭件が必ず含まれる」
//! 保証は `cap >= 1`・`head_keep >= 1` が前提である。
//!
//! # 現状
//!
//! 呼び出し元はまだ無い。`compress_table::compress_rows` の `take(MAX_TABLE_ROWS)`
//! の置き換え（統合）は TASK-16.4（Issue #107）で行う。ページネーション（#105）・
//! 送信ボタン（#106）の優先保持も未実装で、実装済みを装わない（REPAIR-3）。
//! `<select>` の先頭圧縮は本モジュールの対象外（未実装）である。

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
/// TASK-16.2（#105）・16.3（#106）でページネーション・送信ボタン用の
/// variant を追加予定のため `non_exhaustive` とする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RetentionReason {
    /// 先頭確保により保持された。
    Head,
    /// 残り予算による文書順の充填で保持された。
    Fill,
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
    /// 範囲外の index は読み飛ばす（`[]` を使わず panic しない）。
    /// TASK-16.4 で `body_rows` 等へ適用する想定。
    pub fn apply<'a, T>(&self, items: &'a [T]) -> Vec<&'a T> {
        self.kept
            .iter()
            .filter_map(|item| items.get(item.index))
            .collect()
    }
}

/// `len` 件の並びから `policy` に従い保持する項目を選ぶ（`AISNAP-12`）。
///
/// 確保量は `min(len, cap)` で上限され、巨大な `len` でも `len` に比例した
/// 確保・走査はしない。
pub fn select_retained(len: usize, policy: &RetentionPolicy) -> Retention {
    // 正規化: 総予算は cap と len の小さい方。head_keep も予算以下へ抑える。
    let budget = policy.cap.min(len);
    let head = policy.head_keep.min(budget);
    let mut kept = Vec::with_capacity(budget);

    // 先頭確保: 後続の段に追い出されない。
    kept.extend((0..head).map(|index| RetainedItem {
        index,
        reason: RetentionReason::Head,
    }));

    // 優先項目の段（TASK-16.2・16.3 で追加予定。未実装）。
    // ここで残り予算 `budget - kept.len()` の範囲に限って追加し、Head は追い出さない。

    // 文書順の充填（現状は先頭確保の直後から連続して埋まる）。
    kept.extend((head..budget).map(|index| RetainedItem {
        index,
        reason: RetentionReason::Fill,
    }));

    // 現状は index が昇順で構築されるため整列不要。優先項目の段の追加時に整列する。
    let omitted = len.saturating_sub(kept.len());
    Retention { kept, omitted }
}

#[cfg(test)]
mod tests {
    use super::{RetentionPolicy, RetentionReason, select_retained};
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
