//! MMU/cache-off diagnostic DMA. The arena belongs to this Image until SoC
//! reset and is never freed after cancellation, completion or timeout.
use super::{
    Hardware,
    chip::{Bus, Chip, Shared},
    control::{self, ControlPort, DmaMemory},
    msgbuf::Memory,
    pcie::{CS, Io},
};
use core::{
    arch::asm,
    cell::{Cell, UnsafeCell},
    fmt::Write,
    ptr::{read_volatile, write_volatile},
};
const SIZE: usize = 2 * 1024 * 1024;
#[repr(align(4096))]
struct Arena(UnsafeCell<[u8; SIZE]>);
// Single boot CPU; CPU accesses use volatile operations and are serialized.
// Device-owned pages are retained for the whole diagnostic lifetime.
unsafe impl Sync for Arena {}
static ARENA: Arena = Arena(UnsafeCell::new([0; SIZE]));

struct Dma<'a, B> {
    bus: &'a B,
    chip: &'a Chip,
    cursor: Cell<usize>,
}
impl<B: Bus> Dma<'_, B> {
    fn base(&self) -> usize {
        ARENA.0.get() as usize
    }
    fn allocate(&self, bytes: usize) -> Result<u64, &'static str> {
        let start = self
            .cursor
            .get()
            .checked_add(63)
            .ok_or("DMA arena overflow")?
            & !63;
        let end = start.checked_add(bytes).ok_or("DMA arena overflow")?;
        if end > SIZE {
            return Err("diagnostic DMA arena full");
        }
        self.cursor.set(end);
        Ok((self.base() + start) as u64)
    }
    fn range(&self, address: u64, bytes: usize) -> Result<usize, &'static str> {
        let offset = address
            .checked_sub(self.base() as u64)
            .ok_or("DMA access below arena")?;
        if offset
            .checked_add(bytes as u64)
            .is_none_or(|end| end > self.cursor.get() as u64)
        {
            return Err("DMA access outside allocated arena");
        }
        Ok(address as usize)
    }
}
impl<B: Bus> Memory for Dma<'_, B> {
    fn read_tcm(&self, address: u32) -> Result<u32, &'static str> {
        if address & 3 != 0 || !self.chip.ram_contains(address, 4) {
            return Err("invalid ring TCM read");
        }
        self.bus.ram_read32(address)
    }
    fn write_tcm(&self, address: u32, value: u32) -> Result<(), &'static str> {
        if address & 3 != 0 || !self.chip.ram_contains(address, 4) {
            return Err("invalid ring TCM write");
        }
        unsafe {
            asm!("dsb sy", options(nostack));
        }
        self.bus.ram_write32(address, value)
    }
    fn read_dma(&self, address: u64, output: &mut [u8]) -> Result<(), &'static str> {
        let start = self.range(address, output.len())?;
        for (index, byte) in output.iter_mut().enumerate() {
            *byte = unsafe { read_volatile((start + index) as *const u8) };
        }
        Ok(())
    }
    fn write_dma(&self, address: u64, input: &[u8]) -> Result<(), &'static str> {
        let start = self.range(address, input.len())?;
        for (index, byte) in input.iter().enumerate() {
            unsafe {
                write_volatile((start + index) as *mut u8, *byte);
            }
        }
        Ok(())
    }
    fn sync_for_device(&self, address: u64, bytes: usize) -> Result<(), &'static str> {
        self.range(address, bytes)?;
        unsafe {
            asm!("dsb sy", options(nostack));
        }
        Ok(())
    }
    fn sync_for_cpu(&self, address: u64, bytes: usize) -> Result<(), &'static str> {
        self.range(address, bytes)?;
        unsafe {
            asm!("dsb sy", options(nostack));
        }
        Ok(())
    }
    fn doorbell(&self) -> Result<(), &'static str> {
        self.bus.write32(self.chip.pcie.base + 0x140, 1)
    }
}

impl<B: Bus> DmaMemory for Dma<'_, B> {
    fn allocate_dma(&self, bytes: usize) -> Result<u64, &'static str> {
        self.allocate(bytes)
    }
}

pub(super) fn command(
    hw: &Hardware,
    io: &impl Memory,
    port: &mut ControlPort,
    cookie: u64,
    command: u32,
    input: &[u8],
    limit: u16,
    output: &mut [u8],
    events: &mut impl FnMut(&[u8]) -> Result<(), &'static str>,
) -> Result<usize, &'static str> {
    port.submit(io, cookie, command, input, limit)?;
    for _ in 0..2000 {
        match port.poll(io, output)? {
            Some(control::Completion::Response {
                cookie: reply,
                status,
                length,
                cancelled,
            }) => {
                hw.observe("brcm-ioctl-command", command);
                hw.observe("brcm-ioctl-status", status as i32 as u32);
                if reply != cookie || cancelled {
                    return Err("diagnostic command cookie mismatch");
                }
                if status != 0 {
                    return Err("firmware command failed");
                }
                return Ok(length as usize);
            }
            Some(control::Completion::Event { length }) => events(&output[..length as usize])?,
            _ => {}
        }
        hw.delay_us(1000);
    }
    port.quarantine();
    Err("firmware IOCTL completion timed out")
}

pub(super) fn probe(
    hw: &Hardware,
    bus: &impl Bus,
    chip: &Chip,
    shared: Shared,
) -> Result<(), &'static str> {
    let sctlr: u64;
    unsafe {
        asm!("mrs {}, sctlr_el1", out(reg) sctlr, options(nomem, nostack));
    }
    if sctlr & 5 != 0 {
        return Err("diagnostic DMA requires MMU/cache disabled");
    }
    hw.stage("brcm-dma-setup");
    let io = Dma {
        bus,
        chip,
        cursor: Cell::new(0),
    };
    let mut port = ControlPort::new(&io, bus, chip, shared)?;
    hw.observe("brcm-dma-arena", io.base() as u32);
    hw.write32(CS + 4, (hw.read32(CS + 4) & 0xffff) | 6);
    if hw.read32(CS + 4) & 6 != 6 {
        return Err("BCM bus master enable failed");
    }
    if shared.flags & 0x10000000 != 0 {
        bus.write32(chip.pcie.base + 0x144, 1)?;
    }
    port.start(&io)?;
    hw.stage("brcm-ioctl-get-version");
    let mut output = [0; control::BUFFER_SIZE];
    let length = command(
        hw,
        &io,
        &mut port,
        1,
        1,
        &[0; 4],
        4,
        &mut output,
        &mut |_| Ok(()),
    )?;
    if length != 4 || u32::from_le_bytes(output[..4].try_into().unwrap()) != 2 {
        return Err("unsupported firmware IOCTL version");
    }
    let _ = writeln!(
        hw.0.clone(),
        "BRCM_IOCTL_READY version=2 dma_roundtrip=true"
    );
    #[cfg(feature = "radio-scan")]
    super::scan::run(hw, &io, &mut port, &mut output)?;
    Ok(())
}
