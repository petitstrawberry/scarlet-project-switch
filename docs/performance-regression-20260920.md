# Switch の高いアイドル CPU 使用率：引き継ぎ調査

2026-09-20。対象は `Review Scarlet Switch handoff` の boot 8 から続く
「SD rootfs 化の後、デスクトップと入力が遅く、アイドルでも CPU が高い」問題。
元の会話と `Review AArch64 atomics handoff`、MMC/GPU の記録を読んで調査した。
**持続的な高負荷の主要因を Switchvisor の GDB 未接続処理に特定した。**
同じ RAM 起動で DTR だけを切り替えると平均 CPU 負荷が約 33–36% と約 11% の間を再現して往復した。
修正版のソース・ホストテスト・release payload のビルドまで完了。修正した payload 自体の実機投入はまだ行っていない。

## 制約と実機の現在地

- 16 時過ぎにユーザーが帰宅し、本体操作によって RCM が復旧した。
  **Hekate UMS は引き続き使用禁止。今回も未使用。**
- SD のパーティション、rootfs、通常のブートファイルは今回書き換えていない。
  起動中の OS による通常のログ書き込みは発生している。
- Joy-Con 実装は変更していない。高負荷は観測結果であり、原因とは認定しない。
- ストレージ／描画ベンチマークを自動起動していない。foreground の Ctrl-Z も使っていない。
- Switchvisor 経由で診断版 SD 起動には成功した。その後、RAM 起動への比較用
  `reboot-rcm` と nxboot 転送後に Switchvisor USB が戻らなくなった。
  nxboot は最初成功表示、再試行は device ID 読み出し失敗。APX だけを対象にした
  USB reset は timeout (`-7`)。当時は APX・Switchvisor CDC とも列挙されなくなった。
  USB ハブ全体や他の周辺機器のリセットは行っていない。
- 16:10 の手動復旧後、APX の安定した列挙を確認して Hekate `SWV-NX` を一度転送。
  Switchvisor が復旧し、`ram-nosd-shell` の USB 転送・RAM 起動に成功した。
  その同じ起動中に `/bin/stemd` を手動で開始して通常の RAM サービスも計測した。

既存の `../switchvisor/scripts/run-payload.sh` は APX 出現待ちに加えて 1 秒待つ。
IOKit がインターフェースを利用可能にする前に列挙するための待機であり、再開時は
この既存手順を使用する。今回の失敗原因がこのタイミングだったとは断定していない。

## 実機で取得した結果

証跡は `.cache/performance-handoff-20260920/`。

| 条件 | 結果 | 証跡 |
| --- | --- | --- |
| 引き継いだ boot 8、そのまま | `top` 44.1% busy、95 tasks、CPU 1017600 kHz | `baseline-uart.log` |
| MMC 時間・タスク位置の診断版、SD rootfs | 通常サービス起動後、`top` 43.3% busy | `diagnostic-sd-uart.log` |
| 診断版の最後の 2 snapshot、29.006244 秒 | 継続して存在するタスクの CPU 時間合計は 4 コア換算で約 39.13% | `task-deltas.json` |
| RAM root・SD 無効・シェルのみ | `top` 6.4 / 21.1 / 14.0%、11 tasks。MMC 全カウンタ 0 | `ram-nosd-shell-run1-uart.log` |
| 上記と同一 boot で stemd を手動起動 | `top` 36.4 / 34.7%、34.88 秒平均 36.49%。MMC 全カウンタ 0 | `ram-nosd-run1-analysis.json` |

起動直後の別の `top` は 66.1%。起動直後と安定後を速度改善の比較には使わない。
`top` の一回の値や個別行だけでは変動が大きいため、長い区間も確認した。

### 正常完了した MMC I/O の時間は小さい

最後の 29.006244 秒間の差分：

| カウンタ | 差分 |
| --- | ---: |
| read bytes | 32,768 |
| write bytes | 24,576 |
| 正常完了した要求 | 14 |
| host mutex の獲得待ち | 3,804 ns |
| host を獲得してから正常転送完了まで | 13,560,625 ns（13.56 ms） |

