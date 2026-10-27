#![no_std]
#![no_main]

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use core::{
    arch::{asm, global_asm},
    fmt::{self, Write},
    ptr::{read_volatile, write_volatile},
};
static FAULT_CONSOLE: AtomicUsize = AtomicUsize::new(0x700ff000);
static FAULT_ACTIVE: AtomicBool = AtomicBool::new(false);

#[path = "../../../drivers/network/brcmfmac/src/pcie.rs"]
mod brcm_pcie;
#[path = "../../../drivers/network/brcmfmac/src/chip.rs"]
mod chip;
#[cfg(feature = "radio-msgbuf")]
#[path = "../../../drivers/network/brcmfmac/src/control.rs"]
mod control;
#[cfg(feature = "radio-msgbuf")]
mod dma;
#[cfg(feature = "radio-msgbuf")]
#[path = "../../../drivers/network/brcmfmac/src/msgbuf.rs"]
mod msgbuf;
#[path = "../../../drivers/soc/tegra210/src/packet.rs"]
#[allow(dead_code)]
mod packet;
#[path = "../../../drivers/pcie/tegra210/src/controller.rs"]
mod pcie;
mod radio;
mod recovery;
#[cfg(feature = "radio-scan")]
mod scan;
#[path = "../../../drivers/rtc/max77620/src/xusb_supplies.rs"]
mod supplies;
#[cfg(feature = "radio-scan")]
#[path = "../../../drivers/network/brcmfmac/src/wireless.rs"]
mod wireless;

// Inventory uses no heap, IRQs or DMA. Explicit diagnostic options enable the
// controller or recovery watchdog; neither option is enabled by default.
global_asm!(
    r#"
    .section .head,"ax"
    .global _start
_start:
    b entry
    .long 0
    .quad 0x200000
    .quad __image_size
    .quad 2
    .quad 0, 0, 0
    .long 0x644d5241
    .long 0
entry:
    msr daifset, #15
    msr spsel, #1
    ldr x1, =__bss_start
    ldr x2, =__bss_end
0:
    cmp x1, x2
    b.hs 1f
    str xzr, [x1], #8
    b 0b
1:
    ldr x1, =__stack_top
    mov sp, x1
    b probe
    .section .text.vectors,"ax"
    .balign 2048
    .global probe_vectors
probe_vectors:
    .rept 16
    b probe_fault
    .space 124
    .endr
"#
);

#[derive(Clone, Copy)]
enum Console {
    Ns16550(usize),
    Pl011(usize),
}

impl Write for Console {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            let mut ready = false;
            for _ in 0..1_000_000 {
                ready = unsafe {
                    match *self {
                        Self::Ns16550(base) => read_volatile((base + 5) as *const u8) & 0x20 != 0,
                        Self::Pl011(base) => read_volatile((base + 0x18) as *const u32) & 0x20 == 0,
                    }
                };
                if ready {
                    break;
                }
            }
            if !ready {
                return Err(fmt::Error);
            }
            unsafe {
                match *self {
                    Self::Ns16550(base) => write_volatile(base as *mut u8, byte),
                    Self::Pl011(base) => write_volatile(base as *mut u32, byte as u32),
                }
            }
        }
        Ok(())
    }
}

fn park() -> ! {
    loop {
        unsafe {
            asm!("wfe", options(nomem, nostack));
        }
    }
}

fn keep_console_serviced(console: Console) -> ! {
    // U-Boot hands off without a guest GIC/timer driver. In this state a WFE
    // can prevent EL2's exit-driven USB polling, including control requests.
    // A trapped virtual-UART read keeps Switchvisor's service loop running.
    loop {
        unsafe {
            match console {
                Console::Ns16550(base) => {
                    let _ = read_volatile((base + 5) as *const u8);
                }
                Console::Pl011(base) => {
                    let _ = read_volatile((base + 0x18) as *const u32);
                }
            }
        }
        for _ in 0..10_000 {
            core::hint::spin_loop();
        }
    }
}

