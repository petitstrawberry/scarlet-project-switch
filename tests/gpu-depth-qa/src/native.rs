use crate::scene::*;
use gpu_raw::*;
use maxwell_submit_wire as wire;
use scarlet_os::handle::capability::memory_mapping::{MemoryMappingOps, flags, prot};
use sgfx_codegen_maxwell::{ObjectRef, RelocatableCommands};

#[track_caller]
fn checked<T, E: core::fmt::Debug>(value: Result<T, E>) -> Result<T, String> {
    let location = core::panic::Location::caller();
    value.map_err(|error| format!("{}:{}: {error:?}", location.file(), location.line()))
}

fn encode(commands: &RelocatableCommands, resources: &[wire::Resource]) -> Result<Vec<u8>, String> {
    let relocations: Vec<_> = commands
        .fixups
        .iter()
        .map(|fixup| {
            let ObjectRef::External(object) = fixup.object else {
                panic!("unexpected generated shader");
            };
            wire::Relocation {
                commands_word_offset: fixup.word_offset,
                source: wire::RelocationSource::Attachment(object.raw()),
                resource_offset: fixup.object_offset,
                required_size: fixup.required_size,
                access: u32::from(fixup.access.bits()),
                encoding: wire::AddressEncoding::GpuVa64,
            }
        })
        .collect();
    let submit = wire::Submit {
        commands: &commands.words,
        resources,
        relocations: &relocations,
    };
    let mut bytes = vec![0; checked(wire::encoded_len(submit))?];
    checked(wire::encode(submit, &mut bytes))?;
    Ok(bytes)
}

