//! `snapshot::Node::ref` の生成ロジック（`AISNAP-10`・`AISNAP-1`・`TASK-11.6`・
//! `MS-2`・Issue #75）。
//!
//! 役割: role + name のシグネチャから決まる ref を発行するアロケータを提供する。
//! 生成のたびに振り直す連番（`e1`, `e2`, ...）は DOM の軽微な変化で番号がずれる
//! ため、`GET /ai/snapshot` の取り直しで対象を再特定できない（`AISNAP-10`）。
//!
//! 呼び出し文脈: DOM から `Snapshot` を構築する `snapshot::build::build_snapshot`
//! （TASK-11.7・Issue #76）が使い、将来は `/ai/snapshot`（`AISNAP-6`・TASK-19）
//! 経由で使われる。どのノードに ref を振るか、木への一括割り当ては本モジュールの
//! 範囲外で `build_snapshot` が担う（現状はルート以外の全要素）。
//!
//! # 形式
//!
//! `e` + 16 桁の小文字 16 進ダイジェスト（例: `eccfd6f8d27bb0f9b`）。同じ
//! シグネチャの 2 個目以降には `-<出現番号>` を付ける（例: `e78faeeefec1c2870-2`）。
//! 使う文字は `[a-z0-9-]` のみで、name（ページ由来の untrusted な文字列）は ref に
//! 入らない。読める形（`button:Submit` 等）にしないのは、`AISNAP-1` のトークン削減の
//! ため ref を固定長にするためである。
//!
//! # 一意性と衝突
//!
//! 出現番号のカウンタはダイジェストではなくシグネチャ自身（64bit の主ダイジェスト +
//! 独立な 64bit の検査ダイジェストの組）をキーにする。別シグネチャの主ダイジェストが
//! 衝突しても、互いの出現番号は消費し合わない。衝突したシグネチャには最初に見た順で
//! `variant`（1 始まり。最初のものは 0）を振り、`e<16hex>v<variant>[-<n>]` とする。
//! これにより ref は常に一意になり、通常のページ（衝突なし）では別シグネチャの挿入で
//! 既存要素の ref は変わらない。
//!
//! ただし主ダイジェストが衝突する別シグネチャを意図的に作れる敵対的ページでは、
//! 衝突相手を先に置くことで `variant` を押し出せる。ref 全体を可逆にしない限り
//! 一意性と順序非依存は両立しないため、影響は「その衝突ペアの安定性が落ちる」ことに
//! 限定される。FNV は暗号学的ハッシュではなく、ref は認可トークンでもない。
//!
//! # 再特定の安定性（同名要素）
//!
//! 同じ role + name の要素は、出現番号だけでは前方への挿入・並び替えで別要素を
//! 指してしまう。これを避けるため、呼び出し側は [`ElementSignature`] で要素を
//! 区別できる安定情報を渡す。
//!
//! - `discriminator`: `id`・`href`・フォーム部品の `name` など、要素自身に付く
//!   安定した識別属性（untrusted な文字列。ダイジェストにのみ使い ref には入らない）
//! - `scope`: 親要素の ref。親の digest と variant（出現番号は含めない）を
//!   ダイジェストへ折り込み、別コンテナの同名要素を区別する範囲指定
//!
//! 親の出現番号を子のダイジェストへ折り込まないのは、`ElementRef.digest` を通じて
//! 全子孫へ漏れ、同じ role + name の祖先が前に挿入されただけで識別属性を持つ子孫の
//! ref まで変わるためである。ダイジェストは祖先の識別情報（role・name・識別属性）の
//! 連鎖だけで決まり、文書順には依存しない。
//!
//! 同じ識別情報の連鎖を持つ要素（識別属性のない同名カード配下の同名ボタンや、
//! 別カードで `href` が重複するリンク等）は同一ダイジェストになり、先行順の出現番号で
//! 区別される。連鎖が異なる要素は、同名要素の挿入・並び替えの影響を受けない。
//! 連鎖まで同じ要素の前へ同種の要素を挿入すると、それ以降の出現番号は一斉にずれる
//! （原理的な限界。将来は DOM 構造ハッシュの併用を検討する）。親側に
//! `discriminator` を渡すと、親と子の ref は安定する。
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

