#[cfg(feature = "decoder")]
pub mod decoder;
#[cfg(feature = "encoder")]
pub mod encoder;

use std::ffi::CStr;

use media_codec_opus_sys as opus_sys;

pub(crate) fn opus_error_string(error: i32) -> String {
    unsafe { CStr::from_ptr(opus_sys::opus_strerror(error)).to_string_lossy().into_owned() }
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Application {
    VoIP     = opus_sys::OPUS_APPLICATION_VOIP,
    Audio    = opus_sys::OPUS_APPLICATION_AUDIO,
    LowDelay = opus_sys::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
}