転送時間はこの区間の約 0.047% に相当する経過時間で、CPU 専有時間ではない。
**観測した正常 I/O と host mutex 待ちだけでは、約 39–43% の CPU 負荷を説明できない。**
起動時の実行ファイル読み込みの遅さ、SD 初期化による状態変化、I/O 外の
ファイルシステム／メモリ管理処理は、この結果では除外できない。
診断版は成功時に転送時間を加算するため、エラー経路の時間はこの値に含まれない。

最初の snapshot までの累積 host mutex 待ちは約 10.8 秒、転送時間は約 6.58 秒。
これは複数要求の累積値で、boot 全体の所要時間や純粋な CPU 時間とは異なる。
`diagnostic-sd-io.json` に元の 3 snapshot を保存した。

### IRQ と実行位置

引き継いだ稼働中の実機で 38.66 秒間の IRQ 差分を確認した。
おおよそ IPI 1,331/s、timer 1,835/s、rail UART 210/197/s、DC 1/s、GPU 18/s。
前セッションの値と近い。これだけで IRQ storm を示す値ではないが、
**ハンドラの処理時間は未計測**。

長い区間の CPU 時間をプログラム単位に集約すると、1 コア = 100% の尺度で
SWS 約 64.68%、Scarlet shell 約 39.91%、Joy-Con 約 27.94%。
snapshot の global task/TGID は `top` の namespace PID/TGID と異なる。

- SWS の複数 worker：PC `0x2eab68`、poll (202) の呼び出し元 `0x2eab64`。
- shell の複数 worker：PC `0x77832c`、sleep (20) の呼び出し元 `0x778328`。
- 多くの kernel task：最後に保存された PC は `schedule+0x10f8`。

これらは最後に観測された PC／呼び出し元で、時間分布のプロファイルではない。
sleep/poll 内の実行時間、停止している時間、IRQ・scheduler の費用を分離できない。
この情報だけで「sleep が原因」「Joy-Con が原因」とは言えない。

## 比較するバイナリの確認

準備済み initramfs と、SD に配布した元のオフライン ext2 イメージから抽出した
SWS／shell の SHA-256 は一致した。動いている SD 上のファイルを直接再ハッシュした
結果ではない。

| バイナリ | SHA-256 |
| --- | --- |
| SWS | `2513ad9a10497d0707b01cb819dc6e7c07cfbfe07e4144e2e18e42b3a962270b` |
| Scarlet shell | `f724b1d9c74de34fc516372c63d2e8d2369b9813db77409450a4bcd8204ef3f5` |

initramfs と SD full rootfs はサービス構成が異なる。通常起動同士の CPU 差だけで
ストレージの効果を断定しない。以前の GPU 調査の低負荷値も、別カーネル・別負荷なので
今回の対照群には使えない。

## 用意した 6 条件

`scripts/prepare-performance-isolation.py` は既存のビルド成果物を入力として、
USB で RAM に転送する Switchvisor bundle を作る。ビルドやデバイス操作は行わない。

| variant | root | Scarlet の SD ドライバ | 最初のプログラム | 実機実行 |
| --- | --- | --- | --- | --- |
| `sd-preloaded` | SD p4/ext2 | 有効 | stemd／通常サービス | `SCR-SWV` (GDB 無効) で実行済み |
| `ram-sd` | initramfs | 有効 | stemd／通常サービス | 未実行 |
| `ram-nosd` | initramfs | FDT status=disabled | stemd／通常サービス | 未実行 |
| `sd-preloaded-shell` | SD p4/ext2 | 有効 | `/bin/sh` | 未実行 |
| `ram-sd-shell` | initramfs | 有効 | `/bin/sh` | 未実行 |
| `ram-nosd-shell` | initramfs | FDT status=disabled | `/bin/sh` | 実行済み。その後同一 boot で stemd を手動起動 |