/// 主ダイジェスト（FNV-1a 64bit）の offset basis。
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// 主ダイジェスト（FNV-1a 64bit）の prime。
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
/// 検査ダイジェストの offset basis（主とは独立に選んだ値）。
const CHECK_OFFSET: u64 = 0x9e37_79b9_7f4a_7c15;
/// 検査ダイジェストの乗数（主と異なる奇数。主の衝突が検査へ波及しにくくする）。
const CHECK_PRIME: u64 = 0x9e37_79b1_85eb_ca87;

/// 主・検査の 2 系統ハッシュ状態。
#[derive(Clone, Copy)]
struct Hashes {
    primary: u64,
    check: u64,
}

impl Hashes {
    fn new() -> Self {
        Self {
            primary: FNV_OFFSET,
            check: CHECK_OFFSET,
        }
    }

    /// 両系統を 1 区間ぶん進める。
    ///
    /// `wrapping_mul` はハッシュ定義そのもの（mod 2^64 の乗算）であり、
    /// オーバーフローの見逃しではない。
    fn update(mut self, bytes: &[u8]) -> Self {
        for &b in bytes {
            self.primary ^= u64::from(b);
            self.primary = self.primary.wrapping_mul(FNV_PRIME);
            self.check ^= u64::from(b);
            self.check = self.check.wrapping_mul(CHECK_PRIME);
        }
        self
    }
}

/// role + name のシグネチャダイジェスト（64bit）を返す純粋関数（`AISNAP-10`）。
///
/// 入力は `(role のバイト長: u64 LE) ++ role ++ name` と符号化し、
/// `("ab","c")` と `("a","bc")` が別物になるようにする（NUL 区切りは name に
/// U+0000 が入ると曖昧になるため使わない）。
pub fn ref_signature(role: &str, name: &str) -> u64 {
    ElementSignature::new(role, name).digest()
}

/// ref の元になる要素シグネチャ（`AISNAP-10`）。
///
/// `role`・`name` に加え、同名要素を区別する安定情報（`discriminator`・`scope`）を
/// 任意で持つ。TASK-11.7 の木構築が DOM 属性・親 ref から組み立てる想定。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElementSignature<'a> {
    /// アクセシビリティ role。
    pub role: &'a str,
    /// アクセシブル name。
    pub name: &'a str,
    /// 要素自身の安定した識別属性（`id`・`href` 等）。無ければ `None`。
    pub discriminator: Option<&'a str>,
    /// 親要素の ref。ルート直下は `None`。
    ///
    /// 親の digest と variant をシグネチャへ折り込む（出現番号は折り込まない。
    /// 理由はモジュール冒頭の「再特定の安定性」を参照）。
    pub scope: Option<ElementRef>,
}

impl<'a> ElementSignature<'a> {
    /// role + name のみのシグネチャを作る。
    pub fn new(role: &'a str, name: &'a str) -> Self {
        Self {
            role,
            name,
            discriminator: None,
            scope: None,
        }
    }

    /// 識別属性を設定する。
    pub fn with_discriminator(mut self, discriminator: &'a str) -> Self {
        self.discriminator = Some(discriminator);
        self
    }

    /// 親要素の ref を設定する。
    pub fn with_scope(mut self, scope: ElementRef) -> Self {
        self.scope = Some(scope);
        self
    }

