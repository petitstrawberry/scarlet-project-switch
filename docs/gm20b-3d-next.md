# Switch: Boxcraft → HWDC → Sound の到達点

2026-09-20。RAM区画の修正後、ユーザーが指定した次の目標。
優先順位は **Boxcraft → 動画のハードウェアデコード（HWDC）→ 音声ドライバ**。
Vulkan対応はその後へ延期する。
Depth32Floatの実装とホスト検証を追加した。実機検証結果は後述の記録を参照。

## 着手時の障害

- Boxcraft: ScarletUIのcanvas描画が `supports_depth() == false` で拒否される。
  実機の反復エラーは [調査記録](performance-regression-20260920.md) に保存。
- `userspace/sgfx-backend-scarlet-maxwell/src/lib.rs` はdepthを常にfalseにする。
  `resource.rs` のlayout検証もDepth32Floatを拒否する。
- `userspace/sgfx-codegen-maxwell/src/compiler.rs` の `require_depth_surface` と
  `emit.rs` のdepth draw / clearが未対応。
- `drivers/gpu/nvidia-gm20b/src/executor.rs` はBGRA8以外とdepth image usageを
  拒否する。操作検証は64-word recordの末尾を予約値として検証している。
- `drivers/gpu/nvidia-gm20b/src/graphics.rs` はZETAとdepth test/writeを無効化。
- Vulkanはdepth対応だけでは足りない。`../sgfx/crates/sgfx/src/driver.rs` の
  programmable driverのdevice/resource/queueにはMaxwellの実装がない。
  現在のMaxwell shader packは固定pipeline用である。

## 実装順序

### 1. Depth32Float を全層で扱う

1. NVIDIA B197と既存のpinned Mesa実装に基づき、非圧縮depth storageの
   pitch、tile、GMMU mapping、alignmentを決定する。
2. kernel image layout / backing / attachment validationを追加する。
   color、depth、sampled imageの取り違えと権限外の参照を拒否する。
3. version/capabilityを伴うcanonical operationのdepth clear、depth attachment、
   compare、write-enable表現を定義し、compilerとkernel validatorを揃える。
4. trusted method emissionにZETA、clear、compare、writeを追加する。
   depthを使わない後続passでは状態を戻す。
5. backend resource import / layout validationとSGFX capabilityを接続する。

受け入れ確認:

- 重なる図形の提出順を逆にしてもdepth有効時のreadbackが一致する。
- depth無効・write無効・比較条件変更の対照で結果が変化する。
- 部分clear、color passへの切替、異なるサイズのattachmentを確認する。
- 不正なdepth format/usage/range/relocationと予約bitはGPUに投入する前に拒否。
- 既存の2D合成・texture upload/readback・scanoutが継続する。

### 2. Boxcraft を実機で確認する

Boxcraftの通常のworld生成、depth付きcanvas提出、最初の表示までを通す。
Joy-Conまたはタッチによる視点の変化、前後関係、終了後のデスクトップ復帰を
確認する。既存のworld設定を縮小して動作成功の代用にしない。
起動前後と終了後のPMM空き、およびGPUエラーを記録する。

### 3. 動画のHWDC

Boxcraftの実機確認後、video playerの既存decode経路とTegra210の動画decode engine、
firmware、bufferとdisplayの連携を調べる。実際の動画をhardware decodeして表示し、
software decodeとの経路の区別、seek、終了時の資源解放を確認する。

### 4. 音声ドライバ

HWDCの次に、搭載codec、AHUB/I2S、DMA、clock/railと音量制御の接続を調べ、
PCM出力からvideo playerの音声再生へ進む。再生・停止・再開、speaker/headphone
経路を実機で確認する。既存のユーザー設定に基づく音量を維持する。

### 後回し: Vulkan-SGFX Cube を動かす

最初の対象は `../Scarlet/user/vulkan-canvas-demo`。
SPIR-V vertex/fragment shader、indexed textured draw、D32 depth、dynamic UBO
offset、viewport/scissor、readback、shared GPU imageの表示を必要とする。

programmable SGFX driverへMaxwell adapter/device/resources/queueを接続し、
shader lowering・resource binding・実行の対応範囲を実装する。
特定のdemo shaderを固定画像や既存の固定pipelineへ置換して完了にしない。
既存demoのGPU readback検証と継続したframe presentationを実機で通す。
通常のVulkan loader/ICDを使うLinuxアプリは、その後の独立した到達点とする。

## 参照と実機運用

