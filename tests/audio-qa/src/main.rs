//! Manually launched audible left/right/reopen/resampling check. Never autostarts.
use sas_client::{SasClient, StreamConfig};
use std::{
    thread::sleep,
    time::{Duration, Instant},
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let silence = args.iter().any(|arg| arg == "--silence");
    let short = args.iter().any(|arg| arg == "--short");
    let rates: &[u32] = if silence || short {
        &[48000]
    } else {
        &[48000, 44100]
    };
    for &rate in rates {
        let mut client = SasClient::connect().expect("connect SAS");
        let mut stream = client
            .configure(&StreamConfig {
                format: 1,
                rate,
                channels: 2,
                period_frames: rate / 50,
                buffer_frames: rate / 5,
            })
            .expect("configure SAS");
        println!(
            "audio-qa: {rate} Hz: {}",
            if silence {
                "digital silence (2 seconds)"
            } else {
                "left 440 Hz, right 880 Hz, stereo, silence"
            }
        );
        let start = Instant::now();
        let frames_per_segment = if short { rate / 4 } else { rate };
        for second in 0..if silence { 2 } else { 4 } {
            let mut pcm = Vec::with_capacity(frames_per_segment as usize * 4);
            for frame in 0..frames_per_segment {
                // Short fades make channel transitions free of discontinuities.
                let fade = (frame.min(frames_per_segment - frame - 1) as f32
                    / (rate as f32 * 0.02))
                    .min(1.0);
                for ch in 0..2 {
                    let enabled = !silence
                        && (second == 2 || (second == 0 && ch == 0) || (second == 1 && ch == 1));
                    let phase = core::f32::consts::TAU
                        * (if ch == 0 { 440.0 } else { 880.0 })
                        * frame as f32
                        / rate as f32;
                    let value = if enabled {
                        (phase.sin() * fade * 12000.0) as i16
                    } else {
                        0
                    };
                    pcm.extend_from_slice(&value.to_le_bytes());
                }
            }
            let mut position = 0;
            let deadline = Instant::now() + Duration::from_secs(5);
            while position < pcm.len() {
                assert!(!stream.is_closed(), "SAS closed the stream");
                let frames = stream.write(&pcm[position..]);
                position += frames * 4;
                if frames == 0 {
                    assert!(Instant::now() < deadline, "SAS failed to consume PCM");
                    sleep(Duration::from_millis(2));
                }
            }
        }
        client.drain().expect("drain SAS");
        println!(
            "audio-qa: {rate} Hz drained {} frames in {:?}",
            stream.read_frames(),
            start.elapsed()
        );
        client.close().expect("close SAS");
        sleep(Duration::from_millis(300));
    }
    println!("audio-qa: completed; audible channel correctness requires listener confirmation");
}
