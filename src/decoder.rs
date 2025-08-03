// decoder.rs
use anyhow::{Result, anyhow};
use symphonia::core::{
    audio::AudioBufferRef,
    codecs::{Decoder, DecoderOptions},
    errors::Error,
    formats::{FormatOptions, FormatReader, SeekMode, SeekTo},
    io::MediaSourceStream,
    meta::MetadataOptions,
    probe::Hint,
    units::Time,
};
use symphonia::default::{get_codecs, get_probe};
use std::fs::File;
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
        // Open the file
        let file = File::open(path)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        
        // Probe the file format
        let mut hint = Hint::new();
        if let Some(ext) = path.extension() {
            hint.with_extension(ext.to_string_lossy().as_ref());
        }
        
        let probe = get_probe()
            .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())?;
        
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
        
        // Estimate bit depth based on codec
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
        
        // Calculate duration
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
        let packet = match self.format.next_packet() {
            Ok(packet) => packet,
            Err(Error::IoError(_)) => {
                println!("End of stream reached");
                return Ok(0); // End of stream
            }
            Err(err) => {
                println!("Error reading packet: {}", err);
                return Err(anyhow!("Error reading packet: {}", err));
            }
        };
        
        // Only decode packets for our track
        if packet.track_id() != self.track_id {
            println!("Skipping packet for track {}", packet.track_id());
            return Ok(0);
        }
        
        // Decode the packet
        match self.decoder.decode(&packet) {
            Ok(decoded) => {
                let samples = convert_to_f32(&decoded, output, self.info.channels);
                Ok(samples)
            }
            Err(Error::DecodeError(_)) => {
                println!("Decode error, skipping packet");
                Ok(0) // Skip this packet
            }
            Err(err) => {
                println!("Decode error: {}", err);
                Err(anyhow!("Decode error: {}", err))
            }
        }
    }
    
    pub fn seek(&mut self, position_seconds: f64) -> Result<()> {
        let time = Time::from(position_seconds);
        
        // Seek in the format reader
        let _seeked_to = self.format.seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time,
                track_id: Some(self.track_id),
            },
        )?;
        
        // Reset decoder
        self.decoder.reset();
        
        Ok(())
    }
}

fn convert_to_f32(audio_buf: &AudioBufferRef, output: &mut [f32], channels: u32) -> usize {
    match audio_buf {
        AudioBufferRef::F32(buf) => {
            let mut sample_count = 0;
            let planes = buf.planes();
            let frames = planes.planes()[0].len();
            
            for frame_idx in 0..frames {
                for ch in 0..channels as usize {
                    if sample_count < output.len() {
                        output[sample_count] = planes.planes()[ch][frame_idx];
                        sample_count += 1;
                    }
                }
            }
            frames
        }
        AudioBufferRef::S16(buf) => {
            let mut sample_count = 0;
            let planes = buf.planes();
            let frames = planes.planes()[0].len();
            
            for frame_idx in 0..frames {
                for ch in 0..channels as usize {
                    if sample_count < output.len() {
                        output[sample_count] = planes.planes()[ch][frame_idx] as f32 / 32768.0;
                        sample_count += 1;
                    }
                }
            }
            frames
        }
        AudioBufferRef::S24(buf) => {
            let mut sample_count = 0;
            let planes = buf.planes();
            let frames = planes.planes()[0].len();
            
            for frame_idx in 0..frames {
                for ch in 0..channels as usize {
                    if sample_count < output.len() {
                        output[sample_count] = planes.planes()[ch][frame_idx].inner() as f32 / 8388608.0;
                        sample_count += 1;
                    }
                }
            }
            frames
        }
        AudioBufferRef::S32(buf) => {
            let mut sample_count = 0;
            let planes = buf.planes();
            let frames = planes.planes()[0].len();
            
            for frame_idx in 0..frames {
                for ch in 0..channels as usize {
                    if sample_count < output.len() {
                        output[sample_count] = planes.planes()[ch][frame_idx] as f32 / 2147483648.0;
                        sample_count += 1;
                    }
                }
            }
            frames
        }
        _ => 0,
    }
}