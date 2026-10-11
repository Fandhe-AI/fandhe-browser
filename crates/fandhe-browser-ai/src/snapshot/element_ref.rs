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
//! `e` + 8 桁の小文字 16 進（64bit ダイジェストの先頭 8 桁。例: `eccfd6f8d`）。
//! 同じシグネチャの 2 個目以降には `-<出現番号>` を付ける（例: `e78faeeef-2`）。
//! 使う文字は `[a-z0-9-]` のみで、name（ページ由来の untrusted な文字列）は ref に
//! 入らない。読める形（`button:Submit` 等）にしないのは、`AISNAP-1` のトークン削減の
//! ため ref を短く保つためである。
//!
//! # 表示長（8 桁）と延長規則（TASK-23.5・Issue #836）
//!
//! ref は snapshot の総トークンの大半を占める（1 個あたり約 12 トークン）ため、
//! 表示を 16 桁から 8 桁へ短縮した（オーナー判断 2026-10-10。ref 3,000 個での表示
//! 衝突の期待値は 8 桁で約 0.001 件、6 桁だと約 0.27 件）。内部の 64bit ダイジェスト
//! （FNV-1a 主 + 検査ダイジェスト）と、子の `scope` へ折り込む値は変えない。
//!
//! 先頭 8 桁が既発行の別ダイジェストと衝突した場合は、**後から現れた側だけ**を
//! 一意になる最小の桁数（9〜16 桁）まで延ばす。先に発行した側は 8 桁のまま変わらない。
//! 桁数はダイジェストの初回発行時に 1 度だけ決め、以後変えない（同一ダイジェストの
//! `variant` 同士は桁数を共有する）。`scope` には桁数を折り込まない。折り込むと
//! 親の表示衝突が全子孫のダイジェストへ波及するためである。
//!
//! 桁の確定は発行時点で行うため、ref を表示しない内部ノード（折り畳み対象等）も桁を
//! 確保する。そのため表示ノードが不要に延長されることがまれにあるが、一意性は保たれ、
//! 確率も無視できる（約 N²/2³³）。
//!
//! # 一意性と衝突
//!
//! 出現番号のカウンタはダイジェストではなくシグネチャ自身（64bit の主ダイジェスト +
//! 独立な 64bit の検査ダイジェストの組）をキーにする。別シグネチャの主ダイジェストが
//! 衝突しても、互いの出現番号は消費し合わない。衝突したシグネチャには最初に見た順で
//! `variant`（1 始まり。最初のものは 0）を振り、`e<8..16hex>v<variant>[-<n>]` とする。
//! 衝突は 2 段階ある。先頭 8 桁だけが衝突する「表示衝突」は後着側の桁延長で、64bit
//! 全体が衝突する「完全衝突」は `variant` で解消する。これにより ref は常に一意に
//! なり、通常のページ（衝突なし）では別シグネチャの挿入で既存要素の ref は変わらない。
//!
//! ただし主ダイジェストが衝突する別シグネチャを意図的に作れる敵対的ページでは、
//! 衝突相手を先に置くことで `variant` や後着側の桁数を押し出せる。表示衝突ペアでは
//! 後着側の桁数が出現順に依存する（先着側の 8 桁表示は後着側にも前方一致しうる）。
//! ref 全体を可逆にしない限り一意性と順序非依存は両立しないため、影響は「その衝突
//! ペアの安定性が落ちる」ことに限定される。FNV は暗号学的ハッシュではなく、ref は
//! 認可トークンでもない。
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

/// ref 表示の最小桁数（16 進。ダイジェスト先頭 32bit）。
const MIN_HEX_LEN: u8 = 8;
/// ref 表示の最大桁数（ダイジェスト全体）。
const MAX_HEX_LEN: u8 = 16;
/// 同一先頭 8 桁のバケットを桁数決定のために走査する上限。敵対的ページが大量の
/// 表示衝突を作っても発行コストが二乗化しないよう、超えたら即最大桁にする。
const BUCKET_SCAN_LIMIT: usize = 64;

/// ダイジェストの先頭 `hex_len` 桁ぶん（上位 `4 * hex_len` ビット）を返す。
/// `hex_len` は 8..=16 に丸める（シフト量が 64 以上にならない）。
pub(crate) fn digest_prefix(digest: u64, hex_len: u8) -> u64 {
    let len = u32::from(hex_len.clamp(MIN_HEX_LEN, MAX_HEX_LEN));
    digest.checked_shr(64 - 4 * len).unwrap_or(digest)
}

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