作成済みの保存先は `.cache/performance-handoff-20260920/isolation-v2/`。
6 条件で同じ uImage・initramfs・DT image・UART overlay・BL33 コピーを共有し、
boot script だけ変える。通常の BL33 の `bootcmd` を同じ長さの RAM 内 script 起動に
置換した**コピー**を使用し、SD の U-Boot／設定には書き込まない。
Hekate／ファームウェアまで SD を使わなくなるという意味ではない。

再生成例（Switch プロジェクト直下。出力先は存在しないディレクトリを指定）：

```sh
python3 scripts/prepare-performance-isolation.py \
  .cache/performance-handoff-20260920/diagnostic-sd-bundle \
  .cache/performance-handoff-20260920/isolation-next
```

静的確認済み：legacy image の header/data CRC、転送領域の非重複、展開 DTB
領域との非重複、各ファイルの SHA-256、6 条件の root/SD/init.exec の一致。
凍結した `/init` が `init.exec=` を扱い、`/bin/sh` が含まれることも確認した。
`status=disabled` が platform driver の probe 対象から除かれることはコードで確認した。
**RAM 版の U-Boot script 起動は `ram-nosd-shell` で実機確認済み。**
`ram-nosd` の独立した cold boot とは区別する。手動開始した stemd は PID 24、
元の PID 1 shell は待機したままで、サービス数も SD full rootfs と異なる。

## 当初の比較順序（以下の原因特定により見直し）

USB 未復旧時に立てた比較案。現在は `ram-nosd-shell` と同一起動の DTR 往復比較で
主要因を特定したため、6 条件すべての再起動は行わず通常 SD 起動の復帰確認を優先した。

1. Switchvisor CDC が復旧したら、既存 `run-payload.sh` の APX 待ちを使う。
   UMS に切り替えない。APX すら見えない現在の状態では転送を繰り返さない。
2. まず `ram-nosd-shell` → `ram-sd-shell` → `sd-preloaded-shell`。
   各条件のタグ、root mount、MMC の probe 有無を UART で確認する。
   同程度の起動後時間で `top` と 2 回の `/dev/interrupts` を採る。
3. 次に `ram-nosd` と `ram-sd`。同じ initramfs のため通常サービスも揃う。
   最後に SD の通常サービスを比較し、追加サービスの影響を区別する。
4. SD 無効の RAM 条件でも負荷が高ければ、syscall の on-CPU 時間、IRQ 処理時間、
   scheduler／allocator の時間を測る。block 中の経過時間を CPU 時間に数えない。
   対照なしで polling 周期や Joy-Con を変更しない。

コード確認では Talc allocator の使用、sleep timer の完了／取消時の登録解除、
SD driver に独立した常駐 polling worker がないことを確認した。
page cache の object 処理に全エントリ走査は残るが、今回の持続負荷を説明する
頻度・時間の証拠はないため、原因としての修正は行っていない。

## 診断変更と証跡の保存

一時的に MMC の待ち／転送カウンタと `/dev/interrupts` のタスク snapshot を加えた。
sync-debug／GDB／profiler は有効にしていない。診断版 release build は成功した。

この 2 ファイルへの今回の診断変更は**引き継ぎ時点へ復元済み**。前担当の MMC、
ext2、page cache、IRQ counters、outline atomics の変更は保持した。
診断版の成果物はそのまま保存されているので、RAM 比較に再ビルドは不要。

- `temporary-diagnostics.patch`：今回の診断変更だけ。再適用前に `git apply --check`。
- `kernel__src__drivers__*.diagnostic`：診断版ソース。
- `source-provenance.json`、`scarlet-working-tree.patch`：SHA／tracked 差分。
- `diagnostic-sd-bundle/`：実機で動いた kernel ELF、uImage、initramfs、BL33、hashes。
- `diagnostic-build.log`、`diagnostic-sd-upload.log`、`diagnostic-sd-uart.log`。
- `task-deltas.json`、`diagnostic-sd-io.json`：計測値。
- `ram-nosd-upload.log`：USB device not found で比較転送ができなかった記録。
- `usb-final-state.txt`：APX／Switchvisor が列挙されない最終状態。

