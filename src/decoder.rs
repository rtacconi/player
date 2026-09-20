use anyhow::{Result, anyhow};
use symphonia::core::{
    audio::{AudioBufferRef, AudioBuffer},
    codecs::{Decoder, DecoderOptions},
    errors::Error,
    formats::{FormatOptions, FormatReader, SeekMode, SeekTo},
    io::{MediaSourceStream, ReadOnlySource},
    meta::{
        Metadata, MetadataOptions, MetadataRevision, StandardTagKey, StandardVisualKey, Tag, Value,
        Visual,
    },
    probe::{Hint, ProbedMetadata},
    units::Time,
};
use symphonia::default::{get_codecs, get_probe};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

#[derive(Clone, Copy)]
pub struct AudioInfo {
    pub sample_rate: u32,
    pub channels: u32,
    pub bit_depth: u32,
    pub duration: f64,
}

#[derive(Clone, Debug, Default)]
pub struct TrackMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub track_number: Option<u32>,
    pub track_total: Option<u32>,
    pub disc_number: Option<u32>,
    pub year: Option<String>,
    pub genre: Option<String>,
    pub composer: Option<String>,
    /// Raw embedded picture bytes (e.g. JPEG / PNG), if any.
    pub artwork: Option<Vec<u8>>,
}

pub struct FileDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    info: AudioInfo,
    meta: TrackMetadata,
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::UnsignedInt(n) => Some(n.to_string()),
        Value::SignedInt(n) => Some(n.to_string()),
        Value::Float(f) => Some(format!("{f}")),
        Value::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

fn parse_track_pair(s: &str) -> (Option<u32>, Option<u32>) {
    let s = s.trim();
    if let Some((a, b)) = s.split_once('/') {
        let n = a.trim().parse().ok();
        let t = b.trim().parse().ok();
        (n, t)
    } else {
        (s.parse().ok(), None)
    }
}

fn tag_track_pair(tag: &Tag) -> (Option<u32>, Option<u32>) {
    match &tag.value {
        Value::String(s) => parse_track_pair(s),
        Value::UnsignedInt(n) => (Some(*n as u32), None),
        Value::SignedInt(n) if *n >= 0 => (Some(*n as u32), None),
        _ => (None, None),
    }
}

