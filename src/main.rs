extern crate ffmpeg_next as ffmpeg;

use ffmpeg::{
    codec, format, frame, media, Dictionary, Packet, Rational,
    channel_layout::ChannelLayout,
};
use ffmpeg::software::resampling;
use ffmpeg::format::sample::Type as SampleType;
use ffmpeg::format::Sample as FmtSample;
use std::ptr;

const MP2_FRAME_SAMPLES: usize = 1152;
const CHANNELS: usize = 2; // stereo
// total samples per frame = MP2_FRAME_SAMPLES * CHANNELS
// each sample is i16

struct VideoTranscoder {
    decoder: ffmpeg::decoder::Video,
    encoder: ffmpeg::encoder::Video,
    input_time_base: Rational,
    ost_index: usize,
    frame_count: usize,
}

struct AudioTranscoder {
    decoder: ffmpeg::decoder::Audio,
    encoder: ffmpeg::encoder::Audio,
    input_time_base: Rational,
    ost_index: usize,
    resampler: resampling::Context,
    audio_buffer: Vec<i16>,
    sample_rate: i32,
    channel_layout: ChannelLayout,
    format: ffmpeg::format::Sample,
}

impl VideoTranscoder {
    fn new(
        ist: &format::stream::Stream,
        octx: &mut format::context::Output,
        ost_index: usize,
    ) -> Result<Self, ffmpeg::Error> {
        let decoder_ctx = ffmpeg::codec::context::Context::from_parameters(ist.parameters())?;
        let decoder = decoder_ctx.decoder().video()?;

        let global_header = octx.format().flags().contains(format::Flags::GLOBAL_HEADER);
        let h264_codec = ffmpeg::encoder::find(codec::Id::H264)
            .ok_or(ffmpeg::Error::EncoderNotFound)?;
        let mut ost = octx.add_stream(h264_codec)?;
        let mut encoder_ctx =
            ffmpeg::codec::context::Context::new_with_codec(h264_codec).encoder().video()?;

        // Set resolution and bitrate
        encoder_ctx.set_width(720);
        encoder_ctx.set_height(576);
        encoder_ctx.set_bit_rate(2_500_000); // 2.5 Mbps

        // Match frame rate/time base
        encoder_ctx.set_frame_rate(decoder.frame_rate());
        encoder_ctx.set_time_base(ist.time_base());

        // Same pixel format as input
        encoder_ctx.set_format(decoder.format());

        if global_header {
            encoder_ctx.set_flags(codec::Flags::GLOBAL_HEADER);
        }

        let x264_opts = Dictionary::new();
        let opened_encoder = encoder_ctx.open_with(x264_opts)?;
        ost.set_parameters(&opened_encoder);

        Ok(Self {
            decoder,
            encoder: opened_encoder,
            input_time_base: ist.time_base(),
            ost_index,
            frame_count: 0,
        })
    }

    fn send_packet_to_decoder(&mut self, pkt: &Packet) -> Result<(), ffmpeg::Error> {
        self.decoder.send_packet(pkt)
    }

    fn flush_decoder(&mut self) -> Result<(), ffmpeg::Error> {
        self.decoder.send_eof()
    }

    fn receive_and_encode(
        &mut self,
        octx: &mut format::context::Output,
        ost_time_base: Rational,
    ) -> Result<(), ffmpeg::Error> {
        let mut f = frame::Video::empty();
        while self.decoder.receive_frame(&mut f).is_ok() {
            self.frame_count += 1;
            let ts = f.timestamp();
            f.set_pts(ts);

            self.encoder.send_frame(&f)?;
            self.receive_and_write_packets(octx, ost_time_base)?;
        }
        Ok(())
    }

    fn flush_encoder(
        &mut self,
        octx: &mut format::context::Output,
        ost_time_base: Rational,
    ) -> Result<(), ffmpeg::Error> {
        self.encoder.send_eof()?;
        self.receive_and_write_packets(octx, ost_time_base)
    }

    fn receive_and_write_packets(
        &mut self,
        octx: &mut format::context::Output,
        ost_time_base: Rational,
    ) -> Result<(), ffmpeg::Error> {
        let mut encoded = Packet::empty();
        while self.encoder.receive_packet(&mut encoded).is_ok() {
            encoded.set_stream(self.ost_index);
            encoded.rescale_ts(self.input_time_base, ost_time_base);
            encoded.write_interleaved(octx)?;
        }
        Ok(())
    }
}

