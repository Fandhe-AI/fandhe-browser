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
        format!("{} \"{}\"", one_line(&c.role), one_line(&c.name))
    };
    format!("{label}={}{}", c.r#ref, state_suffix(&c.state))
}

fn render_node(node: &Node, depth: usize, lines: &mut Vec<String>) {
    let indent = "  ".repeat(depth);
    let mut line = format!("{indent}- {}", node.role);
    if !node.name.is_empty() {
        line.push_str(&format!(" \"{}\"", one_line(&node.name)));
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
            .map(|h| format!("{} [{}]", one_line(&h.name), h.r#ref))
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
    // 畳んだ行と children を FoldedRow::index で統合し文書順を復元する。
    // children 側の表示対象データ行は、畳んだ行が使わない index を昇順で占める契約
    // （`AISNAP-12`）。index を持たない非データ行の child が混じる場合の位置は近似になる。
    let mut folded: Vec<&FoldedRow> = node.folded_rows.iter().collect();
    folded.sort_by_key(|f| f.index);
    let mut folded = folded.into_iter().peekable();
    let mut children = node.children.iter().peekable();
    let mut pos = 0usize;
    loop {
        if folded.peek().is_some_and(|f| f.index <= pos) || children.peek().is_none() {
            let Some(f) = folded.next() else { break };
            let mut l = format!("{sub}{}", one_line(&f.text));
            if f.truncated {
                l.push_str(" …(text)");
            }
            lines.push(l);
        } else if let Some(child) = children.next() {
            render_node(child, depth + 1, lines);
        }
        pos += 1;
    }
    for child in children {
        render_node(child, depth + 1, lines);
    }
}
