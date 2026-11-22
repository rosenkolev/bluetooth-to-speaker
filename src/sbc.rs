//! Minimal FFI wrapper around libsbc (decode only).

use std::os::raw::{c_int, c_ulong, c_void};

#[repr(C)]
struct SbcT {
    flags: c_ulong,
    frequency: u8,
    blocks: u8,
    subbands: u8,
    mode: u8,
    allocation: u8,
    bitpool: u8,
    endian: u8,
    priv_: *mut c_void,
    priv_alloc_base: *mut c_void,
}

extern "C" {
    fn sbc_init(sbc: *mut SbcT, flags: c_ulong) -> c_int;
    fn sbc_decode(
        sbc: *mut SbcT,
        input: *const c_void,
        input_len: usize,
        output: *mut c_void,
        output_len: usize,
        written: *mut usize,
    ) -> isize;
    fn sbc_finish(sbc: *mut SbcT);
}

const SBC_LE: u8 = 0x00;
const SBC_MODE_MONO: u8 = 0x00;

/// Max PCM bytes of one SBC frame: 16 blocks * 8 subbands * 2 channels * 2 bytes.
pub const MAX_FRAME_PCM_BYTES: usize = 512;

pub struct Decoder {
    sbc: Box<SbcT>,
}

// libsbc state is only touched from the owning thread.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Self {
        let mut sbc = Box::new(SbcT {
            flags: 0,
            frequency: 0,
            blocks: 0,
            subbands: 0,
            mode: 0,
            allocation: 0,
            bitpool: 0,
            endian: 0,
            priv_: std::ptr::null_mut(),
            priv_alloc_base: std::ptr::null_mut(),
        });
        unsafe {
            sbc_init(sbc.as_mut(), 0);
        }
        sbc.endian = SBC_LE;
        Decoder { sbc }
    }

    /// Decode one SBC frame. Returns (bytes consumed, PCM bytes written).
    pub fn decode_frame(&mut self, input: &[u8], out: &mut [u8]) -> Option<(usize, usize)> {
        let mut written: usize = 0;
        let consumed = unsafe {
            sbc_decode(
                self.sbc.as_mut(),
                input.as_ptr() as *const c_void,
                input.len(),
                out.as_mut_ptr() as *mut c_void,
                out.len(),
                &mut written,
            )
        };
        if consumed <= 0 {
            None
        } else {
            Some((consumed as usize, written))
        }
    }

    pub fn sample_rate(&self) -> u32 {
        match self.sbc.frequency {
            0 => 16000,
            1 => 32000,
            2 => 44100,
            _ => 48000,
        }
    }

    pub fn channels(&self) -> u32 {
        if self.sbc.mode == SBC_MODE_MONO {
            1
        } else {
            2
        }
    }

    pub fn bitpool(&self) -> u8 {
        self.sbc.bitpool
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { sbc_finish(self.sbc.as_mut()) }
    }
}
