//! Manual decoded-pixel correctness test, never launched during normal boot.
mod fixtures;

fn hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}
fn units(bytes: &[u8]) -> Vec<&[u8]> {
    let mut starts: Vec<_> = bytes
        .windows(4)
        .enumerate()
        .filter_map(|(i, w)| {
            (w == [0, 0, 1, 9]).then_some(if i > 0 && bytes[i - 1] == 0 { i - 1 } else { i })
        })
        .collect();
    starts.push(bytes.len());
    starts.windows(2).map(|w| &bytes[w[0]..w[1]]).collect()
}

#[cfg(target_os = "scarlet")]
fn abrupt_exit(mode: &str) -> Result<(), String> {
    use scarlet_video_client::{DecodedOutput, DecoderOptions, ScarletVideoDecoder, VideoFormat};
    // Include a live sibling so exit_group must clean shared handles/mappings.
    let _sibling = std::thread::spawn(|| {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    });
    let mut decoder = ScarletVideoDecoder::open_with_options(
        DecoderOptions::default().with_shared_images(mode == "shared"),
    )?;
    let input = units(fixtures::BASELINE);
    match mode {
        "open" => {}
        "pending" => {
            decoder.configure(VideoFormat::H264)?;
            decoder.submit(input[0], 1)?;
        }
        "mapped" | "shared" => {
            let frame = decoder
                .decode_output(VideoFormat::H264, input[0], 1)?
                .ok_or("missing decoded frame before abrupt exit")?;
            if mode == "shared" && !matches!(&frame, DecodedOutput::Image(_)) {
                return Err("shared output was not negotiated".into());
            }
            // No decoder or image Drop: only kernel process cleanup runs.
            std::process::exit(73);
        }
        _ => return Err("unknown abrupt-exit mode".into()),
    }
    std::process::exit(73);
}

#[cfg(target_os = "scarlet")]
fn wait_until_zombie(pid: u32) -> Result<(), String> {
    use std::{
        process::Command,
        thread,
        time::{Duration, Instant},
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        // Observe the task without wait/try_wait, which would reap it and hide
        // the regression. ps only waits for its own short-lived process.
        let output = Command::new("/bin/ps")
            .output()
            .map_err(|e| e.to_string())?;
        if String::from_utf8_lossy(&output.stdout).lines().any(|line| {
            let mut fields = line.split_whitespace();
            fields.next().and_then(|p| p.parse::<u32>().ok()) == Some(pid)
                && fields.next() == Some("Z")
        }) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("child {pid} did not become an unreaped zombie"));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(target_os = "scarlet")]
fn recovery(unreaped: bool) -> Result<(), String> {
    use scarlet_video_client::{ScarletVideoDecoder, VideoFormat};
    use std::{
        process::Command,
        thread,
        time::{Duration, Instant},
    };
    let executable = std::env::args().next().ok_or("missing executable path")?;
    for round in 0..3 {
        for mode in ["open", "pending", "mapped", "shared"] {
            let mut child = Command::new(&executable)
                .args(["--abrupt-exit", mode])
                .spawn()
                .map_err(|error| format!("spawn abrupt child: {error}"))?;
            let result = (|| -> Result<(), String> {
                if unreaped {
                    wait_until_zombie(child.id())?;
                } else {
                    let status = child.wait().map_err(|e| e.to_string())?;
                    if status.code() != Some(73) {
                        return Err(format!("{mode} child failed before abrupt exit: {status}"));
                    }
                }
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut decoder = loop {
                    match ScarletVideoDecoder::open() {
                        Ok(decoder) => break decoder,
                        Err(error) if Instant::now() >= deadline => {
                            return Err(format!(
                                "{mode} round={round} unreaped={unreaped} reopen failed: {error}"
                            ));
                        }
                        Err(_) => thread::sleep(Duration::from_millis(100)),
                    }
                };
                let input = units(fixtures::BASELINE);
                let frame = decoder
                    .decode(VideoFormat::H264, input[0], 1)?
                    .ok_or("missing recovery frame")?;
                if frame.width() != 160
                    || frame.height() != 90
                    || hash(frame.payload()) != fixtures::BASELINE_HASHES[0]
                {
                    return Err(format!("{mode} round={round}: recovery frame mismatch"));
                }
                Ok(())
            })();
            // Reap only AFTER reopening and decoding. Also clean up a failing
            // case so the old kernel is usable after demonstrating the bug.
            if result.is_err() {
                let _ = child.kill();
            }
            let status = child.wait().map_err(|e| e.to_string())?;
            result?;
            if status.code() != Some(73) {
                return Err(format!("{mode} child failed before abrupt exit: {status}"));
            }
            println!("[nvdec-qa] RECOVERY PASS mode={mode} round={round} unreaped={unreaped}");
        }
    }
    run()?;
    println!(
        "[nvdec-qa] RECOVERY ALL PASS: 12 abrupt exits and verified reopens unreaped={unreaped}"
    );
    Ok(())
}

