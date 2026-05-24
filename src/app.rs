use crate::video_player::{PlaybackEvent, VideoPlayer};
use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer};
use std::{
    cell::RefCell,
    error::Error,
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

slint::include_modules!();

/// Lock helper: recover from poisoning instead of panicking.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn format_time(secs: f64) -> String {
    let total_secs = secs.round() as u64;
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if hours > 0 {
        format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
    } else {
        format!("{:02}:{:02}", minutes, seconds)
    }
}

/// Everything tied to one playback. Replaced wholesale on each load. Held in
/// an `Rc<RefCell<…>>` because all access happens on the Slint main thread;
/// the spawned listener task does not capture this.
struct Session {
    player: VideoPlayer,
    listener: tokio::task::JoinHandle<()>,
    /// cpal output stream — must outlive the player; dropped here when the
    /// session is replaced.
    #[allow(dead_code)]
    audio_stream: Option<cpal::Stream>,
}

pub struct App {}

impl App {
    pub async fn run(&self, url: String) -> Result<(), Box<dyn Error>> {
        let app = AppWindow::new()?;
        app.set_current_url(url.clone().into());

        let session: Rc<RefCell<Option<Session>>> = Rc::new(RefCell::new(None));

        let stop_playback = {
            let session = session.clone();
            let app_weak = app.as_weak();
            move || {
                if let Some(mut s) = session.borrow_mut().take() {
                    s.listener.abort();
                    s.player.stop_playback();
                }

                if let Some(ui) = app_weak.upgrade() {
                    ui.set_is_playing(false);
                    ui.set_status_text("Stopped".into());
                    ui.set_timeline_progress(0.0);
                    ui.set_elapsed_time("00:00".into());
                    ui.set_total_duration("--:--".into());
                    ui.set_video_frame(Image::default());
                }
            }
        };

        let start_playback = {
            let session = session.clone();
            let app_weak = app.as_weak();
            let stop_playback_clone = stop_playback.clone();
            move |stream_url: String| {
                stop_playback_clone();

                let (mut new_player, audio_stream) = VideoPlayer::new();

                let start_res = tokio::task::block_in_place(|| {
                    let rt = tokio::runtime::Handle::current();
                    rt.block_on(async { new_player.start_playback(&stream_url).await })
                });

                let mut rx = match start_res {
                    Ok(rx) => rx,
                    Err(e) => {
                        tracing::warn!(target: "app", "failed to start playback: {e:?}");
                        if let Some(ui) = app_weak.upgrade() {
                            ui.set_status_text(format!("Error: {}", e).into());
                        }
                        return;
                    }
                };

                let duration_secs_listener = Arc::new(Mutex::new(0.0_f64));
                let app_weak_listener = app_weak.clone();
                let listener = tokio::spawn(async move {
                    while let Some(ev) = rx.recv().await {
                        match ev {
                            PlaybackEvent::Frame(frame) => {
                                let slint_buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                                    &frame.data,
                                    frame.width,
                                    frame.height,
                                );

                                let elapsed_secs = frame.ts_us as f64 / 1_000_000.0;
                                let elapsed_str = format_time(elapsed_secs);

                                let total_secs = *lock(&duration_secs_listener);
                                let progress = if total_secs > 0.0 {
                                    (elapsed_secs / total_secs).clamp(0.0, 1.0)
                                } else {
                                    0.0
                                };

                                let app_weak_ui = app_weak_listener.clone();
                                let _ = app_weak_ui.upgrade_in_event_loop(move |ui| {
                                    let slint_img = Image::from_rgba8(slint_buf);
                                    ui.set_video_frame(slint_img);
                                    ui.set_elapsed_time(elapsed_str.into());
                                    ui.set_timeline_progress(progress as f32);
                                });
                            }
                            PlaybackEvent::Duration(us) => {
                                let secs = us as f64 / 1_000_000.0;
                                *lock(&duration_secs_listener) = secs;
                                let formatted = format_time(secs);
                                let app_weak_ui = app_weak_listener.clone();
                                let _ = app_weak_ui.upgrade_in_event_loop(move |ui| {
                                    ui.set_total_duration(formatted.into());
                                });
                            }
                            PlaybackEvent::Ended => {
                                let app_weak_ui = app_weak_listener.clone();
                                let _ = app_weak_ui.upgrade_in_event_loop(move |ui| {
                                    ui.set_status_text("Finished".into());
                                    ui.set_is_playing(false);
                                });
                                break;
                            }
                            PlaybackEvent::Error(msg) => {
                                let app_weak_ui = app_weak_listener.clone();
                                let _ = app_weak_ui.upgrade_in_event_loop(move |ui| {
                                    ui.set_status_text(format!("Error: {}", msg).into());
                                    ui.set_is_playing(false);
                                });
                                break;
                            }
                        }
                    }
                });

                *session.borrow_mut() = Some(Session {
                    player: new_player,
                    listener,
                    audio_stream,
                });

                if let Some(ui) = app_weak.upgrade() {
                    ui.set_status_text("Playing".into());
                    ui.set_is_playing(true);
                }
            }
        };

        let session_clone = session.clone();
        let app_weak = app.as_weak();
        app.on_play_pause_clicked(move || {
            if let Some(ref s) = *session_clone.borrow() {
                let is_paused = s.player.toggle_pause();
                if let Some(ui) = app_weak.upgrade() {
                    ui.set_is_playing(!is_paused);
                    ui.set_status_text(if is_paused {
                        "Paused".into()
                    } else {
                        "Playing".into()
                    });
                }
            }
        });

        let stop_playback_clone = stop_playback.clone();
        app.on_stop_clicked(move || {
            stop_playback_clone();
        });

        let start_playback_clone = start_playback.clone();
        app.on_load_url_clicked(move |stream_url| {
            start_playback_clone(stream_url.to_string());
        });

        let session_clone = session.clone();
        app.on_volume_changed(move |val| {
            let vol = (val.clamp(0.0, 1.0) * 100.0).round() as u32;
            if let Some(ref s) = *session_clone.borrow() {
                s.player.set_volume(vol);
            }
        });

        if !url.is_empty() {
            start_playback(url);
        }

        app.run()?;

        stop_playback();

        Ok(())
    }
}