診断版 uImage SHA-256：
`d97a473f17f20dc19f77ca7b438a1d4b07a7e38e70c6e6635728da856697a84d`。

## 追記：OWC 経由のポート電源による復旧試行

ユーザーからポート電源操作での復旧を依頼され、14:33 以降に実行した。
過去のこのセッションの USB ログに `APX@02110000` があり、接続位置を
`2-1` (Fresco Logic `1d5c:5801`) の port 1 と特定した。
このハブと OWC 配下のハブは per-port power switching を広告している。
Switch のポートは `0101 power connect []` で、接続検出はあるがデバイスの
列挙ができていない状態だった。Mac に address/enumeration failure の記録もある。

`uhubctl 2.6.0` に libusb 1.0.30 を指定して、対象 hub/port を明示し、
自動的な他ハブへの作用は `-e` で無効にして操作した。

1. `2-1` port 1 を 3 秒 OFF → ON。ハブの状態は `0000 off` → `0100 power`
   → `0101 power connect []`。APX/CDC は現れなかった。
2. USB3 側の `2-2` (`8087:0b40`) port 1 が未使用であることを確認し、
   上記の USB2 port 1 と合わせて 10 秒 OFF → ON。
   USB2/USB3 とも OFF の状態応答と ON への復元を確認したが、列挙は復旧しなかった。
   USB3 port 1 との物理的対応は未確定であり、この試行で VBUS の実測をしたわけではない。
3. `2-1` の VID/PID、bus、port chain をすべて照合し、port 1 に限って
   USB hub class `SET_FEATURE(PORT_RESET)` を一度実行。要求は成功し、
   `0101` → `0103 power enable connect` になったが、APX/CDC は現れなかった。

最後は両ポートとも給電 ON。ハブ全体や別の接続済みポートをリセットしていない。
キーボード、オーディオ、LAN は元の USB registry entry のまま認識されている。
**この時点ではホスト側のポート制御だけで Switch の遠隔接続は復旧しなかった。**
その後 16:10 にユーザーの本体操作で RCM が復旧した。
ハブが返す power 状態と実際の VBUS 遮断、本体の再起動は区別する。
UMS は今回の復旧試行でも未使用。