impl AudioTranscoder {
    fn new(
        ist: &format::stream::Stream,
        octx: &mut format::context::Output,
        ost_index: usize,
    ) -> Result<Self, ffmpeg::Error> {
        let decoder_ctx = ffmpeg::codec::context::Context::from_parameters(ist.parameters())?;
        let decoder = decoder_ctx.decoder().audio()?;

        let global_header = octx.format().flags().contains(format::Flags::GLOBAL_HEADER);
        let mp2_codec = ffmpeg::encoder::find(codec::Id::MP2)
            .ok_or(ffmpeg::Error::EncoderNotFound)?;
        let mut ost = octx.add_stream(mp2_codec)?;
        let mut encoder_ctx =
            ffmpeg::codec::context::Context::new_with_codec(mp2_codec).encoder().audio()?;

        // MP2: 48kHz, stereo, I16 packed, 256 kbps
        let sample_rate = 48_000;
        let channel_layout = ChannelLayout::STEREO;
        let format = FmtSample::I16(SampleType::Packed);

        encoder_ctx.set_rate(sample_rate);
        encoder_ctx.set_channel_layout(channel_layout);
        encoder_ctx.set_format(format);
        encoder_ctx.set_bit_rate(256_000);
        encoder_ctx.set_time_base(ist.time_base());

        if global_header {
            encoder_ctx.set_flags(codec::Flags::GLOBAL_HEADER);
        }

        let opened_encoder = encoder_ctx.open_as(mp2_codec)?;
        ost.set_parameters(&opened_encoder);

        // Convert i32 to u32 for sample_rate
        let resampler = resampling::Context::get(
            decoder.format(),
            decoder.channel_layout(),
            decoder.rate(),
            format,
            channel_layout,
            sample_rate as u32,
        )?;

        Ok(Self {
            decoder,
            encoder: opened_encoder,
            input_time_base: ist.time_base(),
            ost_index,
            resampler,
            audio_buffer: Vec::new(),
            sample_rate,
            channel_layout,
            format,
        })
    }

    fn send_packet_to_decoder(&mut self, pkt: &Packet) -> Result<(), ffmpeg::Error> {
        self.decoder.send_packet(pkt)
    }

    fn flush_decoder(&mut self) -> Result<(), ffmpeg::Error> {
        self.decoder.send_eof()
    }

    fn receive_and_encode(
        &mut self,
        octx: &mut format::context::Output,
        ost_time_base: Rational,
    ) -> Result<(), ffmpeg::Error> {
        let mut decoded = frame::Audio::empty();
        while self.decoder.receive_frame(&mut decoded).is_ok() {
            let value = decoded.timestamp(); // Option<i64>
            let mut resampled = frame::Audio::empty();

            self.resampler.run(&decoded, &mut resampled)?;

            // Extract samples
            let plane_opt = resampled.data(0);
//            if plane_opt.is_none() {
//                // No audio data
//                continue;
//            }
            let plane = plane_opt; // &[u8]

            let sample_count = resampled.samples() * CHANNELS;
            let sample_data = unsafe {
                std::slice::from_raw_parts(plane.as_ptr() as *const i16, sample_count)
            };

            self.audio_buffer.extend_from_slice(sample_data);

            let frame_samples = MP2_FRAME_SAMPLES * CHANNELS;
            while self.audio_buffer.len() >= frame_samples {
                // Drain required_samples from the front of the buffer
                let current_frame_data: Vec<i16> =
                    self.audio_buffer.drain(..frame_samples).collect();

                let mut out_frame = frame::Audio::empty();
                out_frame.set_format(self.format);
                out_frame.set_channel_layout(self.channel_layout);
                out_frame.set_rate(self.sample_rate as u32);
                out_frame.set_samples(MP2_FRAME_SAMPLES);

                // Allocate buffer for out_frame
                unsafe {
                    out_frame.alloc(self.format, MP2_FRAME_SAMPLES, self.channel_layout);
                }

                let out_buf_opt: &mut [u8] = out_frame.data_mut(0);
//                if out_buf_opt.is_none() {
//                    eprintln!("Failed to get mutable audio buffer");
//                    continue;
//                }
                let out_buf = out_buf_opt;

                let required_bytes = frame_samples * std::mem::size_of::<i16>();
                assert!(out_buf.len() >= required_bytes);

                unsafe {
                    ptr::copy_nonoverlapping(
                        current_frame_data.as_ptr() as *const u8,
                        out_buf.as_mut_ptr(),
                        required_bytes,
                    );
                }

                // Set pts from decoded frame's timestamp (Option<i64>)
                out_frame.set_pts(value);
                self.encoder.send_frame(&out_frame)?;
                self.receive_and_write_packets(octx, ost_time_base)?;
            }
        }
        Ok(())
    }

