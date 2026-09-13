//! Single-CPU diagnostics before Scarlet changes EL or installs a page table.
//! No allocator, logging locks, or uninitialized firmware UARTs are used here.

use core::fmt::{self, Write};
use core::ptr::{read_volatile, write_volatile};
use fdt::{Fdt, node::FdtNode};
use font8x8::UnicodeFonts;

unsafe extern "C" {
    static __KERNEL_SPACE_START: u8;
    static __KERNEL_SPACE_END: u8;
}

pub fn report(dtb_paddr: usize, current_el: usize) -> bool {
    let Some(fdt) = validated_fdt(dtb_paddr) else {
        return false;
    };
    let mut console = Console {
        framebuffer: fdt
            .all_nodes()
            .find_map(|node| Framebuffer::from_node(&node, &fdt, dtb_paddr)),
        uart: fdt
            .chosen()
            .stdout()
            .and_then(|node| Uart::from_node(&node)),
        x: 24,
        y: 24,
    };
    if let Some(fb) = &console.framebuffer {
        fb.clear();
    }
    let _ = writeln!(console, "SCARLET SWITCH");
    let _ = writeln!(console, "BSP boot probe reached");
    let _ = writeln!(console, "CurrentEL = EL{current_el}");
    let _ = writeln!(console, "DTB = {dtb_paddr:#018x}");
    let _ = writeln!(console, "DTB size = {} bytes", fdt.total_size());
    let _ = writeln!(console, "MMU = OFF; CPU = single core");
    let kernel = fdt
        .find_node("/chosen")
        .and_then(|node| node.property("scarlet,boot-mode"))
        .and_then(|prop| prop.as_str())
        == Some("kernel");
    if kernel {
        let _ = writeln!(console, "Entering Scarlet Linux Image bootstrap");
    } else {
        let _ = writeln!(console, "PROBE COMPLETE; CPU parked");
    }
    // MMU/cache were off on entry, so framebuffer stores reach scanout memory.
    unsafe {
        core::arch::asm!("dsb sy", options(nostack));
    }
    kernel
}

fn validated_fdt(paddr: usize) -> Option<Fdt<'static>> {
    if paddr == 0 || paddr & 7 != 0 {
        return None;
    }
    // SAFETY: x0 is accessible firmware-owned RAM under the arm64 boot contract.
    unsafe {
        if u32::from_be(read_volatile(paddr as *const u32)) != 0xd00d_feed {
            return None;
        }
        let size = u32::from_be(read_volatile((paddr + 4) as *const u32)) as usize;
        if !(40..=2 * 1024 * 1024).contains(&size) {
            return None;
        }
        Fdt::from_ptr(paddr as *const u8).ok()
    }
}

fn enabled(node: &FdtNode<'_, '_>) -> bool {
    node.property("status")
        .and_then(|prop| prop.as_str())
        .is_none_or(|status| status == "okay" || status == "ok")
}

fn compatible(node: &FdtNode<'_, '_>, value: &str) -> bool {
    node.compatible()
        .is_some_and(|list| list.all().any(|item| item == value))
}

fn overlaps(start: usize, end: usize, other_start: usize, other_end: usize) -> bool {
    start < other_end && other_start < end
}

struct Framebuffer {
    base: usize,
    width: usize,
    height: usize,
    stride: usize,
    rotate: bool,
    red_low: bool,
}

