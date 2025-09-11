use std::{collections::VecDeque, os::raw::c_int, sync::Arc};

use bytemuck;
use ctor::ctor;
use media_codec::{
    codec::{Codec, CodecBuilder, CodecID},
    decoder::{register_decoder, AudioDecoderConfiguration, AudioDecoderParameters, Decoder, DecoderBuilder},
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

use crate::{opus_error_string, opus_sys};

struct OpusDecoder {
    decoder: *mut opus_sys::OpusDecoder,
    pending: VecDeque<Frame<'static>>,
}

unsafe impl Send for OpusDecoder {}
unsafe impl Sync for OpusDecoder {}

impl Codec<AudioDecoderConfiguration> for OpusDecoder {
    fn configure(&mut self, _parameters: Option<&AudioDecoderParameters>, _options: Option<&Variant>) -> Result<()> {
        Ok(())
    }

    fn set_option(&mut self, _key: &str, _value: &Variant) -> Result<()> {
        Ok(())
    }
}

impl Decoder<AudioDecoderConfiguration> for OpusDecoder {
    fn send_packet(&mut self, config: &AudioDecoderConfiguration, packet: &Packet) -> Result<()> {
        let audio_params = &config.audio;
        let sample_rate = audio_params.sample_rate.ok_or(invalid_param_error!(config))?.get();
        let sample_format = if audio_params.format.ok_or(invalid_param_error!(config))? == SampleFormat::F32 {
            SampleFormat::F32
        } else {
            SampleFormat::S16
        };
        let channel_layout = audio_params.channel_layout.as_ref().ok_or(invalid_param_error!(config))?;
        // Opus spec defines the maximum frame duration as 120ms
        let max_samples = sample_rate * 120 / 1000;

        let desc = AudioFrameDescriptor::try_from_channel_layout(sample_format, max_samples, sample_rate, channel_layout.clone())?;
        let mut frame = Frame::audio_creator().create_with_descriptor(desc)?;

        let ret = if let Ok(mut guard) = frame.map_mut() {
            let mut planes = guard.planes_mut().unwrap();
            let packet_data = packet.data();

            if sample_format == SampleFormat::F32 {
                let data = bytemuck::cast_slice_mut::<u8, f32>(planes.plane_data_mut(0).unwrap());
                unsafe {
                    opus_sys::opus_decode_float(
                        self.decoder,
                        packet_data.as_ptr(),
                        packet_data.len() as i32,
                        data.as_mut_ptr(),
                        max_samples as i32,
                        false as i32,
                    )
                }
            } else {
                let data = bytemuck::cast_slice_mut::<u8, i16>(planes.plane_data_mut(0).unwrap());
                unsafe {
                    opus_sys::opus_decode(
                        self.decoder,
                        packet_data.as_ptr(),
                        packet_data.len() as i32,
                        data.as_mut_ptr(),
                        max_samples as i32,
                        false as i32,
                    )
                }
            }
        } else {
            return Err(Error::Invalid("not writable".to_string()));
        };

        let samples = if ret < 0 {
            return Err(Error::Failed(opus_error_string(ret)));
        } else {
            ret as u32
        };

        frame.truncate(samples)?;

        self.pending.push_back(frame);

        Ok(())
    }

    fn receive_frame(&mut self, _config: &AudioDecoderConfiguration) -> Result<Frame<'static>> {
        self.pending.pop_front().ok_or(Error::Again("no frame available".to_string()))
    }

    fn receive_frame_borrowed(&mut self, _config: &AudioDecoderConfiguration) -> Result<Frame<'_>> {
        Err(Error::Unsupported("borrowed frame not supported".to_string()))
    }

    fn flush(&mut self, _config: &AudioDecoderConfiguration) -> Result<()> {
        unsafe { opus_sys::opus_decoder_ctl(self.decoder, opus_sys::OPUS_RESET_STATE) };
        Ok(())
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        unsafe { opus_sys::opus_decoder_destroy(self.decoder) }
    }
}

impl OpusDecoder {
    pub fn new(codec_id: CodecID, parameters: &AudioDecoderParameters, _options: Option<&Variant>) -> Result<Self> {
        if codec_id != CodecID::Opus {
            return Err(unsupported_error!(codec_id));
        }

        let audio_params = &parameters.audio;
        let sample_rate = audio_params.sample_rate.ok_or(invalid_param_error!(parameters))?.get() as opus_sys::opus_int32;
        let channels = audio_params.channel_layout.as_ref().ok_or(invalid_param_error!(parameters))?.channels.get() as c_int;

        let mut ret = 0;
        let decoder = unsafe { opus_sys::opus_decoder_create(sample_rate, channels, &mut ret) };
        if decoder.is_null() || ret != opus_sys::OPUS_OK {
            return Err(Error::CreationFailed(opus_error_string(ret)));
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

impl DecoderBuilder<AudioDecoderConfiguration> for OpusDecoderBuilder {
    fn new_decoder(
        &self,
        codec_id: CodecID,
        parameters: &AudioDecoderParameters,
        options: Option<&Variant>,
    ) -> Result<Box<dyn Decoder<AudioDecoderConfiguration>>> {
        Ok(Box::new(OpusDecoder::new(codec_id, parameters, options)?))
    }
}

impl CodecBuilder<AudioDecoderConfiguration> for OpusDecoderBuilder {
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