    /// 主ダイジェスト・検査ダイジェストの組を返す。
    fn hashes(&self) -> Hashes {
        let mut h = Hashes::new().update(&(self.role.len() as u64).to_le_bytes());
        h = h.update(self.role.as_bytes()).update(self.name.as_bytes());
        if self.discriminator.is_some() || self.scope.is_some() {
            // 追加情報あり。name の長さを入れて name と後続の境界を固定する。
            h = h
                .update(&[0xff])
                .update(&(self.name.len() as u64).to_le_bytes());
            if let Some(d) = self.discriminator {
                h = h
                    .update(&[1])
                    .update(&(d.len() as u64).to_le_bytes())
                    .update(d.as_bytes());
            }
            if let Some(sc) = self.scope {
                h = h
                    .update(&[2])
                    .update(&sc.digest.to_le_bytes())
                    .update(&sc.variant.to_le_bytes());
                // 親の出現番号は折り込まない。折り込むと ElementRef.digest 経由で全子孫へ
                // 漏れ、同名祖先の挿入だけで識別属性を持つ子孫の ref まで変わるため。
                // 同名親（識別属性なし）の子は親の (digest, variant) が同じで同一シグネチャ
                // になり、出現番号（先行順）で区別される。
            }
        }
        h
    }

    /// 64bit ダイジェストを返す。追加情報が無ければ [`ref_signature`] と同値。
    pub fn digest(&self) -> u64 {
        self.hashes().primary
    }
}

/// 生成された ref（ダイジェスト・衝突 variant・出現番号）。`Node::ref` へは
/// [`ElementRef::to_ref_string`] で文字列化して入れる（`AISNAP-10`）。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ElementRef {
    /// role + name（+ 識別属性・親スコープ）シグネチャの 64bit ダイジェスト。
    pub digest: u64,
    /// 主ダイジェストが衝突した別シグネチャの区別番号（衝突なしは 0）。
    pub variant: u32,
    /// 同じシグネチャの中での先行順の出現番号（1 始まり）。
    pub occurrence: u32,
}

impl ElementRef {
    /// `Node::ref` に入れる文字列（`e<16hex>[v<variant>][-<n>]`）を返す。
    pub fn to_ref_string(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for ElementRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "e{:016x}", self.digest)?;
        if self.variant > 0 {
            write!(f, "v{}", self.variant)?;
        }
        if self.occurrence > 1 {
            write!(f, "-{}", self.occurrence)?;
        }
        Ok(())
    }
}

/// ref 生成の失敗（将来の拡張に備え `non_exhaustive`。REPAIR-4）。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefError {
    /// 同一シグネチャの出現番号、または衝突 variant が `u32` を超えた。
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

/// 同一主ダイジェスト内の 1 シグネチャ分の状態。
#[derive(Debug)]
struct Slot {
    /// 検査ダイジェスト（シグネチャの同一性判定に使う）。
    check: u64,
    /// 衝突 variant（最初は 0）。
    variant: u32,
    /// これまでに発行した出現数。
    count: u32,
}

/// 1 スナップショット分の ref アロケータ（`AISNAP-10`）。
///
/// メモリ使用量は異なるシグネチャの種類数に比例し、name の長さに依存しない。
#[derive(Debug, Default)]
pub struct RefAllocator {
    seen: HashMap<u64, Vec<Slot>>,
}

impl RefAllocator {
    /// 空のアロケータを作る。
    pub fn new() -> Self {
        Self::default()
    }

    /// role + name から ref を 1 つ発行する。同じシグネチャは呼ぶたびに
    /// 出現番号が増え、必ず異なる ref を返す。
    pub fn allocate(&mut self, role: &str, name: &str) -> Result<ElementRef, RefError> {
        self.allocate_signature(&ElementSignature::new(role, name))
    }

    /// 識別属性・親スコープ付きのシグネチャから ref を発行する。同名要素でも
    /// 識別情報が異なれば、挿入・並び替えに影響されない ref になる。
    pub fn allocate_signature(
        &mut self,
        sig: &ElementSignature<'_>,
    ) -> Result<ElementRef, RefError> {
        let h = sig.hashes();
        self.allocate_with_hashes(h.primary, h.check)
    }

