use crate::video_player::{get_video_info, VideoPlayer};
use std::{
    error::Error,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};
use slint::{Image, SharedPixelBuffer, Rgba8Pixel, ComponentHandle};

slint::include_modules!();

/// Lock helper: recover from poisoning instead of panicking.
fn lock<'a, T>(m: &'a Mutex<T>) -> MutexGuard<'a, T> {
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

pub struct App {}

impl App {
    pub async fn run(&self, url: String) -> Result<(), Box<dyn Error>> {
        // Initialize the Slint window
        let app = AppWindow::new()?;
        app.set_current_url(url.clone().into());

        // Thread-safe containers for playback state
        let player = Arc::new(Mutex::new(None::<VideoPlayer>));
        let listener_handle = Arc::new(Mutex::new(None::<tokio::task::JoinHandle<()>>));

        // Shared duration state for timeline percentage calculations
        let duration_secs = Arc::new(Mutex::new(0.0));

        // Define a helper closure to stop any active playback
        let stop_playback = {
            let player = player.clone();
            let listener_handle = listener_handle.clone();
            let app_weak = app.as_weak();
            move || {
                // Abort the active frame updater thread
                if let Some(handle) = lock(&listener_handle).take() {
                    handle.abort();
                }

                // Stop the actual decoder thread
                if let Some(mut p) = lock(&player).take() {
                    p.stop_playback();
                }

                // Reset the UI state
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

        // Define a helper closure to start playing a stream
        let start_playback = {
            let player = player.clone();
            let listener_handle = listener_handle.clone();
            let duration_secs = duration_secs.clone();
            let app_weak = app.as_weak();
            let stop_playback_clone = stop_playback.clone();
            move |stream_url: String| {
                // First stop existing
                stop_playback_clone();

                let mut new_player = VideoPlayer::new();
                let app_weak_task = app_weak.clone();

                // 1. Kick off async FFmpeg metadata probe to get the video duration and size
                let url_clone = stream_url.clone();
                let duration_secs_clone = duration_secs.clone();
                tokio::spawn(async move {
                    if let Ok(info) = get_video_info(&url_clone) {
                        if let Some(dur_us) = info.duration_us {
                            let secs = dur_us as f64 / 1_000_000.0;
                            *lock(&duration_secs_clone) = secs;
                            
                            let formatted = format_time(secs);
                            let _ = app_weak_task.upgrade_in_event_loop(move |ui| {
                                ui.set_total_duration(formatted.into());
                            });
                        }
                    }
                });

                // 2. Start the decoder background thread
                let start_res = tokio::task::block_in_place(|| {
                    let rt = tokio::runtime::Handle::current();
                    rt.block_on(async {
                        new_player.start_playback(
                            &stream_url,
                            Default::default()
                        ).await
                    })
                });

                if let Err(e) = start_res {
                    eprintln!("Failed to start playback: {:?}", e);
                    if let Some(ui) = app_weak.upgrade() {
                        ui.set_status_text(format!("Error: {}", e).into());
                    }
                    return;
                }

                // Subscribe to frames before saving the player
                let mut frame_rx = new_player.frame_buffer.subscribe();
                *lock(&player) = Some(new_player);

                // 3. Spawn a tokio task that waits for frames and updates the UI
                let app_weak_listener = app_weak.clone();
                let duration_secs_listener = duration_secs.clone();
                let handle = tokio::spawn(async move {
                    while frame_rx.changed().await.is_ok() {
                        let frame_opt = frame_rx.borrow().clone();
                        if let Some(frame) = frame_opt {
                            let width = frame.width;
                            let height = frame.height;
                            let ts_us = frame.ts_us;
                            let data = frame.data.clone();

                            // Convert raw RGB/RGBA frame to Slint SharedPixelBuffer (which is Send)
                            let slint_buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                                &data,
                                width,
                                height,
                            );

                            let elapsed_secs = ts_us as f64 / 1_000_000.0;
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
                        } else {
                            // End of stream reached
                            let app_weak_ui = app_weak_listener.clone();
                            let _ = app_weak_ui.upgrade_in_event_loop(move |ui| {
                                ui.set_status_text("Finished".into());
                                ui.set_is_playing(false);
                            });
                            break;
                        }
                    }
                });

                *lock(&listener_handle) = Some(handle);

                // Update UI status to playing
                if let Some(ui) = app_weak.upgrade() {
                    ui.set_status_text("Playing".into());
                    ui.set_is_playing(true);
                }
            }
        };

        // --- Bind Slint UI Callbacks ---

        // Play/Pause Callback
        let player_clone = player.clone();
        let app_weak = app.as_weak();
        app.on_play_pause_clicked(move || {
            if let Some(ref p) = *lock(&player_clone) {
                let is_paused = p.toggle_pause();
                if let Some(ui) = app_weak.upgrade() {
                    ui.set_is_playing(!is_paused);
                    ui.set_status_text(if is_paused { "Paused".into() } else { "Playing".into() });
                }
            }
        });

        // Stop Callback
        let stop_playback_clone = stop_playback.clone();
        app.on_stop_clicked(move || {
            stop_playback_clone();
        });

        // Load Stream URL Callback
        let start_playback_clone = start_playback.clone();
        app.on_load_url_clicked(move |stream_url| {
            start_playback_clone(stream_url.to_string());
        });

        // Volume Callback — immediately update the atomic read by cpal
        let player_clone = player.clone();
        app.on_volume_changed(move |val| {
            let vol = (val.clamp(0.0, 1.0) * 100.0).round() as u32;
            if let Some(ref p) = *lock(&player_clone) {
                p.set_volume(vol);
            }
        });

        // Kickoff initial playback if a URL was passed
        if !url.is_empty() {
            start_playback(url);
        }

        // Run the Slint main event loop
        app.run()?;

        // Clean up everything upon exiting the window
        stop_playback();

        Ok(())
    }
}
