use std::{collections::VecDeque, os::raw::c_int, sync::Arc};

use bytemuck;
use ctor::ctor;
use media_codec::{
    codec::{AudioParameters, Codec, CodecBuilder, CodecID},
    encoder::{register_encoder, AudioEncoder, AudioEncoderParameters, Encoder, EncoderBuilder, EncoderParameters},
    packet::Packet,
    CodecInformation, CodecParameters,
};
use media_core::{
    audio::{AudioFrame, SampleFormat},
    buffer::BufferPool,
    error::Error,
    frame::SharedFrame,
    invalid_param_error,
    rational::Rational64,
    unsupported_error,
    variant::Variant,
    Result,
};

use crate::{opus_error_string, opus_sys};

struct OpusOptions {
    application: i32,
    frame_duration: f32,
    frame_size: u32,
    packet_loss: i32,
    fec: bool,
    vbr: u32,
    max_bandwidth: u32,
    complexity: u32,
}

impl Default for OpusOptions {
    fn default() -> Self {
        OpusOptions {
            application: opus_sys::OPUS_APPLICATION_AUDIO,
            frame_duration: 20.0,
            frame_size: 960,
            packet_loss: 0,
            fec: false,
            vbr: 1,
            max_bandwidth: 0,
            complexity: 10,
        }
    }
}

impl OpusOptions {
    fn from_variant(variant: Option<&Variant>) -> Self {
        if let Some(variant) = variant {
            let application = variant["application"].get_int32().unwrap_or(opus_sys::OPUS_APPLICATION_AUDIO);
            let frame_duration = variant["frame_duration"].get_float().unwrap_or(20.0);
            let packet_loss = variant["packet_loss"].get_int32().unwrap_or(0);
            let fec = variant["fec"].get_bool().unwrap_or(false);
            let vbr = variant["vbr"].get_uint32().unwrap_or(1);
            let max_bandwidth = variant["max_bandwidth"].get_uint32().unwrap_or(0);
            let complexity = variant["complexity"].get_uint32().unwrap_or(10);

            OpusOptions {
                application,
                frame_duration,
                frame_size: (frame_duration * 48000f32 / 1000f32) as u32,
                packet_loss,
                fec,
                vbr,
                max_bandwidth,
                complexity,
            }
        } else {
            Self::default()
        }
    }
}

struct OpusEncoder {
    encoder: *mut opus_sys::OpusEncoder,
    pending: VecDeque<Packet<'static>>,
    options: OpusOptions,
    buffer: Vec<u8>,
}

unsafe impl Send for OpusEncoder {}
unsafe impl Sync for OpusEncoder {}

impl Codec<AudioEncoder> for OpusEncoder {
    fn configure(&mut self, params: Option<&CodecParameters>, options: Option<&Variant>) -> Result<()> {
        if let Some(params) = params {
            let params: &AudioEncoderParameters = &params.try_into()?;
            self.set_audio_parameters(&params.audio)?;
            self.set_encoder_parameters(&params.encoder)?;
        }

        if let Some(options) = options {
            self.options = OpusOptions::from_variant(Some(options));
            self.update_options()?;
        }

        Ok(())
    }

    fn set_option(&mut self, key: &str, value: &Variant) -> Result<()> {
        let value = match value {
            Variant::Bool(value) => *value as i32,
            _ => value.get_int32().ok_or_else(|| invalid_param_error!(value))?,
        };

        match key {
            "bit_rate" => self.encoder_ctl(opus_sys::OPUS_SET_BITRATE_REQUEST, value),
            "packet_loss_percent" => {
                self.options.packet_loss = value;
                self.encoder_ctl(opus_sys::OPUS_SET_PACKET_LOSS_PERC_REQUEST, value)
            }
            "fec" => {
                self.options.fec = value != 0;
                self.encoder_ctl(opus_sys::OPUS_SET_INBAND_FEC_REQUEST, value)
            }
            "vbr" => {
                self.options.vbr = value as u32;
                self.encoder_ctl(opus_sys::OPUS_SET_VBR_REQUEST, value)
            }
            "max_bandwidth" => {
                self.options.max_bandwidth = value as u32;
                self.encoder_ctl(opus_sys::OPUS_SET_MAX_BANDWIDTH_REQUEST, value)
            }
            "complexity" => {
                self.options.complexity = value as u32;
                self.encoder_ctl(opus_sys::OPUS_SET_COMPLEXITY_REQUEST, value)
            }
            _ => Err(unsupported_error!(key)),
        }
    }
}

const DEFAULT_PACKET_PENDING_CAPACITY: usize = 8;

// The maximum frame size is 1275 bytes
const MAX_FRAME_SIZE: usize = 1275;
// 120ms packets consist of 6 frames in one packet
const MAX_FRAMES: usize = 6;
// The packet header size is 7 bytes
const PACKET_HEADER_SIZE: usize = 7;