    /// ダイジェストを直接指定して発行する。衝突経路のテスト用で crate 外へは出さない。
    pub(crate) fn allocate_with_hashes(
        &mut self,
        digest: u64,
        check: u64,
    ) -> Result<ElementRef, RefError> {
        let slots = self.seen.entry(digest).or_default();
        let idx = match slots.iter().position(|s| s.check == check) {
            Some(i) => i,
            None => {
                let variant =
                    u32::try_from(slots.len()).map_err(|_| RefError::OccurrenceOverflow)?;
                slots.push(Slot {
                    check,
                    variant,
                    count: 0,
                });
                slots.len() - 1
            }
        };
        let slot = slots.get_mut(idx).ok_or(RefError::OccurrenceOverflow)?;
        let occurrence = slot
            .count
            .checked_add(1)
            .ok_or(RefError::OccurrenceOverflow)?;
        slot.count = occurrence;
        Ok(ElementRef {
            digest,
            variant: slot.variant,
            occurrence,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn s(r: ElementRef) -> String {
        r.to_ref_string()
    }

    /// `AISNAP-10`: FNV-1a 64bit による決定的な値（Python で独立に算出した期待値）。
    #[test]
    fn aisnap_10_ref_signature_is_deterministic_fnv1a() {
        let mut a = RefAllocator::new();
        assert_eq!(
            s(a.allocate("button", "Submit").unwrap()),
            "eccfd6f8d27bb0f9b"
        );
        assert_eq!(
            s(a.allocate("heading", "Example Domain").unwrap()),
            "e287a7dcba36a4eea"
        );
    }

    /// `AISNAP-10`（受入基準）: 同じ role + name でも ref は重複しない。
    #[test]
    fn aisnap_10_duplicate_role_name_yields_unique_refs() {
        let mut a = RefAllocator::new();
        let refs: Vec<String> = (0..3)
            .map(|_| s(a.allocate("link", "More").unwrap()))
            .collect();
        assert_eq!(
            refs,
            vec![
                "e78faeeefec1c2870",
                "e78faeeefec1c2870-2",
                "e78faeeefec1c2870-3"
            ]
        );
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
        assert_eq!(run(&base).last().unwrap(), "eccfd6f8d27bb0f9b");
        assert_eq!(run(&with_banner).last().unwrap(), "eccfd6f8d27bb0f9b");

        let links = [("link", "More"), ("link", "More")];
        let links_banner = [("generic", "banner"), ("link", "More"), ("link", "More")];
        assert_eq!(run(&links).last().unwrap(), "e78faeeefec1c2870-2");
        assert_eq!(run(&links_banner).last().unwrap(), "e78faeeefec1c2870-2");
    }

    /// `AISNAP-10`: 識別属性が違う同名要素は、前方への挿入・並び替えで ref が入れ替わらない。
    #[test]
    fn aisnap_10_discriminator_keeps_same_name_refs_stable() {
        let run = |hrefs: &[&str]| {
            let mut a = RefAllocator::new();
            hrefs
                .iter()
                .map(|h| {
                    let sig = ElementSignature::new("link", "More").with_discriminator(h);
                    (h.to_string(), s(a.allocate_signature(&sig).unwrap()))
                })
                .collect::<std::collections::HashMap<_, _>>()
        };
        let before = run(&["/a", "/b"]);
        let inserted = run(&["/new", "/a", "/b"]);
        let reordered = run(&["/b", "/a"]);
        assert_eq!(before["/a"], inserted["/a"]);
        assert_eq!(before["/b"], inserted["/b"]);
        assert_eq!(before["/a"], reordered["/a"]);
        assert_ne!(before["/a"], before["/b"]);
    }

    /// `AISNAP-10`: 親スコープが違えば同名要素の出現番号は共有されない。
    #[test]
    fn aisnap_10_scope_separates_same_name_in_different_containers() {
        let mut a = RefAllocator::new();
        let p1 = ElementRef {
            digest: 1,
            variant: 0,
            occurrence: 1,
        };
        let p2 = ElementRef {
            digest: 2,
            variant: 0,
            occurrence: 1,
        };
        let first = ElementSignature::new("button", "Delete").with_scope(p1);
        let second = ElementSignature::new("button", "Delete").with_scope(p2);
        let r1 = a.allocate_signature(&first).unwrap();
        let r2 = a.allocate_signature(&second).unwrap();
        assert_eq!(r1.occurrence, 1);
        assert_eq!(r2.occurrence, 1);
        assert_ne!(s(r1), s(r2));
        assert_eq!(
            ElementSignature::new("button", "Submit").digest(),
            ref_signature("button", "Submit")
        );
    }

    /// `AISNAP-10`: 識別属性のない同名親の子は同一ダイジェストになり、出現番号で区別される。
    #[test]
    fn aisnap_10_undiscriminated_children_of_same_name_parents_are_distinct() {
        let mut a = RefAllocator::new();
        let mut refs = Vec::new();
        for _ in 0..2 {
            let card = a
                .allocate_signature(&ElementSignature::new("article", "Card"))
                .unwrap();
            let del = ElementSignature::new("button", "Delete").with_scope(card);
            refs.push(a.allocate_signature(&del).unwrap());
        }
        assert_eq!(refs[0].digest, refs[1].digest);
        assert_eq!(refs[0].occurrence, 1);
        assert_eq!(refs[1].occurrence, 2);
        assert_ne!(s(refs[0]), s(refs[1]));
    }

    /// 識別属性を持つ子孫の ref を、カード（同名祖先）の前方挿入前後で作る。
    /// 構造は card(識別属性なし) > section(識別属性なし) > link(識別属性 = 引数)。
    fn nested_refs(cards: &[&str]) -> std::collections::HashMap<String, String> {
        let mut a = RefAllocator::new();
        let mut out = std::collections::HashMap::new();
        for c in cards {
            let card = a
                .allocate_signature(&ElementSignature::new("article", "Card"))
                .unwrap();
            let section = a
                .allocate_signature(&ElementSignature::new("group", "Details").with_scope(card))
                .unwrap();
            let link = ElementSignature::new("link", "More")
                .with_discriminator(c)
                .with_scope(section);
            out.insert(c.to_string(), s(a.allocate_signature(&link).unwrap()));
        }
        out
    }

    /// `AISNAP-10`: 同名祖先が前に挿入されても、識別属性を持つ直接の子・孫の ref は
    /// 変わらない（親の出現番号が digest 経由で子孫へ漏れない）。
    #[test]
    fn aisnap_10_ancestor_occurrence_does_not_leak_into_descendants() {
        let before = nested_refs(&["/a", "/b"]);
        let inserted = nested_refs(&["/new", "/a", "/b"]);
        assert_eq!(before["/a"], inserted["/a"]);
        assert_eq!(before["/b"], inserted["/b"]);
        assert_ne!(before["/a"], before["/b"]);
        assert_eq!(before["/a"], "e081f3e4f8974fb31");
        assert_eq!(before["/b"], "e004e3575fb2b5dca");
    }

    /// `AISNAP-10`: 子に識別属性があれば、同じ role + name の親が前に挿入されても
    /// 子の ref は変わらない。
    #[test]
    fn aisnap_10_child_ref_ignores_parent_occurrence() {
        let run = |cards: &[&str]| {
            let mut a = RefAllocator::new();
            let mut out = std::collections::HashMap::new();
            for c in cards {
                let card = a
                    .allocate_signature(&ElementSignature::new("article", "Card"))
                    .unwrap();
                let del = ElementSignature::new("button", "Delete")
                    .with_discriminator(c)
                    .with_scope(card);
                out.insert(c.to_string(), s(a.allocate_signature(&del).unwrap()));
            }
            out
        };
        let before = run(&["a", "b"]);
        let inserted = run(&["new", "a", "b"]);
        assert_eq!(before["a"], inserted["a"]);
        assert_eq!(before["b"], inserted["b"]);
        assert_ne!(before["a"], before["b"]);
    }

    /// `AISNAP-10`: 別カードで識別属性（href）が重複する子は、同一ダイジェスト +
    /// 出現番号で一意になる。連鎖の異なる要素の挿入では既存 ref は動かない。
    #[test]
    fn aisnap_10_duplicate_discriminator_across_parents_stays_unique() {
        let links = |cards: &[(&str, &str)]| {
            let mut a = RefAllocator::new();
            let mut out = Vec::new();
            for (name, href) in cards {
                let card = a
                    .allocate_signature(&ElementSignature::new("article", name))
                    .unwrap();
                let link = ElementSignature::new("link", "More")
                    .with_discriminator(href)
                    .with_scope(card);
                out.push(s(a.allocate_signature(&link).unwrap()));
            }
            out
        };
        // 同名カード 2 枚が同じ href を持つ: 出現番号で一意。
        let same = links(&[("Card", "/x"), ("Card", "/x")]);
        assert_ne!(same[0], same[1]);
        assert_eq!(same[1], format!("{}-2", same[0]));
        // 別名カード（連鎖が異なる）を前に挿入しても不変。
        let inserted = links(&[("Other", "/x"), ("Card", "/x"), ("Card", "/x")]);
        assert_eq!(inserted[1], same[0]);
        assert_eq!(inserted[2], same[1]);
    }

    /// `AISNAP-10`: role と name の境界が曖昧にならない。
    #[test]
    fn aisnap_10_signature_is_unambiguous() {
        assert_ne!(ref_signature("ab", "c"), ref_signature("a", "bc"));
        let mut a = RefAllocator::new();
        assert_eq!(s(a.allocate("ab", "c").unwrap()), "e606a0cce17a38ee9");
        assert_eq!(s(a.allocate("a", "bc").unwrap()), "eb60b96e484addb6c");
    }

    /// `AISNAP-10`: 主ダイジェストが衝突しても ref は一意で、先に発行した
    /// シグネチャの出現番号は衝突相手に消費されない。
    #[test]
    fn aisnap_10_digest_collision_still_unique_and_stable() {
        let mut a = RefAllocator::new();
        let first = a.allocate_with_hashes(0xdead_beef, 1).unwrap();
        assert_eq!(s(first), "e00000000deadbeef");
        // 別シグネチャ（検査ダイジェストが異なる）が衝突。
        let other = a.allocate_with_hashes(0xdead_beef, 2).unwrap();
        assert_eq!(s(other), "e00000000deadbeefv1");
        // 衝突相手の挿入後も、元シグネチャの 2 個目は出現番号を消費されない。
        let again = a.allocate_with_hashes(0xdead_beef, 1).unwrap();
        assert_eq!(s(again), "e00000000deadbeef-2");
        let other2 = a.allocate_with_hashes(0xdead_beef, 2).unwrap();
        assert_eq!(s(other2), "e00000000deadbeefv1-2");
    }

    /// `AISNAP-10`: 出現番号のあふれは panic せず Err を返し、状態を壊さない。
    #[test]
    fn aisnap_10_occurrence_overflow_returns_error() {
        let mut a = RefAllocator::new();
        a.seen.insert(
            7,
            vec![Slot {
                check: 3,
                variant: 0,
                count: u32::MAX,
            }],
        );
        assert_eq!(
            a.allocate_with_hashes(7, 3),
            Err(RefError::OccurrenceOverflow)
        );
        assert_eq!(a.seen.get(&7).unwrap().first().unwrap().count, u32::MAX);
    }

    /// `AISNAP-10`: 空 name・非 ASCII でも `e` + 16 桁 16 進になる。
    #[test]
    fn aisnap_10_empty_name_and_non_ascii() {
        let mut a = RefAllocator::new();
        for (r, n) in [("generic", ""), ("button", "送信")] {
            let text = s(a.allocate(r, n).unwrap());
            assert_eq!(text.len(), 17);
            assert!(text.starts_with('e'));
            assert!(
                text[1..]
                    .chars()
                    .all(|c| matches!(c, '0'..='9' | 'a'..='f'))
            );
        }
    }
}
