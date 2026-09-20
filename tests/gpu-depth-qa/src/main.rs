//! Explicit correctness probe; never started by boot or the desktop launcher.
#[cfg(target_os = "scarlet")]
mod native;
mod scene;

fn main() {
    #[cfg(target_os = "scarlet")]
    if let Err(error) = native::run() {
        eprintln!("[gm20b-depth-qa] FAIL: {error}");
        std::process::exit(1);
    }
    #[cfg(not(target_os = "scarlet"))]
    eprintln!("Run this correctness probe on Scarlet; host checks use cargo test.");
}
