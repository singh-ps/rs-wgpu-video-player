# WGPU Video Player

A Rust-based video player leveraging WGPU for hardware-accelerated GPU rendering and FFmpeg for video decoding. Designed for high-performance video playback with support for various formats including HLS streams.

## Features

- **GPU-Accelerated Rendering**: Uses WGPU (WebGPU) for efficient video frame rendering
- **FFmpeg Integration**: Supports wide range of video formats and codecs
- **HLS Streaming**: Play HTTP Live Streaming (m3u8) content
- **Frame-Accurate Timing**: Synchronized playback using PTS-based timing
- **Aspect Ratio Preservation**: Automatic letterboxing/pillarboxing
- **Cross-Platform**: Works on Windows, macOS, and Linux

## Architecture

- **Video Decoder**: FFmpeg-based decoder running in separate thread
- **Frame Buffer**: Lock-free single-slot buffer using Tokio watch channels
- **Renderer**: WGPU pipeline with fullscreen triangle rendering
- **Async Runtime**: Tokio for concurrent task management

## Requirements

- **Rust**: 1.70 or newer (latest stable recommended)
- **FFmpeg**: System FFmpeg libraries (development headers)
- **GPU**: Graphics card with Vulkan, Metal, or DirectX 12 support

### Installing FFmpeg

**Ubuntu/Debian:**
```bash
sudo apt install libavcodec-dev libavformat-dev libavutil-dev libswscale-dev
```

**macOS:**
```bash
brew install ffmpeg
```

**Windows:**
Download FFmpeg shared libraries from [ffmpeg.org](https://ffmpeg.org/download.html) and set `FFMPEG_DIR` environment variable.

## Usage

### Run with default video stream

```bash
cargo run
```

This will play the default test stream: `https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8`

### Run with custom video URL

```bash
cargo run -- "https://your-video-url.com/video.m3u8"
```

### Supported formats
The player supports any format/codec that your system's FFmpeg installation can decode, including:
- HLS streams (.m3u8)
- MP4, MKV, AVI, WebM containers
- Common codecs (H.264, H.265, VP8, VP9, AV1, etc.)

### Controls

- **ESC**: Exit the player
- **Close Window**: Stop playback and exit

## Building

### Debug build
```bash
cargo build
```

### Release build (optimized)
```bash
cargo build --release
```

## Running the release build

```bash
./target/release/rs-wgpu-video-player "https://your-video-url.com/video.m3u8"
```

## Project Structure

```
src/
├── main.rs           # Application entry point
├── app.rs            # Event loop and playback coordination
├── renderer.rs       # WGPU rendering pipeline
├── shader.wgsl       # GPU shader code
└── video_player/     # Video playback module
    ├── mod.rs        # Public API
    ├── decoder.rs    # FFmpeg decoder loop
    ├── frame_buffer.rs  # Frame synchronization
    └── probe.rs      # Video metadata extraction
```

## Future Plans

- Audio playback support
- Playback controls (play/pause, seek, volume)
- GUI overlay with egui
- Extract video player into reusable crate
- Hardware video decoding (VAAPI, NVDEC, VideoToolbox)

## License

Licensed under the MIT License. See [LICENSE](LICENSE) file for details.
