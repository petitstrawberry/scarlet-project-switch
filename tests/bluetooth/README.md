# Switch Bluetooth HID input in QEMU

Run from the Switch repository root with its development toolchain.

```sh
direnv exec . python3 tests/bluetooth/run.py
```

By default Cargo resolves the Scarlet revision pinned by the Switch project. The runner builds Scarlet’s QEMU kernel, init, loader, Bluetooth broker and CLI, links the board’s `scarlet-driver-joycon-bt` initcall, and installs the board’s `joycond` layout daemon. The guest launches these services and runs `btctl` commands over the production broker. Each boot uses fresh ext2 bond storage.

For integration before a board repin, pass `--scarlet-source /path/to/Scarlet`. The runner uses that checkout for the host, libraries, CLI, and SWS navigation policy, and applies Cargo path patches to the board packages using scratch lockfiles. Add `--output /path/to/scratch` to retain binaries and serial output.

The QA kernel models a single H4 adapter with one shared host ACL packet credit. Its left and right peers have different ACL handles, link keys and remote HID channel IDs. Each encrypted link first opens SDP. The radio returns the HID service's control and interrupt protocol lists and numbered report descriptor across continuation responses. Its Device ID record supplies USB-IF vendor 0x057e and left/right product 0x2006/0x2007. The fixture requires SDP teardown before the HID channels open. The external Switch driver identifies each half, sets full-report mode, publishes the physical input device and handles LED acknowledgements. The radio delivers H4 packets in short reads, interleaves fragmented HID reports across handles and sends both peers’ L2CAP echo requests with the same signaling identifier. It checks the echoed payload and handle, and fails if the host exceeds the advertised credit or sends plaintext ACL data. The guest uses a test-only radio device to supply button and stick reports, read the lamps and simulate link loss. All observations of pairing and gamepad input go through the production broker and input devices.

| Guest marker | Required observations |
| --- | --- |
| `BT_QA_SDP_PASS` | The right peer completes HID and Device ID discovery, including continuation and SDP teardown. The board driver negotiates full reports and publishes a physical right half. |
| `BT_QA_RIGHT_PASS` | Scan resolves both names. Pairing the right half publishes `/dev/joycon-right0` and one logical gamepad. Both expose RX/RY. Repeated reports produce a single Left press, Left release, Right press and Right release through the SWS menu policy. |
| `BT_QA_MULTILINK_PASS` | Both bonds are saved, both peers remain encrypted and both upright pads deliver interleaved reports. The separate players show one and two steady lamps. The left gamepad delivers D-pad input during a lost player-two LED acknowledgement and again in the retry reply. |
| `BT_QA_COMBINED_PASS` | L+R creates one logical gamepad with independent sticks, hat and face input, and one lamp on each half. |
| `BT_QA_MINIS_PASS` | SL+SR on each half produces two sideways gamepads. The guest checks opposite stick rotations, rotated face buttons, suppressed grip buttons and the player lamps on both physical halves. |
| `BT_QA_PEER_HID_PASS` | The bonded left Joy-Con wakes and initiates an ACL. After encryption, it requests HID control and interrupt on fresh peripheral CIDs while the host is opening SDP. Each request receives a pending reply during discovery and a final success on the same identifier after SDP teardown. Both channels complete configuration. The guest checks the left player lamp and reads a fresh button press and release while the right mini stays connected. |
| `BT_QA_GUI_GRIPS_PASS` | The desktop client lists two halves, combines and splits them, and turns one sideways. The native gamepads reflect each change, and a request using the previous revision is rejected. |
| `BT_QA_RECONNECT_PASS` | Losing the left ACL releases its inputs. The right mini stays usable. Each half reconnects with its own saved key on a new handle while the other remains usable. Addressed disconnects and link loss release held navigation keys, remove the corresponding device nodes and leave zero connected peers. A fresh left page immediately after a confirmed manual disconnect also restores HID when the host initiates the channels. |

A successful boot ends with `BLUETOOTH_QEMU_PASS`. With `--output`, inspect `serial.log` or repeat the boot using `--no-build` and the same output directory.
