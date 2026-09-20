use anyhow::{Result, anyhow};
use coreaudio::audio_unit::{AudioUnit, IOType, Scope, Element};
use coreaudio_sys::{
    AudioDeviceID, AudioObjectPropertyAddress,
    AudioObjectID, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize,
    kAudioHardwarePropertyDevices, kAudioObjectPropertyScopeGlobal,
    kAudioDevicePropertyDeviceName,
    kAudioHardwarePropertyDefaultOutputDevice,
    kAudioObjectPropertyElementMaster,
    kAudioUnitProperty_StreamFormat,
    AudioStreamBasicDescription,
    kAudioDevicePropertyStreamConfiguration,
    kAudioObjectPropertyScopeOutput,
};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU32, Ordering};
use std::ffi::CStr;
use std::os::raw::c_void;
use std::ptr;
use std::thread;
use std::time::Duration;
use crossbeam_channel::{bounded, Sender, Receiver};
use rtrb::{RingBuffer, Producer, Consumer};

use crate::decoder::FileDecoder;

const RING_BUFFER_SIZE: usize = 2_097_152; // 2MB buffer (~12 seconds at 44.1kHz stereo)

pub struct DeviceInfo {
    pub id: AudioDeviceID,
    pub name: String,
}

enum DecoderCommand {
    Play,
    Pause,
    Stop,
    Reload(String, f64), // path and position to reload at
}

pub struct AudioEngine {
    audio_unit: Option<AudioUnit>,
    current_device: AudioDeviceID,
    
    // Audio format
    sample_rate: Arc<AtomicU32>,
    channels: Arc<AtomicU32>,
    bit_depth: Arc<AtomicU32>,
    duration: Arc<Mutex<f64>>,
    
    // Playback state
    is_playing: Arc<AtomicBool>,
    position_frames: Arc<AtomicU64>,
    decoded_frames: Arc<AtomicU64>,  // Total frames decoded by decoder
    volume: Arc<Mutex<f32>>,
    current_file_path: Arc<Mutex<Option<String>>>,
    is_eof: Arc<AtomicBool>,
    
    // Ring buffer for audio data
    ring_producer: Arc<Mutex<Option<Producer<f32>>>>,
    ring_consumer: Arc<Mutex<Option<Consumer<f32>>>>,
    
    // Decoder thread control
    decoder_command_tx: Option<Sender<DecoderCommand>>,
    decoder_thread: Option<thread::JoinHandle<()>>,
    
    // Stats
    underrun_count: Arc<AtomicU64>,
    buffer_level: Arc<AtomicU64>,
}

impl AudioEngine {
    pub fn new() -> Result<Self> {
        Ok(Self {
            audio_unit: None,
            current_device: get_default_device()?,
            sample_rate: Arc::new(AtomicU32::new(44100)),
            channels: Arc::new(AtomicU32::new(2)),
            bit_depth: Arc::new(AtomicU32::new(16)),
            duration: Arc::new(Mutex::new(0.0)),
            is_playing: Arc::new(AtomicBool::new(false)),
            position_frames: Arc::new(AtomicU64::new(0)),
            decoded_frames: Arc::new(AtomicU64::new(0)),
            volume: Arc::new(Mutex::new(0.8)),
            current_file_path: Arc::new(Mutex::new(None)),
            is_eof: Arc::new(AtomicBool::new(false)),
            ring_producer: Arc::new(Mutex::new(None)),
            ring_consumer: Arc::new(Mutex::new(None)),
            decoder_command_tx: None,
            decoder_thread: None,
            underrun_count: Arc::new(AtomicU64::new(0)),
            buffer_level: Arc::new(AtomicU64::new(0)),
        })
    }
    
    pub fn enumerate_devices(&self) -> Result<Vec<DeviceInfo>> {
        unsafe {
            let property = AudioObjectPropertyAddress {
                mSelector: kAudioHardwarePropertyDevices,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMaster,
            };
            
            let mut size = 0u32;
            let status = AudioObjectGetPropertyDataSize(
                K_AUDIO_OBJECT_SYSTEM_OBJECT,
                &property,
                0,
                ptr::null(),
                &mut size,
            );
            
            if status != 0 {
                return Err(anyhow!("Failed to get device list size"));
            }
            
            let device_count = size / std::mem::size_of::<AudioDeviceID>() as u32;
            let mut devices = vec![0 as AudioDeviceID; device_count as usize];
            
            let status = AudioObjectGetPropertyData(
                K_AUDIO_OBJECT_SYSTEM_OBJECT,
                &property,
                0,
                ptr::null(),
                &mut size,
                devices.as_mut_ptr() as *mut c_void,
            );
            
            if status != 0 {
                return Err(anyhow!("Failed to get device list"));
            }
            
            let mut device_infos = Vec::new();
            for device_id in devices {
                if self.is_output_device(device_id)? {
                    if let Ok(info) = self.get_device_info(device_id) {
                        // Filter out microphone and input devices
                        let name_lower = info.name.to_lowercase();
                        if !name_lower.contains("microphone") && 
                           !name_lower.contains("input") && 
                           !name_lower.contains("mic") {
                            device_infos.push(info);
                        }
                    }
                }
            }
            
            Ok(device_infos)
        }
    }
    
