# Switch boot video IMG_9080

Source: `/Users/petitstrawberry/Downloads/IMG_9080.mov`, 17.3867 seconds,
1920×1080 HEVC, approximately 59.93 fps. SHA256:
`c47fe3a3523f77899780b6742ba2734841f1ddcc85394726ecae2bb578504f1d`.

Frames were extracted with ffmpeg at 10 fps into `.cache/video-9080/`.
`ap-detail-*` retains the original cadence from 13.25–13.65 seconds.
Readings are visual and timestamps are approximate. Candidate attribution
follows the verified GPIO6-corrected SD deployment in
`gpu-gpio6-verification.json`; the video does not display an artifact hash.
The original recording was not changed.

| Frame | Visible evidence | Scope |
| --- | --- | --- |
| `frame-110` | CPU frequency backend ready; CPU mask `0xf`, current `1017600 kHz`; timer unchanged | Backend initialized and read the inherited CAR rate. No independent frequency measurement is shown. |
| `frame-115` | Right `Rate -> Ready`; first HID `id=0x30`; `/dev/gamepad0` registered | Right attached Joy-Con initialization and HID reception succeeded. |
| `frame-117` | Left `Rate -> Ready`; first HID `id=0x30` | Left initialization and HID reception succeeded; the remaining HID fields are partly overwritten and are not transcribed. |
| `frame-118`–`frame-119` | `inherited GPIO6=0x02 ALT6=0x00`; `rail ready GPIO6=0x09 ALT6=0x00 (push-pull high)` | Hekate's inherited input/open-drain setting was actually present. The corrected driver set push-pull/high/output. These are register readbacks, not an independent voltage measurement. |
| `frame-119` | `MC flush complete ctrl=0x00000000 status=0x00000000`; `MC_BOOT_0=0x12b000a1 read_us=15` | MC release and a prompt, valid GM20B identity read succeeded. The previous multi-second all-ones read did not recur. |
| `frame-119` | `identified MC_BOOT_0=0x12b000a1`, zero stall/nonstall interrupt status, `/dev/gpu0`; `execution support=0` | The normal GPU control endpoint registered. Rendering, firmware execution and DMA were not implemented or validated by this result. |
| `frame-119` | RTC wall-clock seed; touch chip info, sensing enabled, `/dev/touchscreen0 ready, ten contacts` | RTC seed and touch initialization succeeded; accuracy and user-operated touch are not established. |
| `frame-132`, `frame-134`, `ap-detail-004` | CPU_ON succeeds for CPUs 1/2/3; AP 1/2 online with local timer ready; CPU 3 per-CPU controller initialization done | All APs entered successfully. CPU 3's final online summary is partly overwritten, so no new `4/4` transcription or sustained per-core timer claim is made from this video. |
| `ap-detail-012` | Deferred CPU frequency transition begin `pstate=6 freq_khz=710400`, followed by complete | The driver reports applying a 710400 kHz request. No post-transition rate query or repeated switching is shown. |
| `ap-detail-012` | Right Ready UART RX error `lsr=0x2 received=3` | A framing-error diagnostic occurred after initialization. Its effect on interactive input is not established by this recording. |
| `frame-150`–`frame-165` | Scarlet Shell's Library view, status bar, controls and pointer | Ordinary SWS/Shell graphical startup succeeded after GPU registration. No SError or fatal trap is visible. |
| `frame-170`–`frame-172` | `task-cpu-watchdog`, global task 49, pid 58, CPU 0, user mode, `usage=99.8%`, `pc=0x24f90`, `last_syscall=13` | One user task occupied almost all of its approximately one-second sampling window. The message does not identify its executable or prove an infinite loop. |
| `frame-174` | UI changes to show cards and a selection outline | Screen updates continue after the CPU-usage diagnostic. The recording ends without showing which input caused the transition or launching an application. |

## Result

The GPIO6 correction resolves the observed GPU identity-read failure in this
boot. The inherited drive-mode hypothesis is now supported by the logged
`0x02` input/open-drain value, the corrected `0x09` output/push-pull/high
value, and the successful 15 µs GPU transaction. The earlier asynchronous
SError did not recur during the recorded boot and short graphical session;
long-running and repeated-boot stability remain unmeasured.

The last CPU watchdog message is emitted by
`Scarlet/kernel/src/sched/scheduler.rs::sample_current_task_cpu_hog`.
At kernel revision `fd9cc06a37466d278bda97cf598b2587fb4e9986`, that path prints
the sample and returns; it does not panic, kill the task, or halt the CPU.
Its one-second window and 99% threshold are defined in `kernel/src/task/mod.rs`.
The video subsequently shows a UI update. The high CPU use remains a separate
performance observation; the executable and hot path cannot be attributed
from this shared userspace instruction address alone.

Power/identity bring-up and ordinary graphical startup are physically
validated for this candidate. SGFX execution support remains zero. The next
GPU implementation stages are GMMU/DMA ordering, firmware/GR initialization,
and channel submission before a Maxwell SGFX execution dialect is advertised.
