use std::sync::Arc;

use gpui::{
    div, img, prelude::*, ClickEvent, Context, Entity, ImageSource, ObjectFit, RenderImage,
    SharedString, Task, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    slider::{Slider, SliderEvent, SliderState},
    theme::ActiveTheme,
    v_flex,
};
use player_core::{AudioStream, Frame, PixelFormat, PlaybackConfig, PlaybackEvent, VideoPlayer};

fn format_time(us: u64) -> String {
    let total_secs = (us as f64 / 1_000_000.0).round() as u64;
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if hours > 0 {
        format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
    } else {
        format!("{:02}:{:02}", minutes, seconds)
    }
}

/// Speaker glyph for a 0.0–1.0 level, matching the three steps player-slint
/// shows.
fn speaker_icon(level: f32) -> &'static str {
    if level <= 0.0 {
        "🔇"
    } else if level < 0.5 {
        "🔉"
    } else {
        "🔊"
    }
}

/// GPUI's renderer reads `RenderImage` bytes as BGRA, so the player is
/// configured with [`PixelFormat::Bgra`] and the scaler hands us that layout
/// directly — the buffer below is `RgbaImage` only because that is the
/// container `RenderImage` is built from; its bytes are B, G, R, A. Video
/// frames are opaque, so premultiplied alpha is a non-issue.
///
/// Still a full-frame copy (`RenderImage` owns its pixels), so call this off
/// the foreground executor — see the frame listener.
fn to_render_image(frame: &Frame) -> Arc<RenderImage> {
    let buffer = image::RgbaImage::from_raw(frame.width, frame.height, frame.data.to_vec())
        .expect("frame data length matches dimensions");
    Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(
        buffer
    )]))
}

pub struct PlayerView {
    /// `None` until the startup task has opened the stream; also holds the
    /// cpal stream, which must stay alive for audio to keep playing.
    player: Option<VideoPlayer>,
    _audio_stream: Option<AudioStream>,
    /// Startup + event/frame pump; dropping the view cancels it.
    _listener: Option<Task<()>>,
    frame: Option<Arc<RenderImage>>,
    /// A decoded frame landed since the last draw; see `render`.
    frame_pending: bool,
    duration_us: u64,
    elapsed_us: u64,
    is_playing: bool,
    status: SharedString,
    timeline: Entity<SliderState>,
    volume: Entity<SliderState>,
    /// 0.0–1.0, mirroring the volume slider. Kept here so the speaker icon can
    /// read it without borrowing the slider entity, and so a level set before
    /// the stream opens can be applied to the player once it exists.
    volume_level: f32,
}