/// 生成された ref（ダイジェスト・表示桁数・衝突 variant・出現番号）。`Node::ref` へは
/// [`ElementRef::to_ref_string`] で文字列化して入れる（`AISNAP-10`）。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ElementRef {
    /// role + name（+ 識別属性・親スコープ）シグネチャの 64bit ダイジェスト。
    pub digest: u64,
    /// 表示するダイジェスト先頭の 16 進桁数（8..=16。通常は 8、先頭 8 桁が既発行の
    /// 別ダイジェストと衝突した後着側のみ延長。TASK-23.5）。
    pub hex_len: u8,
    /// 主ダイジェストが衝突した別シグネチャの区別番号（衝突なしは 0）。
    pub variant: u32,
    /// 同じシグネチャの中での先行順の出現番号（1 始まり）。
    pub occurrence: u32,
}

impl ElementRef {
    /// `Node::ref` に入れる文字列（`e<8..16hex>[v<variant>][-<n>]`）を返す。
    pub fn to_ref_string(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for ElementRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let len = usize::from(self.hex_len.clamp(MIN_HEX_LEN, MAX_HEX_LEN));
        write!(
            f,
            "e{:0width$x}",
            digest_prefix(self.digest, self.hex_len),
            width = len
        )?;
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

/// 1 つの主ダイジェストの状態（表示桁数と、衝突 variant ごとの [`Slot`]）。
#[derive(Debug)]
struct DigestEntry {
    /// 初回発行時に決めた表示桁数。以後変えない。
    hex_len: u8,
    slots: Vec<Slot>,
}

/// 1 スナップショット分の ref アロケータ（`AISNAP-10`）。
///
/// メモリ使用量は異なるシグネチャの種類数に比例し、name の長さに依存しない。
#[derive(Debug, Default)]
pub struct RefAllocator {
    seen: HashMap<u64, DigestEntry>,
    /// 先頭 8 桁（`digest >> 32`）ごとの既発行ダイジェスト。表示衝突の検出用。
    buckets: HashMap<u32, Vec<u64>>,
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

    /// 先頭 8 桁（上位 32bit）をバケットのキーにする。
    fn bucket_key(digest: u64) -> u32 {
        u32::try_from(digest >> 32).unwrap_or(u32::MAX)
    }

    /// 初回発行のダイジェストの表示桁数を決める（TASK-23.5）。
    ///
    /// 既発行ダイジェストと先頭 8 桁が衝突しなければ 8、衝突すれば先頭 L 桁が
    /// バケット内の全ダイジェストと異なる最小の L（9..=16）。バケットが走査上限を
    /// 超えていれば最大桁にする。ダイジェストは互いに異なるので 16 で必ず一意になる。
    fn decide_hex_len(&self, digest: u64) -> u8 {
        let Some(bucket) = self.buckets.get(&Self::bucket_key(digest)) else {
            return MIN_HEX_LEN;
        };
        if bucket.len() > BUCKET_SCAN_LIMIT {
            return MAX_HEX_LEN;
        }
        (MIN_HEX_LEN..=MAX_HEX_LEN)
            .find(|&l| {
                let mine = digest_prefix(digest, l);
                bucket.iter().all(|&o| digest_prefix(o, l) != mine)
            })
            .unwrap_or(MAX_HEX_LEN)
    }

    /// ダイジェストを直接指定して発行する。衝突経路のテスト用で crate 外へは出さない。
    pub(crate) fn allocate_with_hashes(
        &mut self,
        digest: u64,
        check: u64,
    ) -> Result<ElementRef, RefError> {
        if !self.seen.contains_key(&digest) {
            let hex_len = self.decide_hex_len(digest);
            self.buckets
                .entry(Self::bucket_key(digest))
                .or_default()
                .push(digest);
            self.seen.insert(
                digest,
                DigestEntry {
                    hex_len,
                    slots: Vec::new(),
                },
            );
        }
        let entry = self
            .seen
            .get_mut(&digest)
            .ok_or(RefError::OccurrenceOverflow)?;
        let hex_len = entry.hex_len;
        let slots = &mut entry.slots;
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
            hex_len,
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
        assert_eq!(s(a.allocate("button", "Submit").unwrap()), "eccfd6f8d");
        assert_eq!(
            s(a.allocate("heading", "Example Domain").unwrap()),
            "e287a7dcb"
        );
    }

    /// `AISNAP-10`（受入基準）: 同じ role + name でも ref は重複しない。
    #[test]
    fn aisnap_10_duplicate_role_name_yields_unique_refs() {
        let mut a = RefAllocator::new();
        let refs: Vec<String> = (0..3)
            .map(|_| s(a.allocate("link", "More").unwrap()))
            .collect();
        assert_eq!(refs, vec!["e78faeeef", "e78faeeef-2", "e78faeeef-3"]);
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
        assert_eq!(run(&base).last().unwrap(), "eccfd6f8d");
        assert_eq!(run(&with_banner).last().unwrap(), "eccfd6f8d");

        let links = [("link", "More"), ("link", "More")];
        let links_banner = [("generic", "banner"), ("link", "More"), ("link", "More")];
        assert_eq!(run(&links).last().unwrap(), "e78faeeef-2");
        assert_eq!(run(&links_banner).last().unwrap(), "e78faeeef-2");
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
            hex_len: 8,
            variant: 0,
            occurrence: 1,
        };
        let p2 = ElementRef {
            digest: 2,
            hex_len: 8,
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
        assert_eq!(before["/a"], "e081f3e4f");
        assert_eq!(before["/b"], "e004e3575");
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
        assert_eq!(s(a.allocate("ab", "c").unwrap()), "e606a0cce");
        assert_eq!(s(a.allocate("a", "bc").unwrap()), "eb60b96e4");
    }

    /// `AISNAP-10`: 主ダイジェストが衝突しても ref は一意で、先に発行した
    /// シグネチャの出現番号は衝突相手に消費されない。
    #[test]
    fn aisnap_10_digest_collision_still_unique_and_stable() {
        let mut a = RefAllocator::new();
        let first = a.allocate_with_hashes(0xdead_beef, 1).unwrap();
        assert_eq!(s(first), "e00000000");
        // 別シグネチャ（検査ダイジェストが異なる）が衝突。
        let other = a.allocate_with_hashes(0xdead_beef, 2).unwrap();
        assert_eq!(s(other), "e00000000v1");
        // 衝突相手の挿入後も、元シグネチャの 2 個目は出現番号を消費されない。
        let again = a.allocate_with_hashes(0xdead_beef, 1).unwrap();
        assert_eq!(s(again), "e00000000-2");
        let other2 = a.allocate_with_hashes(0xdead_beef, 2).unwrap();
        assert_eq!(s(other2), "e00000000v1-2");
    }

    /// `AISNAP-10`: 出現番号のあふれは panic せず Err を返し、状態を壊さない。
    #[test]
    fn aisnap_10_occurrence_overflow_returns_error() {
        let mut a = RefAllocator::new();
        a.seen.insert(
            7,
            DigestEntry {
                hex_len: 8,
                slots: vec![Slot {
                    check: 3,
                    variant: 0,
                    count: u32::MAX,
                }],
            },
        );
        assert_eq!(
            a.allocate_with_hashes(7, 3),
            Err(RefError::OccurrenceOverflow)
        );
        assert_eq!(
            a.seen.get(&7).unwrap().slots.first().unwrap().count,
            u32::MAX
        );
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

    /// 表示 ref の hex 部（`e` の後ろから `v`/`-` の手前まで）を返す。
    fn hex_part(text: &str) -> &str {
        let body = text.strip_prefix('e').unwrap();
        let end = body.find(['v', '-']).unwrap_or(body.len());
        &body[..end]
    }

    /// `AISNAP-10`/TASK-23.5: 先頭 8 桁が衝突したら後着側だけが最小桁（9 桁）に延びる。
    #[test]
    fn aisnap_10_display_prefix_collision_extends_later_side_only() {
        let mut a = RefAllocator::new();
        let first = s(a.allocate_with_hashes(0x1234_5678_a000_0000, 1).unwrap());
        let second = s(a.allocate_with_hashes(0x1234_5678_b000_0000, 2).unwrap());
        assert_eq!(first, "e12345678");
        assert_eq!(second, "e12345678b");
        assert_ne!(first, second);
    }

    /// TASK-23.5: 先頭 12 桁まで一致する組は、後着側が必要最小の 13 桁に延びる。
    #[test]
    fn aisnap_10_display_extension_is_minimal() {
        let mut a = RefAllocator::new();
        let first = s(a.allocate_with_hashes(0x1234_5678_9abc_0000, 1).unwrap());
        let second = s(a.allocate_with_hashes(0x1234_5678_9abc_d000, 2).unwrap());
        assert_eq!(first, "e12345678");
        assert_eq!(second, "e123456789abcd");
        assert_eq!(hex_part(&second).len(), 13);
    }

    /// TASK-23.5: 無関係なダイジェストを挿入しても、先着側・後着側・無関係側の ref は不変。
    #[test]
    fn aisnap_10_unrelated_insertion_keeps_prefix_collision_refs_stable() {
        let run = |with_unrelated: bool| {
            let mut a = RefAllocator::new();
            let mut out = Vec::new();
            if with_unrelated {
                out.push(s(a.allocate_with_hashes(0x00ab_cdef_0000_0001, 9).unwrap()));
            }
            out.push(s(a.allocate_with_hashes(0x1234_5678_a000_0000, 1).unwrap()));
            if with_unrelated {
                out.push(s(a.allocate_with_hashes(0x7777_7777_0000_0002, 8).unwrap()));
            }
            out.push(s(a.allocate_with_hashes(0x1234_5678_b000_0000, 2).unwrap()));
            out
        };
        let base = run(false);
        let with = run(true);
        assert_eq!(base, vec!["e12345678", "e12345678b"]);
        assert_eq!(
            with,
            vec!["e00abcdef", "e12345678", "e77777777", "e12345678b"]
        );
    }

    /// TASK-23.5: 延長側にも出現番号・variant の規則が同じく適用される。
    #[test]
    fn aisnap_10_extended_ref_keeps_occurrence_and_variant_rules() {
        let mut a = RefAllocator::new();
        a.allocate_with_hashes(0x1234_5678_a000_0000, 1).unwrap();
        let b1 = s(a.allocate_with_hashes(0x1234_5678_b000_0000, 2).unwrap());
        let b2 = s(a.allocate_with_hashes(0x1234_5678_b000_0000, 2).unwrap());
        let bv = s(a.allocate_with_hashes(0x1234_5678_b000_0000, 3).unwrap());
        assert_eq!(b1, "e12345678b");
        assert_eq!(b2, "e12345678b-2");
        assert_eq!(bv, "e12345678bv1");
    }

    /// TASK-23.5: hex 部は自ダイジェストの先頭 L 桁と一致する（ハーネスの前方一致照合の前提）。
    #[test]
    fn aisnap_10_hex_part_is_prefix_of_own_digest() {
        let mut a = RefAllocator::new();
        for (d, c) in [
            (0x1234_5678_a000_0000_u64, 1_u64),
            (0x1234_5678_b000_0000, 2),
            (0x1234_5678_b000_0000, 3),
            (0x1234_5678_9abc_d000, 4),
        ] {
            let r = a.allocate_with_hashes(d, c).unwrap();
            let text = s(r);
            let hex = hex_part(&text);
            assert_eq!(hex.len(), usize::from(r.hex_len));
            assert_eq!(
                u64::from_str_radix(hex, 16).unwrap(),
                digest_prefix(d, r.hex_len)
            );
            assert!(format!("{d:016x}").starts_with(hex));
        }
    }

    /// TASK-23.5: 走査上限を超えたバケットは最大桁（16 桁）で発行し、状態は壊れない。
    #[test]
    fn aisnap_10_oversized_bucket_falls_back_to_full_digest() {
        let mut a = RefAllocator::new();
        for i in 0..=(BUCKET_SCAN_LIMIT as u64 + 1) {
            a.allocate_with_hashes(0x1234_5678_0000_0000 | (i << 8), i)
                .unwrap();
        }
        let last = a.allocate_with_hashes(0x1234_5678_ffff_0000, 999).unwrap();
        assert_eq!(last.hex_len, 16);
        assert_eq!(s(last), "e12345678ffff0000");
    }
}
