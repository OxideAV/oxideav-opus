//! Runtime libopus interop for the cross-decode fuzz harnesses.
//!
//! libopus is loaded via `dlopen` at first call — there is no
//! `opus-sys`-style build-script dep that would pull libopus source
//! into the workspace's cargo dep tree. The harness checks
//! `available()` up front and `eprintln!`s a `[oracle skip]` marker
//! and `return`s early when the shared library isn't installed, so
//! fuzz binaries built on a host without libopus simply do nothing
//! instead of panicking. **No `#[ignore]` is used** — the skip is
//! pure runtime.
//!
//! Workspace policy bars consulting libopus / ffmpeg / libavcodec
//! source; we only inspect the public C header (`<opus/opus.h>`)
//! for function signatures. The libopus C API has been ABI stable
//! since 1.3 (current release: 1.5.x).
//!
//! Install on Debian / Ubuntu with `apt-get install libopus0
//! libopus-dev`. The loader probes the conventional shared-object
//! names for both Linux and macOS.

#![allow(unsafe_code)]

pub mod libopus {
    use libloading::{Library, Symbol};
    use std::ffi::c_void;
    use std::sync::OnceLock;

    /// Conventional libopus shared-object names the loader will try
    /// in order. Covers Linux (versioned + plain `.so`), macOS
    /// (`.dylib`), and a bare `opus` for `LD_LIBRARY_PATH`-style
    /// resolution.
    const CANDIDATES: &[&str] = &[
        "libopus.so.0",
        "libopus.so",
        "libopus.0.dylib",
        "libopus.dylib",
        "opus",
    ];

    fn lib() -> Option<&'static Library> {
        static LIB: OnceLock<Option<Library>> = OnceLock::new();
        LIB.get_or_init(|| {
            for name in CANDIDATES {
                // SAFETY: `Library::new` is documented as unsafe because
                // the loaded library may run code at load time. We
                // accept that risk for fuzz tooling — libopus is a
                // well-behaved shared library.
                if let Ok(l) = unsafe { Library::new(name) } {
                    return Some(l);
                }
            }
            None
        })
        .as_ref()
    }

    /// True iff a libopus shared library was successfully loaded.
    /// Cross-decode fuzz harnesses early-return when this is false so
    /// the binary still runs without an oracle (the assertions just
    /// don't fire). The harness should also `eprintln!` a
    /// `[oracle skip]` marker so a CI grep can confirm the oracle was
    /// available.
    pub fn available() -> bool {
        lib().is_some()
    }

    /// Outcome of a single libopus decode attempt.
    pub struct OracleDecode {
        /// Sample count per channel, as returned by `opus_decode`.
        pub samples_per_channel: i32,
        /// Channel count this decode used (1 or 2 — set by caller).
        pub channels: i32,
        /// Interleaved S16 LE PCM buffer of length `samples_per_channel * channels`.
        pub pcm: Vec<i16>,
    }

    /// Decode an Opus packet via libopus's `opus_decode`. Returns
    /// `None` when libopus isn't available, when the decoder
    /// constructor fails, or when `opus_decode` returns a negative
    /// error code (libopus rejected the packet — caller should also
    /// treat that as "no oracle outcome to compare against").
    ///
    /// `sample_rate` must be one of 8000, 12000, 16000, 24000, 48000.
    /// `channels` must be 1 or 2.
    pub fn decode(data: &[u8], sample_rate: i32, channels: i32) -> Option<OracleDecode> {
        type DecoderCreateFn = unsafe extern "C" fn(i32, i32, *mut i32) -> *mut c_void;
        type DecodeFn =
            unsafe extern "C" fn(*mut c_void, *const u8, i32, *mut i16, i32, i32) -> i32;
        type DecoderDestroyFn = unsafe extern "C" fn(*mut c_void);

        let l = lib()?;
        unsafe {
            let dec_create: Symbol<DecoderCreateFn> = l.get(b"opus_decoder_create").ok()?;
            let dec_decode: Symbol<DecodeFn> = l.get(b"opus_decode").ok()?;
            let dec_destroy: Symbol<DecoderDestroyFn> = l.get(b"opus_decoder_destroy").ok()?;

            let mut err: i32 = 0;
            let dec = dec_create(sample_rate, channels, &mut err);
            if dec.is_null() || err != 0 {
                if !dec.is_null() {
                    dec_destroy(dec);
                }
                return None;
            }

            // Largest legal Opus output: 120 ms at 48 kHz = 5760
            // samples/channel. We allocate that many per channel up
            // front so opus_decode never overflows.
            const MAX_SAMPLES_PER_CH: usize = 5760;
            let mut pcm = vec![0i16; MAX_SAMPLES_PER_CH * channels as usize];
            let n = dec_decode(
                dec,
                data.as_ptr(),
                data.len() as i32,
                pcm.as_mut_ptr(),
                MAX_SAMPLES_PER_CH as i32,
                0,
            );
            dec_destroy(dec);

            if n < 0 {
                return None;
            }
            pcm.truncate(n as usize * channels as usize);
            Some(OracleDecode {
                samples_per_channel: n,
                channels,
                pcm,
            })
        }
    }
}
