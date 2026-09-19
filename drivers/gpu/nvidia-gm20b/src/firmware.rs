// SPDX-License-Identifier: GPL-2.0-only
//! Checked NVIDIA firmware decoding and the GM200/GM20B WPR layout.
//! Structure meanings follow Linux v6.12 nvfw/{fw,hs,ls,acr,flcn,pmu},
//! acr/{lsfw,gm200,gm20b} and falcon/fw. See NOTICE for upstream licences.

use alloc::{vec, vec::Vec};
use scarlet::{
    device::manager::PROBE_DEFER, fs::manager::get_global_vfs_manager_safe, object::KernelObject,
};

pub fn word(bytes: &[u8], offset: usize) -> Result<u32, &'static str> {
    let end = offset.checked_add(4).ok_or("firmware offset overflow")?;
    let value = bytes
        .get(offset..end)
        .ok_or("firmware field outside blob")?;
    Ok(u32::from_le_bytes(value.try_into().unwrap()))
}

fn range(bytes: &[u8], offset: u32, size: u32) -> Result<&[u8], &'static str> {
    let end = offset.checked_add(size).ok_or("firmware range overflow")?;
    bytes
        .get(offset as usize..end as usize)
        .ok_or("firmware range outside blob")
}

fn align(value: usize, alignment: usize) -> usize {
    // All values have already been bounded to the small pinned firmware blobs.
    (value + alignment - 1) & !(alignment - 1)
}

fn load(name: &str, expected: usize) -> Result<Vec<u8>, &'static str> {
    let vfs = get_global_vfs_manager_safe().ok_or(PROBE_DEFER)?;
    let path = alloc::format!("/lib/firmware/nvidia/gm20b/{name}.bin");
    let object = vfs
        .open(&path, 0)
        .map_err(|_| "GM20B firmware file unavailable")?;
    let KernelObject::File(file) = object else {
        return Err("GM20B firmware path is not a file");
    };
    if file
        .metadata()
        .map_err(|_| "firmware metadata failed")?
        .size
        != expected
    {
        return Err("GM20B firmware size differs from pinned image");
    }
    let mut bytes = vec![0; expected];
    let mut offset = 0;
    while offset < bytes.len() {
        let count = file
            .read(&mut bytes[offset..])
            .map_err(|_| "firmware read failed")?;
        if count == 0 || count > bytes.len() - offset {
            return Err("GM20B firmware read length invalid");
        }
        offset += count;
    }
    Ok(bytes)
}

pub struct Boot {
    pub code: Vec<u8>,
    pub address: u32,
}

impl Boot {
    fn parse(bytes: &[u8]) -> Result<Self, &'static str> {
        let container = Container::parse(bytes)?;
        let header = range(bytes, container.header, 24)?;
        let code = range(container.data, word(header, 8)?, word(header, 12)?)?;
        if code.is_empty() || code.len() > 4096 {
            return Err("Falcon bootloader size invalid");
        }
        let address = word(header, 0)?
            .checked_mul(256)
            .ok_or("Falcon boot tag overflow")?;
        let mut padded = vec![0; align(code.len(), 256)];
        padded[..code.len()].copy_from_slice(code);
        Ok(Self {
            code: padded,
            address,
        })
    }
}

struct Container<'a> {
    header: u32,
    data: &'a [u8],
}

impl<'a> Container<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self, &'static str> {
        // Nouveau's earlier NVIDIA files have bin_size=0 and direct patch offsets.
        if word(bytes, 0)? != 0x3b1d14f0 || word(bytes, 4)? != 1 {
            return Err("unsupported NVIDIA firmware container");
        }
        let size = word(bytes, 8)?;
        if size != 0 && size as usize != bytes.len() {
            return Err("NVIDIA firmware container length invalid");
        }
        Ok(Self {
            header: word(bytes, 12)?,
            data: range(bytes, word(bytes, 16)?, word(bytes, 20)?)?,
        })
    }
}

pub struct Acr {
    pub image: Vec<u8>,
    pub boot: Boot,
    pub nonsecure_offset: u32,
    pub nonsecure_size: u32,
    pub secure_offset: u32,
    pub secure_size: u32,
    pub data_offset: u32,
    pub data_size: u32,
    pub signature_offset: usize,
    signatures: [[u8; 16]; 2],
}

