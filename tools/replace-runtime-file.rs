//! Replace a network-staged runtime file without truncating the live path.
//! Build with the Scarlet Rust toolchain's aarch64-unknown-scarlet target.

use std::{env, fs, io, path::Path, process::ExitCode};

fn replace(source: &Path, destination: &Path, expected_size: u64) -> io::Result<()> {
    if !source.is_absolute()
        || !destination.is_absolute()
        || source == destination
        || source.parent() != destination.parent()
    {
        return Err(io::Error::other(
            "use distinct absolute paths in the same directory",
        ));
    }
    let metadata = fs::symlink_metadata(source)?;
    if !metadata.is_file() || expected_size == 0 || metadata.len() != expected_size {
        return Err(io::Error::other(
            "staged file is not a complete regular file",
        ));
    }
    fs::rename(source, destination)
}

fn main() -> ExitCode {
    let args: Vec<_> = env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: replace-runtime-file STAGED DESTINATION EXPECTED_BYTES");
        return ExitCode::from(2);
    }
    let result = args[3]
        .parse::<u64>()
        .map_err(io::Error::other)
        .and_then(|size| replace(Path::new(&args[1]), Path::new(&args[2]), size));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("replace-runtime-file: {error}");
            ExitCode::FAILURE
        }
    }
}