impl Encoder<AudioEncoder> for OpusEncoder {
    fn send_frame(&mut self, _config: &AudioEncoder, pool: Option<&Arc<BufferPool>>, frame: SharedFrame<AudioFrame<'static>>) -> Result<()> {
        self.encode(frame, pool)?;
        Ok(())
    }

    fn receive_packet(&mut self, _parameters: &AudioEncoder, _pool: Option<&Arc<BufferPool>>) -> Result<Packet<'static>> {
        self.pending.pop_front().ok_or_else(|| Error::Again("no packet available".into()))
    }

    fn flush(&mut self, _config: &AudioEncoder) -> Result<()> {
        self.buffer.fill(0);

        Ok(())
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        unsafe { opus_sys::opus_encoder_destroy(self.encoder) }
    }
}

impl OpusEncoder {
    pub fn new(codec_id: CodecID, parameters: &AudioEncoderParameters, options: Option<&Variant>) -> Result<Self> {
        if codec_id != CodecID::OPUS {
            return Err(unsupported_error!(codec_id));
        }

        let mut opts = OpusOptions::from_variant(options);

        let audio_params = &parameters.audio;
        let sample_format = audio_params.format.ok_or_else(|| invalid_param_error!(parameters))?;

        if sample_format != SampleFormat::S16 && sample_format != SampleFormat::F32 {
            return Err(unsupported_error!(sample_format));
        }

        let sample_rate = audio_params.sample_rate.ok_or_else(|| invalid_param_error!(parameters))?.get() as opus_sys::opus_int32;
        let channels = audio_params.channel_layout.as_ref().ok_or_else(|| invalid_param_error!(parameters))?.channels.get() as c_int;

        // Calculate frame size in samples at 48kHz to validate frame duration
        let frame_size = (opts.frame_duration * 48000f32 / 1000f32) as u32;
        match frame_size {
            // 2.5ms | 5ms
            120 | 240 => {
                if opts.application != opus_sys::OPUS_APPLICATION_RESTRICTED_LOWDELAY {
                    opts.application = opus_sys::OPUS_APPLICATION_RESTRICTED_LOWDELAY;
                }
            }
            // 10ms | 20ms | 40ms | 60ms | 80ms | 100ms | 120ms
            480 | 960 | 1920 | 2880 | 3840 | 4800 | 5760 => {}
            _ => return Err(Error::Invalid("frame duration".into())),
        }

        opts.frame_size = frame_size * sample_rate as u32 / 48000;

        let mut error = 0;
        let opus_encoder = unsafe { opus_sys::opus_encoder_create(sample_rate, channels, opts.application, &mut error) };
        if opus_encoder.is_null() || error != opus_sys::OPUS_OK {
            return Err(Error::CreationFailed(opus_error_string(error)));
        }

        let mut encoder: OpusEncoder = OpusEncoder {
            encoder: opus_encoder,
            pending: VecDeque::with_capacity(DEFAULT_PACKET_PENDING_CAPACITY),
            options: opts,
            buffer: vec![0u8; frame_size as usize * channels as usize * sample_format.bytes() as usize],
        };

        encoder.set_audio_parameters(audio_params)?;
        encoder.set_encoder_parameters(&parameters.encoder)?;
        encoder.update_options()?;

        Ok(encoder)
    }

    fn encoder_ctl(&mut self, key: i32, value: i32) -> Result<()> {
        let ret = unsafe { opus_sys::opus_encoder_ctl(self.encoder, key, value) };

        if ret != opus_sys::OPUS_OK {
            return Err(Error::SetFailed(opus_error_string(ret)));
        }

        Ok(())
    }

    fn set_audio_parameters(&mut self, _audio_params: &AudioParameters) -> Result<()> {
        Ok(())
    }

    fn set_encoder_parameters(&mut self, encoder_params: &EncoderParameters) -> Result<()> {
        if let Some(bit_rate) = encoder_params.bit_rate {
            self.encoder_ctl(opus_sys::OPUS_SET_BITRATE_REQUEST, bit_rate as i32)?;
        }

        if let Some(level) = encoder_params.level {
            self.options.complexity = if !(0..=10).contains(&level) {
                10
            } else {
                level as u32
            };
        }

        Ok(())
    }

