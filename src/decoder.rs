use std::{collections::VecDeque, ffi::CStr, sync::Arc};

use bytemuck;
use ctor::ctor;
use media_codec::{
    codec::{Codec, CodecBuilder, CodecID, CodecParameters},
    decoder::{register_decoder, Decoder, DecoderBuilder},
    packet::Packet,
};
use media_core::{
    audio::{AudioFrameDescriptor, SampleFormat},
    error::Error,
    frame::Frame,
    invalid_param_error, unsupported_error,
    variant::Variant,
    Result,
};

use crate::opus_sys;

pub struct OpusDecoder {
    decoder: *mut opus_sys::OpusDecoder,
    pending: VecDeque<Frame<'static>>,
}

unsafe impl Send for OpusDecoder {}
unsafe impl Sync for OpusDecoder {}

impl Codec for OpusDecoder {
    fn configure(&mut self, _parameters: Option<&CodecParameters>, _options: Option<&Variant>) -> Result<()> {
        Ok(())
    }

    fn set_option(&mut self, _name: &str, _value: &Variant) -> Result<()> {
        Ok(())
    }
}

impl Decoder for OpusDecoder {
    fn send_packet(&mut self, parameters: Option<&CodecParameters>, packet: &Packet) -> Result<()> {
        let audio_params = parameters.as_ref().and_then(|codec_params| codec_params.audio()).ok_or(invalid_param_error!(parameters))?;
        let sample_rate = audio_params.sample_rate.ok_or(invalid_param_error!(parameters))?.get();
        let sample_format = if audio_params.format.ok_or(invalid_param_error!(parameters))? == SampleFormat::F32 {
            SampleFormat::F32
        } else {
            SampleFormat::S16
        };
        let channel_layout = audio_params.channel_layout.as_ref().ok_or(invalid_param_error!(parameters))?;
        // Opus spec defines the maximum frame duration as 120ms
        let max_samples = sample_rate * 120 / 1000;

        let desc = AudioFrameDescriptor::try_from_channel_layout(sample_format, max_samples, sample_rate, channel_layout.clone())?;
        let mut frame = Frame::audio_creator().create_with_descriptor(desc)?;

        let samples = if let Ok(mut guard) = frame.map_mut() {
            let mut planes = guard.planes_mut().unwrap();

            if sample_format == SampleFormat::F32 {
                let data = bytemuck::cast_slice_mut::<u8, f32>(planes.plane_data_mut(0).unwrap());
                unsafe {
                    opus_sys::opus_decode_float(
                        self.decoder,
                        packet.data.as_ptr(),
                        packet.data.len() as i32,
                        data.as_mut_ptr(),
                        max_samples as i32,
                        false as i32,
                    ) as u32
                }
            } else {
                let data = bytemuck::cast_slice_mut::<u8, i16>(planes.plane_data_mut(0).unwrap());
                unsafe {
                    opus_sys::opus_decode(
                        self.decoder,
                        packet.data.as_ptr(),
                        packet.data.len() as i32,
                        data.as_mut_ptr(),
                        max_samples as i32,
                        false as i32,
                    ) as u32
                }
            }
        } else {
            return Err(Error::Invalid("not writable".to_string()));
        };

        frame.truncate(samples)?;

        self.pending.push_back(frame);

        Ok(())
    }

    fn receive_frame(&mut self, _parameters: Option<&CodecParameters>) -> Result<Frame<'static>> {
        self.pending.pop_front().ok_or(Error::Again("no frame available".to_string()))
    }

    fn receive_frame_borrowed(&mut self, _parameters: Option<&CodecParameters>) -> Result<Frame<'_>> {
        Err(Error::Unsupported("borrowed frame not supported".to_string()))
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        unsafe {
            opus_sys::opus_decoder_destroy(self.decoder);
        }
    }
}

impl OpusDecoder {
    pub fn new(codec_id: CodecID, parameters: Option<CodecParameters>, _options: Option<Variant>) -> Result<Self> {
        if codec_id != CodecID::Opus {
            return Err(unsupported_error!(codec_id));
        }

        let audio_params = parameters.as_ref().and_then(|codec_params| codec_params.audio()).ok_or(invalid_param_error!(parameters))?;
        let sample_rate = audio_params.sample_rate.ok_or(invalid_param_error!(parameters))?.get() as i32;
        let channels = audio_params.channel_layout.as_ref().ok_or(invalid_param_error!(parameters))?.channels.get() as i32;

        let mut error = 0;
        let decoder = unsafe { opus_sys::opus_decoder_create(sample_rate, channels, &mut error) };
        if decoder.is_null() || error != opus_sys::OPUS_OK as i32 {
            return Err(Error::CreationFailed(unsafe { CStr::from_ptr(opus_sys::opus_strerror(error)).to_string_lossy().into_owned() }));
        }

        Ok(OpusDecoder {
            decoder,
            pending: VecDeque::new(),
        })
    }
}

pub struct OpusDecoderBuilder {
    codec_id: CodecID,
    name: &'static str,
}

impl DecoderBuilder for OpusDecoderBuilder {
    fn new_decoder(&self, codec_id: CodecID, parameters: Option<CodecParameters>, options: Option<Variant>) -> Result<Box<dyn Decoder>> {
        Ok(Box::new(OpusDecoder::new(codec_id, parameters, options)?))
    }
}

impl CodecBuilder for OpusDecoderBuilder {
    fn id(&self) -> CodecID {
        self.codec_id
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

const OPUS_DECODER_NAME: &str = "opus-dec";

const OPUS_DECODER_BUILDER: OpusDecoderBuilder = OpusDecoderBuilder {
    codec_id: CodecID::Opus,
    name: OPUS_DECODER_NAME,
};

#[ctor]
fn initialize() {
    register_decoder(Arc::new(OPUS_DECODER_BUILDER), false);
}
