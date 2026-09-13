# Switch boot video IMG_9079

Source: `/Users/petitstrawberry/Downloads/IMG_9079.mov`, 6.585 seconds,
1920×1080 HEVC. SHA256 recorded before extraction:
`494b1b049f7202e4449fe9f96b1c7e6ad550521261813532ae821c60dc8c0fe5`.

Frames were extracted with ffmpeg at 10 fps into `.cache/video-9079/`.
Readings are visual and timestamps are approximate. Candidate attribution
follows the last verified SD deployment in `gpu-mc-verification.json`; the
video does not display an artifact hash. The earlier `IMG_9078.HEIC` ends at
the same MC-flush log seen at the beginning of this video.

| Frame | Visible evidence | Scope |
| --- | --- | --- |
| `frame-001`–`frame-024` | `gm20b: MC flush complete ctrl=0x00000000 status=0x00000004` remains on screen | The corrected MC release returned. The recording begins after this line, so the full duration of the following operation is unknown. |
| `frame-027` (about 2.6 s) | `gm20b: unexpected MC_BOOT_0=0xffffffff`; `GPU identity is not GM20B` | The first GPU BAR0 identity read eventually returned all ones. No GPU endpoint was registered. |
| `frame-027` | RTC wall-clock seed; touch chip info, sensing enabled, `/dev/touchscreen0 ready, ten contacts` | Startup continued after the failed GPU probe. No physical touch interaction or RTC accuracy check is shown. |
| `frame-029`–`frame-034` | Framebuffer registration and ordinary kernel initialization | The earlier static screen was not a permanent halt. |
| `frame-036`–`frame-042` | Init ELF loaded; CPU_ON succeeds for CPUs 1/2/3; AP 1/2 online with local timer ready; CPU 3 controller initialization done | AP startup progressed. This recording does not establish sustained four-core execution. |
| `frame-043`–`frame-066` | `[trap] asynchronous SError: ESR=0xbf000002`; exception handler stops | A fatal asynchronous error occurred after the failed GPU read. FAR and the interrupted PC cannot identify the original access. No interactive shell startup is shown. |

This establishes two failures: GPU BAR0 was inaccessible, and the subsequent
boot received an asynchronous SError. A delayed error from the GPU transaction
is a plausible connection, not a proven attribution from the interrupted PC.
No GPU rollback-error line is visible in the sampled frames.

## GPIO6 correction

The selected Noble DTB's MAX77620 `pin_gpio5_6_7` configuration specifies
`drive-push-pull = <1>`. [Linux's MAX77620 pinctrl driver](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/pinctrl/pinctrl-max77620.c)
implements this with GPIO configuration bit 0. The prior Scarlet GPU driver
set output direction and the high latch, but preserved this drive bit.

[Hekate's hardware initialization](https://github.com/CTCaer/hekate/blob/v6.5.3/bdk/soc/hw_init.c)
disables GPU GPIO6 using `max77620_config_gpio(6, ...DISABLE)`.
[That helper](https://github.com/CTCaer/hekate/blob/v6.5.3/bdk/power/max7762x.c)
selects input/open-drain for disable and output/push-pull/high for enable.
Consequently, the prior driver could retain open-drain and release the enable
line instead of actively driving it high. The inherited GPIO value was not
logged in this video, so this remains a source-backed fix awaiting hardware
confirmation.

The corrected driver explicitly selects push-pull/high/output on GPIO6 and
verifies all three bits. It preserves debounce and interrupt settings, and
the existing rollback restores the prior GPIO configuration. Added logs show
inherited and enabled GPIO6/ALT6, identity-read start, returned value and
elapsed microseconds, then the remaining register/endpoint stages.
`gpu-gpio6-verification.json` records the new candidate separately. Its next
physical checks are a prompt valid `MC_BOOT_0`, normal console session startup,
and absence of the observed asynchronous fault.