fn tag_u32(tag: &Tag) -> Option<u32> {
    match &tag.value {
        Value::UnsignedInt(n) => Some(*n as u32),
        Value::SignedInt(n) if *n >= 0 => Some(*n as u32),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn set_if_empty(opt: &mut Option<String>, val: String) {
    if opt.is_none() && !val.is_empty() {
        *opt = Some(val);
    }
}

fn looks_like_image_bytes(data: &[u8]) -> bool {
    data.starts_with(&[0xff, 0xd8, 0xff])
        || data.starts_with(b"\x89PNG\r\n\x1a\n")
        || (data.len() > 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP")
}

fn visual_is_image(v: &Visual) -> bool {
    let mt = v.media_type.to_lowercase();
    if mt.starts_with("image/") {
        return true;
    }
    if mt.is_empty() || mt == "application/octet-stream" {
        return looks_like_image_bytes(&v.data);
    }
    false
}

fn merge_visuals(meta: &mut TrackMetadata, rev: &MetadataRevision) {
    for v in rev.visuals() {
        if !visual_is_image(v) {
            continue;
        }
        let is_front = matches!(v.usage, Some(StandardVisualKey::FrontCover));
        match &meta.artwork {
            None => meta.artwork = Some(v.data.to_vec()),
            Some(_) if is_front => meta.artwork = Some(v.data.to_vec()),
            Some(_) => {}
        }
    }
}

fn merge_tag(meta: &mut TrackMetadata, tag: &Tag) {
    let Some(std_key) = tag.std_key else {
        return;
    };
    match std_key {
        StandardTagKey::TrackTitle => {
            if let Some(s) = value_to_string(&tag.value) {
                set_if_empty(&mut meta.title, s);
            }
        }
        StandardTagKey::Artist => {
            if let Some(s) = value_to_string(&tag.value) {
                set_if_empty(&mut meta.artist, s);
            }
        }
        StandardTagKey::AlbumArtist => {
            if let Some(s) = value_to_string(&tag.value) {
                set_if_empty(&mut meta.album_artist, s);
            }
        }
        StandardTagKey::Album => {
            if let Some(s) = value_to_string(&tag.value) {
                set_if_empty(&mut meta.album, s);
            }
        }
        StandardTagKey::TrackNumber => {
            let (n, t) = tag_track_pair(tag);
            if meta.track_number.is_none() {
                meta.track_number = n;
            }
            if meta.track_total.is_none() {
                meta.track_total = t;
            }
        }
        StandardTagKey::TrackTotal => {
            if meta.track_total.is_none() {
                meta.track_total = tag_u32(tag);
            }
        }
        StandardTagKey::DiscNumber => {
            if meta.disc_number.is_none() {
                meta.disc_number = tag_u32(tag);
            }
        }
        StandardTagKey::Date => {
            if let Some(s) = value_to_string(&tag.value) {
                let year_candidate: String = s.chars().take(4).collect();
                if year_candidate.len() == 4 && year_candidate.chars().all(|c| c.is_ascii_digit()) {
                    set_if_empty(&mut meta.year, year_candidate);
                } else {
                    set_if_empty(&mut meta.year, s);
                }
            }
        }
        StandardTagKey::Genre => {
            if let Some(s) = value_to_string(&tag.value) {
                set_if_empty(&mut meta.genre, s);
            }
        }
        StandardTagKey::Composer => {
            if let Some(s) = value_to_string(&tag.value) {
                set_if_empty(&mut meta.composer, s);
            }
        }
        _ => {}
    }
}

fn merge_revision(meta: &mut TrackMetadata, rev: &MetadataRevision) {
    for tag in rev.tags() {
        merge_tag(meta, tag);
    }
    merge_visuals(meta, rev);
}

fn merge_revision_log(meta: &mut TrackMetadata, md: &mut Metadata<'_>) {
    loop {
        if let Some(rev) = md.current() {
            merge_revision(meta, rev);
        }
        if md.pop().is_none() {
            break;
        }
    }
}

fn merge_all_metadata(
    meta: &mut TrackMetadata,
    probed: &mut ProbedMetadata,
    format: &mut Box<dyn FormatReader>,
) {
    if let Some(mut md) = probed.get() {
        merge_revision_log(meta, &mut md);
    }
    {
        let mut md = format.metadata();
        merge_revision_log(meta, &mut md);
    }
}

fn compute_duration(
    params: &symphonia::core::codecs::CodecParameters,
    file_size: u64,
    bit_depth: u32,
    channels: u32,
) -> f64 {
    if let Some(time_base) = params.time_base {
        if let Some(n_frames) = params.n_frames {
            let t = time_base.calc_time(n_frames);
            t.seconds as f64 + t.frac
        } else if let Some(sr) = params.sample_rate {
            let bytes_per_sample = (bit_depth as f64 / 8.0).max(1.0);
            (file_size as f64) / (sr as f64 * channels as f64 * bytes_per_sample)
        } else {
            0.0
        }
    } else {
        0.0
    }
}

/// Probe tags, artwork, and duration without constructing a decoder (for playlist scans).
pub fn probe_metadata(path: &Path) -> Result<(TrackMetadata, f64)> {
    let file = File::open(path)?;
    let file_size = file.metadata()?.len();

    let buffer_size = std::cmp::min(1024 * 1024, (file_size / 10) as usize).max(65536);
    let buffered_file = BufReader::with_capacity(buffer_size, file);
    let read_only_source = ReadOnlySource::new(buffered_file);
    let mss = MediaSourceStream::new(Box::new(read_only_source), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension() {
        hint.with_extension(ext.to_string_lossy().as_ref());
    }

    let format_opts = FormatOptions {
        enable_gapless: true,
        ..Default::default()
    };

    let mut probe = get_probe().format(&hint, mss, &format_opts, &MetadataOptions::default())?;

    let track = probe
        .format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow!("No audio tracks found"))?;

    let params = &track.codec_params;
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
    let channels = params.channels.map(|ch| ch.count()).unwrap_or(2) as u32;
    let duration = compute_duration(params, file_size, bit_depth, channels);

    let mut meta = TrackMetadata::default();
    merge_all_metadata(&mut meta, &mut probe.metadata, &mut probe.format);

    Ok((meta, duration))
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
        
        let probe_result = get_probe()
            .format(&hint, mss, &format_opts, &MetadataOptions::default())?;

        let mut format = probe_result.format;
        let mut probed_metadata = probe_result.metadata;

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

        let duration = compute_duration(params, file_size, bit_depth, channels);

        let mut meta = TrackMetadata::default();
        merge_all_metadata(&mut meta, &mut probed_metadata, &mut format);

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
            meta,
        })
    }

    pub fn get_info(&self) -> &AudioInfo {
        &self.info
    }

    pub fn get_metadata(&self) -> &TrackMetadata {
        &self.meta
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