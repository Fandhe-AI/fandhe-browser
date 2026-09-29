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
//! # 再特定の安定性（同名要素）
//!
//! 同じ role + name の要素は、出現番号だけでは前方への挿入・並び替えで別要素を
//! 指してしまう。これを避けるため、呼び出し側は [`ElementSignature`] で要素を
//! 区別できる安定情報を渡す。
//!
//! - `discriminator`: `id`・`href`・フォーム部品の `name` など、要素自身に付く
//!   安定した識別属性（untrusted な文字列。ダイジェストにのみ使い ref には入らない）
//! - `scope`: 親要素の ref のダイジェスト。別コンテナの同名要素と出現番号を
//!   共有しないための範囲指定
//!
//! これらでダイジェストが分かれる要素は、同名要素の挿入・並び替えの影響を受けない。
//! 識別属性も親も同じ完全に同一の要素だけは、文書順の出現番号に頼る（外形上
//! 区別できないため原理的な限界。将来は DOM 構造ハッシュの併用を検討する）。
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
    /// 親要素の ref のダイジェスト（[`ElementRef::digest`]）。ルート直下は `None`。
    pub scope: Option<u32>,
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

    /// 親のダイジェストを設定する。
    pub fn with_scope(mut self, scope: u32) -> Self {
        self.scope = Some(scope);
        self
    }

    /// 32bit ダイジェストを返す。追加情報が無ければ [`ref_signature`] と同値。
    pub fn digest(&self) -> u32 {
        let len = (self.role.len() as u64).to_le_bytes();
        let mut h = fnv1a_update(FNV_OFFSET, &len);
        h = fnv1a_update(h, self.role.as_bytes());
        h = fnv1a_update(h, self.name.as_bytes());
        if self.discriminator.is_some() || self.scope.is_some() {
            // 追加情報あり。name の長さを入れて name と後続の境界を固定する。
            h = fnv1a_update(h, &[0xff]);
            h = fnv1a_update(h, &(self.name.len() as u64).to_le_bytes());
            if let Some(d) = self.discriminator {
                h = fnv1a_update(h, &[1]);
                h = fnv1a_update(h, &(d.len() as u64).to_le_bytes());
                h = fnv1a_update(h, d.as_bytes());
            }
            if let Some(sc) = self.scope {
                h = fnv1a_update(h, &[2]);
                h = fnv1a_update(h, &sc.to_le_bytes());
            }
        }
        ((h >> 32) as u32) ^ (h as u32)
    }
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
        self.allocate_signature(&ElementSignature::new(role, name))
    }

    /// 識別属性・親スコープ付きのシグネチャから ref を発行する。同名要素でも
    /// 識別情報が異なれば、挿入・並び替えに影響されない ref になる。
    pub fn allocate_signature(
        &mut self,
        sig: &ElementSignature<'_>,
    ) -> Result<ElementRef, RefError> {
        self.allocate_with_digest(sig.digest())
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
        let first = ElementSignature::new("button", "Delete").with_scope(1);
        let second = ElementSignature::new("button", "Delete").with_scope(2);
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