impl Framebuffer {
    fn from_node(node: &FdtNode<'_, '_>, fdt: &Fdt<'_>, dtb: usize) -> Option<Self> {
        if !enabled(node) || !compatible(node, "simple-framebuffer") {
            return None;
        }
        let reg = node.reg()?.next()?;
        let base = reg.starting_address as usize;
        let size = reg.size?;
        let width = node.property("width")?.as_usize()?;
        let height = node.property("height")?.as_usize()?;
        let stride = node.property("stride")?.as_usize()?;
        let format = node.property("format")?.as_str()?;
        let red_low = match format {
            "a8b8g8r8" | "x8b8g8r8" => true,
            "a8r8g8b8" | "x8r8g8b8" => false,
            _ => return None,
        };
        let rotation = node
            .property("scarlet,rotation")
            .and_then(|prop| prop.as_usize())
            .unwrap_or(0);
        if rotation != 0 && rotation != 3 {
            return None;
        }
        let bytes = stride.checked_mul(height)?;
        let end = base.checked_add(bytes)?;
        if base == 0
            || base & 3 != 0
            || stride & 3 != 0
            || !(64..=4096).contains(&width)
            || !(64..=4096).contains(&height)
            || stride < width.checked_mul(4)?
            || bytes > size
            || bytes > 16 * 1024 * 1024
        {
            return None;
        }
        let kernel_start = (&raw const __KERNEL_SPACE_START) as usize;
        let kernel_end = (&raw const __KERNEL_SPACE_END) as usize;
        if overlaps(base, end, kernel_start, kernel_end)
            || overlaps(base, end, dtb, dtb.checked_add(fdt.total_size())?)
        {
            return None;
        }
        if let Some(chosen) = fdt.find_node("/chosen") {
            let initrd_start = chosen
                .property("linux,initrd-start")
                .and_then(|p| p.as_usize());
            let initrd_end = chosen
                .property("linux,initrd-end")
                .and_then(|p| p.as_usize());
            if let (Some(start), Some(stop)) = (initrd_start, initrd_end) {
                if overlaps(base, end, start, stop) {
                    return None;
                }
            }
        }
        Some(Self {
            base,
            width,
            height,
            stride,
            rotate: rotation == 3,
            red_low,
        })
    }

    fn logical_size(&self) -> (usize, usize) {
        if self.rotate {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }

    fn pixel(&self, x: usize, y: usize, color: u32) {
        let (w, h) = self.logical_size();
        if x >= w || y >= h {
            return;
        }
        // Same coordinate transform as U-Boot's vidconsole3.
        let (px, py) = if self.rotate {
            (y, self.height - 1 - x)
        } else {
            (x, y)
        };
        let value = if self.red_low {
            (color & 0xff00_ff00) | ((color & 0xff) << 16) | ((color >> 16) & 0xff)
        } else {
            color
        };
        // SAFETY: dimensions/stride/reg and their product were checked above.
        unsafe {
            write_volatile((self.base + py * self.stride + px * 4) as *mut u32, value);
        }
    }

    fn clear(&self) {
        let (width, height) = self.logical_size();
        for y in 0..height {
            for x in 0..width {
                self.pixel(x, y, 0xff10_1520);
            }
        }
    }

    fn glyph(&self, x: usize, y: usize, ch: char) {
        let Some(glyph) = font8x8::BASIC_FONTS.get(ch) else {
            return;
        };
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..8 {
                if bits & (1 << col) == 0 {
                    continue;
                }
                for dy in 0..2 {
                    for dx in 0..2 {
                        self.pixel(x + col * 2 + dx, y + row * 2 + dy, 0xfff4_766f);
                    }
                }
            }
        }
    }
}

enum Uart {
    Pl011(usize),
    Tegra(usize),
}

impl Uart {
    fn from_node(node: &FdtNode<'_, '_>) -> Option<Self> {
        if !enabled(node) {
            return None;
        }
        let base = node.reg()?.next()?.starting_address as usize;
        if compatible(node, "arm,pl011") {
            return Some(Self::Pl011(base));
        }
        if compatible(node, "nvidia,tegra20-uart")
            && [0x7000_6000, 0x7000_6040, 0x7000_6200].contains(&base)
        {
            return Some(Self::Tegra(base));
        }
        None
    }

    fn putc(&self, byte: u8) {
        let (base, status, mask, ready_when_set) = match *self {
            Self::Pl011(base) => (base, 0x18, 1 << 5, false),
            Self::Tegra(base) => (base, 5 * 4, 1 << 5, true),
        };
        // Firmware owns clock/reset/pinmux/baud setup. Never hang on a dead UART.
        for _ in 0..100_000 {
            let value = unsafe { read_volatile((base + status) as *const u32) };
            if (value & mask != 0) == ready_when_set {
                unsafe {
                    write_volatile(base as *mut u32, byte as u32);
                }
                return;
            }
            core::hint::spin_loop();
        }
    }
}

struct Console {
    framebuffer: Option<Framebuffer>,
    uart: Option<Uart>,
    x: usize,
    y: usize,
}

impl Write for Console {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for ch in text.chars() {
            if let Some(uart) = &self.uart {
                if ch == '\n' {
                    uart.putc(b'\r');
                }
                uart.putc(ch as u8);
            }
            if ch == '\n' {
                self.x = 24;
                self.y += 24;
                continue;
            }
            if let Some(fb) = &self.framebuffer {
                fb.glyph(self.x, self.y, ch);
            }
            self.x += 16;
        }
        Ok(())
    }
}
