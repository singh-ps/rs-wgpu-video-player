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
use player_core::{AudioStream, Frame, PlaybackEvent, VideoPlayer};

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

/// GPUI's renderer reads `RenderImage` bytes as BGRA; player-core delivers
/// RGBA, so swizzle while copying. Video frames are opaque, so premultiplied
/// alpha is a non-issue.
fn to_render_image(frame: &Frame) -> Arc<RenderImage> {
    let mut data = frame.data.to_vec();
    for px in data.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(frame.width, frame.height, data)
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
    duration_us: u64,
    elapsed_us: u64,
    is_playing: bool,
    status: SharedString,
    timeline: Entity<SliderState>,
}

impl PlayerView {
    pub fn new(url: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let timeline = cx.new(|_| SliderState::new().min(0.0).max(1.0).step(0.0001));
        cx.subscribe(&timeline, |this, _, ev, _cx| {
            let SliderEvent::Change(value) = ev;
            this.seek_to_fraction(value.start());
        })
        .detach();

        let listener = cx.spawn_in(window, async move |this, cx| {
            // Created inside the task rather than in `new` so the cpal stream
            // and decoder only spin up once the window actually exists.
            let (mut player, audio_stream) = VideoPlayer::new();
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
                        let render = to_render_image(&frame);
                        let res = this.update_in(cx, |view, window, cx| {
                            // Every frame is a fresh RenderImage with a new
                            // id; without this the sprite atlas accumulates
                            // one texture per decoded frame.
                            if let Some(prev) = view.frame.take() {
                                let _ = window.drop_image(prev);
                            }
                            view.frame = Some(render);
                            view.elapsed_us = frame.ts_us;
                            if view.duration_us > 0 {
                                let frac =
                                    (frame.ts_us as f64 / view.duration_us as f64).clamp(0.0, 1.0);
                                view.timeline.update(cx, |slider, cx| {
                                    slider.set_value(frac as f32, window, cx);
                                });
                            }
                            cx.notify();
                            window.refresh();
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
            duration_us: 0,
            elapsed_us: 0,
            is_playing: false,
            status: "Loading…".into(),
            timeline,
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
        // Drive redraws off the display link while playing. Entity notifies
        // alone don't sustain draws here: they mark the window dirty, but the
        // macOS display link only polls that flag while it runs, and nothing
        // restarts it for a notify. An armed next-frame callback does, so each
        // render schedules the next one for as long as video is advancing.
        // `window.refresh()` in the frame listener kick-starts the loop: the
        // first event-driven draw that sees `is_playing` re-arms it.
        if self.is_playing {
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
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .bg(gpui::black())
                    .child(video),
            )
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
                    .child(total_label),
            )
    }
}