impl Acr {
    fn parse(bytes: &[u8], boot: &[u8]) -> Result<Self, &'static str> {
        let container = Container::parse(bytes)?;
        let header = range(bytes, container.header, 32)?;
        let layout = range(bytes, word(header, 24)?, word(header, 28)?)?;
        if word(layout, 16)? != 1 || word(header, 4)? != 16 || word(header, 12)? != 16 {
            return Err("unsupported ACR application/signature count");
        }
        let nonsecure_offset = word(layout, 0)?;
        let nonsecure_size = word(layout, 4)?;
        let data_offset = word(layout, 8)?;
        let data_size = word(layout, 12)?;
        let secure_offset = word(layout, 20)?;
        let secure_size = word(layout, 24)?;
        range(container.data, nonsecure_offset, nonsecure_size)?;
        range(container.data, secure_offset, secure_size)?;
        range(container.data, data_offset, data_size)?;
        if data_size < 0x270 || !secure_offset.is_multiple_of(256) {
            return Err("ACR data descriptor or code alignment invalid");
        }
        let signature_offset = word(header, 16)? as usize;
        if signature_offset < data_offset as usize
            || signature_offset
                .checked_add(16)
                .is_none_or(|end| end > (data_offset + data_size) as usize)
        {
            return Err("ACR signature patch outside DMEM image");
        }
        let mut signatures = [[0; 16]; 2];
        for (signature, offset) in signatures
            .iter_mut()
            .zip([word(header, 8)?, word(header, 0)?])
        {
            let offset = offset
                .checked_add(word(header, 20)?)
                .ok_or("ACR signature offset overflow")?;
            signature.copy_from_slice(range(bytes, offset, 16)?);
        }
        Ok(Self {
            image: container.data.to_vec(),
            boot: Boot::parse(boot)?,
            nonsecure_offset,
            nonsecure_size,
            secure_offset,
            secure_size,
            data_offset,
            data_size,
            signature_offset,
            signatures,
        })
    }

    pub fn patch(&self, image: &mut [u8], debug: bool, shadow: u64, size: u32) {
        image[self.signature_offset..self.signature_offset + 16]
            .copy_from_slice(&self.signatures[debug as usize]);
        // flcn_acr_desc: union[0x200], region fields, blob_size@0x240,
        // eight-byte-aligned blob_base@0x248. Secure firmware performs the copy.
        let descriptor = self.data_offset as usize;
        image[descriptor + 0x240..descriptor + 0x244].copy_from_slice(&size.to_le_bytes());
        image[descriptor + 0x248..descriptor + 0x250].copy_from_slice(&shadow.to_le_bytes());
    }
}

pub struct Ls {
    image: Vec<u8>,
    signature: [u8; 76],
    boot_size: u32,
    boot_address: u32,
    app_start: u32,
    app_size: u32,
    app_entry: u32,
    code_offset: u32,
    code_size: u32,
    data_offset: u32,
    data_size: u32,
    ucode_size: u32,
}

impl Ls {
    fn signature(bytes: &[u8], id: u32) -> Result<[u8; 76], &'static str> {
        if bytes.len() != 76 || word(bytes, 72)? != id {
            return Err("LS firmware signature Falcon ID invalid");
        }
        Ok(bytes.try_into().unwrap())
    }

    fn pmu(desc: &[u8], image: Vec<u8>, signature: &[u8]) -> Result<Self, &'static str> {
        if word(desc, 0)? as usize != desc.len() || word(desc, 4)? as usize != image.len() {
            return Err("PMU firmware descriptor length invalid");
        }
        let boot_size = align(word(desc, 84)? as usize, 256) as u32;
        let app_start = word(desc, 96)?;
        let app_size = align(word(desc, 100)? as usize, 256) as u32;
        let code_offset = word(desc, 116)?;
        let code_size = word(desc, 120)?;
        let data_offset = word(desc, 124)?;
        let data_size = word(desc, 128)?;
        range(&image, word(desc, 80)?, boot_size)?;
        range(&image, app_start, app_size)?;
        range(
            &image,
            app_start
                .checked_add(code_offset)
                .ok_or("PMU code offset overflow")?,
            code_size,
        )?;
        range(
            &image,
            app_start
                .checked_add(data_offset)
                .ok_or("PMU data offset overflow")?,
            data_size,
        )?;
        let ucode_size = (align(data_offset as usize, 256) as u32)
            .checked_add(boot_size)
            .ok_or("PMU ucode size overflow")?;
        if ucode_size > boot_size + app_size {
            return Err("PMU ucode/data split invalid");
        }
        Ok(Self {
            image,
            signature: Self::signature(signature, 0)?,
            boot_size,
            boot_address: word(desc, 88)?,
            app_start,
            app_size,
            app_entry: word(desc, 108)?,
            code_offset,
            code_size,
            data_offset,
            data_size,
            ucode_size,
        })
    }

    fn fecs(boot: &[u8], inst: &[u8], data: &[u8], signature: &[u8]) -> Result<Self, &'static str> {
        let boot = Boot::parse(boot)?;
        let boot_size = boot.code.len() as u32;
        let code_size = align(inst.len(), 256) as u32;
        let data_size = align(data.len(), 256) as u32;
        let mut image = vec![0; (boot_size + code_size + data_size) as usize];
        image[..boot.code.len()].copy_from_slice(&boot.code);
        image[boot_size as usize..boot_size as usize + inst.len()].copy_from_slice(inst);
        image[(boot_size + code_size) as usize..(boot_size + code_size) as usize + data.len()]
            .copy_from_slice(data);
        Ok(Self {
            image,
            signature: Self::signature(signature, 2)?,
            boot_size,
            boot_address: boot.address,
            app_start: boot_size,
            app_size: code_size + data_size,
            app_entry: 0,
            code_offset: 0,
            code_size,
            data_offset: code_size,
            data_size,
            ucode_size: boot_size + code_size,
        })
    }
}