#[unsafe(no_mangle)]
extern "C" fn probe_fault() -> ! {
    unsafe {
        asm!("msr daifset, #15", options(nomem, nostack));
    }
    let mut out = if FAULT_CONSOLE.load(Ordering::Relaxed) == 0x9000000 {
        Console::Pl011(0x9000000)
    } else {
        Console::Ns16550(0x700ff000)
    };
    if FAULT_ACTIVE.load(Ordering::Relaxed) {
        let _ = out.write_str("WIRELESS_PROBE_RECURSIVE_FAULT\n");
        keep_console_serviced(out);
    }
    FAULT_ACTIVE.store(true, Ordering::Relaxed);
    let (esr, elr, far): (u64, u64, u64);
    unsafe {
        asm!("mrs {}, esr_el1", out(reg) esr);
        asm!("mrs {}, elr_el1", out(reg) elr);
        asm!("mrs {}, far_el1", out(reg) far);
    }
    // Keep the first fault report independent of fmt machinery and avoid a
    // second asynchronous exception before the first ESR has been recorded.
    let _ = out.write_str("WIRELESS_PROBE_FAULT esr=");
    fault_hex(&mut out, esr);
    let _ = out.write_str(" elr=");
    fault_hex(&mut out, elr);
    let _ = out.write_str(" far=");
    fault_hex(&mut out, far);
    let _ = out.write_str("\n");
    pcie::Io::delay_us(&Hardware(out, core::cell::Cell::new(false)), 5000);
    recovery::disarm(&Hardware(out, core::cell::Cell::new(false)));
    let _ = writeln!(out, "WIRELESS_PROBE_COMPLETE");
    keep_console_serviced(out)
}

fn fault_hex(out: &mut Console, value: u64) {
    let _ = out.write_str("0x");
    for shift in (0..16).rev() {
        let byte = b"0123456789abcdef"[((value >> (shift * 4)) & 15) as usize];
        // Every byte selected above is ASCII; avoid formatting or allocation.
        let _ =
            out.write_str(unsafe { core::str::from_utf8_unchecked(core::slice::from_ref(&byte)) });
    }
}

fn recovery_fixture(console: &mut Console) {
    use core::cell::Cell;
    struct Model {
        config: Cell<u32>,
        scratch: Cell<u32>,
    }
    impl recovery::Io for Model {
        fn read(&self, address: usize) -> u32 {
            match address {
                0x60006010 => 1 << 5,
                0x7000e450 => self.scratch.get(),
                0x60005100 => self.config.get(),
                _ => 0,
            }
        }
        fn write(&self, address: usize, value: u32) {
            match address {
                0x7000e450 => self.scratch.set(value),
                0x60005100 => self.config.set(value),
                _ => {}
            }
        }
    }
    let model = Model {
        config: Cell::new(0),
        scratch: Cell::new(0x80000000),
    };
    let armed = recovery::arm(&model).is_ok() && model.scratch.get() == 2;
    let duplicate_rejected = recovery::arm(&model).is_err();
    recovery::disarm(&model);
    let restored = model.scratch.get() == 0x80000000;
    let _ = writeln!(
        console,
        "RECOVERY_FIXTURE armed={armed} duplicate_rejected={duplicate_rejected} restored={restored}"
    );
}

