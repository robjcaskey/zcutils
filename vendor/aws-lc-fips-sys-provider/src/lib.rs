// The public declarations are generated from the exact aws-lc-fips-sys
// 0.13.11 bindings by build.rs with only the crate's symbol-prefix link_name
// attributes removed. The native provider remains outside this crate.
#![allow(
    clippy::all,
    dead_code,
    improper_ctypes,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    unpredictable_function_pointer_comparisons,
    unexpected_cfgs,
    unused_imports
)]

mod generated {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}
pub use generated::*;
include!(concat!(env!("OUT_DIR"), "/provider_metadata.rs"));

use core::ffi::{c_char, c_long, c_void};

#[allow(non_snake_case)]
#[must_use]
pub fn ERR_GET_LIB(packed_error: u32) -> i32 {
    ((packed_error >> 24) & 0xff) as i32
}

#[allow(non_snake_case)]
#[must_use]
pub fn ERR_GET_REASON(packed_error: u32) -> i32 {
    (packed_error & 0x0fff) as i32
}

#[allow(non_snake_case)]
#[must_use]
pub fn ERR_GET_FUNC(_packed_error: u32) -> i32 {
    0
}

#[allow(non_snake_case)]
pub fn BIO_get_mem_data(bio: *mut BIO, output: *mut *mut c_char) -> c_long {
    unsafe { BIO_ctrl(bio, BIO_CTRL_INFO, 0, output.cast::<c_void>()) }
}

#[allow(non_snake_case)]
#[must_use]
pub fn CFG_CPU_JITTER_ENTROPY() -> bool {
    false
}

pub fn init() {
    unsafe { CRYPTO_library_init() }
}
