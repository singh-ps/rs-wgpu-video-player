# rs-wgpu-video-player

A Rust video player using FFmpeg for decoding (hardware-accelerated where
available), cpal for audio output, and a choice of UI toolkit.

The playback pipeline lives in `player-core` and knows nothing about the UI.
Two frontends drive it:

| Crate | Toolkit | Status |
| --- | --- | --- |
| `player-slint` | [Slint](https://slint.dev) | default workspace member |
| `player-gpui` | [GPUI](https://www.gpui.rs) | newer, fewer controls |

> The repo is still named `rs-wgpu-video-player` for git-history continuity;
> neither frontend uses wgpu directly any more.

## Features

- **Hardware video decoding** when FFmpeg exposes a HW config for the codec
  (D3D11VA / DXVA2 on Windows, VAAPI on Linux, VideoToolbox on macOS).
  Falls back to software decoding silently.
- **Audio playback** via cpal (48 kHz stereo f32, resampled from source).
- **Threaded pipeline**: demux, video decode, audio decode, audio output,
  and UI each run independently with bounded queues between them.
- **Audio-paced presentation**: video frames timed against the audio
  sample-consumption clock; late frames dropped.
- **Seeking** by timeline slider; rapid scrubbing coalesces to the most
  recent target.
- **Frontend-chosen pixel layout**: the scaler writes RGBA or BGRA per
  `PlaybackConfig::output_format`, so neither frontend has to swizzle
  frames on its UI thread.
- **HLS / network streams** through FFmpeg's `avformat`.

## Requirements

- Rust 1.85+ (GPUI is edition 2024; the Slint frontend alone needs less)
- FFmpeg 7.x development libraries — `ffmpeg-next` is pinned to `=8.0.0`,
  which targets FFmpeg 7.1 (`libavformat` 61)
- A working default audio output device

### Installing FFmpeg

**Ubuntu/Debian**
```bash
sudo apt install libavcodec-dev libavformat-dev libavutil-dev \
                 libswscale-dev libswresample-dev
```
Check the version first — distributions through Ubuntu 24.04 ship FFmpeg
6.x, which `ffmpeg-sys-next 8.0` will not build against. Build FFmpeg 7.x
from source or use a backport if `apt` gives you 6.x.

**macOS**
```bash
brew install ffmpeg
```

**Windows**
Either install via vcpkg:
```bash
vcpkg install ffmpeg:x64-windows
```
or get the shared libraries from <https://ffmpeg.org/download.html> and set
`FFMPEG_DIR` to the install root before `cargo build`.

Either way the FFmpeg DLLs must be on `PATH` **at runtime**, not just at
build time — otherwise the binary dies at startup with a
`avformat-61.dll was not found` dialog and no log output. For a vcpkg
install that is:
```powershell
$env:PATH = "$env:VCPKG_ROOT\installed\x64-windows\bin;" + $env:PATH
```

## Usage

The Slint frontend is the workspace's default member, so a bare `cargo run`
builds it. Pass `-p player-gpui` for the GPUI one.

```bash
# Slint — default test stream
cargo run --release

# GPUI
cargo run --release -p player-gpui

# specific URL or file (either frontend)
cargo run --release -- "https://example.com/stream.m3u8"
cargo run --release -- "C:\path\to\video.mp4"
```

Controls:

| | Slint | GPUI |
| --- | --- | --- |
| Play / Pause | ✅ | ✅ |
| Timeline scrub | ✅ | ✅ |
| Volume slider (0–100%, applied in the cpal callback) | ✅ | ✅ |
| Elapsed / duration | ✅ | ✅ |
| Stop — tear down decoder threads | ✅ | — |
| Load Stream — open a URL from a text field | ✅ | — |

The GPUI frontend takes its URL from `argv` only; there is no in-app field.

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
  After probing, unused streams are discarded in FFmpeg so HLS playback
  does not keep downloading every bitrate variant.
- **Video decoder** opens HW context before `avcodec_open2`, transfers HW
  frames to system memory with `av_hwframe_transfer_data`, scales to the
  frontend's chosen 32-bit layout (RGBA or BGRA) with libswscale, paces
  against the audio clock, then pushes to the
  `FrameBuffer`. Already-late frames are dropped before readback. If the
  audio clock stops moving for longer than the stall timeout, pacing latches
  onto wall-clock and stays there until audio resumes — so audio that never
  comes back cannot hold the picture for the rest of the session.
- **Audio decoder** resamples to 48 kHz stereo f32 packed, pushes into a
  capped ring with drop-oldest on overflow.
- **cpal callback** drains the ring and advances the master clock by media
  samples consumed, with driver-reported output latency subtracted.
  Temporary underrun silence does not advance the media clock, so video
  waits for audio during buffering. Overflow skips are included in the
  clock; after audio EOF, the clock can continue for a longer video track.
- **UI task** awaits `changed()` on the watch channel and copies the frame
  into the toolkit's own image type. Both frontends do that copy off the UI
  thread — Slint on a Tokio worker, then `upgrade_in_event_loop`; GPUI on
  its background executor, then an entity update. Doing it on the UI thread
  starves the window's paint messages and the window stops redrawing.

## Project structure

```
crates/
├── player-core/                  # UI-agnostic playback pipeline
│   ├── src/lib.rs                # VideoPlayer facade
│   ├── src/config.rs             # PlaybackConfig + PixelFormat
│   ├── src/audio_player.rs       # cpal stream + sample ring + clock
│   ├── src/frame_buffer.rs       # watch-channel latest-frame buffer
│   ├── src/state.rs              # shared pause / seek / volume state
│   ├── src/decoder/
│   │   ├── demux.rs              # packet dispatch + seek commands
│   │   ├── video.rs              # decode, scale, pace
│   │   ├── audio.rs              # decode + resample
│   │   └── hw.rs                 # hardware device negotiation
│   └── examples/playback_probe.rs
├── player-slint/
│   ├── src/app.rs                # Slint window + callback wiring
│   └── ui/app_window.slint       # UI definition
└── player-gpui/
    ├── src/main.rs               # entry point — parses URL arg
    └── src/player_view.rs        # GPUI view + frame pump
```

## Known limitations

- **Does not follow the Windows default-output change.** cpal binds the
  stream to a device at open time. Changing the OS default audio output
  won't migrate playback. Restart the app to pick up the new device.
- **A/V sync is audio-master with no PLL.** Long playback can drift if the
  source's audio rate differs from the device rate; small late frames are
  dropped, but no continuous resampling.
- **Frame queue is single-slot (watch).** If UI is slower than decoder
  push rate, intermediate frames are dropped before display.
- **No subtitle / multi-stream / HDR support.**
- **GPUI frontend has no Stop or URL field.** It plays whatever `argv` gave
  it until the stream ends or the window closes.
- **The timeline slider has no drag latch.** Both frontends write the
  playhead into the slider from every decoded frame, with no suppression
  while the pointer is down, so a scrub competes with the position updates
  arriving underneath it.

## Building

```bash
cargo build                    # default member (player-slint), debug
cargo build --release          # optimized
cargo build --workspace        # both frontends
cargo test -p player-core      # pipeline unit tests
```

## Playback diagnostics

Run the core pipeline with audio output but without any UI:

```bash
cargo run -p player-core --example playback_probe -- "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8" 60
```

This reports observed frames per interval, video/audio timestamps, their
difference in milliseconds, and queued audio samples. A negative difference
means the latest observed video frame trails the audio clock. It measures
the core pipeline, not the time a frame actually appears on screen.

### Logging

Logs are tagged by subsystem — `video`, `audio`, `demux`, `decoder`, `app`
— rather than by module path, so a crate-name directive like
`player_core=info` matches nothing and silences everything. Both frontends
default to `info` on each of those targets; override per subsystem:

```bash
RUST_LOG=video=debug,app=debug cargo run -p player-gpui
```

`app=debug` on a frontend traces frame arrival, draws and volume changes,
which is the quickest way to tell a stalled decoder apart from a UI that
has stopped redrawing.

## License

MIT. See [LICENSE](LICENSE).