    fn is_output_device(&self, device_id: AudioDeviceID) -> Result<bool> {
        unsafe {
            let property = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyStreamConfiguration,
                mScope: kAudioObjectPropertyScopeOutput,
                mElement: kAudioObjectPropertyElementMaster,
            };

            let mut size = 0u32;
            let status = AudioObjectGetPropertyDataSize(
                device_id,
                &property,
                0,
                ptr::null(),
                &mut size,
            );

            Ok(status == 0 && size > 0)
        }
    }
    
    fn get_device_info(&self, device_id: AudioDeviceID) -> Result<DeviceInfo> {
        unsafe {
            let property = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyDeviceName,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMaster,
            };
            
            let mut name_buffer = vec![0u8; 256];
            let mut size = name_buffer.len() as u32;
            
            let status = AudioObjectGetPropertyData(
                device_id,
                &property,
                0,
                ptr::null(),
                &mut size,
                name_buffer.as_mut_ptr() as *mut c_void,
            );
            
            if status != 0 {
                return Err(anyhow!("Failed to get device name"));
            }
            
            let name = CStr::from_ptr(name_buffer.as_ptr() as *const i8)
                .to_string_lossy()
                .into_owned();
            
            Ok(DeviceInfo { id: device_id, name })
        }
    }
    
    pub fn select_device(&mut self, device_id: AudioDeviceID) -> Result<()> {
        self.current_device = device_id;
        
        if self.audio_unit.is_some() {
            self.stop();
            self.setup_audio_unit()?;
        }
        
        Ok(())
    }
    
    pub fn load_decoder(&mut self, decoder: FileDecoder, file_path: String) -> Result<()> {
        println!("Loading decoder...");
        
        // Stop any existing decoder thread
        if let Some(tx) = self.decoder_command_tx.take() {
            tx.send(DecoderCommand::Stop).ok();
        }
        if let Some(handle) = self.decoder_thread.take() {
            handle.join().ok();
        }
        
        // Stop any existing playback
        self.stop();
        
        // Get audio info
        let info = decoder.get_info();
        println!("Audio info: {} Hz, {} channels, {} bit, {:.1}s duration", 
            info.sample_rate, info.channels, info.bit_depth, info.duration);
        
        self.sample_rate.store(info.sample_rate, Ordering::Relaxed);
        self.channels.store(info.channels, Ordering::Relaxed);
        self.bit_depth.store(info.bit_depth, Ordering::Relaxed);
        *self.duration.lock().unwrap() = info.duration;
        *self.current_file_path.lock().unwrap() = Some(file_path);
        self.position_frames.store(0, Ordering::Relaxed);
        self.decoded_frames.store(0, Ordering::Relaxed);
        self.underrun_count.store(0, Ordering::Relaxed);
        self.is_eof.store(false, Ordering::Relaxed);
        
        // Create new ring buffer
        let (producer, consumer) = RingBuffer::new(RING_BUFFER_SIZE);
        *self.ring_producer.lock().unwrap() = Some(producer);
        *self.ring_consumer.lock().unwrap() = Some(consumer);
        
        // Create command channel
        let (tx, rx) = bounded::<DecoderCommand>(10);
        self.decoder_command_tx = Some(tx);
        
        // Start decoder thread
        self.start_decoder_thread(decoder, rx)?;
        
        Ok(())
    }
    
    fn start_decoder_thread(&mut self, decoder: FileDecoder, command_rx: Receiver<DecoderCommand>) -> Result<()> {
        let producer = self.ring_producer.clone();
        let buffer_level = self.buffer_level.clone();
        let channels = self.channels.load(Ordering::Relaxed);
        let is_eof = self.is_eof.clone();
        let decoded_frames = self.decoded_frames.clone();
        
        let handle = thread::spawn(move || {
            decoder_thread_main(decoder, producer, command_rx, buffer_level, channels, is_eof, decoded_frames);
        });
        
        self.decoder_thread = Some(handle);
        Ok(())
    }
    
    fn setup_audio_unit(&mut self) -> Result<()> {
        println!("Setting up audio unit...");
        let mut audio_unit = AudioUnit::new(IOType::DefaultOutput)?;
        
        let sample_rate = self.sample_rate.load(Ordering::Relaxed);
        let channels = self.channels.load(Ordering::Relaxed);
        
        // Configure format - INTERLEAVED
        let format = AudioStreamBasicDescription {
            mSampleRate: sample_rate as f64,
            mFormatID: 1819304813, // kAudioFormatLinearPCM
            mFormatFlags: 0x00000001 | 0x00000008, // Float32, Packed
            mBytesPerPacket: (channels * 4) as u32,
            mFramesPerPacket: 1,
            mBytesPerFrame: (channels * 4) as u32,
            mChannelsPerFrame: channels,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        
        audio_unit.set_property(
            kAudioUnitProperty_StreamFormat,
            Scope::Input,
            Element::Output,
            Some(&format),
        )?;
        
        // Set up render callback
        let consumer = self.ring_consumer.clone();
        let volume = self.volume.clone();
        let position = self.position_frames.clone();
        let underrun_count = self.underrun_count.clone();
        let buffer_level_ref = self.buffer_level.clone();
        let channels_usize = channels as usize;
        let is_eof_ref = self.is_eof.clone();

        audio_unit.set_render_callback(move |args| {
            use coreaudio::audio_unit::render_callback::{Args, data::Interleaved};
            let Args::<Interleaved<f32>> { num_frames, data, .. } = args;

            let frames = num_frames as usize;
            let samples_needed = frames * channels_usize;
            let buffer = data.buffer;

            let is_eof = is_eof_ref.load(Ordering::Relaxed);

            // Read whatever is available (up to samples_needed), aligned to whole frames.
            // Fill any remainder with silence. This keeps position tracking accurate even
            // when the decoder has finished and the ring buffer is draining its tail.
            let mut frames_read = 0usize;
            if let Some(ref mut consumer) = *consumer.lock().unwrap() {
                let available = consumer.slots();
                buffer_level_ref.store(available as u64, Ordering::Relaxed);

                let frames_available = available / channels_usize;
                let frames_to_read = frames_available.min(frames);
                let samples_to_read = frames_to_read * channels_usize;

                if samples_to_read > 0 {
                    if let Ok(chunk) = consumer.read_chunk(samples_to_read) {
                        let vol = *volume.lock().unwrap();
                        let (first, second) = chunk.as_slices();
                        let mut idx = 0;
                        for &sample in first {
                            if idx >= samples_to_read { break; }
                            buffer[idx] = sample * vol;
                            idx += 1;
                        }
                        for &sample in second {
                            if idx >= samples_to_read { break; }
                            buffer[idx] = sample * vol;
                            idx += 1;
                        }
                        chunk.commit_all();
                        frames_read = frames_to_read;
                    }
                }

                // Silence-fill any remaining samples in the output buffer
                for i in (frames_read * channels_usize)..samples_needed {
                    buffer[i] = 0.0;
                }

                // Count underruns only mid-song (not while draining at EOF)
                if frames_read < frames && !is_eof {
                    underrun_count.fetch_add(1, Ordering::Relaxed);
                }
            } else {
                for i in 0..samples_needed {
                    buffer[i] = 0.0;
                }
            }

            if frames_read > 0 {
                position.fetch_add(frames_read as u64, Ordering::Relaxed);
            }

            Ok(())
        })?;
        
        audio_unit.initialize()?;
        self.audio_unit = Some(audio_unit);
        
        Ok(())
    }
    
    pub fn play(&mut self) -> Result<()> {
        println!("Play requested");
        
        if self.audio_unit.is_none() {
            self.setup_audio_unit()?;
        }
        
        // Send play command to decoder
        if let Some(tx) = &self.decoder_command_tx {
            tx.send(DecoderCommand::Play).ok();
        }
        
        // Start audio unit
            if let Some(audio_unit) = &mut self.audio_unit {
                audio_unit.start()?;
        }
        
        self.is_playing.store(true, Ordering::Relaxed);
        println!("Playback started");
        
        Ok(())
    }
    
    pub fn pause(&mut self) {
        self.is_playing.store(false, Ordering::Relaxed);
        
        if let Some(tx) = &self.decoder_command_tx {
            tx.send(DecoderCommand::Pause).ok();
        }
        
        if let Some(audio_unit) = &mut self.audio_unit {
            audio_unit.stop().ok();
        }
    }
    
    pub fn stop(&mut self) {
        self.is_playing.store(false, Ordering::Relaxed);
        
        if let Some(audio_unit) = &mut self.audio_unit {
            audio_unit.stop().ok();
        }
        
        // Just pause the decoder, don't reset position
        if let Some(tx) = &self.decoder_command_tx {
            tx.send(DecoderCommand::Pause).ok();
        }
    }
    
    pub fn seek(&mut self, position_seconds: f64) {
        // Clamp position to valid range
        let duration = self.get_duration();
        let clamped_position = position_seconds.max(0.0).min(duration);
        
        // Update position immediately for UI feedback
        let frames = (clamped_position * self.sample_rate.load(Ordering::Relaxed) as f64) as u64;
        self.position_frames.store(frames, Ordering::Relaxed);
        
        // Clear the ring buffer to prevent old audio from playing
        if let Some(ref mut consumer) = *self.ring_consumer.lock().unwrap() {
            while consumer.slots() > 0 {
                if let Ok(chunk) = consumer.read_chunk(consumer.slots()) {
                    chunk.commit_all();
                } else {
                    break;
                }
            }
        }
        
        // Always use reload since Symphonia's BufReader only supports forward seeking
        if let Some(tx) = &self.decoder_command_tx {
            if let Some(file_path) = &*self.current_file_path.lock().unwrap() {
                // Only send reload command if we have a valid file path
                if !file_path.is_empty() {
                    tx.send(DecoderCommand::Reload(file_path.clone(), clamped_position)).ok();
                }
            }
        }
    }
    
    pub fn set_volume(&mut self, volume: f32) {
        *self.volume.lock().unwrap() = volume.clamp(0.0, 1.0);
    }
    
    pub fn get_volume(&self) -> Arc<Mutex<f32>> {
        self.volume.clone()
    }
    
    pub fn get_position(&self) -> f64 {
        let frames = self.position_frames.load(Ordering::Relaxed);
        let sample_rate = self.sample_rate.load(Ordering::Relaxed).max(1);
        let duration = self.get_duration();
        let is_eof = self.is_eof.load(Ordering::Relaxed);
        let buffer_level = self.buffer_level.load(Ordering::Relaxed);

        let position = frames as f64 / sample_rate as f64;

        // When the decoder is done AND the ring buffer is fully drained, the song has
        // truly finished playing - snap the slider exactly to the end.
        if is_eof && buffer_level == 0 && duration > 0.0 {
            return duration;
        }

        // Otherwise track real playback position, capped at duration so the slider
        // can't visually exceed its maximum.
        if duration > 0.0 {
            position.min(duration)
        } else {
            position
        }
    }
    
    pub fn get_sample_rate(&self) -> u32 {
        self.sample_rate.load(Ordering::Relaxed)
    }
    
    pub fn get_bit_depth(&self) -> u32 {
        self.bit_depth.load(Ordering::Relaxed)
    }
    
    pub fn is_exclusive_mode(&self) -> bool {
        false
    }
    
    pub fn get_buffer_stats(&self) -> (usize, usize, f32) {
        let used = self.buffer_level.load(Ordering::Relaxed) as usize;
        let capacity = RING_BUFFER_SIZE;
        let percentage = (used as f32 / capacity as f32) * 100.0;
        (used, capacity, percentage)
    }
    
    pub fn get_underrun_count(&self) -> u64 {
        self.underrun_count.load(Ordering::Relaxed)
    }
    
    pub fn get_duration(&self) -> f64 {
        *self.duration.lock().unwrap()
    }
    
    pub fn should_stop_playback(&self) -> bool {
        let is_eof = self.is_eof.load(Ordering::Relaxed);
        let buffer_level = self.buffer_level.load(Ordering::Relaxed);
        let position = self.position_frames.load(Ordering::Relaxed);
        let decoded = self.decoded_frames.load(Ordering::Relaxed);
        
        // Stop if we're at EOF and either:
        // 1. Buffer is empty, or
        // 2. We've reached the decoded position
        is_eof && (buffer_level == 0 || position >= decoded)
    }
}

