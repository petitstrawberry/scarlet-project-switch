//! Optional YCbCr v2 ABI, matching the SGFX loader extension while the main
//! BackendApi and DriverApi retain their existing binary layouts.
//!
//! These scalar-only records are local until the shared backend-abi crate
//! includes this extension in the revision pinned by the Maxwell crates.
use sgfx_backend_abi::{self as abi, Object};
use sgfx_core::ir;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct YcbcrConversion {
    pub size: u32,
    pub reserved: u32,
    pub matrix: u32,
    pub range: u32,
    pub chroma_x: u32,
    pub chroma_y: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct YcbcrApi {
    pub version: u32,
    pub size: u32,
    /// Consumes the transferred Scarlet handle on every return path.
    pub import_ycbcr: unsafe extern "C" fn(Object, u32, i32, YcbcrConversion) -> i32,
}

pub(crate) fn decode(value: YcbcrConversion) -> Result<ir::YcbcrConversion, i32> {
    if value.size as usize != core::mem::size_of::<YcbcrConversion>() || value.reserved != 0 {
        return Err(abi::INVALID);
    }
    fn chroma(value: u32) -> Result<ir::ChromaLocation, i32> {
        match value {
            1 => Ok(ir::ChromaLocation::Cosited),
            2 => Ok(ir::ChromaLocation::Midpoint),
            _ => Err(abi::INVALID),
        }
    }
    Ok(ir::YcbcrConversion {
        matrix: match value.matrix {
            1 => ir::YcbcrMatrix::Bt601,
            2 => ir::YcbcrMatrix::Bt709,
            _ => return Err(abi::INVALID),
        },
        range: match value.range {
            1 => ir::YcbcrRange::Limited,
            2 => ir::YcbcrRange::Full,
            _ => return Err(abi::INVALID),
        },
        chroma_x: chroma(value.chroma_x)?,
        chroma_y: chroma(value.chroma_y)?,
    })
}

const _: () = {
    assert!(core::mem::size_of::<YcbcrConversion>() == 24);
    assert!(core::mem::offset_of!(YcbcrConversion, matrix) == 8);
    assert!(core::mem::offset_of!(YcbcrConversion, chroma_y) == 20);
    assert!(core::mem::size_of::<YcbcrApi>() == 16);
    assert!(core::mem::offset_of!(YcbcrApi, import_ycbcr) == 8);
};

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: YcbcrConversion = YcbcrConversion {
        size: 24,
        reserved: 0,
        matrix: 1,
        range: 1,
        chroma_x: 1,
        chroma_y: 1,
    };

    #[test]
    fn conversion_decodes_every_supported_scalar_combination() {
        for matrix in 1..=2 {
            for range in 1..=2 {
                for chroma_x in 1..=2 {
                    for chroma_y in 1..=2 {
                        let actual = decode(YcbcrConversion {
                            matrix,
                            range,
                            chroma_x,
                            chroma_y,
                            ..VALID
                        })
                        .unwrap();
                        assert_eq!(
                            actual.matrix,
                            if matrix == 1 {
                                ir::YcbcrMatrix::Bt601
                            } else {
                                ir::YcbcrMatrix::Bt709
                            }
                        );
                        assert_eq!(
                            actual.range,
                            if range == 1 {
                                ir::YcbcrRange::Limited
                            } else {
                                ir::YcbcrRange::Full
                            }
                        );
                        assert_eq!(
                            actual.chroma_x,
                            if chroma_x == 1 {
                                ir::ChromaLocation::Cosited
                            } else {
                                ir::ChromaLocation::Midpoint
                            }
                        );
                        assert_eq!(
                            actual.chroma_y,
                            if chroma_y == 1 {
                                ir::ChromaLocation::Cosited
                            } else {
                                ir::ChromaLocation::Midpoint
                            }
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn conversion_rejects_header_and_unknown_discriminants() {
        for invalid in [
            YcbcrConversion { size: 0, ..VALID },
            YcbcrConversion { size: 32, ..VALID },
            YcbcrConversion {
                reserved: 1,
                ..VALID
            },
            YcbcrConversion { matrix: 0, ..VALID },
            YcbcrConversion { matrix: 3, ..VALID },
            YcbcrConversion { range: 0, ..VALID },
            YcbcrConversion { range: 3, ..VALID },
            YcbcrConversion {
                chroma_x: 0,
                ..VALID
            },
            YcbcrConversion {
                chroma_x: 3,
                ..VALID
            },
            YcbcrConversion {
                chroma_y: 0,
                ..VALID
            },
            YcbcrConversion {
                chroma_y: u32::MAX,
                ..VALID
            },
        ] {
            assert_eq!(decode(invalid), Err(abi::INVALID));
        }
    }
}