struct Hardware(Console, core::cell::Cell<bool>);
impl Hardware {
    fn arm_recovery(&self) -> Result<(), &'static str> {
        use pcie::Io;
        self.observe("recovery-original-boot-flags", self.read32(0x7000e450));
        recovery::arm(self)?;
        let before = self.read32(0x60005104);
        self.delay_us(300_000);
        let after = self.read32(0x60005104);
        self.observe("recovery-counter", after);
        if before == after {
            recovery::disarm(self);
            return Err("recovery watchdog counter did not advance");
        }
        Ok(())
    }
    fn ticks(&self) -> u64 {
        let value: u64;
        unsafe {
            asm!("mrs {}, cntvct_el0", out(reg) value, options(nomem, nostack));
        }
        value
    }
    fn frequency(&self) -> u64 {
        let value: u64;
        unsafe {
            asm!("mrs {}, cntfrq_el0", out(reg) value, options(nomem, nostack));
        }
        value
    }
    fn service(&self) {
        if let Console::Ns16550(base) = self.0 {
            unsafe {
                let _ = read_volatile((base + 5) as *const u8);
            }
        }
    }
}
impl pcie::Io for Hardware {
    fn read32(&self, address: usize) -> u32 {
        unsafe { read_volatile(address as *const u32) }
    }
    fn write32(&self, address: usize, value: u32) {
        unsafe {
            write_volatile(address as *mut u32, value);
            asm!("dsb sy", options(nostack));
        }
    }
    fn delay_us(&self, us: u64) {
        let start = self.ticks();
        let duration = us.saturating_mul(self.frequency()) / 1_000_000;
        while self.ticks().wrapping_sub(start) < duration {
            self.service();
        }
    }
    fn stage(&self, name: &'static str) {
        if name == "root-port" {
            self.1.set(true);
        }
        let _ = writeln!(self.0.clone(), "PCIE_STAGE {name}");
        self.delay_us(2000);
    }
    fn observe(&self, name: &'static str, value: u32) {
        let _ = writeln!(self.0.clone(), "PCIE_OBSERVE {name}={value:#010x}");
        self.delay_us(2000);
    }
}
impl packet::Registers for Hardware {
    fn read(&self, offset: usize) -> u32 {
        pcie::Io::read32(self, 0x7000d000 + offset)
    }
    fn write(&self, offset: usize, value: u32) {
        pcie::Io::write32(self, 0x7000d000 + offset, value);
    }
    fn now_ns(&self) -> u64 {
        self.service();
        ((self.ticks() as u128 * 1_000_000_000) / self.frequency() as u128) as u64
    }
}
impl recovery::Io for Hardware {
    fn read(&self, address: usize) -> u32 {
        pcie::Io::read32(self, address)
    }
    fn write(&self, address: usize, value: u32) {
        pcie::Io::write32(self, address, value);
    }
}
impl supplies::Registers for Hardware {
    fn read(&self, register: u8) -> Result<u8, &'static str> {
        packet::normal_transfer(self, 0x3c, &mut [register], false)
            .map_err(|_| "PMIC register select failed")?;
        let mut byte = [0];
        packet::normal_transfer(self, 0x3c, &mut byte, true).map_err(|_| "PMIC read failed")?;
        Ok(byte[0])
    }
    fn write(&self, register: u8, value: u8) -> Result<(), &'static str> {
        packet::normal_transfer(self, 0x3c, &mut [register, value], false)
            .map_err(|_| "PMIC write failed")
    }
    fn wait_us(&self, us: u64) {
        pcie::Io::delay_us(self, us);
    }
    fn diagnose(&self, s: &supplies::Snapshot) {
        let _ = writeln!(self.0.clone(), "PCIE_SUPPLIES {s:?}");
    }
}

