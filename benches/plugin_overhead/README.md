# plugin_overhead: MCP プラグインの性能計測

TASK-95.1（Issue #375・`PLUG-3`・`MS-9`）。MCP 参照プラグイン `fandhe-browser-mcp` の
アイドル RSS 増分を計測する。cold start（TASK-95.2）・レイテンシ（TASK-95.3）は未実装で、
`--metric` として追加する。

## 使い方

```bash
make measure-plugin-overhead                                  # mcp を release ビルドして計測
make measure-plugin-overhead PLUGIN_OVERHEAD_ARGS="--samples 5"
make check-plugin-overhead                                    # 実バイナリでの e2e smoke
```

引数: `--metric idle-rss`（既定・複数指定可）、`--samples 1..=20`（既定 3）、
`--settle-ms 0..=30000`（既定 800）、`--interval-ms 0..=5000`（既定 200）。

要件: unix（Linux・macOS）、`ps(1)`、ネットワーク不使用（loopback のみ）。

## 計測定義

| 項目 | 内容 |
| ---- | ---- |
| ホスト | ベンチプロセス自身（一時プロファイル上の `AppState` を本物の ai ルータで loopback 公開） |
| (a) 本体単体 | mcp 起動前のベンチ自身を根とするプロセスツリーの RSS 合計 |
| (b) 本体＋プラグイン | mcp 接続後（登録・initialize 済みで stdin 待ち。ツール未呼び出し）の同じ木の RSS 合計 |
| 採取 | 静置後に一定間隔で N 回、`ps` の表 1 枚から合算し、中央値を取る（PoC-15 と同じ手法） |
| 増分 | 中央値(b) − 中央値(a)。符号付きで保持し、負でも丸めない |
| 単位 | `ps` の `rss`（KiB）。MB = KiB ÷ 1024（PoC-15・`PLUG3_TARGETS` と同じ換算） |
| 判定 | しない。目標（`PLUG-3`: 10MB 以内）は参考値として出力するのみ。達成判定は #379（人間担当） |

## PoC-15 との差

- ホストは独立プロセスの `fandhe-browser` ではなく in-process。cli が引数（ポート・プロファイル）を
  持ったら実バイナリへ切り替える（REPAIR-3。TASK-47・`CLI-1`）
- (b) にはベンチ側クライアント（読み書きスレッド・パイプ）の増分が乗るため、増分はやや大きめ（保守的）に出る。
  内訳としてプラグイン単体の値（`plugin_kib`）を併記する
- (a) の前に採取を 1 回捨て打ちし、採取機構自身の確保分を基準へ含める

## 終了コード

計測できれば値に関係なく 0。バイナリ不在・登録／ハンドシェイク失敗・採取失敗・引数不正・非 unix は非 0。
