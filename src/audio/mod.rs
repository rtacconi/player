// audio/mod.rs
use anyhow::{Result, anyhow};
use coreaudio::audio_unit::{AudioUnit, IOType, Scope, Element, render_callback::data::Interleaved};
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
};
use std::sync::{Arc, Mutex, RwLock};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ringbuf::{HeapRb, HeapProducer, HeapConsumer};
use std::ffi::CStr;
use std::os::raw::c_void;
use std::ptr;

use crate::decoder::FileDecoder;

pub struct DeviceInfo {
    pub id: AudioDeviceID,
    pub name: String,
    pub _sample_rates: Vec<u32>,
    pub _max_channels: u32,
}

pub struct AudioEngine {
    audio_unit: Option<AudioUnit>,
    current_device: AudioDeviceID,
    sample_rate: u32,
    channels: u32,
    bit_depth: u32,
    buffer_size: u32,
    
    // Playback state
    is_playing: AtomicBool,
    position: Arc<AtomicU64>,
    volume: Arc<RwLock<f32>>,
    
    // Audio pipeline
    decoder: Arc<Mutex<Option<FileDecoder>>>,
    ring_buffer_producer: Arc<Mutex<HeapProducer<f32>>>,
    ring_buffer_consumer: Arc<Mutex<HeapConsumer<f32>>>,
    
    // Exclusive mode
    exclusive_mode: bool,
}

impl AudioEngine {
    pub fn new() -> Result<Self> {
        let (producer, consumer) = HeapRb::<f32>::new(480000).split(); // Increased buffer size
        
        Ok(Self {
            audio_unit: None,
            current_device: get_default_device()?,
            sample_rate: 44100,
            channels: 2,
            bit_depth: 24,
            buffer_size: 512,
            is_playing: AtomicBool::new(false),
            position: Arc::new(AtomicU64::new(0)),
            volume: Arc::new(RwLock::new(0.8)),
            decoder: Arc::new(Mutex::new(None)),
            ring_buffer_producer: Arc::new(Mutex::new(producer)),
            ring_buffer_consumer: Arc::new(Mutex::new(consumer)),
            exclusive_mode: false,
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
                if let Ok(info) = self.get_device_info(device_id) {
                    device_infos.push(info);
                }
            }
            
            Ok(device_infos)
        }
    }
    
