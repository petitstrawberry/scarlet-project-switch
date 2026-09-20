# Switch: Boxcraft → HWDC → Sound の到達点

2026-09-20。RAM区画の修正後、ユーザーが指定した次の目標。
優先順位は **Boxcraft → 動画のハードウェアデコード（HWDC）→ 音声ドライバ**。
Vulkan対応はその後へ延期する。
ここに記載するGPU機能は実装前であり、動作済みという記録ではない。

## 現在の障害

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

Hekate UMSは使用禁止。USB bundleは既存のRCM待機手順を使う。
現在の稼働版はRAM区画修正版、`SCR-SWV`、SD p4 rootfs。
実機の再起動・デバッガ停止は操作前に説明し、長時間のlive memory readを避ける。
通常アプリの再現と必要な正しさの確認を行い、起動時の自動benchmarkを追加しない。
