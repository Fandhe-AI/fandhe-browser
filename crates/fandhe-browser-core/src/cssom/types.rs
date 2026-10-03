//! CSSOM の公開型定義（TASK-105.1・#255・MS-8・ビヘイビア `CORE-5`）。
//!
//! 後続の sub-issue が共有する値型だけを持つ。宣言の生成は #256、詳細度の計算は #257、
//! ルール・スタイルシートの組み立ては #551・#552、マッチング・カスケードは #259・#260 の
//! 担当で、本モジュールにはパース・計算ロジックを含めない。型のコンストラクタは
//! 呼び出し側が確保済みのデータを受け取るだけで、外部入力の長さから確保しない。
//! 入力長・ルール数・宣言数の上限は各パーサーと #261 が確保前に検証する。

use crate::selector::SelectorList;

/// CSS 詳細度（a = ID 数、b = クラス・属性・疑似クラス数、c = 型・疑似要素数）。
///
/// #257 が [`SelectorList`] から計算し、#259・#260 がカスケードの比較に使う。
/// 1 ルールは複雑セレクタごとに別の詳細度を持つため、[`StyleRule`] のフィールドにはしない。
/// 各成分が `u32` で足りるのは、セレクタの構成要素数に上限があるため。
/// 加算などの演算は定義しない（#257 で必要なら panic しない `saturating_*` / `checked_*` で追加する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct Specificity {
    // 派生 `Ord` はフィールド宣言順の辞書式比較になり、この順が仕様上の比較順そのもの。
    // 並べ替えてはならない。
    ids: u32,
    classes: u32,
    types: u32,
}

impl Specificity {
    /// すべて 0 の詳細度（`Default` と同じ値）。
    pub const ZERO: Self = Self {
        ids: 0,
        classes: 0,
        types: 0,
    };

    /// 各成分から詳細度を作る。
    pub const fn new(ids: u32, classes: u32, types: u32) -> Self {
        Self {
            ids,
            classes,
            types,
        }
    }

    /// ID セレクタ数（a）。
    pub const fn ids(&self) -> u32 {
        self.ids
    }

    /// クラス・属性・疑似クラスの数（b）。
    pub const fn classes(&self) -> u32 {
        self.classes
    }

    /// 型セレクタ・疑似要素の数（c）。
    pub const fn types(&self) -> u32 {
        self.types
    }
}

/// 宣言の重要度。
///
/// bool ではなく拡張できる enum にしている（REPAIR-4）。**値を持つだけで、カスケードでは
/// まだ使っていない**（#260 の範囲は詳細度・ソース順・inline 優先まで。`!important` を
/// 反映するかは後続で決める。REPAIR-3）。後から [`Declaration::new`] の引数を増やすと
/// 破壊的変更になるため、先に入れている。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Importance {
    /// 通常の宣言。
    #[default]
    Normal,
    /// `!important` 付きの宣言。
    Important,
}

/// 1 つの CSS 宣言 `property: value [!important]`。
///
/// style 属性とルール内の宣言の双方で使う。#256 が生成し、TASK-100（`PLUG-8`）は
/// property 名で gating する。値の型付き解釈（長さ・色など）は未実装で、値は文字列のまま
/// 持つ（REPAIR-3。`CORE-5`・TASK-105）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    property: String,
    value: String,
    importance: Importance,
}

impl Declaration {
    /// 宣言を作る。
    ///
    /// property 名は正規化する: `--` で始まらない名前は ASCII 小文字にし
    /// （`COLOR` → `color`）、`--` で始まるカスタムプロパティは大文字小文字を区別するため
    /// そのまま残す。ASCII 以外の文字は変えない。value は渡された文字列をそのまま持つ
    /// （前後の空白除去・`!important` の切り出し・構文の妥当性は #256 の責務）。
    pub fn new(
        property: impl Into<String>,
        value: impl Into<String>,
        importance: Importance,
    ) -> Self {
        let mut property = property.into();
        if !property.starts_with("--") {
            property.make_ascii_lowercase();
        }
        Self {
            property,
            value: value.into(),
            importance,
        }
    }

    /// 正規化済みの property 名。
    pub fn property(&self) -> &str {
        &self.property
    }

    /// 値（文字列のまま）。
    pub fn value(&self) -> &str {
        &self.value
    }

    /// 重要度。
    pub fn importance(&self) -> Importance {
        self.importance
    }
}

/// 1 つのスタイルルール `selectors { declarations }`。
///
/// #551 / #552 が組み立て、#259 がマッチングに、#260 がカスケードに使う。セレクタは
/// 既存の [`SelectorList`] をそのまま使う（#263・#257 の方針）。複雑セレクタごとの詳細度は
/// 持たず、#257 / #259 が AST から計算する（キャッシュは private フィールドなので後から
/// 非破壊で追加できる）。ソース順は [`Stylesheet::rules`] 内の位置と
/// [`StyleRule::declarations`] 内の位置で表す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleRule {
    selectors: SelectorList,
    declarations: Vec<Declaration>,
}