    fn get_device_info(&self, device_id: AudioDeviceID) -> Result<DeviceInfo> {
        unsafe {
            // Get device name
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
            
            Ok(DeviceInfo {
                id: device_id,
                name,
                _sample_rates: vec![44100, 48000, 88200, 96000, 176400, 192000],
                _max_channels: 2,
            })
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
    
    pub fn load_decoder(&mut self, decoder: FileDecoder) -> Result<()> {
        println!("Loading decoder...");
        let info = decoder.get_info();
        println!("Audio info: {} Hz, {} channels, {} bit", info.sample_rate, info.channels, info.bit_depth);
        self.sample_rate = info.sample_rate;
        self.channels = info.channels;
        self.bit_depth = info.bit_depth;
        
        *self.decoder.lock().unwrap() = Some(decoder);
        println!("Decoder stored successfully");
        self.position.store(0, Ordering::Relaxed);
        
        // Clear ring buffer
        let mut rb = self.ring_buffer_consumer.lock().unwrap();
        while rb.pop().is_some() {}
        println!("Ring buffer cleared");
        
        Ok(())
    }
    
    fn setup_audio_unit(&mut self) -> Result<()> {
        println!("Creating audio unit...");
        let mut audio_unit = AudioUnit::new(IOType::DefaultOutput)?;
        println!("Audio unit created successfully");
        
        // Configure format
        let format = AudioStreamBasicDescription {
            mSampleRate: self.sample_rate as f64,
            mFormatID: 1819304813, // kAudioFormatLinearPCM
            mFormatFlags: 0x00000001 | 0x00000008, // kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked
            mBytesPerPacket: (self.channels * 4) as u32, // 4 bytes per f32 sample
            mFramesPerPacket: 1,
            mBytesPerFrame: (self.channels * 4) as u32,
            mChannelsPerFrame: self.channels,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        
        println!("Setting stream format: {} Hz, {} channels", self.sample_rate, self.channels);
        match audio_unit.set_property(
            kAudioUnitProperty_StreamFormat,
            Scope::Input,
            Element::Output,
            Some(&format),
        ) {
            Ok(_) => println!("Stream format set successfully"),
            Err(e) => {
                println!("Failed to set stream format: {:?}", e);
                return Err(e.into());
            }
        }
        
        // Set up render callback
        let consumer = self.ring_buffer_consumer.clone();
        let volume = self.volume.clone();
        let position = self.position.clone();
        
        audio_unit.set_render_callback(move |args: coreaudio::audio_unit::render_callback::Args<Interleaved<f32>>| {
            let mut rb = consumer.lock().unwrap();
            let vol = *volume.read().unwrap();

            let buffer = args.data.buffer; // &mut [f32]
            let mut samples_available = 0;
            
            for sample in buffer.iter_mut() {
                if let Some(s) = rb.pop() {
                    *sample = s * vol;
                    samples_available += 1;
                } else {
                    *sample = 0.0;
                }
            }
            
            if samples_available == 0 {
                // Only print warning occasionally to avoid spam
                static mut WARNING_COUNT: u32 = 0;
                unsafe {
                    WARNING_COUNT += 1;
                    if WARNING_COUNT % 100 == 0 { // Only print every 100th warning
                        let count = WARNING_COUNT;
                        println!("Warning: No samples available in render callback (count: {})", count);
                    }
                }
            }

            // Update position
            position.fetch_add(args.num_frames as u64, Ordering::Relaxed);

            Ok(())
        })?;
        
        audio_unit.initialize()?;
        self.audio_unit = Some(audio_unit);
        
        Ok(())
    }
    
    pub fn play(&mut self) -> Result<()> {
        println!("Play method called");
        
        if self.audio_unit.is_none() {
            println!("Setting up audio unit...");
            self.setup_audio_unit()?;
            println!("Audio unit setup complete");
        }
        
        if let Some(audio_unit) = &mut self.audio_unit {
            println!("Starting audio unit...");
            audio_unit.start()?;
            println!("Audio unit started successfully");
            self.is_playing.store(true, Ordering::Relaxed);
            
            // Start decoder thread first
            println!("Starting decoder thread...");
            if let Some(_) = &*self.decoder.lock().unwrap() {
                println!("Decoder is available, starting thread");
                self.start_decoder_thread();
                println!("Decoder thread started");
                
                // Wait for initial buffering before starting audio unit
                println!("Buffering...");
                let mut buffer_filled = false;
                for _ in 0..50 { // Wait up to 500ms for buffering
                    let rb = self.ring_buffer_consumer.lock().unwrap();
                    let available = rb.len();
                    if available > 8192 { // Wait for at least 8k samples 
                        buffer_filled = true;
                        println!("Buffer filled with {} samples", available);
                        break;
                    }
                    drop(rb);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                if !buffer_filled {
                    println!("Warning: Buffer not filled, starting anyway");
                }
                println!("Playback started");
            } else {
                println!("ERROR: No decoder available!");
            }
        } else {
            println!("No audio unit available!");
        }
        
        Ok(())
    }
    
    pub fn pause(&mut self) {
        self.is_playing.store(false, Ordering::Relaxed);
        
        if let Some(audio_unit) = &mut self.audio_unit {
            audio_unit.stop().ok();
        }
    }
    
    pub fn stop(&mut self) {
        self.pause();
        self.position.store(0, Ordering::Relaxed);
        
        // Clear ring buffer
        let mut rb = self.ring_buffer_consumer.lock().unwrap();
        while rb.pop().is_some() {}
    }
    
    fn start_decoder_thread(&self) {
        let decoder = self.decoder.clone();
        let ring_buffer_producer = self.ring_buffer_producer.clone();
        let is_playing = Arc::new(AtomicBool::new(true));
        let is_playing_clone = is_playing.clone();
        
        // Store the is_playing flag
        self.is_playing.store(true, Ordering::Relaxed);
        
        std::thread::spawn(move || {
            let mut temp_buffer = vec![0f32; 8192]; // Increased buffer size for stereo
            let mut total_frames = 0;
            
            println!("Decoder thread started");
            
            while is_playing.load(Ordering::Relaxed) {
                if let Some(ref mut dec) = *decoder.lock().unwrap() {
                    match dec.decode_frames(&mut temp_buffer) {
                        Ok(frames) => {
                            if frames > 0 {
                                // Add samples to ring buffer with better flow control
                                let mut rb = ring_buffer_producer.lock().unwrap();
                                // frames is already the total number of samples (frames * channels)
                                if frames <= temp_buffer.len() {
                                    // Check if we have space in the buffer
                                    let free_space = rb.free_len();
                                    let capacity = rb.capacity();
                                    if free_space >= frames {
                                        rb.push_slice(&temp_buffer[..frames]);
                                        total_frames += frames / 2; // Convert back to audio frames
                                        if total_frames % 5000 == 0 { // More frequent logging
                                            println!("Decoded {} samples, total frames: {}, buffer: {}/{}", frames, total_frames, capacity - free_space, capacity);
                                        }
                                    } else {
                                        // Buffer is full, wait a bit
                                        println!("Buffer full (free: {}, need: {}), waiting...", free_space, frames);
                                        drop(rb);
                                        std::thread::sleep(std::time::Duration::from_millis(10));
                                        continue;
                                    }
                                } else {
                                    println!("Warning: Buffer overflow, samples: {}, buffer size: {}", frames, temp_buffer.len());
                                    return;
                                }
                            } else {
                                // End of file
                                println!("End of file reached");
                                break;
                            }
                        }
                        Err(e) => {
                            println!("Decoder error: {:?}", e);
                            break;
                        }
                    }
                } else {
                    println!("No decoder available");
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            
            println!("Decoder thread ending, total frames decoded: {}", total_frames);
            is_playing_clone.store(false, Ordering::Relaxed);
        });
    }
    
    pub fn get_position(&self) -> f64 {
        let frames = self.position.load(Ordering::Relaxed);
        frames as f64 / self.sample_rate as f64
    }
    
    pub fn seek(&mut self, position_seconds: f64) {
        if let Some(ref mut decoder) = *self.decoder.lock().unwrap() {
            decoder.seek(position_seconds).ok();
            let frames = (position_seconds * self.sample_rate as f64) as u64;
            self.position.store(frames, Ordering::Relaxed);
            
            // Clear ring buffer
            let mut rb = self.ring_buffer_consumer.lock().unwrap();
            while rb.pop().is_some() {}
        }
    }
    
    pub fn set_volume(&mut self, volume: f32) {
        *self.volume.write().unwrap() = volume.clamp(0.0, 1.0);
    }
    
    pub fn get_sample_rate(&self) -> u32 {
        self.sample_rate
    }
    
    pub fn get_bit_depth(&self) -> u32 {
        self.bit_depth
    }
    
    pub fn get_buffer_size(&self) -> u32 {
        self.buffer_size
    }
    
    pub fn is_exclusive_mode(&self) -> bool {
        self.exclusive_mode
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

// Core Audio constants
const K_AUDIO_OBJECT_SYSTEM_OBJECT: AudioObjectID = 1;