fn initialize_pcie(console: Console, mac: Option<[u8; 6]>) -> Result<u32, &'static str> {
    use pcie::Io;
    let hw = Hardware(console, core::cell::Cell::new(false));
    supplies::prepare(&hw)?;
    hw.stage("supplies-ready");
    // Hekate's pre-Linux GPIO muxes are not a kernel pinctrl application.
    hw.modify(0x700031b4, 0x5f, 4); // WIFI_EN PH0: GPIO, output, pull-down.
    hw.modify(0x700031b8, 0x5f, 4); // WIFI_RST PH1.
    hw.modify(0x70003044, 0x5f, 0); // PEX_L1_RST: PE1 output.
    hw.modify(0x70003048, 0x5f, 0x40); // PEX_L1_CLKREQ: PE1 input.
    hw.write32(0x6000d080, (1 << 11) | (1 << 12)); // PA3/4 peripheral mode.
    hw.stage("pinmux-ready");
    // Only PH0/PH1 (Wi-Fi enable/reset), using GPIO masked-write aliases.
    // Program output values before enabling the output driver.
    hw.write32(0x6000d1ac, 0x300);
    hw.write32(0x6000d19c, 0x303);
    hw.write32(0x6000d18c, 0x303);
    hw.delay_us(10_000);
    // ODIN's boot-default-output-low explicitly includes PH0 (0x38), while
    // boot-default-output-high includes PH1 (0x39). WIFI_EN is not an inferred
    // active-high power switch. Match that board state before the PCIe host
    // starts, allowing the chip to boot while AFI still holds PERST on PA3.
    // PH1 is not the PCIe PERST pin and must not be pulsed by that callback.
    hw.write32(0x6000d1ac, 0x302);
    hw.delay_us(10_000);
    // Match Linux's GPIO-hog-before-host order and allow board startup time.
    hw.delay_us(100_000);
    let result = pcie::initialize(&hw);
    if result == Ok(0x43ec14e4) {
        let radio = radio::discover(&hw, mac);
        let _ = writeln!(console.clone(), "BRCM_RESULT {radio:?}");
        hw.delay_us(2000);
    }
    for (name, address) in [
        ("GPIO_H_CFG", 0x6000d10c),
        ("GPIO_H_OE", 0x6000d11c),
        ("GPIO_H_OUT", 0x6000d12c),
        ("WIFI_EN_MUX", 0x700031b4),
        ("WIFI_RST_MUX", 0x700031b8),
        ("PLLE_POST", 0x600060ec),
        ("P0_POST", 0x7009f360),
        ("UPHY_MUX_POST", 0x7009f028),
    ] {
        let _ = writeln!(
            console.clone(),
            "PCIE_POST {name}={:#010x}",
            hw.read32(address)
        );
    }
    result
}