証跡：`usb-port-recovery-1.log`、`usb-port-recovery-2.log`、
`usb-port-reset.json`、`usb-port-recovery-final.json`。
仕様の確認：[uhubctl の一次資料](https://github.com/mvp/uhubctl)
（USB2/USB3 の二重構成、VBUS 遮断の機種差、macOS 向け libusb の要件）。


## 原因特定：GDB 未接続時の 20 KiB 初期化

`SWV-NX` で起動した実機の status は GDB の診断項目を返した。一方、
作業ディレクトリの Switchvisor `main` は `8ba0e94` で GDB を含まない。
別タスク「着手するSwitchvisor設計」と git の履歴を照合すると、
`feature/debug-port` の `1589e04` に実機の応答と一致する実装がある。
実機上のファームウェアを直接ハッシュした結果ではないが、GDB 機能が
有効な構成であることは CDC 構成と status で確認した。

この実装の `service_gdb` は GDB CDC の DTR が下がっている間、各 USB
service 呼び出しで RX 4096 byte と TX 16384 byte の FIFO を `Fifo::new()`
に置換する。`new()` は配列全体をゼロ初期化する。EL2 はキャッシュ無効で動き、
USB service は全 CPU からのトラップや IRQ で共有 mutex の下で呼ばれる。
そのため、GDB を使わない状態の初期化コストがゲスト側の多くのタスクへ波及する。

### 同一起動で DTR のみを変更した結果

`/dev/cu.usbmodemSWV00016` は GDB CDC (interface 5/6)。loader は CDC ではなく
vendor bulk の interface 4 であり、別物。picocom で GDB ポートを開き、
ローカル操作の DTR toggle だけを実施した。GDB packet は送っていない。
全区間 `gdb=disconnected`、breakpoints=0、stop CPU=255、USB errors=0。
4 CPU、1017600 kHz、SD 無効、同じサービスとバイナリのまま比較した。

| 条件 | 区間 | 4 CPU 換算の平均 busy | SWS / shell / Joy-Con（1 core=100%） |
| --- | ---: | ---: | --- |
| DTR=0、開始時 | 34.885 秒 | 36.49% | 56.92 / 45.90 / 28.26% |
| DTR=1 | 36.370 秒 | 10.84% | 17.65 / 14.51 / 7.05% |
| DTR=0 に戻す | 23.762 秒 | 33.23% | 49.91 / 39.72 / 29.14% |
| 再び DTR=1 | 210.702 秒 | 10.76% | 17.50 / 14.39 / 7.06% |

平均は継続して存在するタスクの CPU 時間差分。idle task 4 個は除外した。
EL2 の時間を直接計測する profiler ではないが、同一 boot の往復比較なので
再起動・SD・実行ファイル・クロックの差による説明にはならない。
長い DTR=1 区間の `top` も 10.5 / 11.1% だった。
各条件の MMC read/write/request/wait/transfer はすべて 0。

証跡：`ram-nosd-gdb-dtr-analysis.json`、`ram-nosd-run1-snapshots.json`、
`analyze-gdb-dtr.py`、`ram-nosd-shell-run1-uart.log`、`gdb-dtr-only.log`。
これは Joy-Con を止めて負荷を隠した結果ではなく、ドライバはすべて元のまま。
RAM 条件でも残る約 11% の全量や、SD 起動時の追加サービスの費用まで
今回の GDB バグだけで説明したという意味ではない。

### 修正と検証

既存の checkout を切り替えず、`1589e04` から独立した作業ツリー
`../switchvisor-perf-gdb` を作った。

- FIFO の実装を `crates/switchvisor/src/usb_fifo.rs` へ移してホストで検証可能にした。
- runtime の disconnect/reset では head/len のみをクリアする。
  初期化時のゼロ配列は保持し、古い bytes は live range に入らない。
- wrap 後の切断、再接続時に古い bytes を返さないこと、満杯の RX/TX キューの
  再利用、空キューの clear が backing storage を書き直さないことをテストした。
- `cargo test --workspace` 成功。追加の storage-preservation regression を含む
  `cargo test -p switchvisor --test usb_fifo` も成功。
- GDB/UART/control 有効・no-fallback の release payload ビルド成功。
  `switchvisor-gdb-clear-fixed/` と build/test log に保存した。

**修正した GDB 有効 payload 自体は SD へまだ配布していない。**
SD の配布記録にある GDB 無効版 `SCR-SWV` を使い、通常の SD rootfs で復帰確認を行った。
`SWV-NX` と `SCR-SWV` を同じファームウェアだと扱わない。


## 通常の SD rootfs への復帰確認

`SWITCHVISOR_HEKATE_ID=SCR-SWV` を明示し、既存 `run-payload.sh` の
APX 待機 + 1 秒待機を使って一度だけ再起動・転送した。RCM → Switchvisor →
診断カーネルの `sd-preloaded` 起動まで成功した。status は GDB 項目のない
`guest-running`, `cpu-mask=0xf`, `usb=up`, `loader=disabled`, `fallback=disabled`。

`/dev/mmcblk0p4` は 1 回目の試行で mount され、14 個の通常サービスが起動した。
測定したカーネルは最初の SD 診断と同じ凍結済み uImage。MMC/ADMA2、
ext2、SWS、shell、Joy-Con のソースはこの対策のために変更していない。

- `top`: 14.0%、13.3% busy、96 tasks。
- 50.824981 秒間の継続タスク平均: **13.39% busy**（4 CPU 換算）。
- 1 core=100% で SWS 23.74%、shell 15.30%、Joy-Con 6.64%、
  touch 2.18%、SKK 1.74%、Mozc 1.73%。
- 同区間の MMC: read 16384 bytes、write 12288 bytes、7 requests、
  mutex wait 1769 ns、正常転送の経過時間 6.742 ms。
- CPU は全区間 1017600 kHz。SD ドライバを使った正常サービス起動のまま、
  約 43% だった高負荷から低下した。アプリ追加等による負荷差は別途扱う。

証跡: `sd-nogdb-upload.log`, `sd-nogdb-uart.log`, `sd-nogdb-status.txt`,
`sd-nogdb-analysis.json`, `sd-nogdb-snapshots.json`。

この時点では GDB 無効・SD rootfs の通常サービス起動まで復帰した。
その後のアプリ起動障害は、下記の追跡調査で別に確認している。
GDB 有効版を使う場合の修正は `../switchvisor-perf-gdb` の作業ツリーと
`switchvisor-gdb-clear.patch` に保存したが、その新 payload の SD 配布は未実施。
同梱の `switchvisor-gdb-clear-validation.json` に対象 commit、ファイル hash、
ビルドした payload の hash と未配布であることを記録した。

再発を避けるため `docs/switchvisor-usb-debug.md` の反復起動例にも
`SWITCHVISOR_HEKATE_ID=SCR-SWV` を追加した。元の省略例は汎用スクリプトの
既定値 `SWV-NX` を使ってしまっていた。


## 追跡：Vellum / Boxcraft が起動しない・画面停止の申告

アイドル負荷の低下だけでは GUI アプリの正常動作は検証できていなかった。
元の停止した起動の UART ログを `app-hang-uart.log` に保存後、同じ凍結
カーネルを GDB 有効の `SWV-NX` で再起動し、通常のアプリ操作を再現した。
この間は GDB CDC の DTR を上げ、上記の未接続時の負荷増大を避けた。

### 確認した障害

1. **Boxcraft の depth rendering が未実装**。
   `gpu-info` は GM20B ready / execution support `0xbf` / depth false。
   `/bin/boxcraft` は起動するが、毎フレーム
   `SGFX device does not support canvas depth testing` で提出を拒否される。
   Maxwell backend の capability、codegen の depth draw / clear、kernel の
   DEPTH_STENCIL image がいずれも未対応である。capability の反転だけでは直らない。
2. **アプリ追加後の物理メモリ不足**。
   Myrica が 2,791,680 bytes、Vellum が 3,196,816 bytes の確保で失敗した。
   続いて Boxcraft の ELF segment、cat の user stack も確保できなくなった。
   起動した Files を終了すると cat / ps を再び実行できた。
   例外 ESR `0xf2000001` (EC `0x3c`)、命令 `0xd4200020` は BRK #1。
   今回の exit 132 は Rust の OOM abort であり、LSE 命令の証拠ではない。
3. **ARM64 Linux boot が RAM の最大1区画しか PMM に渡していない**。
   旧カーネルの PMM は `0x1052eb000–0x17fffffff` の1区画のみ。
   metadata を除く初期空きは500,113 pages = 約1953.6 MiB、その後さらに
   kernel heap 512 MiBを確保する。FDTが示す低い側のRAMに残る複数区画は
   `largest_usable_area` / `best_usable` の選択で捨てられていた。

証跡：`app-repro-gdb-uart.log`、抜粋 `app-failure-evidence.txt`。
本調査で追加したアプリは終了し、サービス由来の desktop / Files は保持した。

### デバッガによる停止について

GDB で4 CPUを停止し、画面メモリを一度に約3.75 MiB読み出したため長時間停止した。
その間にユーザーから「ハングしてる」と申告を受けた。操作前の説明が足りなかった。
読み出しを中断し、LLDBを終了して GDB DTR を落として全CPUを再開した。
UART の `echo resume-check` が応答した。取得画像はない。
最初の停止時のPCはCPU0がIRQ EOI、CPU1–3がidleであり、これだけでは
自発的なカーネル全体のハングを証明しない。画面の応答は別途確認が必要。
以降は大きな live debugger read を実施していない。

### RAM 区画の修正

Scarlet の `kernel/src/arch/aarch64/boot/linux.rs` を共通 `FdtMemory` に接続し、
`BootInfo::with_usable_memory_regions` に全区画を渡すよう修正した。
FDT reservation table、reserved-memory / no-map、kernel、DTB、initramfs、
framebuffer を除外する。initramfs は予約した元の位置を使い、二重にコピーしない。
`/dev/interrupts` に PMM total/free bytes と initial kernel heap bytes を追加した。

ビルド成功。実ソースの memory parser / sparse direct map / address 型を
ホスト側でコンパイルし、14テスト成功。Noble DTB にfirmware相当のRAM bank /
resident reservation / initrdを適用したfixtureでも全区画の保持と予約の除外を確認。
これは実機から採取した完成DTBそのものではなく、ホスト検証用fixture。

変更前後のソース、差分、host test、ビルド、USB用bundleは `memory-fix/` に保存。
既存の MMC / ext2 / page cache / CPU feature / auxv の差分は保持した。
SDのブートファイルへはまだ配布していない。


### 修正版の実機検証

`SCR-SWV` (GDB無効) を明示し、既存のAPX待機手順で再起動、USB bundle転送に成功。
同じ initramfs、SD p4/ext2 rootfs、通常14サービスの構成。

| 項目 | 旧カーネル | 修正版 |
| --- | ---: | ---: |
| PMM 登録区画 | 1 | 7 |
| PMM 登録 bytes（metadataを含む） | 2,060,537,856 (1.919 GiB) | 4,162,392,064 (3.877 GiB) |
| PMM 初期空き（metadata除外・heap確保前） | 2,048,462,848 | 4,137,996,288 |
| 初期 kernel heap | 536,870,912 | 536,870,912 |
| 通常サービス起動後のPMM空き | この指標は旧版未取得 | 3,000,958,976 (2.795 GiB) |
| Vellum PDF起動後のPMM空き | この指標は旧版未取得 | 2,680,221,696 (2.496 GiB) |

4 GiBとの差約126.4 MiBは、kernel、initramfs、DTB、framebuffer、firmware等の
予約としてPMM外にある。PMM metadataとkernel heapは上表のPMM登録量の内側。
内訳を混同しない。新しい7区画のアドレスは `memory-fix/validation.json`。

- `/bin/vellum /root/sample.pdf` はSGFX renderer初期化まで到達し、プロセス継続。
- `/bin/vellum --dump-png /tmp/vellum-memory-fix.png /root/sample.pdf` が終了し、
  `/tmp` に97,631-byte PNGを生成。今回のVellum実行でOOM/BRKは観測していない。
- その後も `ps -T` / `cat /dev/interrupts` が応答し、PMM空きは2,678,267,904 bytes。
- 画面の表示・入力の成功はまだユーザー確認がない。プロセス継続やPNG生成を
  GUI正常動作の証明として扱わない。Boxcraftのdepth未実装も残る。

**現在の実機はこのメモリ区画修正版・GDB無効・SD rootfsで稼働中。**
確認用Vellum (PID129) を1つ残した。デバッガは接続していない。
修正はソースとUSB起動用bundleに保存してあり、SD側の通常起動kernelは未更新。
証跡：`memory-fix/{upload.log,uart.log,validation.json,host-tests.log,build.log}`。


## コミットと次の作業

- Scarlet `26c75233`: ARM64 Linux boot でFDTの全usable RAM区画を登録。
- Scarlet `9b662f3b`: `/dev/interrupts` のIRQ deliveryとPMM total/free診断。
  引き継いだIRQカウンタ実装を診断デバイスの依存部分として含む。
- Switchvisor `0da6ca6`: 切断中GDB FIFOの繰り返しゼロ初期化を除去。
  `../switchvisor-perf-gdb` の `fix/gdb-disconnect-idle-cpu` に保存。
  新payloadのSD配布・実機実行は引き続き未実施。

MMC / ext2 / page cache / AArch64 CPU feature判定など、別途進行中だった
差分は今回のRAM修正コミットに混ぜていない。実機検証は、それらの差分を
含む現在の作業ツリーからビルドしたカーネルによる。
次の作業は [Boxcraft → HWDC → Sound](gm20b-3d-next.md) の順。Vulkanは後回し。
