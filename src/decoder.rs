use anyhow::{Result, anyhow};
use symphonia::core::{
    audio::{AudioBufferRef, AudioBuffer},
    codecs::{Decoder, DecoderOptions},
    errors::Error,
    formats::{FormatOptions, FormatReader, SeekMode, SeekTo},
    io::{MediaSourceStream, ReadOnlySource},
    meta::MetadataOptions,
    probe::Hint,
    units::Time,
};
use symphonia::default::{get_codecs, get_probe};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

pub struct AudioInfo {
    pub sample_rate: u32,
    pub channels: u32,
    pub bit_depth: u32,
    pub duration: f64,
}

pub struct FileDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    info: AudioInfo,
}

impl FileDecoder {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let file_size = file.metadata()?.len();
        println!("Opening file: {} ({} bytes)", path.display(), file_size);
        
        // Use a large buffer for smooth playback
        let buffer_size = std::cmp::min(1024 * 1024, (file_size / 10) as usize).max(65536);
        let buffered_file = BufReader::with_capacity(buffer_size, file);
        let read_only_source = ReadOnlySource::new(buffered_file);
        let mss = MediaSourceStream::new(Box::new(read_only_source), Default::default());
        
        // Probe the file format
        let mut hint = Hint::new();
        if let Some(ext) = path.extension() {
            hint.with_extension(ext.to_string_lossy().as_ref());
        }
        
        let format_opts = FormatOptions {
            enable_gapless: true,
            ..Default::default()
        };
        
        let probe = get_probe()
            .format(&hint, mss, &format_opts, &MetadataOptions::default())?;
        
        let format = probe.format;
        
        // Find the first audio track
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
            .ok_or_else(|| anyhow!("No audio tracks found"))?;
        
        let track_id = track.id;
        
        // Create decoder
        let decoder = get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())?;
        
        // Get audio info
        let params = &track.codec_params;
        let sample_rate = params.sample_rate.unwrap_or(44100);
        let channels = params.channels.map(|ch| ch.count()).unwrap_or(2) as u32;
        
        let bit_depth = match params.bits_per_sample {
            Some(bits) => bits,
            None => match params.codec {
                symphonia::core::codecs::CODEC_TYPE_FLAC => 24,
                symphonia::core::codecs::CODEC_TYPE_PCM_S16LE => 16,
                symphonia::core::codecs::CODEC_TYPE_PCM_S24LE => 24,
                symphonia::core::codecs::CODEC_TYPE_PCM_S32LE => 32,
                symphonia::core::codecs::CODEC_TYPE_PCM_F32LE => 32,
                _ => 16,
            },
        };
        
        let duration = if let Some(time_base) = params.time_base {
            if let Some(n_frames) = params.n_frames {
                time_base.calc_time(n_frames).seconds as f64
            } else {
                0.0
            }
        } else {
            0.0
        };
        
        let info = AudioInfo {
            sample_rate,
            channels,
            bit_depth,
            duration,
        };
        
        Ok(Self {
            format,
            decoder,
            track_id,
            info,
        })
    }
    
    pub fn get_info(&self) -> &AudioInfo {
        &self.info
    }
    
    pub fn decode_frames(&mut self, output: &mut [f32]) -> Result<usize> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(Error::IoError(ref io_err)) if io_err.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(0); // EOF
                }
                Err(Error::IoError(_)) => {
                    return Ok(0); // Treat other IO errors as EOF
                }
                Err(Error::ResetRequired) => {
                    self.decoder.reset();
                    continue;
                }
                Err(err) => {
                    return Err(anyhow!("Error reading packet: {}", err));
                }
            };
            
            // Only decode packets for our track
            if packet.track_id() != self.track_id {
                continue;
            }
            
            // Decode the packet
            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    let samples = convert_to_f32(&decoded, output, self.info.channels);
                    return Ok(samples);
                }
                Err(Error::DecodeError(err)) => {
                    // Skip decode errors
                    eprintln!("Decode error (skipping): {:?}", err);
                    continue;
                }
                Err(err) => {
                    return Err(anyhow!("Decode error: {}", err));
                }
            }
        }
    }
    
    pub fn seek(&mut self, position_seconds: f64) -> Result<()> {
        let time = Time::from(position_seconds);
        
        println!("Decoder seeking to {:.1}s", position_seconds);
        
        let result = self.format.seek(
            SeekMode::Coarse, // Use Coarse mode for better compatibility
            SeekTo::Time {
                time,
                track_id: Some(self.track_id),
            },
        );
        
        match result {
            Ok(seeked_to) => {
                println!("Decoder seeked to {:?}", seeked_to.actual_ts);
                self.decoder.reset();
                Ok(())
            }
            Err(e) => {
                eprintln!("Seek error: {:?}", e);
                Err(anyhow!("Seek failed: {}", e))
            }
        }
    }
}

