# Audiophile Player - macOS MVP

A high-quality digital audio player for macOS with bit-perfect playback, built with Rust, Core Audio, and Slint.

## Features

- **Bit-perfect playback** through Core Audio
- **Format support**: FLAC, WAV, AIFF, MP3, M4A (via Symphonia)
- **Sample rates**: 44.1kHz to 192kHz
- **Bit depths**: 16, 24, 32-bit
- **Device selection**: Choose any Core Audio output device
- **Low-latency**: Lock-free ring buffer for audio streaming
- **Modern UI**: Built with Slint for a clean, responsive interface

## Prerequisites

1. **macOS** (Intel or Apple Silicon with Rosetta 2)
2. **Rust** (latest stable)
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```
3. **Homebrew** (for dependencies)
   ```bash
   /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
   ```

## Building

1. Clone the repository and navigate to the project directory

2. Install dependencies:
   ```bash
   # Install libsndfile (required by symphonia)
   brew install libsndfile
   ```

3. Build the project:
   ```bash
   cargo build --release
   ```

4. Run the player:
   ```bash
   cargo run --release
   ```

## Project Structure

```
audiophile-player/
├── Cargo.toml          # Project configuration
├── build.rs            # Build script for Slint UI
├── src/
│   ├── main.rs         # Application entry point
│   ├── audio/
│   │   └── mod.rs      # Core Audio engine
│   ├── decoder.rs      # Audio file decoder (Symphonia)
│   └── ui/
│       └── player.slint # UI definition
```

## Architecture Overview

### Audio Pipeline

1. **File Decoder** (Symphonia)
   - Decodes audio files to PCM
   - Supports multiple formats
   - Handles seeking

2. **Ring Buffer**
   - Lock-free audio streaming
   - Decouples decoding from playback
   - Minimizes latency

3. **Core Audio Output**
   - Direct HAL access for bit-perfect playback
   - Device enumeration and selection
   - Format negotiation

### UI Components (Slint)

- **File Browser**: Load audio files
- **Playback Controls**: Play, pause, stop
- **Progress Bar**: Seek functionality
- **Device Selector**: Choose output device
- **Volume Control**: Software volume adjustment
- **Info Display**: Sample rate, bit depth, format

## Usage

1. Click "Load File" to select an audio file
2. Choose your output device from the dropdown
3. Click Play to start playback
4. Use the progress bar to seek
5. Adjust volume with the slider

## Bit-Perfect Playback

The player achieves bit-perfect playback by:
- Using Core Audio's HAL layer directly
- Matching the device's native sample rate
- Avoiding macOS's audio mixer
- Using exclusive mode when available

## Future Enhancements (Phase 1+)

- DSP engine with upsampling
- DSD support
- Exclusive mode (hog mode)
- Integer mode for compatible DACs
- Network streaming (Roon integration)
- Customizable filters

## Troubleshooting

### Build Issues
- Ensure Xcode Command Line Tools are installed: `xcode-select --install`
- Check that libsndfile is properly installed: `pkg-config --libs sndfile`

### Audio Issues
- Check Audio MIDI Setup for device configuration
- Ensure no other apps are using exclusive mode
- Try different buffer sizes if experiencing dropouts

### Performance
- Build in release mode for optimal performance
- Close other audio applications
- Disable any system audio enhancements

## License

MIT License - See LICENSE file for details