- [NVIDIA B197 class header](https://github.com/NVIDIA/open-gpu-doc/blob/master/classes/3d/clb197.h)
- [既存採用リビジョンのMesa Nouveau](https://gitlab.freedesktop.org/mesa/mesa/-/tree/e881540692daac6532cefec76699f7a025563767/src/gallium/drivers/nouveau)
- [現在のSGFX GPU経路](sgfx-bringup.md)
- [Vulkan側の既存検証](../../Scarlet/docs/graphics/vulkan-games.md)

Hekate UMSは調査開始時に禁止されていたが、同日ユーザーが明示的に使用を許可した。
USB bundleは既存のRCM待機手順を使う。
現在の稼働版はRAM区画修正版、`SCR-SWV`、SD p4 rootfs。
実機の再起動・デバッガ停止は操作前に説明し、長時間のlive memory readを避ける。
通常アプリの再現と必要な正しさの確認を行い、起動時の自動benchmarkを追加しない。

## Depth32Float実装（2026-09-20）

- 既存バイナリの画像配置は維持。新しいMaxwell backendがcolor render targetに
  `GPU_IMAGE_USAGE_DEPTH_COMPATIBLE`を指定した場合に、任意サイズのH4 block-linear
  layoutを選ぶ。従来の1280×720 scanoutも同じ配置のまま。
- colorはkind `0xfe`、ZF32は非圧縮kind `0x7b`。両方ともrow pitchを64byte、
  高さを128行、private GPU allocationを8192byteに整列する。
  depthのmodifierはkindを区別する `0x030000000007b014`。
- `GPU_EXECUTION_SUPPORT_DEPTH`で追加機能を通知する。wire v1の64-word
  operationにopcode 4（depth clear）を追加。opcode 2では54–55にdepth relocation、
  56–59にwidth/height/pitch/tile、60にcomparison（0=無効、1–8=NeverからAlways）、
  61にwrite enableを置く。62–63は予約値。既存のdepthなしrecordは変更不要。
- clearはwrite authority、drawはreadと必要な場合だけwrite authorityを要求する。
  kernelはformat、usage、modifier、extent、range、reserved fieldを独立に検証し、
  GPU method/addressは検証後にカーネル内で生成する。
- CPU upload/readbackはgeneric backingをlinear stagingとして使い、private
  block-linear color allocationとの間でGOB変換する。depthのCPU accessは公開しない。

確認用コマンド:

```sh
nix develop --command cargo test --manifest-path tests/gpu-depth-qa/Cargo.toml
nix develop --command cargo test --manifest-path drivers/gpu/nvidia-gm20b/Cargo.toml
```

前者4件はcompilerのdepth authority/stateと不正layout、後者2件はGOBの境界と
全byteの一意性を検証する。kernel executor全体のhost実行テストではない。
`tests/gpu-depth-qa`のScarlet用binaryは明示的に実行する正しさの確認用であり、
bootにもdesktop launcherにも登録しない。67×131の非整列サイズで8種類の比較、
write無効、2D stateへの復帰を描画し、全pixelをreadbackして照合する。
続いて不正recordの拒否と、拒否後もqueueを使用できることを確認する。

## 実機で見つかった問題と修正

- 最初の試験binaryは旧配布Rustのprebuilt stdを取り込み、環境変数初期化の
  `casa`命令でSIGILLになった。以前記録したOOM時の`BRK #1`とは別の障害。
  配布済み修正版Rust `39c689a4859b9d8ee1828720135defd125c03d31`を使うよう
  flake.lockを更新し、SDKも`116882ab42b48a23613d3d574e2c987aca277786`に固定。
  `-Zbuild-std`やCPU機能の偽装は使わない。ローカルstage1と配布版の成果物を
  混在させると同じrustc versionでもmetadataが合わないため、target directoryを分けた。
- generic image backingのalignmentに8192を要求すると、4096までのallocatorに
  拒否された。generic backingはlinear CPU stagingなので4096とし、独立した
  private GPU allocationのPA/VAを8192に整列する。
- 修正後、実機の67×131 targetで8比較、write無効、depthなしdrawへの切替が
  全pixel一致。描画順反転probeの手編集がvertex rangeを更新しておらず、validatorに
  拒否されたため、record全体とrelocationを一緒に入れ替えるようprobeを修正した。
- ユーザーがBoxcraftの地形表示を確認。静止時の表示は約5 FPS、gamepad操作は未対応。
  アプリは静止時にsunlightを4 Hzで更新するため、この数値だけで連続描画の性能は
  判断しない。Boxcraftへnative gamepad入力を追加し、操作中の描画を別途確認する。
- `uart-gamepad.log`の再試験では、描画順反転、部分depth clear、部分tiled CPU
  upload/readback、不正recordの拒否後のqueue再使用まで**ALL PASS**。
  kernel/boot stackは直前と同一で、probeとBoxcraftだけ更新した。

ログは`.cache/performance-handoff-20260920/boxcraft-depth/`に保存。
`uart-distributed.log`は上記の実機試験、`package-distributed.json`はそのimage hash。
試験initramfsはSD rootへpivotするため、重複する大型desktop binaryだけ省いて
USB転送量を約93 MBから約30 MBに減らした。SDの通常アプリはこの時点で未変更。

### Boxcraft操作と性能の到達点

Joy-Con入力をBoxcraftのnative gamepad hookへ接続。左stickはanalog移動、右は
時間基準の視点移動、Bはjump、ZR/ZLは破壊/設置、L/Rはhotbar、+は設定、−はfullscreen。
pointer lockなしで操作できる。focus/resetで入力を解放し、設定中だけmenu navigationを有効化。
hostでdead zone・押下edge・複数deviceのresetを3件、移動速度・衝突等を3件確認した。

ユーザーの実機報告は操作中約15 FPS。`performance`でGPUを307.2 MHzに固定した
対照では約20 FPS。CPUはschedutilで1017.6 MHz、GPUの設定可能範囲は76.8–307.2 MHz。
CPU側の無駄という仮説は未確定であり、ユーザー指示により詳細調査を後回しにする。
GPUは`simple_ondemand`へ戻したことを`/dev/devfreq`で確認（idle 76.8 MHz、失敗sample 0）。
この測定は通常のBoxcraft操作で行い、worldサイズやrender distanceを変えていない。
GPU上限拡張・benchmark・起動時の追加計測は行っていない。次はHWDC、その後sound。

最新版を実機の`/old_root/bin/boxcraft-depth`からSD rootfsの`/bin/boxcraft`へコピー済み。
`storage-check hash`で両者が5,845,960 bytes、SHA-256
`1d90b5580d1fd39687b9bde83dfb223f19ac7209d6e444d8ce8b21ac899526ba`
で一致した。rootfsに旧版の退避ファイルは残していない。
このコピーはuserspaceのみで、起動中のdepth対応kernelはUSB bundleからロードしたもの。
