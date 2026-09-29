//! `snapshot::Node::ref` の生成ロジック（`AISNAP-10`・`AISNAP-1`・`TASK-11.6`・
//! `MS-2`・Issue #75）。
//!
//! 役割: role + name のシグネチャから決まる ref を発行するアロケータを提供する。
//! 生成のたびに振り直す連番（`e1`, `e2`, ...）は DOM の軽微な変化で番号がずれる
//! ため、`GET /ai/snapshot` の取り直しで対象を再特定できない（`AISNAP-10`）。
//!
//! 呼び出し文脈: 現時点では呼び出し元がない。DOM から `Snapshot` を構築する
//! TASK-11.7（Issue #76）が、将来は `/ai/snapshot`（`AISNAP-6`・TASK-19）経由で
//! 使う想定である（実装済みを装わない。REPAIR-3）。どのノードに ref を振るか、
//! 木への一括割り当ては本モジュールの範囲外で TASK-11.7 が担う。
//!
//! # 形式
//!
//! `e` + 8 桁の小文字 16 進ダイジェスト（例: `eeb466016`）。同じダイジェストの
//! 2 個目以降には `-<出現番号>` を付ける（例: `e94e6c69f-2`）。使う文字は
//! `[a-z0-9-]` のみで、name（ページ由来の untrusted な文字列）は ref に入らない。
//! 読める形（`button:Submit` 等）にしないのは、`AISNAP-1` のトークン削減のため
//! ref を固定長にするためである。
//!
//! # 一意性
//!
//! 出現番号のカウンタは `(role, name)` ではなくダイジェストをキーにする。
//! 別シグネチャが 32bit で衝突しても `eXXXXXXXX` と `eXXXXXXXX-2` になるため、
//! 一意性はハッシュの品質に依存しない。衝突の影響は安定性が少し落ちることだけ
//! である。FNV は暗号学的ハッシュではなく、ref は認可トークンでもない。
//!
//! # 既知の限界
//!
//! 同じ role + name の要素が対象より前（文書順）に挿入されると、対象の出現番号
//! の接尾辞がずれる。DOM 構造ハッシュの併用などによる改善は将来の課題である。
//! 無関係な要素の挿入では ref は変わらない。
//!
//! # 呼び出し側の契約（TASK-11.7 向け）
//!
//! - 1 回のスナップショットにつき [`RefAllocator`] を 1 つ作る
//! - ref を振るノードごとに、先行順（pre-order）の文書順で [`RefAllocator::allocate`]
//!   を 1 回ずつ呼ぶ。ルートには呼ばない
//! - 木の走査は深い DOM でのスタックオーバーフローを避けるため、明示的な
//!   スタックによる反復で行う
//!
//! ハッシュに `std::hash::DefaultHasher` を使わないのは、アルゴリズムが未規定で
//! リリースをまたぐと ref が黙って変わるおそれがあるためである。

use std::collections::HashMap;
use std::fmt;

/// FNV-1a 64bit の offset basis。
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64bit の prime。
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a を 1 区間ぶん進める。
///
/// `wrapping_mul` はハッシュ定義そのもの（mod 2^64 の乗算）であり、
/// オーバーフローの見逃しではない。
fn fnv1a_update(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// role + name のシグネチャダイジェスト（32bit）を返す純粋関数（`AISNAP-10`）。
///
/// 入力は `(role のバイト長: u64 LE) ++ role ++ name` と符号化し、
/// `("ab","c")` と `("a","bc")` が別物になるようにする（NUL 区切りは name に
/// U+0000 が入ると曖昧になるため使わない）。64bit の結果は上位・下位の XOR で
/// 32bit に畳み込む。
pub fn ref_signature(role: &str, name: &str) -> u32 {
    // usize -> u64 は 32/64bit ターゲットで情報を失わない。
    let len = (role.len() as u64).to_le_bytes();
    let mut h = fnv1a_update(FNV_OFFSET, &len);
    h = fnv1a_update(h, role.as_bytes());
    h = fnv1a_update(h, name.as_bytes());
    ((h >> 32) as u32) ^ (h as u32)
}

/// 生成された ref（ダイジェストと出現番号）。`Node::ref` へは
/// [`ElementRef::to_ref_string`] で文字列化して入れる（`AISNAP-10`）。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ElementRef {
    /// role + name シグネチャの 32bit ダイジェスト。
    pub digest: u32,
    /// 同じダイジェストの中での先行順の出現番号（1 始まり）。
    pub occurrence: u32,
}

impl ElementRef {
    /// `Node::ref` に入れる文字列（`e<8hex>` または `e<8hex>-<n>`）を返す。
    pub fn to_ref_string(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for ElementRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.occurrence <= 1 {
            write!(f, "e{:08x}", self.digest)
        } else {
            write!(f, "e{:08x}-{}", self.digest, self.occurrence)
        }
    }
}

/// ref 生成の失敗（将来の拡張に備え `non_exhaustive`。REPAIR-4）。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefError {
    /// 同一ダイジェストの出現番号が `u32` を超えた。
    OccurrenceOverflow,
}

impl fmt::Display for RefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RefError::OccurrenceOverflow => {
                write!(f, "ref occurrence counter overflowed")
            }
        }
    }
}

impl std::error::Error for RefError {}