#[unsafe(no_mangle)]
extern "C" fn probe(dtb: usize) -> ! {
    if dtb & 7 != 0 || !(0x4000_0000..0xf000_0000).contains(&dtb) {
        park();
    }
    let header = unsafe { core::slice::from_raw_parts(dtb as *const u8, 8) };
    if header[..4] != [0xd0, 0x0d, 0xfe, 0xed] {
        park();
    }
    let size = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
    if !(40..=1024 * 1024).contains(&size) {
        park();
    }
    let Ok(tree) = (unsafe { fdt::Fdt::from_ptr(dtb as *const u8) }) else {
        park();
    };
    let chosen = tree.find_node("/chosen");
    let fixture = chosen
        .and_then(|n| n.property("scarlet,wireless-probe-fixture"))
        .is_some();
    let Some(path) = chosen
        .and_then(|n| n.property("stdout-path"))
        .and_then(|p| p.as_str())
    else {
        park();
    };
    let Some(node) = tree.find_node(path) else {
        park();
    };
    let console = if path == "/serial@700ff000"
        && node
            .compatible()
            .is_some_and(|c| c.all().any(|s| s == "ns16550a"))
        && node
            .reg()
            .and_then(|mut r| r.next())
            .is_some_and(|r| r.starting_address as usize == 0x700ff000)
        && node.property("reg-shift").and_then(|p| p.as_usize()) == Some(0)
        && node.property("reg-io-width").and_then(|p| p.as_usize()) == Some(1)
    {
        Console::Ns16550(0x700ff000)
    } else if fixture && path == "/pl011@9000000" {
        Console::Pl011(0x9000000)
    } else {
        park();
    };
    let mut out = console;
    FAULT_CONSOLE.store(
        match console {
            Console::Pl011(a) | Console::Ns16550(a) => a,
        },
        Ordering::Relaxed,
    );
    unsafe extern "C" {
        static probe_vectors: u8;
    }
    unsafe {
        asm!("msr vbar_el1, {}", "isb", "msr daifclr, #4", in(reg) core::ptr::addr_of!(probe_vectors), options(nostack));
    }
    let _ = writeln!(
        out,
        "WIRELESS_PROBE_BEGIN version=1 dtb={dtb:#x} bytes={size}"
    );
    let el: u64;
    let sctlr: u64;
    unsafe {
        asm!("mrs {}, CurrentEL", out(reg) el);
        asm!("mrs {}, sctlr_el1", out(reg) sctlr);
    }
    let _ = writeln!(out, "ENTRY el={} sctlr_el1={sctlr:#x}", el >> 2);
    // Preserve the exact firmware/U-Boot handoff, including phandles and rails.
    // Hex framing is independent of terminal newline conversion.
    let bytes = unsafe { core::slice::from_raw_parts(dtb as *const u8, size) };
    for (i, chunk) in bytes.chunks(64).enumerate() {
        let _ = write!(out, "DTB {:08x} ", i * 64);
        for byte in chunk {
            let _ = write!(out, "{byte:02x}");
        }
        let _ = writeln!(out);
    }
    let odin = tree.root().compatible().all().any(|s| s == "nvidia,odin");
    if odin && !fixture {
        // Always-on CAR/PMC reads only. Do not touch gated PCIe pads/AFI/CS.
        for (name, address) in [
            ("HIDREV", 0x70000804usize),
            ("CAR_RST_H", 0x60006008),
            ("CAR_CLK_H", 0x60006014),
            ("PLLE_BASE", 0x600060e8),
            ("CAR_RST_U", 0x6000600c),
            ("CAR_CLK_U", 0x60006018),
            ("PLLE_MISC", 0x600060ec),
            ("PLLE_AUX", 0x6000648c),
            ("PMC_PWRGATE_STATUS", 0x7000e438),
            ("PMC_CLAMP_STATUS", 0x7000e42c),
        ] {
            let value = unsafe { read_volatile(address as *const u32) };
            let _ = writeln!(
                out,
                "REG {name} address={address:#010x} value={value:#010x}"
            );
        }
        if chosen
            .and_then(|n| n.property("scarlet,wireless-probe-initialize"))
            .is_some()
        {
            let hw = Hardware(console, core::cell::Cell::new(false));
            let armed = hw.arm_recovery();
            let _ = writeln!(out, "RECOVERY_ARM {armed:?}");
            let mac = chosen
                .and_then(|n| n.property("nvidia,wifi-mac"))
                .and_then(|p| p.as_str())
                .and_then(radio::parse_mac);
            let result = armed.and_then(|()| initialize_pcie(console, mac));
            recovery::disarm(&hw);
            match result {
                Ok(id) => {
                    let _ = writeln!(out, "PCIE_RESULT Ok({id:#010x})");
                }
                Err(error) => {
                    let _ = writeln!(out, "PCIE_RESULT Err({error:?})");
                }
            }
        } else {
            let _ = writeln!(out, "PCIE_MMIO skipped=inventory-only");
        }
        if chosen
            .and_then(|n| n.property("scarlet,wireless-probe-watchdog-test"))
            .is_some()
        {
            let hw = Hardware(console, core::cell::Cell::new(false));
            let armed = hw.arm_recovery();
            let _ = writeln!(out, "RECOVERY_TEST {armed:?}");
            pcie::Io::delay_us(&hw, 5000);
            if armed.is_ok() {
                // Keep USB serviced even if the watchdog is misconfigured.
                // A successful hardware reset interrupts this loop at 30s.
                for _ in 0..40 {
                    let _ = writeln!(
                        out,
                        "RECOVERY_STATUS {:#010x}",
                        pcie::Io::read32(&hw, 0x60005104)
                    );
                    pcie::Io::delay_us(&hw, 1_000_000);
                }
                recovery::disarm(&hw);
                let _ = writeln!(out, "RECOVERY_TEST_FAILED no reset within 40s");
            }
        }
    } else {
        let _ = writeln!(out, "TEGRA_MMIO skipped=fixture-or-unsupported-board");
        if fixture {
            recovery_fixture(&mut out);
        }
    }
    if fixture
        && chosen
            .and_then(|n| n.property("scarlet,wireless-probe-fault"))
            .is_some()
    {
        unsafe {
            asm!("brk #0x777", options(nostack));
        }
    }
    let _ = writeln!(out, "WIRELESS_PROBE_COMPLETE");
    keep_console_serviced(console)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let mut out = Console::Ns16550(0x700ff000);
    let _ = writeln!(out, "WIRELESS_PROBE_PANIC {info}");
    recovery::disarm(&Hardware(out, core::cell::Cell::new(false)));
    let _ = writeln!(out, "WIRELESS_PROBE_COMPLETE");
    keep_console_serviced(out)
}
