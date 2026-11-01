# egress-isolation（Servo ホストの egress 隔離の実測。Linux・macOS・Windows）

Issue #1377（Linux）・#1387（macOS・Windows）・TASK-38・`RENDER-3`・`SEC-2`・`PLUG-1`・`XOS-1`。親判断 #1003 の比較設計
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

# docker-none の準備（glibc をそろえたホストを Debian trixie のイメージ内でビルドして取り出す。#1386）
bash harness/egress-isolation/container/prepare.sh build fandhe-egress-isolation:local
bash harness/egress-isolation/container/prepare.sh extract fandhe-egress-isolation:local "$(mktemp -d)"
# 続けて、表示された取り出し先のバイナリを --host に渡す:
#   measure.py --host <取り出したバイナリ> --modes baseline,bwrap-netns,docker-none \
#     --docker-image fandhe-egress-isolation:local --out "$(mktemp -d)"

# Landlock ABI・netns の作り方ごとの可否・sysctl（読み取りのみ）
python3 -I -B harness/egress-isolation/platform_probe.py [--docker-image <image>]
```

`--out` に `result.json`・`summary.md`・モードごとの内側結果が出る。`make ci` と CI には含めない
（実測は人間の判断材料。後続の候補: `harness-ubuntu` への自己テストのステップ追加）。

### docker-none で glibc をそろえる理由

measure.py は全モードで同一バイナリ（sha256 一致。`binary_match`）を要求する。ホストの新しい glibc でビルドした
バイナリをイメージ（Debian trixie・glibc 2.41）へ COPY すると `GLIBC_2.43` 等を要求して起動できない
（#1377 の再実測 run 2）。そこで `container/Dockerfile` が trixie 内でホストをビルドし、`prepare.sh extract` が
同じファイルを取り出して全モードの `--host` に使う（glibc 2.41 でリンクしたバイナリは新しいホストでも動く）。
イメージは push・配布しない（MPL-2.0 部分を含む）。ベースイメージのダイジェストは
`harness/render-screenshot/container/Dockerfile` と同値で、`test_container.py` がドリフトを検査する。

## 安全上の規則

- 宛先は 127.0.0.0/8 のダミーだけ。実在の内部アドレスへの connect / fetch は、隔離下の内側で `lo` 以外の
  インターフェース・経路が無いことを確かめた後にだけ行う（満たさなければ exit 2 で中止し何も読み込まない）。
- argv は定数リスト・`shell=False`。`--host` は実行可能ファイルの絶対パス、`--docker-image` は正規表現で検証。
- ログ・結果に URL のクエリ・本文・プロキシ系環境変数の値を載せない（有無だけ）。
- ホストへ渡す環境変数は `servo_bbox_driver.build_host_env` の許可リストのみ（`SEC-2`）。UA・ヘッダには触らない。

## 限界

- `dns` / `udp_direct` はハーネス自身のプローブで、Servo 自身の DNS・UDP 送信は外から観測していない
  （root か CAP_NET_ADMIN が要る。承認事項）。
- 送信先の監視は `monitor.py`（`egress_guard` の走査関数を流用し、ハーネスが意図的に接続させるダミー宛先を除外して継続する）。接続済みソケットを 25 ms 間隔で走査するため短命な接続は拾えず、経路ごとに走査が 1 回以上完了していなければ合格にしない。補助の観測手段。
- `dns` は `/etc/resolv.conf` の実体位置に固定の非ループバック nameserver（192.0.2.53）を重ね、その宛先への UDP connect が経路なしで失敗したときだけ試行を観測した扱いにする（`/run` の隠蔽による resolv.conf 欠落と netns 由来の失敗を区別する。区別できなければ unobservable）。docker-none は resolv.conf を差し替えないため、nameserver が得られなければ unobservable になる。
- docker-none の不採用案: render-screenshot のイメージの流用（Chromium の導入で遅く無関係な依存が入る）、
  測定機の OS にそろえたベースイメージへのローカルビルドの COPY（配布コンテナと同じ glibc という目的から外れる）。
- AppArmor の `apparmor_restrict_unprivileged_userns=1` の環境では `unshare -rn` が使えず、bwrap（setuid 相当の
  プロファイル）経由でのみ netns を作れる。ns 内に CAP_NET_ADMIN は無く、veth は作れない。
- 内側の `/etc/hosts` は bind マウント（docker は `--add-host`）。Servo が参照することはベースラインの受信で確認する。

## macOS・Windows（#1387）

`xos-measure.yml` の suite `egress-isolation`（`gh workflow run xos-measure.yml -f suite=egress-isolation`。
macOS・Windows の GitHub ホステッドランナー。結果は #1003 に記録する）か、手元の実機で `measure.py` を実行する。
判定語彙（`pass` / `fail` / `unobservable`）と fail-closed は Linux と同じ。OS ごとのモードは `judge.PLATFORM_MODES`。

| OS | モード | 内容 |
| -- | ------ | ---- |
| macOS | `baseline` / `seatbelt` | baseline は `sandbox-exec -p "(version 1)(allow default)"`（隔離と無関係な失敗の切り分け）。`seatbelt` は `(deny network-outbound (remote ip))` ＋ページ配信用 loopback 1 ポートの許可（`seatbelt.py`） |
| Windows | `baseline` / `appcontainer` | baseline は直接起動。`appcontainer` は**未実装**（起動に `CreateProcessW` の `SECURITY_CAPABILITIES` を ctypes で呼ぶ FFI が要り、設計 §7 の承認事項。全経路を理由つきの `unobservable` で出す） |

観測手段が Linux と異なる点（`method` 列にも出る）:

- プロセス単位の監視（`egress_guard`）が無い。pass の条件から外す代わりに、外側 listener の受信 0 件・baseline の到達・
  試行の観測・`--selftest` と loopback の `load` の成功をすべて要する。
- 試行の観測（#1395）。macOS・Windows の TCP 経路の試行は次の順で根拠にする。いずれも ip:port と件数だけを記録する。
  - 失敗経路のビーコン（主。img・fetch・ws・internal）: 同じページが、許可外宛先への要求が失敗したとき
    （`onerror` / `catch`）だけ許可オリジンへ `/beacon/<route>` を送る。ブラウザが要求を発行して失敗した後にしか走らず、
    外側 0 件・baseline の到達と合わせて「Servo の事前ブロックではなく遮断された」ことを推論する（直接の観測ではない。
    結果の `beacons`・`attempt_evidence`）。Linux のページは変えない。
  - サンドボックス拒否ログ（副。`log show`）: 取りこぼしの原因を切り分けるため、診断カウンタ（読んだ行・JSON 行・拒否行・
    宛先を取れた行・pid の帰属で除外した行・帰属した行。`deny_log.diagnostics`）を記録する。内側ランナー自身の UDP
    プローブが拒否されたのに拒否ログに無いときは自己検査が不成立（`deny_log.available=false`）となり、拒否ログを根拠にしない。
  - `redirect`（トップレベルの 302）はページの JS が走らずビーコンを付けられない。根拠は拒否ログだけで、得られなければ
    `unobservable`（理由は結果の `reason`）。redirect にはビーコンを付けない（補足用 fetch が同じ宛先へ接続し拒否ログ・件数に混ざるため）。
  - `udp_direct` は `sendto` が拒否された errno。
  - Servo ホスト側の接続試行ログ（`egress_guard` 相当）は採らない。`egress_guard` は `/proc/net` を走査する Linux 専用で、
    macOS では connect が syscall で拒否されソケットが現れない。`servo-host/src` に接続試行のログも無く、追加には Servo の
    net スタックの改変か servo-host の改修が要る。
  - 後続の候補（今回は入れない）: SBPL の `(with message ...)` 等の修飾子、`log stream` の並行収集、workflow の sudo による `log show`。
- 共有 loopback で内側の写し listener を立てられない。hosts も変えないため、ページは IP リテラル（127.0.0.x）で配る。
- `internal_real` / `dns` は netns 前提の構造検査で、Seatbelt・AppContainer では成り立たないため `unobservable`
  （実在の内部アドレスにも向けない）。
- macOS は lo0 に 127.0.0.2 / 127.0.0.3 の alias が要る（`sudo ifconfig lo0 alias 127.0.0.2 up`）。ハーネスは sudo を
  使わず、bind できなければ exit 2 で案内する。workflow は使い捨て runner で固定 argv の alias 追加を行う。
- Windows の runner は管理者で動くため、「非管理者で loopback 例外を作れるか」には答えられない（設計 §6.2）。

## 承認事項（この issue では実装しない）

Landlock ルールの適用（`landlock` クレートまたは `unsafe`）・seccomp-bpf・許可リスト付きプロキシと netns 内への中継・
Servo 自身の DNS/UDP の外部観測・Windows の AppContainer ランチャー（ctypes による FFI）・`CheckNetIsolation LoopbackExempt` の設定変更・古いカーネルでの再測定。