fn decoder_thread_main(
    mut decoder: FileDecoder,
    producer: Arc<Mutex<Option<Producer<f32>>>>,
    command_rx: Receiver<DecoderCommand>,
    buffer_level: Arc<AtomicU64>,
    channels: u32,
    is_eof: Arc<AtomicBool>,
    decoded_frames: Arc<AtomicU64>,
) {
    println!("Decoder thread started");
    
    let mut temp_buffer = vec![0f32; 65536]; // 64K samples
    let mut is_paused = true;
    let mut total_frames = 0u64;
    let mut eof_reached = false;
    
    loop {
        // Check for commands (non-blocking)
        match command_rx.try_recv() {
            Ok(DecoderCommand::Play) => {
                println!("Decoder: Play command received");
                is_paused = false;
            }
            Ok(DecoderCommand::Pause) => {
                println!("Decoder: Pause command received");
                is_paused = true;
            }
            Ok(DecoderCommand::Stop) => {
                println!("Decoder: Stop command received");
                break;
            }
            Ok(DecoderCommand::Reload(file_path, position)) => {
                println!("Decoder: Reload file at {:.1}s", position);
                // Create a new decoder from the file
                match FileDecoder::open(std::path::Path::new(&file_path)) {
                    Ok(new_decoder) => {
                        decoder = new_decoder;
                        eof_reached = false;
                        is_eof.store(false, Ordering::Relaxed);
                        
                        // Always seek to the desired position, even if it's 0
                        // This ensures we start from the exact position requested
                        match decoder.seek(position) {
                            Ok(_) => {
                                println!("Decoder: Reload and seek to {:.1}s successful", position);
                            }
                            Err(e) => {
                                println!("Decoder: Seek after reload failed: {:?}", e);
                                // If seek fails, we'll start from the beginning
                            }
                        }
                        
                        // Update total frames based on position
                        let sample_rate = decoder.get_info().sample_rate as f64;
                        total_frames = (position * sample_rate) as u64;
                        decoded_frames.store(total_frames, Ordering::Relaxed);
                    }
                    Err(e) => {
                        println!("Decoder: Failed to reload file: {:?}", e);
                    }
                }
            }
            Err(_) => {} // No command
        }
        
        if is_paused || eof_reached {
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        
        // Check buffer space
        if let Some(ref mut producer) = *producer.lock().unwrap() {
            let free_slots = producer.slots();
            buffer_level.store((RING_BUFFER_SIZE - free_slots) as u64, Ordering::Relaxed);
            
            // If buffer is nearly full, wait
            if free_slots < temp_buffer.len() / 2 {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            
            // Decode audio
            match decoder.decode_frames(&mut temp_buffer) {
                Ok(samples) => {
                    if samples == 0 {
                        println!("Decoder: EOF reached after {} frames", total_frames);
                        decoded_frames.store(total_frames, Ordering::Relaxed);
                        eof_reached = true;
                        is_eof.store(true, Ordering::Relaxed);
                        continue;
                    }
                    
                    // Write to ring buffer
                    match producer.write_chunk(samples) {
                        Ok(mut chunk) => {
                            // Get mutable slices from the chunk
                            let (first, second) = chunk.as_mut_slices();
                            let mut written = 0;
                            
                            // Copy to first slice
                            let first_len = first.len().min(samples);
                            first[..first_len].copy_from_slice(&temp_buffer[..first_len]);
                            written += first_len;
                            
                            // Copy to second slice if needed (ring buffer wrapped)
                            if written < samples && !second.is_empty() {
                                let second_len = (samples - written).min(second.len());
                                second[..second_len].copy_from_slice(&temp_buffer[written..written + second_len]);
                            }
                            
                            chunk.commit_all();
                            
                            total_frames += (samples / channels as usize) as u64;
                            decoded_frames.store(total_frames, Ordering::Relaxed);
                        }
                        Err(_) => {
                            // Buffer full, wait a bit
                            thread::sleep(Duration::from_millis(5));
                        }
                    }
                }
                Err(e) => {
                    println!("Decoder error: {:?}", e);
                    thread::sleep(Duration::from_millis(10));
                }
            }
        } else {
            break; // No producer available
        }
    }
    
    decoded_frames.store(total_frames, Ordering::Relaxed);
    println!("Decoder thread ended. Total frames: {}", total_frames);
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

fn get_default_device() -> Result<AudioDeviceID> {
    unsafe {
        let property = AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyDefaultOutputDevice,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMaster,
        };
        
        let mut device_id: AudioDeviceID = 0;
        let mut size = std::mem::size_of::<AudioDeviceID>() as u32;
        
        let status = AudioObjectGetPropertyData(
            K_AUDIO_OBJECT_SYSTEM_OBJECT,
            &property,
            0,
            ptr::null(),
            &mut size,
            &mut device_id as *mut _ as *mut c_void,
        );
        
        if status != 0 {
            return Err(anyhow!("Failed to get default device"));
        }
        
        Ok(device_id)
    }
}

const K_AUDIO_OBJECT_SYSTEM_OBJECT: AudioObjectID = 1;