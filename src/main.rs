mod audio;
mod decoder;

use anyhow::Result;
use slint::{ModelRc, SharedString, VecModel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use audio::AudioEngine;
use decoder::FileDecoder;

slint::include_modules!();

fn main() -> Result<()> {
    env_logger::init();
    
    let app = PlayerWindow::new()?;
    let app_weak = app.as_weak();
    
    // Create audio engine
    let audio_engine = Arc::new(Mutex::new(AudioEngine::new()?));
    let audio_engine_clone = audio_engine.clone();
    
    // Initialize device list
    update_device_list(&app, &audio_engine)?;
    
    // Set up callbacks
    app.on_load_file({
        let app_weak = app_weak.clone();
        let audio_engine = audio_engine.clone();
        move || {
            if let Some(app) = app_weak.upgrade() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Audio Files", &["flac", "wav", "aiff", "aif", "m4a", "mp3"])
                    .pick_file()
                {
                    let path_str = path.to_string_lossy().to_string();
                    
                    match FileDecoder::open(&path) {
                        Ok(decoder) => {
                            let info = decoder.get_info();
                            let filename = path.file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string();
                            app.set_current_file(SharedString::from(filename));
                            app.set_file_info(SharedString::from(format!(
                                "{} Hz, {} bit, {} ch",
                                info.sample_rate, info.bit_depth, info.channels
                            )));
                            app.set_duration(info.duration as f32);
                            app.set_position(0.0);
                            app.set_is_playing(false); // Reset play button to show play icon
                            
                            let mut engine = audio_engine.lock().unwrap();
                            engine.load_decoder(decoder, path_str).ok();
                        }
                        Err(e) => {
                            app.set_current_file(SharedString::from("Error loading file"));
                            app.set_file_info(SharedString::from(format!("{}", e)));
                        }
                    }
                }
            }
        }
    });
    
    app.on_play({
        let app_weak = app_weak.clone();
        let audio_engine = audio_engine.clone();
        move || {
            if let Some(app) = app_weak.upgrade() {
                let mut engine = audio_engine.lock().unwrap();
                if engine.play().is_ok() {
                    app.set_is_playing(true);
                    update_playback_info(&app, &engine);
                }
            }
        }
    });
    
    app.on_pause({
        let app_weak = app_weak.clone();
        let audio_engine = audio_engine.clone();
        move || {
            if let Some(app) = app_weak.upgrade() {
                let mut engine = audio_engine.lock().unwrap();
                engine.pause();
                app.set_is_playing(false);
            }
        }
    });
    
    app.on_stop({
        let app_weak = app_weak.clone();
        let audio_engine = audio_engine.clone();
        move || {
            if let Some(app) = app_weak.upgrade() {
                let mut engine = audio_engine.lock().unwrap();
                engine.stop();
                app.set_is_playing(false);
                app.set_position(0.0);
            }
        }
    });
    
    app.on_seek({
        let audio_engine = audio_engine.clone();
        move |position| {
            let mut engine = audio_engine.lock().unwrap();
            engine.seek(position as f64);
        }
    });
    
    app.on_volume_changed({
        let audio_engine = audio_engine.clone();
        move |volume| {
            let mut engine = audio_engine.lock().unwrap();
            engine.set_volume(volume);
        }
    });
    
    app.on_device_changed({
        let app_weak = app_weak.clone();
        let audio_engine = audio_engine.clone();
        move |device_name| {
            let mut engine = audio_engine.lock().unwrap();
            let devices = engine.enumerate_devices().unwrap_or_default();
            
            if let Some(device) = devices.iter().find(|d| d.name == device_name.as_str()) {
                if engine.select_device(device.id).is_ok() {
                    if let Some(app) = app_weak.upgrade() {
                        update_playback_info(&app, &engine);
                    }
                }
            }
        }
    });
    
    // Update timer for position and playback info
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(100), {
        let app_weak = app_weak.clone();
        let audio_engine = audio_engine_clone;
        move || {
            if let Some(app) = app_weak.upgrade() {
                let engine = audio_engine.lock().unwrap();
                app.set_position(engine.get_position() as f32);
                if app.get_is_playing() {
                    update_playback_info(&app, &engine);
                }
            }
        }
    });
    
    app.run()?;
    Ok(())
}

fn update_device_list(app: &PlayerWindow, audio_engine: &Arc<Mutex<AudioEngine>>) -> Result<()> {
    let engine = audio_engine.lock().unwrap();
    let devices = engine.enumerate_devices()?;
    
    let device_names: Vec<SharedString> = devices
        .iter()
        .map(|d| SharedString::from(&d.name))
        .collect();
    
    let model = ModelRc::new(VecModel::from(device_names));
    app.set_audio_devices(model);
    
    Ok(())
}

fn update_playback_info(app: &PlayerWindow, engine: &AudioEngine) {
    let (buffer_used, buffer_capacity, buffer_percentage) = engine.get_buffer_stats();
    let underrun_count = engine.get_underrun_count();
    
    let info = format!(
        "{}Hz {}bit | Buffer: {:.1}% ({}/{}) | Underruns: {} | {}",
        engine.get_sample_rate(),
        engine.get_bit_depth(),
        buffer_percentage,
        buffer_used,
        buffer_capacity,
        underrun_count,
        if engine.is_exclusive_mode() { "Exclusive" } else { "Shared" }
    );
    app.set_playback_info(SharedString::from(info));
}