pub struct Firmware {
    pub acr: Acr,
    pmu: Ls,
    fecs: Ls,
    pub gpccs_inst: Vec<u8>,
    pub gpccs_data: Vec<u8>,
    pub noncontext: Vec<u8>,
    pub context: Vec<u8>,
    pub bundle: Vec<u8>,
    pub methods: Vec<u8>,
}

impl Firmware {
    pub fn load() -> Result<Self, &'static str> {
        let firmware = Self {
            acr: Acr::parse(&load("acr/ucode_load", 18592)?, &load("acr/bl", 832)?)?,
            pmu: Ls::pmu(
                &load("pmu/desc", 652)?,
                load("pmu/image", 47872)?,
                &load("pmu/sig", 76)?,
            )?,
            fecs: Ls::fecs(
                &load("gr/fecs_bl", 576)?,
                &load("gr/fecs_inst", 17021)?,
                &load("gr/fecs_data", 1964)?,
                &load("gr/fecs_sig", 76)?,
            )?,
            gpccs_inst: load("gr/gpccs_inst", 9964)?,
            gpccs_data: load("gr/gpccs_data", 2068)?,
            noncontext: load("gr/sw_nonctx", 1432)?,
            context: load("gr/sw_ctx", 5448)?,
            bundle: load("gr/sw_bundle_init", 7616)?,
            methods: load("gr/sw_method_init", 10800)?,
        };
        scarlet::println!("gm20b: firmware decoded; signed PMU/FECS, GPCCS and GR tables ready");
        Ok(firmware)
    }

    pub fn wpr(&self, base: u64, dmem_size: u32) -> Result<Vec<u8>, &'static str> {
        if dmem_size < 44 {
            return Err("PMU DMEM cannot hold secure-mode arguments");
        }
        let mut image = vec![0; 11 * 20]; // MAX_LSF WPR headers, as in Linux.
        let write = |image: &mut Vec<u8>, offset: usize, value: u32| {
            image[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        };
        for (index, (id, ls)) in [(0, &self.pmu), (2, &self.fecs)].into_iter().enumerate() {
            let lsb = align(image.len(), 256);
            let code = align(lsb + 124, 4096); // Signature76 + lsb_header_tail48.
            let bld = align(code + ls.image.len(), 256);
            image.resize(bld + 256, 0);
            for (word, value) in [id, lsb as u32, 0, u32::from(id != 0), 1]
                .into_iter()
                .enumerate()
            {
                write(&mut image, index * 20 + word * 4, value);
            }
            image[lsb..lsb + 76].copy_from_slice(&ls.signature);
            for (word, value) in [
                code as u32,
                ls.ucode_size,
                ls.app_size + ls.boot_size - ls.ucode_size,
                ls.boot_size,
                ls.boot_address,
                bld as u32,
                256,
                ls.app_start + ls.code_offset,
                ls.code_size,
                ls.app_start + ls.data_offset,
                ls.data_size,
                if id == 0 { 4 } else { 0 }, // DMACTL_REQ_CTX for PMU.
            ]
            .into_iter()
            .enumerate()
            {
                write(&mut image, lsb + 76 + word * 4, value);
            }
            image[code..code + ls.image.len()].copy_from_slice(&ls.image);
            let code_addr = base
                .checked_add((code as u32 + ls.app_start + ls.code_offset) as u64)
                .ok_or("WPR code address overflow")?;
            let data_addr = base
                .checked_add((code as u32 + ls.app_start + ls.data_offset) as u64)
                .ok_or("WPR data address overflow")?;
            if id == 0 {
                let words = [
                    0, // FALCON_DMAIDX_UCODE
                    (code_addr >> 8) as u32,
                    ls.app_size,
                    ls.code_size,
                    ls.app_entry,
                    (data_addr >> 8) as u32,
                    ls.data_size,
                    (code_addr >> 8) as u32,
                    1,
                    dmem_size - 44,
                    (code_addr >> 40) as u32,
                    (data_addr >> 40) as u32,
                    (code_addr >> 40) as u32,
                ];
                for (word, value) in words.into_iter().enumerate() {
                    write(&mut image, bld + word * 4, value);
                }
            } else {
                let words = [
                    0, // FALCON_DMAIDX_UCODE
                    (code_addr >> 8) as u32,
                    ls.code_offset,
                    ls.code_size,
                    0,
                    0,
                    ls.app_entry,
                    (data_addr >> 8) as u32,
                    ls.data_size,
                    (code_addr >> 40) as u32,
                    (data_addr >> 40) as u32,
                ];
                for (word, value) in words.into_iter().enumerate() {
                    write(&mut image, bld + 32 + word * 4, value);
                }
            }
        }
        write(&mut image, 2 * 20, u32::MAX);
        Ok(image)
    }
}