impl PlayerView {
    pub fn new(url: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let timeline = cx.new(|_| SliderState::new().min(0.0).max(1.0).step(0.0001));
        cx.subscribe(&timeline, |this, _, ev, _cx| {
            let SliderEvent::Change(value) = ev;
            this.seek_to_fraction(value.start());
        })
        .detach();

        let volume = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(1.0)
                .step(0.01)
                .default_value(1.0)
        });
        cx.subscribe(&volume, |this, _, ev, cx| {
            let SliderEvent::Change(value) = ev;
            this.set_volume(value.start());
            // The speaker glyph reads `volume_level`; without this it would
            // only catch up on the next decoded frame, so it would sit stale
            // while paused.
            cx.notify();
        })
        .detach();

        let listener = cx.spawn_in(window, async move |this, cx| {
            // Created inside the task rather than in `new` so the cpal stream
            // and decoder only spin up once the window actually exists.
            let (mut player, audio_stream) = VideoPlayer::with_config(PlaybackConfig {
                // GPUI uploads `RenderImage` bytes as BGRA; have the scaler
                // emit that directly rather than swizzling every frame here.
                output_format: PixelFormat::Bgra,
                ..PlaybackConfig::default()
            });
            let mut events_rx = match player.start_playback(&url).await {
                Ok(rx) => rx,
                Err(e) => {
                    tracing::warn!(target: "app", "failed to start playback: {e:?}");
                    let _ = this.update(cx, |view, cx| {
                        view.status = format!("Error: {e}").into();
                        cx.notify();
                    });
                    return;
                }
            };
            let mut frame_rx = player.frame_buffer.subscribe();

            if this
                .update(cx, |view, cx| {
                    // The slider is live before the stream opens, so carry
                    // whatever it reads now onto the player rather than
                    // letting that first move silently do nothing.
                    player.set_volume((view.volume_level * 100.0).round() as u32);
                    view.player = Some(player);
                    view._audio_stream = audio_stream;
                    view.is_playing = true;
                    view.status = "Playing".into();
                    cx.notify();
                })
                .is_err()
            {
                return;
            }

            loop {
                tokio::select! {
                    ev = events_rx.recv() => match ev {
                        Some(PlaybackEvent::Duration(us)) => {
                            if this.update(cx, |view, cx| {
                                view.duration_us = us;
                                cx.notify();
                            }).is_err() {
                                break;
                            }
                        }
                        Some(PlaybackEvent::Ended) => {
                            let _ = this.update(cx, |view, cx| {
                                view.status = "Finished".into();
                                view.is_playing = false;
                                cx.notify();
                            });
                            break;
                        }
                        Some(PlaybackEvent::Error(msg)) => {
                            let _ = this.update(cx, |view, cx| {
                                view.status = format!("Error: {msg}").into();
                                view.is_playing = false;
                                cx.notify();
                            });
                            break;
                        }
                        None => break,
                    },
                    changed = frame_rx.changed() => {
                        if let Err(e) = changed {
                            tracing::warn!(target: "app", "frame watch closed: {e}");
                            continue;
                        }
                        let frame_opt = frame_rx.borrow_and_update().clone();
                        let Some(frame) = frame_opt else { continue };
                        tracing::debug!(target: "app",
                            "frame received {}x{} ts={}us", frame.width, frame.height, frame.ts_us);
                        // The copy into `RenderImage` is ~4 MB a frame. Run on
                        // the foreground executor it starves the window's
                        // paint messages — those are WM_PAINT on Windows, the
                        // lowest-priority message there is, so a steady drip
                        // of posted task wake-ups is enough to stop the window
                        // redrawing entirely and have the OS mark it as not
                        // responding.
                        let converting = frame.clone();
                        let render = cx
                            .background_spawn(async move { to_render_image(&converting) })
                            .await;
                        let res = this.update_in(cx, |view, window, cx| {
                            // Every frame is a fresh RenderImage with a new
                            // id; without this the sprite atlas accumulates
                            // one texture per decoded frame.
                            if let Some(prev) = view.frame.take() {
                                let _ = window.drop_image(prev);
                            }
                            view.frame = Some(render);
                            view.frame_pending = true;
                            view.elapsed_us = frame.ts_us;
                            if view.duration_us > 0 {
                                let frac =
                                    (frame.ts_us as f64 / view.duration_us as f64).clamp(0.0, 1.0);
                                view.timeline.update(cx, |slider, cx| {
                                    slider.set_value(frac as f32, window, cx);
                                });
                            }
                            // `cx.notify()` alone — `window.refresh()` would
                            // invalidate every view in the window and discard
                            // cached elements, once per decoded frame.
                            cx.notify();
                        });
                        if let Err(e) = res {
                            tracing::warn!(target: "app", "frame update failed, stopping listener: {e}");
                            break;
                        }
                    }
                }
            }
        });

        Self {
            player: None,
            _audio_stream: None,
            _listener: Some(listener),
            frame: None,
            frame_pending: false,
            duration_us: 0,
            elapsed_us: 0,
            is_playing: false,
            status: "Loading…".into(),
            timeline,
            volume,
            volume_level: 1.0,
        }
    }

    fn seek_to_fraction(&self, fraction: f32) {
        // Live streams / unknown duration: nothing sensible to seek to.
        if self.duration_us == 0 {
            return;
        }
        let target_us = (fraction.clamp(0.0, 1.0) as f64 * self.duration_us as f64) as u64;
        if let Some(player) = &self.player {
            player.seek_to_us(target_us);
        }
    }

    /// `level` is 0.0–1.0; player-core takes 0–100.
    fn set_volume(&mut self, level: f32) {
        self.volume_level = level.clamp(0.0, 1.0);
        let vol = (self.volume_level * 100.0).round() as u32;
        tracing::debug!(target: "app", "volume set to {vol}");
        if let Some(player) = &self.player {
            player.set_volume(vol);
        }
    }

    fn on_play_pause(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(player) = &self.player {
            let is_paused = player.toggle_pause();
            self.is_playing = !is_paused;
            self.status = if is_paused { "Paused" } else { "Playing" }.into();
            cx.notify();
        }
    }
}

impl Render for PlayerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        tracing::debug!(target: "app",
            "render: frame={} elapsed={}us", self.frame.is_some(), self.elapsed_us);
        // Re-arm the display link only for a frame that actually arrived.
        // Entity notifies alone don't sustain draws on macOS — the display
        // link only polls the dirty flag while it runs — so a decoded frame
        // schedules one more callback to carry the loop forward. Arming this
        // on `is_playing` instead pins the window at the refresh rate whether
        // or not video advanced, which is enough on its own to saturate the
        // foreground executor and leave the window unresponsive.
        if std::mem::take(&mut self.frame_pending) {
            window.request_animation_frame();
        }
        let video = match self.frame.clone() {
            Some(frame) => img(ImageSource::Render(frame))
                .object_fit(ObjectFit::Contain)
                .size_full()
                .into_any_element(),
            None => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(self.status.clone())
                .into_any_element(),
        };

        let total_label = if self.duration_us > 0 {
            format_time(self.duration_us)
        } else {
            "--:--".to_string()
        };

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .child(div().flex_1().min_h_0().bg(gpui::black()).child(video))
            .child(
                h_flex()
                    .gap_3()
                    .p_3()
                    .items_center()
                    .child(
                        Button::new("play-pause")
                            .primary()
                            .label(if self.is_playing { "Pause" } else { "Play" })
                            .on_click(cx.listener(Self::on_play_pause)),
                    )
                    .child(format_time(self.elapsed_us))
                    .child(
                        div()
                            .flex_1()
                            .child(Slider::new(&self.timeline).disabled(self.duration_us == 0)),
                    )
                    .child(total_label)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(speaker_icon(self.volume_level))
                            .child(div().w_24().child(Slider::new(&self.volume))),
                    ),
            )
    }
}