    fn update_options(&mut self) -> Result<()> {
        self.encoder_ctl(opus_sys::OPUS_SET_VBR_REQUEST, (self.options.vbr > 0) as i32)?;
        self.encoder_ctl(opus_sys::OPUS_SET_VBR_CONSTRAINT_REQUEST, (self.options.vbr == 2) as i32)?;
        self.encoder_ctl(opus_sys::OPUS_SET_PACKET_LOSS_PERC_REQUEST, self.options.packet_loss)?;
        self.encoder_ctl(opus_sys::OPUS_SET_INBAND_FEC_REQUEST, self.options.fec as i32)?;

        if self.options.complexity > 0 {
            self.encoder_ctl(opus_sys::OPUS_SET_COMPLEXITY_REQUEST, self.options.complexity as i32)?;
        }

        if self.options.max_bandwidth > 0 {
            self.encoder_ctl(opus_sys::OPUS_SET_MAX_BANDWIDTH_REQUEST, self.options.max_bandwidth as i32)?;
        }

        Ok(())
    }

    fn encode(&mut self, frame: SharedFrame<AudioFrame<'static>>, pool: Option<&Arc<BufferPool>>) -> Result<()> {
        let frame = frame.read();
        let desc = frame.descriptor();
        let sample_format = desc.format;

        if sample_format != SampleFormat::S16 && sample_format != SampleFormat::F32 {
            return Err(unsupported_error!(sample_format));
        }

        let guard = frame.map().map_err(|_| Error::Invalid("not readable".into()))?;
        let planes = guard.planes().unwrap();
        let packet_size = PACKET_HEADER_SIZE + MAX_FRAME_SIZE * MAX_FRAMES;
        let channels = desc.channels().get() as usize;
        let sample_size = channels * sample_format.bytes() as usize;
        let frame_data = planes.plane_data(0).unwrap();
        let frame_data_size = desc.samples.get() as usize * sample_size;
        let chunk_size = self.options.frame_size as usize * sample_size;
        let mut pts = frame.pts.unwrap_or(0);

        self.buffer.fill(0);

        for chunk in frame_data[..frame_data_size].chunks(chunk_size) {
            let mut packet = if let Some(pool) = pool {
                Packet::from_buffer(pool.get_buffer_with_length(packet_size))
            } else {
                Packet::new(packet_size)
            };

            let packet_data = packet.data_mut().ok_or_else(|| Error::Invalid("packet not writable".into()))?;
            let data = if chunk.len() < chunk_size {
                self.buffer[..chunk.len()].copy_from_slice(chunk);
                self.buffer.as_slice()
            } else {
                chunk
            };

            let ret = match desc.format {
                SampleFormat::S16 => {
                    let data = bytemuck::cast_slice::<u8, i16>(data);
                    unsafe {
                        opus_sys::opus_encode(
                            self.encoder,
                            data.as_ptr(),
                            (data.len() / channels) as i32,
                            packet_data.as_mut_ptr(),
                            packet_data.len() as i32,
                        )
                    }
                }
                SampleFormat::F32 => {
                    let data = bytemuck::cast_slice::<u8, f32>(data);
                    unsafe {
                        opus_sys::opus_encode_float(
                            self.encoder,
                            data.as_ptr(),
                            (data.len() / channels) as i32,
                            packet_data.as_mut_ptr(),
                            packet_data.len() as i32,
                        )
                    }
                }
                _ => return Err(unsupported_error!(sample_format)),
            };

            if ret < 0 {
                return Err(Error::Failed(opus_error_string(ret)));
            }

            let samples = self.options.frame_size as i64;
            let (duration, time_base) = if let Some(time_base) = frame.time_base {
                let duration = (Rational64::new(samples, desc.sample_rate.get() as i64) / time_base).to_integer();
                (duration, time_base)
            } else {
                let time_base = Rational64::new(1, desc.sample_rate.get() as i64);
                let duration = samples;
                (duration, time_base)
            };

            packet.pts = Some(pts);
            packet.duration = Some(duration);
            packet.time_base = Some(time_base);
            pts += duration;

            packet.truncate(ret as usize)?;

            self.pending.push_back(packet);
        }

        Ok(())
    }
}

const CODEC_NAME: &str = "opus-enc";

pub struct OpusEncoderBuilder;

impl EncoderBuilder<AudioEncoder> for OpusEncoderBuilder {
    fn new_encoder(&self, codec_id: CodecID, params: &CodecParameters, options: Option<&Variant>) -> Result<Box<dyn Encoder<AudioEncoder>>> {
        Ok(Box::new(OpusEncoder::new(codec_id, &params.try_into()?, options)?))
    }
}

impl CodecBuilder<AudioEncoder> for OpusEncoderBuilder {
    fn id(&self) -> CodecID {
        CodecID::OPUS
    }

    fn name(&self) -> &'static str {
        CODEC_NAME
    }
}

impl CodecInformation for OpusEncoder {
    fn id(&self) -> CodecID {
        CodecID::OPUS
    }

    fn name(&self) -> &'static str {
        CODEC_NAME
    }
}

#[ctor]
pub fn initialize() {
    register_encoder(Arc::new(OpusEncoderBuilder), false);
}