#[cfg(target_os = "scarlet")]
fn run() -> Result<(), String> {
    use scarlet_video_client::{ScarletVideoDecoder, VideoFormat};
    for (name, encoded, hashes, width, height) in [
        (
            "baseline",
            fixtures::BASELINE,
            &fixtures::BASELINE_HASHES[..],
            160,
            90,
        ),
        ("high", fixtures::HIGH, &fixtures::HIGH_HASHES[..], 320, 180),
    ] {
        let input = units(encoded);
        if input.len() != hashes.len() {
            return Err("fixture access unit count mismatch".into());
        }
        // A second fresh session checks teardown/reopen and reference isolation.
        for round in 0..2 {
            let mut decoder = ScarletVideoDecoder::open()?;
            for (index, au) in input.iter().enumerate() {
                let frame = decoder
                    .decode(VideoFormat::H264, au, index as u64 + 1)?
                    .ok_or("missing decoded frame")?;
                if frame.width() != width
                    || frame.height() != height
                    || frame.timestamp() != index as u64 + 1
                {
                    return Err(format!(
                        "frame {index}: dimensions/timestamp wrong: {}x{} ts={}",
                        frame.width(),
                        frame.height(),
                        frame.timestamp()
                    ));
                }
                let value = hash(frame.payload());
                if value != hashes[index] {
                    println!("first luma bytes: {:02x?}", &frame.payload()[..64]);
                    return Err(format!(
                        "{name} frame {index}: NV12 hash={value:016x} expected={:016x}",
                        hashes[index]
                    ));
                }
                println!(
                    "[nvdec-qa] PASS {name} round={round} frame={index} {width}x{height} NV12={value:016x}"
                );
            }
        }
    }
    println!(
        "[nvdec-qa] ALL PASS: 72 exact frames, Baseline/High, crop, P/B references, 3 slices, reopen"
    );
    Ok(())
}
fn main() {
    #[cfg(target_os = "scarlet")]
    {
        let arguments: Vec<_> = std::env::args().skip(1).collect();
        let result = match arguments.first().map(String::as_str) {
            Some("--recovery") => recovery(false),
            Some("--recovery-unreaped") => recovery(true),
            Some("--abrupt-exit") => {
                abrupt_exit(arguments.get(1).map(String::as_str).unwrap_or(""))
            }
            None => run(),
            _ => Err("usage: nvdec-qa [--recovery | --recovery-unreaped]".into()),
        };
        if let Err(error) = result {
            eprintln!("[nvdec-qa] FAIL: {error}");
            std::process::exit(1);
        }
    }
    #[cfg(not(target_os = "scarlet"))]
    println!("NVDEC QA runs on Scarlet; host uses cargo test.");
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixtures_contain_expected_access_units() {
        assert_eq!(
            units(fixtures::BASELINE).len(),
            fixtures::BASELINE_HASHES.len()
        );
        assert_eq!(hash(b""), 0xcbf29ce484222325);
        assert_eq!(units(fixtures::HIGH).len(), fixtures::HIGH_HASHES.len());
    }
    #[test]
    fn fixtures_parse_in_expected_picture_order() {
        for (encoded, order, poc_type) in [
            (fixtures::BASELINE, &fixtures::BASELINE_ORDER[..], 2),
            (fixtures::HIGH, &fixtures::HIGH_ORDER[..], 0),
        ] {
            let mut context = scarlet_codecs::H264RequestContext::default();
            for (i, au) in units(encoded).iter().enumerate() {
                let request = context
                    .params_for_access_unit_with_timestamp(au, i as u64 + 1)
                    .unwrap();
                assert_eq!(request.params.sps.pic_order_cnt_type, poc_type);
                assert_eq!(
                    request.params.decode_params.top_field_order_cnt,
                    2 * order[i] as i32
                );
                assert_eq!(
                    request.params.scaling_matrix.scaling_list_4x4,
                    [[16; 16]; 6]
                );
                assert_eq!(
                    request.params.scaling_matrix.scaling_list_8x8,
                    [[16; 64]; 6]
                );
                for reference in &request.params.decode_params.dpb {
                    if reference.flags & 1 != 0 {
                        assert_eq!(reference.fields, 3);
                        assert!(reference.reference_ts < i as u64 + 1);
                    }
                }
            }
        }
    }
}
