# rs-wgpu-video-player

A Rust video player using FFmpeg for decoding (hardware-accelerated where
available), cpal for audio output, and Slint for the UI.

> The crate is still named `rs-wgpu-video-player` for git-history continuity;
> the current renderer is Slint (was wgpu in early commits).

## Features

- **Hardware video decoding** when FFmpeg exposes a HW config for the codec
  (D3D11VA / DXVA2 on Windows, VAAPI on Linux, VideoToolbox on macOS).
  Falls back to software decoding silently.
- **Audio playback** via cpal (48 kHz stereo f32, resampled from source).
- **Threaded pipeline**: demux, video decode, audio decode, audio output,
  and UI each run independently with bounded queues between them.
- **Audio-paced presentation**: video frames timed against the audio
  sample-consumption clock; late frames dropped.
- **Slint UI** with play/pause/stop, volume slider, elapsed/duration
  display, and read-only progress bar.
- **HLS / network streams** through FFmpeg's `avformat`.

## Requirements

- Rust 1.70+
- FFmpeg 6.x or 7.x development libraries
- A working default audio output device

### Installing FFmpeg

**Ubuntu/Debian**
```bash
sudo apt install libavcodec-dev libavformat-dev libavutil-dev \
                 libswscale-dev libswresample-dev
```

**macOS**
```bash
brew install ffmpeg
```

**Windows**
Get FFmpeg shared libraries from <https://ffmpeg.org/download.html> and set
`FFMPEG_DIR` to the install root before `cargo build`.

## Usage

```bash
# default test stream
cargo run --release

# specific URL or file
cargo run --release -- "https://example.com/stream.m3u8"
cargo run --release -- "C:\path\to\video.mp4"
```

UI controls:

- **Load Stream** — opens the URL from the text field
- **Play / Pause** — toggle playback
- **Stop** — terminate decoder threads
- **Volume slider** — 0–100% (applied in cpal callback)
- **Progress bar** — display only (see *Known limitations*)

## Architecture

```
┌──────────┐   pkts    ┌──────────────┐  frames  ┌──────────────┐
│  demux   │──────────▶│ video decode │─────────▶│ FrameBuffer  │──▶ UI
│ (ffmpeg) │           │ (HW + sws)   │          │ (watch chan) │
└────┬─────┘           └──────────────┘          └──────────────┘
     │ apkts
     ▼
┌──────────────┐  samples  ┌─────────────────┐  cpal cb  ┌────────┐
│ audio decode │──────────▶│ AudioPlayer ring│──────────▶│ device │
│ (swresample) │           │ (bounded 5 s)   │           │  out   │
└──────────────┘           └─────────────────┘           └────────┘
```

- **Demux thread** pulls packets, dispatches video freely; audio dispatch
  is throttled by sample-ring depth (≥ 3 s pre-buffer pauses demux audio).
- **Video decoder** opens HW context before `avcodec_open2`, transfers HW
  frames to system memory with `av_hwframe_transfer_data`, scales to RGBA
  with libswscale, paces against the audio clock, then pushes to the
  `FrameBuffer`.
- **Audio decoder** resamples to 48 kHz stereo f32 packed, pushes into a
  capped ring with drop-oldest on overflow.
- **cpal callback** drains the ring, advances `samples_consumed` (the
  master clock) and tracks driver-reported output latency.
- **UI task** awaits `changed()` on the watch channel, copies the frame
  into a Slint `SharedPixelBuffer`, and triggers `upgrade_in_event_loop`.

## Project structure

```
src/
├── main.rs                       # entry point — parses URL arg
├── app.rs                        # Slint window + callback wiring
└── video_player/
    ├── mod.rs                    # VideoPlayer facade
    ├── decoder.rs                # demux + video/audio decode threads
    ├── audio_player.rs           # cpal stream + sample ring + clock
    ├── frame_buffer.rs           # watch-channel based latest-frame buffer
    └── probe.rs                  # duration / dimensions probe
ui/
└── app_window.slint              # UI definition
```

## Known limitations

- **No seeking.** The decoder thread has no seek path; the progress bar is
  read-only.
- **Does not follow the Windows default-output change.** cpal binds the
  stream to a device at open time. Changing the OS default audio output
  won't migrate playback. Restart the app to pick up the new device.
- **A/V sync is audio-master with no PLL.** Long playback can drift if the
  source's audio rate differs from the device rate; small late frames are
  dropped, but no continuous resampling.
- **Frame queue is single-slot (watch).** If UI is slower than decoder
  push rate, intermediate frames are dropped before display.
- **No subtitle / multi-stream / HDR support.**

## Building

```bash
cargo build           # debug
cargo build --release # optimized
```

## License

MIT. See [LICENSE](LICENSE).