pub fn run() -> Result<(), String> {
    let gpu = checked(Gpu::open("/dev/gpu0"))?;
    let info = checked(gpu.query_info())?;
    if info.backend_id_bytes() != b"nvidia-gm20b"
        || info.execution_support & GPU_EXECUTION_SUPPORT_DEPTH == 0
    {
        return Err("GM20B depth support required".into());
    }
    let context = checked(gpu.create_context(&checked(gpu.query_dialect(0))?))?;
    let queue = checked(context.create_queue())?;
    let color = checked(gpu.create_image_with_format_and_usage(
        GPU_IMAGE_FORMAT_BGRA8_UNORM,
        WIDTH,
        HEIGHT,
        GPU_IMAGE_USAGE_RENDER_TARGET
            | GPU_IMAGE_USAGE_DEPTH_COMPATIBLE
            | GPU_IMAGE_USAGE_TRANSFER_SRC
            | GPU_IMAGE_USAGE_TRANSFER_DST,
    ))?;
    let depth = checked(gpu.create_image_with_format_and_usage(
        GPU_IMAGE_FORMAT_DEPTH32_FLOAT,
        WIDTH,
        HEIGHT,
        GPU_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT,
    ))?;
    let vertex = checked(gpu.create_buffer(VERTEX_BYTES, GPU_BUFFER_FLAG_CPU_VISIBLE))?;
    let tokens = [
        checked(context.attach_image(&color))?,
        checked(context.attach_image(&depth))?,
        checked(context.attach_buffer(&vertex))?,
    ];
    let resources = resources();
    for (image, meta) in [(&color, &resources[0]), (&depth, &resources[1])] {
        let layout = checked(image.query_layout())?;
        let sgfx_codegen_maxwell::ResourceKind::Image(meta) = &meta.kind else {
            unreachable!()
        };
        if layout.planes[0].row_pitch != meta.planes[0].stride
            || layout.planes[0].size != meta.planes[0].size
        {
            return Err(format!("unexpected image layout: {layout:?}"));
        }
    }
    let mapping = checked(vertex.as_handle().as_memory_mapping())?;
    let length = vertex.allocated_size() as usize;
    // The buffer is exclusively owned and no command is submitted until filled.
    let address =
        checked(unsafe { mapping.mmap(0, length, prot::READ | prot::WRITE, flags::SHARED, 0) })?;
    let points = [
        [-1.0f32, -1.0],
        [1.0, -1.0],
        [-1.0, 1.0],
        [-1.0, 1.0],
        [1.0, -1.0],
        [1.0, 1.0],
    ];
    for (quad, z) in [-0.5f32, 0.5].into_iter().enumerate() {
        for (i, [x, y]) in points.into_iter().enumerate() {
            let data = [x, y, z, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
            unsafe {
                core::ptr::copy_nonoverlapping(
                    data.as_ptr() as *const u8,
                    (address + (quad * 6 + i) * STRIDE as usize) as *mut u8,
                    STRIDE as usize,
                );
            }
        }
    }
    checked(unsafe { MemoryMappingOps::munmap(address, length) })?;
    let authority: Vec<_> = resources
        .iter()
        .zip(tokens)
        .map(|(meta, token)| wire::Resource {
            attachment_token: token,
            range_offset: 0,
            range_size: meta.size,
            access: if meta.id.raw() == 2 {
                wire::ACCESS_READ
            } else {
                wire::ACCESS_READ | wire::ACCESS_WRITE
            },
        })
        .collect();
    let mut pixels = vec![0; (WIDTH * HEIGHT * 4) as usize];
    for &(compare, write, disable, expected) in CASES {
        println!("[gm20b-depth-qa] testing {compare:?} write={write} disable={disable}");
        let commands = checked(compile_scene(&resources, compare, write, disable))?;
        checked(queue.submit(&encode(&commands, &authority)?))?;
        checked(context.readback_image_bgra(
            &color,
            &mut pixels,
            WIDTH * 4,
            GpuImageBgraRect::new(0, 0, WIDTH, HEIGHT),
        ))?;
        if let Some((index, pixel)) = pixels
            .chunks_exact(4)
            .enumerate()
            .find(|(_, pixel)| *pixel != expected)
        {
            return Err(format!(
                "{compare:?} write={write} disable={disable} pixel({}, {})={pixel:?}, expected {expected:?}",
                index % WIDTH as usize,
                index / WIDTH as usize
            ));
        }
        println!("[gm20b-depth-qa] {compare:?} write={write} disable={disable} PASS");
    }
    let base = checked(compile_scene(
        &resources,
        sgfx_core::ir::CompareFunction::Less,
        true,
        false,
    ))?;
    let mut reversed = base.clone();
    // Each draw carries the exact authorized vertex range. Move the complete
    // records and their relocations together, including that range.
    for field in 0..64 {
        reversed.words.swap(2 * 64 + field, 3 * 64 + field);
    }
    for fixup in &mut reversed.fixups {
        match fixup.word_offset / 64 {
            2 => fixup.word_offset += 64,
            3 => fixup.word_offset -= 64,
            _ => {}
        }
    }
    reversed
        .fixups
        .sort_unstable_by_key(|fixup| fixup.word_offset);
    checked(queue.submit(&encode(&reversed, &authority)?))?;
    checked(context.readback_image_bgra(
        &color,
        &mut pixels,
        WIDTH * 4,
        GpuImageBgraRect::new(0, 0, WIDTH, HEIGHT),
    ))?;
    if pixels
        .chunks_exact(4)
        .any(|pixel| pixel != [0, 0, 255, 255])
    {
        return Err("reversing draw order changed the nearest surface".into());
    }
    println!("[gm20b-depth-qa] reversed draw order PASS");

    let mut partial = base.clone();
    // Clear depth to zero everywhere, then reopen only the left half to one.
    let mut rect_clear = partial.words[..64].to_vec();
    rect_clear[15] = WIDTH / 2;
    partial.words[32] = 0;
    partial.words.splice(64..64, rect_clear);
    let mut clear_fixup = partial.fixups[0];
    clear_fixup.word_offset += 64;
    for fixup in &mut partial.fixups[1..] {
        fixup.word_offset += 64;
    }
    partial.fixups.insert(1, clear_fixup);
    checked(queue.submit(&encode(&partial, &authority)?))?;
    checked(context.readback_image_bgra(
        &color,
        &mut pixels,
        WIDTH * 4,
        GpuImageBgraRect::new(0, 0, WIDTH, HEIGHT),
    ))?;
    for (i, pixel) in pixels.chunks_exact(4).enumerate() {
        let expected = if i % (WIDTH as usize) < (WIDTH / 2) as usize {
            [0, 0, 255, 255]
        } else {
            [0, 0, 0, 255]
        };
        if pixel != expected {
            return Err(format!(
                "partial depth clear failed at pixel {i}: {pixel:?}"
            ));
        }
    }
    println!("[gm20b-depth-qa] partial depth clear PASS");

    let patch = [0, 255, 0, 255].repeat(17 * 5);
    checked(context.upload_image_bgra(
        &color,
        &patch,
        17 * 4,
        GpuImageBgraRect::new(15, 126, 17, 5),
    ))?;
    checked(context.readback_image_bgra(
        &color,
        &mut pixels,
        WIDTH * 4,
        GpuImageBgraRect::new(0, 0, WIDTH, HEIGHT),
    ))?;
    for (i, pixel) in pixels.chunks_exact(4).enumerate() {
        let (x, y) = (i % WIDTH as usize, i / WIDTH as usize);
        let expected = if (15..32).contains(&x) && (126..131).contains(&y) {
            [0, 255, 0, 255]
        } else if x < (WIDTH / 2) as usize {
            [0, 0, 255, 255]
        } else {
            [0, 0, 0, 255]
        };
        if pixel != expected {
            return Err(format!("partial tiled upload changed pixel {i}: {pixel:?}"));
        }
    }
    println!("[gm20b-depth-qa] partial tiled upload/readback preserves untouched pixels PASS");
    for (word, value) in [
        (2 * 64 + 60, 9),
        (2 * 64 + 61, 2),
        (2 * 64 + 56, WIDTH - 1),
        (32, f32::NAN.to_bits()),
    ] {
        let mut invalid = base.clone();
        invalid.words[word] = value;
        if queue.submit(&encode(&invalid, &authority)?).is_ok() {
            return Err(format!("invalid field {word} accepted"));
        }
    }
    checked(queue.submit(&encode(&base, &authority)?))?;
    println!("[gm20b-depth-qa] invalid records rejected; queue remains usable PASS");
    checked(context.detach_buffer(&vertex))?;
    checked(context.detach_image(&depth))?;
    checked(context.detach_image(&color))?;
    println!("[gm20b-depth-qa] ALL PASS");
    Ok(())
}
