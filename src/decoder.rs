use std::{collections::VecDeque, os::raw::c_int, sync::Arc};

use bytemuck;
use ctor::ctor;
use media_codec::{
    codec::{Codec, CodecBuilder, CodecID},
    decoder::{register_decoder, AudioDecoder, AudioDecoderParameters, Decoder, DecoderBuilder},
    packet::Packet,
    CodecInformation, CodecParameters,
};
use media_core::{
    audio::{AudioFrameDescriptor, SampleFormat},
    error::Error,
    frame::{Frame, SharedFrame},
    frame_pool::FramePool,
    invalid_param_error, unsupported_error,
    variant::Variant,
    Result,
};

use crate::{opus_error_string, opus_sys};

struct OpusDecoder {
    decoder: *mut opus_sys::OpusDecoder,
    pending: VecDeque<SharedFrame<Frame<'static>>>,
}

unsafe impl Send for OpusDecoder {}
unsafe impl Sync for OpusDecoder {}

impl Codec<AudioDecoder> for OpusDecoder {
    fn configure(&mut self, _params: Option<&CodecParameters>, _options: Option<&Variant>) -> Result<()> {
        Ok(())
    }

    fn set_option(&mut self, _key: &str, _value: &Variant) -> Result<()> {
        Ok(())
    }
}

impl Decoder<AudioDecoder> for OpusDecoder {
    fn send_packet(&mut self, config: &AudioDecoder, pool: Option<&Arc<FramePool<Frame<'static>>>>, packet: Packet) -> Result<()> {
        let desc = self.create_descriptor(config)?;

        let mut frame = if let Some(pool) = pool {
            pool.get_frame_with_descriptor(desc.clone().into())?
        } else {
            SharedFrame::<Frame<'static>>::new(Frame::audio_creator().create_with_descriptor(desc.clone())?)
        };

        self.decode(&desc, packet, frame.write().unwrap())?;

        self.pending.push_back(frame);

        Ok(())
    }

    fn receive_frame(&mut self, _config: &AudioDecoder, _pool: Option<&Arc<FramePool<Frame<'static>>>>) -> Result<SharedFrame<Frame<'static>>> {
        self.pending.pop_front().ok_or(Error::Again("no frame available".to_string()))
    }

    fn receive_frame_borrowed(&mut self, _config: &AudioDecoder) -> Result<Frame<'_>> {
        Err(Error::Unsupported("borrowed frame".to_string()))
    }

    fn flush(&mut self, _config: &AudioDecoder) -> Result<()> {
        unsafe { opus_sys::opus_decoder_ctl(self.decoder, opus_sys::OPUS_RESET_STATE) };
        Ok(())
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        unsafe { opus_sys::opus_decoder_destroy(self.decoder) }
    }
}

const DEFAULT_PACKET_PENDING_CAPACITY: usize = 2;

impl OpusDecoder {
    pub fn new(codec_id: CodecID, params: &AudioDecoderParameters, _options: Option<&Variant>) -> Result<Self> {
        if codec_id != CodecID::Opus {
            return Err(unsupported_error!(codec_id));
        }

        let audio_params = &params.audio;
        let sample_rate = audio_params.sample_rate.ok_or_else(|| invalid_param_error!(params))?.get() as opus_sys::opus_int32;
        let channels = audio_params.channel_layout.as_ref().ok_or_else(|| invalid_param_error!(params))?.channels.get() as c_int;

        let mut ret = 0;
        let decoder = unsafe { opus_sys::opus_decoder_create(sample_rate, channels, &mut ret) };
        if decoder.is_null() || ret != opus_sys::OPUS_OK {
            return Err(Error::CreationFailed(opus_error_string(ret)));
        }

        Ok(OpusDecoder {
            decoder,
            pending: VecDeque::with_capacity(DEFAULT_PACKET_PENDING_CAPACITY),
        })
    }

    fn create_descriptor(&self, config: &AudioDecoder) -> Result<AudioFrameDescriptor> {
        let audio_params = &config.audio;
        let sample_rate = audio_params.sample_rate.ok_or_else(|| invalid_param_error!(config))?.get();
        let sample_format = if audio_params.format.ok_or_else(|| invalid_param_error!(config))? == SampleFormat::F32 {
            SampleFormat::F32
        } else {
            SampleFormat::S16
        };
        let channel_layout = audio_params.channel_layout.as_ref().ok_or_else(|| invalid_param_error!(config))?;
        // Opus spec defines the maximum frame duration as 120ms
        let max_samples = sample_rate * 120 / 1000;

        AudioFrameDescriptor::try_from_channel_layout(sample_format, max_samples, sample_rate, channel_layout.clone())
    }

    fn decode(&mut self, desc: &AudioFrameDescriptor, packet: Packet, frame: &mut Frame) -> Result<()> {
        let ret = if let Ok(mut guard) = frame.map_mut() {
            let mut planes = guard.planes_mut().unwrap();
            let packet_data = packet.data();

            if desc.format == SampleFormat::F32 {
                let data = bytemuck::cast_slice_mut::<u8, f32>(planes.plane_data_mut(0).unwrap());
                unsafe {
                    opus_sys::opus_decode_float(
                        self.decoder,
                        packet_data.as_ptr(),
                        packet_data.len() as i32,
                        data.as_mut_ptr(),
                        desc.samples.get() as i32,
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
                        desc.samples.get() as i32,
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

        Ok(())
    }
}

const CODEC_NAME: &str = "opus-dec";

pub struct OpusDecoderBuilder;

impl DecoderBuilder<AudioDecoder> for OpusDecoderBuilder {
    fn new_decoder(&self, codec_id: CodecID, params: &CodecParameters, options: Option<&Variant>) -> Result<Box<dyn Decoder<AudioDecoder>>> {
        Ok(Box::new(OpusDecoder::new(codec_id, &params.try_into()?, options)?))
    }
}

impl CodecBuilder<AudioDecoder> for OpusDecoderBuilder {
    fn id(&self) -> CodecID {
        CodecID::Opus
    }

    fn name(&self) -> &'static str {
        CODEC_NAME
    }
}

impl CodecInformation for OpusDecoder {
    fn id(&self) -> CodecID {
        CodecID::Opus
    }

    fn name(&self) -> &'static str {
        CODEC_NAME
    }
}

#[ctor]
pub fn initialize() {
    register_decoder(Arc::new(OpusDecoderBuilder), false);
}