/// 1 スナップショット分の ref アロケータ（`AISNAP-10`）。
///
/// メモリ使用量は異なるダイジェストの種類数以下で、name の長さに依存しない。
#[derive(Debug, Default)]
pub struct RefAllocator {
    seen: HashMap<u32, u32>,
}

impl RefAllocator {
    /// 空のアロケータを作る。
    pub fn new() -> Self {
        Self::default()
    }

    /// role + name から ref を 1 つ発行する。同じシグネチャは呼ぶたびに
    /// 出現番号が増え、必ず異なる ref を返す。
    pub fn allocate(&mut self, role: &str, name: &str) -> Result<ElementRef, RefError> {
        self.allocate_with_digest(ref_signature(role, name))
    }

    /// ダイジェストを直接指定して発行する。衝突経路のテスト用で crate 外へは出さない。
    pub(crate) fn allocate_with_digest(&mut self, digest: u32) -> Result<ElementRef, RefError> {
        let count = self.seen.entry(digest).or_insert(0);
        let occurrence = count.checked_add(1).ok_or(RefError::OccurrenceOverflow)?;
        *count = occurrence;
        Ok(ElementRef { digest, occurrence })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn s(r: ElementRef) -> String {
        r.to_ref_string()
    }

    /// `AISNAP-10`: FNV-1a による決定的な値（Python で独立に算出した期待値）。
    #[test]
    fn aisnap_10_ref_signature_is_deterministic_fnv1a() {
        let mut a = RefAllocator::new();
        assert_eq!(s(a.allocate("button", "Submit").unwrap()), "eeb466016");
        assert_eq!(
            s(a.allocate("heading", "Example Domain").unwrap()),
            "e8b103321"
        );
    }

    /// `AISNAP-10`（受入基準）: 同じ role + name でも ref は重複しない。
    #[test]
    fn aisnap_10_duplicate_role_name_yields_unique_refs() {
        let mut a = RefAllocator::new();
        let refs: Vec<String> = (0..3)
            .map(|_| s(a.allocate("link", "More").unwrap()))
            .collect();
        assert_eq!(refs, vec!["e94e6c69f", "e94e6c69f-2", "e94e6c69f-3"]);
        assert_eq!(refs.iter().collect::<HashSet<_>>().len(), 3);
    }

    /// `AISNAP-10`: 無関係な要素の挿入で対象の ref が変わらない。
    #[test]
    fn aisnap_10_unrelated_insertion_keeps_ref_stable() {
        let run = |seq: &[(&str, &str)]| {
            let mut a = RefAllocator::new();
            seq.iter()
                .map(|(r, n)| s(a.allocate(r, n).unwrap()))
                .collect::<Vec<_>>()
        };
        let base = [("heading", "Example Domain"), ("button", "Submit")];
        let with_banner = [
            ("generic", "お知らせ: メンテナンス予定"),
            ("heading", "Example Domain"),
            ("button", "Submit"),
        ];
        assert_eq!(run(&base).last().unwrap(), "eeb466016");
        assert_eq!(run(&with_banner).last().unwrap(), "eeb466016");

        let links = [("link", "More"), ("link", "More")];
        let links_banner = [("generic", "banner"), ("link", "More"), ("link", "More")];
        assert_eq!(run(&links).last().unwrap(), "e94e6c69f-2");
        assert_eq!(run(&links_banner).last().unwrap(), "e94e6c69f-2");
    }

    /// `AISNAP-10`: role と name の境界が曖昧にならない。
    #[test]
    fn aisnap_10_signature_is_unambiguous() {
        assert_ne!(ref_signature("ab", "c"), ref_signature("a", "bc"));
        let mut a = RefAllocator::new();
        assert_eq!(s(a.allocate("ab", "c").unwrap()), "e77c98227");
        assert_eq!(s(a.allocate("a", "bc").unwrap()), "e32a64d88");
    }

    /// `AISNAP-10`: ダイジェストが衝突しても ref は一意になる。
    #[test]
    fn aisnap_10_digest_collision_still_unique() {
        let mut a = RefAllocator::new();
        assert_eq!(s(a.allocate_with_digest(0xdead_beef).unwrap()), "edeadbeef");
        assert_eq!(
            s(a.allocate_with_digest(0xdead_beef).unwrap()),
            "edeadbeef-2"
        );
    }

    /// `AISNAP-10`: 出現番号のあふれは panic せず Err を返し、状態を壊さない。
    #[test]
    fn aisnap_10_occurrence_overflow_returns_error() {
        let mut a = RefAllocator::new();
        a.seen.insert(7, u32::MAX);
        assert_eq!(a.allocate_with_digest(7), Err(RefError::OccurrenceOverflow));
        assert_eq!(a.seen.get(&7), Some(&u32::MAX));
    }

    /// `AISNAP-10`: 空 name・非 ASCII でも `e` + 8 桁 16 進になる。
    #[test]
    fn aisnap_10_empty_name_and_non_ascii() {
        let mut a = RefAllocator::new();
        for (r, n) in [("generic", ""), ("button", "送信")] {
            let text = s(a.allocate(r, n).unwrap());
            assert_eq!(text.len(), 9);
            assert!(text.starts_with('e'));
            assert!(
                text[1..]
                    .chars()
                    .all(|c| matches!(c, '0'..='9' | 'a'..='f'))
            );
        }
    }
}