fn convert_to_f32(audio_buf: &AudioBufferRef, output: &mut [f32], channels: u32) -> usize {
    match audio_buf {
        AudioBufferRef::F32(buf) => {
            convert_planar_to_interleaved(buf, output, channels)
        }
        AudioBufferRef::S16(buf) => {
            convert_planar_to_interleaved_i16(buf, output, channels)
        }
        AudioBufferRef::S24(buf) => {
            convert_planar_to_interleaved_i24(buf, output, channels)
        }
        AudioBufferRef::S32(buf) => {
            convert_planar_to_interleaved_i32(buf, output, channels)
        }
        _ => {
            eprintln!("Unsupported audio format");
            0
        }
    }
}

fn convert_planar_to_interleaved(
    buf: &AudioBuffer<f32>,
    output: &mut [f32],
    channels: u32,
) -> usize {
    let planes = buf.planes();
    let num_planes = planes.planes().len();
    let channels = channels as usize;
    
    if num_planes == 1 && channels == 1 {
        // Mono
        let samples = &planes.planes()[0];
        let count = samples.len().min(output.len());
        output[..count].copy_from_slice(&samples[..count]);
        count
    } else if num_planes == 1 {
        // Interleaved stereo in single plane (unusual but handle it)
        let samples = &planes.planes()[0];
        let count = samples.len().min(output.len());
        output[..count].copy_from_slice(&samples[..count]);
        count
    } else {
        // Planar - one plane per channel
        let frames_per_channel = planes.planes()[0].len();
        let frames_to_copy = frames_per_channel.min(output.len() / channels);
        let mut sample_count = 0;
        
        for frame_idx in 0..frames_to_copy {
            for ch in 0..channels.min(num_planes) {
                output[sample_count] = planes.planes()[ch][frame_idx];
                sample_count += 1;
            }
        }
        sample_count
    }
}

fn convert_planar_to_interleaved_i16(
    buf: &AudioBuffer<i16>,
    output: &mut [f32],
    channels: u32,
) -> usize {
    let planes = buf.planes();
    let num_planes = planes.planes().len();
    let channels = channels as usize;
    
    if num_planes == 1 && channels == 1 {
        // Mono
        let samples = &planes.planes()[0];
        let count = samples.len().min(output.len());
        for i in 0..count {
            output[i] = samples[i] as f32 / 32768.0;
        }
        count
    } else if num_planes == 1 {
        // Interleaved stereo in single plane
        let samples = &planes.planes()[0];
        let count = samples.len().min(output.len());
        for i in 0..count {
            output[i] = samples[i] as f32 / 32768.0;
        }
        count
    } else {
        // Planar
        let frames_per_channel = planes.planes()[0].len();
        let frames_to_copy = frames_per_channel.min(output.len() / channels);
        let mut sample_count = 0;
        
        for frame_idx in 0..frames_to_copy {
            for ch in 0..channels.min(num_planes) {
                output[sample_count] = planes.planes()[ch][frame_idx] as f32 / 32768.0;
                sample_count += 1;
            }
        }
        sample_count
    }
}

fn convert_planar_to_interleaved_i24(
    buf: &AudioBuffer<symphonia::core::sample::i24>,
    output: &mut [f32],
    channels: u32,
) -> usize {
    let planes = buf.planes();
    let num_planes = planes.planes().len();
    let channels = channels as usize;
    let frames_per_channel = planes.planes()[0].len();
    let frames_to_copy = frames_per_channel.min(output.len() / channels);
    let mut sample_count = 0;
    
    for frame_idx in 0..frames_to_copy {
        for ch in 0..channels.min(num_planes) {
            let sample = planes.planes()[ch][frame_idx].inner();
            output[sample_count] = sample as f32 / 8388608.0;
            sample_count += 1;
        }
    }
    sample_count
}

fn convert_planar_to_interleaved_i32(
    buf: &AudioBuffer<i32>,
    output: &mut [f32],
    channels: u32,
) -> usize {
    let planes = buf.planes();
    let num_planes = planes.planes().len();
    let channels = channels as usize;
    
    if num_planes == 1 && channels == 1 {
        // Mono
        let samples = &planes.planes()[0];
        let count = samples.len().min(output.len());
        for i in 0..count {
            output[i] = samples[i] as f32 / 2147483648.0;
        }
        count
    } else if num_planes == 1 {
        // Interleaved stereo in single plane
        let samples = &planes.planes()[0];
        let count = samples.len().min(output.len());
        for i in 0..count {
            output[i] = samples[i] as f32 / 2147483648.0;
        }
        count
    } else {
        // Planar
        let frames_per_channel = planes.planes()[0].len();
        let frames_to_copy = frames_per_channel.min(output.len() / channels);
        let mut sample_count = 0;
        
        for frame_idx in 0..frames_to_copy {
            for ch in 0..channels.min(num_planes) {
                output[sample_count] = planes.planes()[ch][frame_idx] as f32 / 2147483648.0;
                sample_count += 1;
            }
        }
        sample_count
    }
}