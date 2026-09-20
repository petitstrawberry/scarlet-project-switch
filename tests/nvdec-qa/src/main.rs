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
    if let Err(error) = run() {
        eprintln!("[nvdec-qa] FAIL: {error}");
        std::process::exit(1);
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