impl StyleRule {
    /// セレクタと宣言列からルールを作る。
    pub fn new(selectors: SelectorList, declarations: Vec<Declaration>) -> Self {
        Self {
            selectors,
            declarations,
        }
    }

    /// セレクタリスト。
    pub fn selectors(&self) -> &SelectorList {
        &self.selectors
    }

    /// 宣言列（ソース順）。
    pub fn declarations(&self) -> &[Declaration] {
        &self.declarations
    }
}

/// 1 つのスタイル源（`<style>` 要素・外部スタイルシート文字列）から作ったルールの並び。
///
/// #552 が組み立て、#259 / #260 が複数を順に扱う。起源の区別（UA・user・author）・
/// media 条件・ルール単位のエラー記録（#258）は未実装（REPAIR-3。private フィールドなので
/// 後から非破壊で追加できる）。inline の `style` 属性は Stylesheet として表さない
/// （表し方は #552 / #260 が決める）。上限検証は型ではせず、各パーサーと #261 が行う。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Stylesheet {
    rules: Vec<StyleRule>,
}

impl Stylesheet {
    /// ルール列（ソース順）からスタイルシートを作る。
    pub fn new(rules: Vec<StyleRule>) -> Self {
        Self { rules }
    }

    /// ルール列（ソース順）。
    pub fn rules(&self) -> &[StyleRule] {
        &self.rules
    }

    /// ルール数。
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// ルールが 0 件か。
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selector::parse_selector_list;

    fn rule(sel: &str, props: &[&str]) -> StyleRule {
        let decls = props
            .iter()
            .map(|p| Declaration::new(*p, "x", Importance::Normal))
            .collect();
        StyleRule::new(parse_selector_list(sel).unwrap(), decls)
    }

    /// CORE-5: 詳細度は ids → classes → types の辞書式順。
    #[test]
    fn core_5_specificity_orders_lexicographically() {
        assert!(Specificity::new(0, 1, 0) > Specificity::new(0, 0, 255));
        assert!(Specificity::new(1, 0, 0) > Specificity::new(0, 99, 99));
        assert!(Specificity::new(0, 2, 1) > Specificity::new(0, 2, 0));
        assert_eq!(Specificity::new(1, 2, 3), Specificity::new(1, 2, 3));
        let mut v = vec![
            Specificity::new(1, 0, 0),
            Specificity::new(0, 0, 1),
            Specificity::new(0, 1, 0),
        ];
        v.sort();
        assert_eq!(
            v,
            vec![
                Specificity::new(0, 0, 1),
                Specificity::new(0, 1, 0),
                Specificity::new(1, 0, 0)
            ]
        );
    }

    /// CORE-5: 既定値は ZERO。
    #[test]
    fn core_5_specificity_default_is_zero() {
        assert_eq!(Specificity::default(), Specificity::ZERO);
        assert_eq!(Specificity::ZERO.ids(), 0);
        assert_eq!(Specificity::ZERO.classes(), 0);
        assert_eq!(Specificity::ZERO.types(), 0);
    }

    /// CORE-5: アクセサは各成分を返す。
    #[test]
    fn core_5_specificity_accessors_return_components() {
        let s = Specificity::new(1, 2, 3);
        assert_eq!((s.ids(), s.classes(), s.types()), (1, 2, 3));
    }

    /// CORE-5: 標準 property 名は小文字化し、値は変えない。
    #[test]
    fn core_5_declaration_lowercases_standard_property() {
        let d = Declaration::new("COLOR", "Red", Importance::Normal);
        assert_eq!(d.property(), "color");
        assert_eq!(d.value(), "Red");
    }

    /// CORE-5: カスタムプロパティは大文字小文字を保つ。
    #[test]
    fn core_5_declaration_preserves_custom_property_case() {
        let d = Declaration::new("--Main-Color", "#FFF", Importance::Normal);
        assert_eq!(d.property(), "--Main-Color");
    }

    /// CORE-5: 重要度を保持し、既定は Normal。
    #[test]
    fn core_5_declaration_keeps_importance() {
        let d = Declaration::new("color", "red", Importance::Important);
        assert_eq!(d.importance(), Importance::Important);
        assert_eq!(Importance::default(), Importance::Normal);
    }

    /// CORE-5: ルールはセレクタと宣言の順序を保つ。
    #[test]
    fn core_5_style_rule_holds_selectors_and_declarations_in_order() {
        let r = rule("div.a, #b", &["color", "margin"]);
        assert_eq!(r.selectors().selectors().len(), 2);
        let names: Vec<&str> = r.declarations().iter().map(|d| d.property()).collect();
        assert_eq!(names, vec!["color", "margin"]);
    }

    /// CORE-5: スタイルシートはルール順を保つ。
    #[test]
    fn core_5_stylesheet_preserves_rule_order() {
        let sheet = Stylesheet::new(vec![rule("p", &["color"]), rule("a", &["margin"])]);
        assert_eq!(sheet.len(), 2);
        assert!(!sheet.is_empty());
        assert_eq!(sheet.rules()[0].declarations()[0].property(), "color");
        assert_eq!(sheet.rules()[1].declarations()[0].property(), "margin");
        assert!(Stylesheet::default().is_empty());
    }
}
