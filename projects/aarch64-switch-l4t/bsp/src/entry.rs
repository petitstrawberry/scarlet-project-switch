use core::arch::{asm, naked_asm};
use scarlet_modules::scarlet;

#[repr(C, align(16))]
struct BootStack([u8; 64 * 1024]);

// Initialized data survives the kernel's subsequent BSS clearing.
#[unsafe(link_section = ".data.switch_boot_stack")]
static mut SWITCH_BOOT_STACK: BootStack = BootStack([0xa5; 64 * 1024]);

#[unsafe(link_section = ".scarlet_ksyms")]
#[used]
static KSYM_PLACEHOLDER: [u64; 65536] = [0; 65536];

/// Physical-link arm64 Image header; the legacy uImage load/entry are identical.
#[unsafe(link_section = ".head.text.switch_header")]
#[unsafe(export_name = "_switch_head")]
#[unsafe(naked)]
pub extern "C" fn image_head() -> ! {
    naked_asm!(
        "b {entry}",
        ".word 0",
        ".quad 0x200000",
        ".quad __KERNEL_IMAGE_SIZE",
        ".quad 0x2",
        ".quad 0", ".quad 0", ".quad 0",
        ".word 0x644d5241",
        ".word 0",
        entry = sym image_entry,
    );
}

/// Capture the incoming EL before Scarlet's Linux bootstrap drops EL2 to EL1.
#[unsafe(link_section = ".head.text.switch_entry")]
#[unsafe(naked)]
pub extern "C" fn image_entry() -> ! {
    naked_asm!(
        "msr daifset, #0xf",
        "mrs x1, CurrentEL",
        "lsr x1, x1, #2",
        "cmp x1, #1",
        "b.eq 1f",
        "cmp x1, #2",
        "b.ne 3f",
        "mrs x2, sctlr_el2",
        "b 2f",
        "1:",
        "mrs x2, sctlr_el1",
        "2:",
        "tbnz x2, #0, 3f",
        "tbnz x2, #2, 3f",
        "msr spsel, #1",
        "adrp x2, {stack}",
        "add x2, x2, :lo12:{stack}",
        "add x2, x2, #16, lsl #12",
        "mov sp, x2",
        "b {rust_entry}",
        "3:", "wfe", "b 3b",
        stack = sym SWITCH_BOOT_STACK,
        rust_entry = sym switch_entry,
    );
}

pub extern "C" fn switch_entry(dtb_paddr: usize, current_el: usize) -> ! {
    scarlet::mem::init_bss();
    if early::report(dtb_paddr, current_el) {
        scarlet_modules::force_link();
        // Re-enter the unmodified standard Image stub with the original DTB.
        unsafe {
            asm!("br {entry}", in("x0") dtb_paddr,
                entry = in(reg) scarlet::arch::aarch64::boot::linux::image_entry as *const (),
                options(noreturn));
        }
    }
    loop {
        unsafe {
            asm!("wfe", options(nomem, nostack));
        }
    }
}

/// Use Scarlet's standard PSCI entry without replaying the BSP boot probe.
#[unsafe(export_name = "_entry_ap")]
#[unsafe(naked)]
pub extern "C" fn secondary_entry() -> ! {
    naked_asm!("b {entry}", entry = sym scarlet::arch::aarch64::boot::linux::secondary_image_entry);
}