    fn flush_encoder(
        &mut self,
        octx: &mut format::context::Output,
        ost_time_base: Rational,
    ) -> Result<(), ffmpeg::Error> {
        // Discard leftover samples for simplicity
        self.encoder.send_eof()?;
        self.receive_and_write_packets(octx, ost_time_base)
    }

    fn receive_and_write_packets(
        &mut self,
        octx: &mut format::context::Output,
        ost_time_base: Rational,
    ) -> Result<(), ffmpeg::Error> {
        let mut encoded = Packet::empty();
        while self.encoder.receive_packet(&mut encoded).is_ok() {
            encoded.set_stream(self.ost_index);
            encoded.rescale_ts(self.input_time_base, ost_time_base);
            encoded.write_interleaved(octx)?;
        }
        Ok(())
    }
}

fn main() -> Result<(), ffmpeg::Error> {
    ffmpeg::init()?;
    let rtmp_input = "rtmp://ipcloud.live/onetv/onemovies";
    let udp_output = "udp://239.0.0.1:1234?pkt_size=1316";

    let mut ictx = format::input(&rtmp_input)?;
    // Set output format to mpegts explicitly
    let mut octx = format::output_as(&udp_output, "mpegts")?;

    // Dump input info (optional)
    format::context::input::dump(&ictx, 0, Some(rtmp_input));

    let nb_streams = ictx.nb_streams();
    let mut stream_mapping = vec![-1; nb_streams as usize];
    let mut ist_time_bases = vec![Rational(0, 1); nb_streams as usize];
    let mut ost_time_bases = vec![Rational(0, 1); nb_streams as usize];

    let mut video_transcoder: Option<VideoTranscoder> = None;
    let mut audio_transcoder: Option<AudioTranscoder> = None;

    let mut ost_index = 0;
    for (i, ist) in ictx.streams().enumerate() {
        ist_time_bases[i] = ist.time_base();
        let medium = ist.parameters().medium();

        if medium == media::Type::Video {
            stream_mapping[i] = ost_index as isize;
            video_transcoder = Some(VideoTranscoder::new(&ist, &mut octx, ost_index)?);
            ost_index += 1;
        } else if medium == media::Type::Audio {
            stream_mapping[i] = ost_index as isize;
            audio_transcoder = Some(AudioTranscoder::new(&ist, &mut octx, ost_index)?);
            ost_index += 1;
        } else {
            // Ignore other streams
            stream_mapping[i] = -1;
        }
    }

    octx.write_header()?;

    for idx in 0..ost_index {
        ost_time_bases[idx] = octx.stream(idx as _).unwrap().time_base();
    }

    // Process packets
    for (stream, packet) in ictx.packets() {
        let ist_index = stream.index();
        let ost_index = stream_mapping[ist_index];
        if ost_index < 0 {
            continue;
        }
        let ost_time_base = ost_time_bases[ost_index as usize];

        match stream.parameters().medium() {
            media::Type::Video => {
                if let Some(v) = &mut video_transcoder {
                    v.send_packet_to_decoder(&packet)?;
                    v.receive_and_encode(&mut octx, ost_time_base)?;
                }
            }
            media::Type::Audio => {
                if let Some(a) = &mut audio_transcoder {
                    a.send_packet_to_decoder(&packet)?;
                    a.receive_and_encode(&mut octx, ost_time_base)?;
                }
            }
            _ => {}
        }
    }

    // Flush decoders and encoders
    if let Some(v) = &mut video_transcoder {
        let ost_time_base = ost_time_bases[v.ost_index];
        v.flush_decoder()?;
        v.receive_and_encode(&mut octx, ost_time_base)?;
        v.flush_encoder(&mut octx, ost_time_base)?;
    }

    if let Some(a) = &mut audio_transcoder {
        let ost_time_base = ost_time_bases[a.ost_index];
        a.flush_decoder()?;
        a.receive_and_encode(&mut octx, ost_time_base)?;
        a.flush_encoder(&mut octx, ost_time_base)?;
    }

    octx.write_trailer()?;
    Ok(())
}
