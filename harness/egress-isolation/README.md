# egress-isolation（Servo ホストの egress 隔離の Linux 実測）

Issue #1377・TASK-38・`RENDER-3`・`SEC-2`・`PLUG-1`・`XOS-1`。親判断 #1003 の比較設計
`docs/design/servo-host-egress-isolation.md` §6（実測計画）のうち、**依存・`unsafe`・pip を使わずに測れる
Linux の範囲**を測るハーネス。**方式の採否は決めない**。結果は測定条件つきの事実として記録する。

## 何を測るか

許可外の宛先を loopback のダミー listener（127.0.0.2 / 127.0.0.3）に置き、Servo ホストに次の経路を試させる。

| 経路 | 内容 | 観測手段 |
| ---- | ---- | -------- |
| `redirect` | 許可外ホストへの 302 | 外側 listener の受信・内側 listener の受信 |
| `img` / `fetch` | 許可外ホストのサブリソース | 同上 |
| `ws` | `new WebSocket()` | 同上 |
| `internal` | 内部アドレスの代替宛先（127.0.0.3） | 同上 |
| `udp_direct` | 内側ランナー自身の UDP `sendto`（Servo ではなくハーネスのプローブ） | 外側 UDP listener の受信 |
| `internal_real` | 実在の内部アドレス（隔離下の内側だけで試す） | 経路表＋connect だけのプローブ（外部からは観測しない） |
| `dns` | hosts に無い名前の解決（ハーネス自身の `getaddrinfo`） | 経路表＋解決失敗（Servo 自身の DNS・UDP 送信は外から観測しない） |

モード: `baseline`（bwrap。`--unshare-net` だけ無し。陽性対照）・`bwrap-netns`（`--unshare-net`）・
`docker-none`（`docker run --network none`。イメージがあるときだけ）。

判定は `pass` / `fail` / `unobservable`（`judge.py`）。外側の受信が 1 件でもあれば `fail`。`pass` は「外側 0 件・
ベースラインで届いた・内側で試行を観測・guard が監視できた・`--selftest` と loopback の `load` が成功」の全部を要し、
欠ければ `unobservable`（合格とは扱わない。fail-closed）。

## 使い方

```bash
# 自己テスト（偽ホストのみ。bwrap・docker 不要）
make check-egress-isolation

# 実測（要 bwrap。docker-none は --docker-image を足す）
python3 -I -B harness/egress-isolation/measure.py \
  --host /abs/path/to/fandhe-browser-servo-host \
  --modes baseline,bwrap-netns [,docker-none --docker-image <image>] \
  --out "$(mktemp -d)"

# Landlock ABI・netns の作り方ごとの可否・sysctl（読み取りのみ）
python3 -I -B harness/egress-isolation/platform_probe.py [--docker-image <image>]
```

`--out` に `result.json`・`summary.md`・モードごとの内側結果が出る。`make ci` と CI には含めない
（実測は人間の判断材料。後続の候補: `harness-ubuntu` への自己テストのステップ追加）。

## 安全上の規則

- 宛先は 127.0.0.0/8 のダミーだけ。実在の内部アドレスへの connect / fetch は、隔離下の内側で `lo` 以外の
  インターフェース・経路が無いことを確かめた後にだけ行う（満たさなければ exit 2 で中止し何も読み込まない）。
- argv は定数リスト・`shell=False`。`--host` は実行可能ファイルの絶対パス、`--docker-image` は正規表現で検証。
- ログ・結果に URL のクエリ・本文・プロキシ系環境変数の値を載せない（有無だけ）。
- ホストへ渡す環境変数は `servo_bbox_driver.build_host_env` の許可リストのみ（`SEC-2`）。UA・ヘッダには触らない。

## 限界

- `dns` / `udp_direct` はハーネス自身のプローブで、Servo 自身の DNS・UDP 送信は外から観測していない
  （root か CAP_NET_ADMIN が要る。承認事項）。
- `egress_guard` は接続済みソケットを 25 ms 間隔で走査するため、短命な接続は拾えない。補助の観測手段。
- AppArmor の `apparmor_restrict_unprivileged_userns=1` の環境では `unshare -rn` が使えず、bwrap（setuid 相当の
  プロファイル）経由でのみ netns を作れる。ns 内に CAP_NET_ADMIN は無く、veth は作れない。
- 内側の `/etc/hosts` は bind マウント（docker は `--add-host`）。Servo が参照することはベースラインの受信で確認する。

## 承認事項（この issue では実装しない）

Landlock ルールの適用（`landlock` クレートまたは `unsafe`）・seccomp-bpf・許可リスト付きプロキシと netns 内への中継・
Servo 自身の DNS/UDP の外部観測・`xos-measure.yml` への suite 追加・macOS / Windows・古いカーネルでの再測定。
