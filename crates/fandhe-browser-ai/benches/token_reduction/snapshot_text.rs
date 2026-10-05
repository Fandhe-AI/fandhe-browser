//! `Snapshot` を行形式のテキストへ直列化する測定用シリアライザ
//! （TASK-14.3・`AISNAP-1`・Issue #94・`MS-2`）。
//!
//! 役割: TASK-14 の削減率測定で、snapshot（方式 B）のトークン量を数えるための
//! テキストを作る。`Snapshot` には文字列化の手段がないため、ベンチ側に閉じた
//! 決定的な行形式を持つ。形式は PoC-4 `reduce.mjs` の `toAccessibilityTree` が出す
//! `tree.text` に倣う。呼び出し元は `reduction.rs`（計測）とユニットテスト。
//!
//! 暫定: これは測定用の暫定形式であり、`/ai/snapshot` の確定応答形式ではない
//! （REPAIR-3）。確定スキーマは TASK-19・`AISNAP-6` で決まり、その時点で差し替える。
//!
//! 方針:
//! - `Snapshot` が保持する情報（role・name・ref・state・children・table・
//!   folded_rows・各 truncated）を落とさず出す。目標値に合わせて削らない。
//! - `data_leaf` は分類用メタデータでエージェントへ渡す本文ではないため出さない。
//! - 再帰の深さは `MAX_TREE_DEPTH`（256）で有界。name は構築側で空白正規化済みの
//!   前提だが、改行・タブが残っていても行形式を壊さないよう空白 1 個へ置換する。

use fandhe_browser_ai::snapshot::{CheckedState, FoldedRow, Node, RowControl, Snapshot, State};

/// `snapshot` を行形式テキストへ直列化する（行は `\n` 連結・末尾改行なし）。
pub fn render_snapshot(snapshot: &Snapshot) -> String {
    let mut lines = Vec::new();
    render_node(&snapshot.tree, 0, &mut lines);
    if snapshot.truncated {
        lines.push("… truncated".to_string());
    }
    lines.join("\n")
}

fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// name を `"` で囲んで出す。値中の `\` と `"` をエスケープし、境界を曖昧にしない。
fn quoted(s: &str) -> String {
    let mut out = String::from("\"");
    for c in one_line(s).chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

fn state_suffix(state: &State) -> String {
    let mut out = String::new();
    if state.disabled {
        out.push_str(" (disabled)");
    }
    match state.checked {
        None => {}
        Some(CheckedState::Checked) => out.push_str(" (checked)"),
        Some(CheckedState::Unchecked) => out.push_str(" (unchecked)"),
        // 将来追加される状態を黙って落とさない。
        Some(other) => out.push_str(&format!(" (checked:{other:?})")),
    }
    out
}

fn render_control(c: &RowControl) -> String {
    // 同名の link と button を区別できるよう、名前の有無にかかわらず role を出す。
    let label = if c.name.is_empty() {
        one_line(&c.role)
    } else {
        format!("{} {}", one_line(&c.role), quoted(&c.name))
    };
    format!("{label}={}{}", c.r#ref, state_suffix(&c.state))
}

fn render_node(node: &Node, depth: usize, lines: &mut Vec<String>) {
    render_head(node, depth, lines);
    let mut merge = Merge::new(node);
    render_children(&node.children, depth + 1, &mut merge, lines);
    merge.flush(&"  ".repeat(depth + 1), lines);
}

/// ノード自身の行と、圧縮表（`table`）の行までを出す（子ノードと畳んだ行は含まない）。
fn render_head(node: &Node, depth: usize, lines: &mut Vec<String>) {
    let indent = "  ".repeat(depth);
    let mut line = format!("{indent}- {}", node.role);
    if !node.name.is_empty() {
        line.push_str(&format!(" {}", quoted(&node.name)));
    }
    if let Some(r) = &node.r#ref {
        line.push_str(&format!(" [{r}]"));
    }
    line.push_str(&state_suffix(&node.state));
    lines.push(line);

    let sub = "  ".repeat(depth + 1);
    if let Some(table) = &node.table {
        let header: Vec<String> = table
            .header
            .iter()
            .map(|h| format!("{} {} [{}]", one_line(&h.role), quoted(&h.name), h.r#ref))
            .collect();
        lines.push(format!("{sub}header: {}", header.join(" | ")));
        for row in &table.rows {
            let mut l = format!("{sub}{}", one_line(&row.text));
            if !row.controls.is_empty() {
                let cs: Vec<String> = row.controls.iter().map(render_control).collect();
                l.push_str(&format!(" {{{}}}", cs.join(", ")));
            }
            // 切り詰め理由を区別する（コントロール列の打ち切りと行テキストの切り詰め）。
            if row.controls_truncated {
                l.push_str(" …(controls)");
            }
            if row.truncated {
                l.push_str(" …(text)");
            }
            lines.push(l);
        }
        if table.truncated_rows > 0 {
            lines.push(format!("{sub}… +{} rows", table.truncated_rows));
        }
    }
}

/// 畳んだ行（`Node::folded_rows`）と展開側の children を `FoldedRow::index` で統合する
/// 状態（`AISNAP-12`）。
///
/// `index` は畳んだ行・展開側を合わせた表示対象データ行の通し位置なので、位置カウンタ
/// `pos` は 1 本で共有し、畳んだ行を出しても展開側のデータ行を出しても進める。
/// ヘッダ行・caption 等の非データ child は数えず元の位置のまま出す。表の行は
/// `rowgroup`（`thead`/`tbody`/`tfoot`）配下に入るため、畳んだ行を持たない `rowgroup` は
/// 透過して同じ状態で走査する。
///
/// 制限: 位置カウンタは `is_data_row` の近似に依存する（詳細は同関数のコメント）。
struct Merge<'a> {
    folded: std::iter::Peekable<std::vec::IntoIter<&'a FoldedRow>>,
    pos: usize,
    /// 走査済みの展開側データ行数と、走査対象全体のデータ行数。
    seen: usize,
    total: usize,
}

impl<'a> Merge<'a> {
    fn new(node: &'a Node) -> Self {
        let mut folded: Vec<&FoldedRow> = node.folded_rows.iter().collect();
        folded.sort_by_key(|f| f.index);
        Self {
            folded: folded.into_iter().peekable(),
            pos: 0,
            seen: 0,
            total: count_data_rows(&node.children),
        }
    }

    /// 位置が `pos` 以下の畳んだ行を順に出す。
    fn flush_due(&mut self, sub: &str, lines: &mut Vec<String>) {
        while let Some(f) = self.folded.next_if(|f| f.index <= self.pos) {
            lines.push(render_folded(sub, f));
            self.pos += 1;
        }
    }

    /// 残りの畳んだ行をすべて出す（展開側のデータ行より後ろにある行）。
    fn flush(&mut self, sub: &str, lines: &mut Vec<String>) {
        for f in self.folded.by_ref() {
            lines.push(render_folded(sub, f));
        }
    }
}

/// 畳んだ行を持たない `rowgroup` か（畳んだ行の統合で透過して走査する対象）。
fn is_transparent_group(node: &Node) -> bool {
    node.role == "rowgroup" && node.folded_rows.is_empty() && node.table.is_none()
}

fn count_data_rows(children: &[Node]) -> usize {
    children
        .iter()
        .map(|c| {
            if is_transparent_group(c) {
                count_data_rows(&c.children)
            } else {
                usize::from(is_data_row(c))
            }
        })
        .sum()
}

fn render_children(children: &[Node], depth: usize, m: &mut Merge<'_>, lines: &mut Vec<String>) {
    let sub = "  ".repeat(depth);
    let last_group = children.iter().rposition(is_transparent_group);
    for (i, child) in children.iter().enumerate() {
        if is_transparent_group(child) {
            render_head(child, depth, lines);
            let before = m.seen;
            render_children(&child.children, depth + 1, m, lines);
            // 最後のデータ行を含むグループの末尾で、残りの畳んだ行を同じ階層へ出す。
            // データ行を持たないグループ（thead 等）では出さない（全行が畳まれた表でも
            // 本文行がヘッダ直後に出ないよう、その場合は呼び出し元の末尾で出す）。
            // 展開側のデータ行が 1 件もない表（全行が畳まれた表）は、最後のグループ
            // （tbody 相当）の中へ出す。
            let is_body = m.seen > before || (m.total == 0 && last_group == Some(i));
            if is_body && m.seen >= m.total {
                m.flush(&"  ".repeat(depth + 1), lines);
            }
            continue;
        }
        if is_data_row(child) {
            m.flush_due(&sub, lines);
            m.seen += 1;
            m.pos += 1;
        }
        render_node(child, depth, lines);
    }
}

fn render_folded(sub: &str, f: &FoldedRow) -> String {
    let mut l = format!("{sub}{}", one_line(&f.text));
    if f.truncated {
        l.push_str(" …(text)");
    }
    l
}

/// 表の本文行・一覧項目に当たる child か（`FoldedRow::index` の数え方に合わせる近似判定）。
///
/// ヘッダ行（全セルが `columnheader`）は除く。
///
/// 制限（受容済みの近似）: `Node` の構造からは行が thead/tbody/tfoot のどれに属するか
/// 判別できないため、`tfoot` 行や th のみの本文行も本文行として数える。tfoot が tbody より
/// 前にある表では、位置カウンタ（`Merge::pos`）がずれて折り畳んだ行の出力順が近似になる。
/// トークン数への影響は軽微。出力形式は TASK-19（`AISNAP-6`）で確定予定で、その際に
/// build 側が位置情報を出す形へ見直す（`FoldedRow` の形式変更を伴うため破壊的変更）。
fn is_data_row(node: &Node) -> bool {
    match node.role.as_str() {
        "listitem" => true,
        "row" => {
            node.children.is_empty() || !node.children.iter().all(|c| c.role == "columnheader")
        }
        _ => false,
    }
}
