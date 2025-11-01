use crate::{renderer::Renderer, video_player::VideoPlayer};
use std::{
    error::Error,
    time::{Duration, Instant},
};
use winit::{
    dpi::LogicalSize,
    event::{Event, KeyEvent, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
    keyboard::{Key, NamedKey},
    window::WindowBuilder,
};

pub struct App {}

impl App {
    pub async fn run(&self, url: String) -> Result<(), Box<dyn Error>> {
        let mut video_player = VideoPlayer::new();
        video_player
            .start_playback(
                &url,
                Default::default(),
            )
            .await?;

        let event_loop = EventLoop::new()?;

        let window = WindowBuilder::new()
            .with_title("WGPU Video Player")
            .with_inner_size(LogicalSize::new(1270, 720))
            .with_resizable(false)
            .build(&event_loop)?;

        let mut renderer = Renderer::new(&window).await?;

        let mut last_ts_us: u64 = 0;
        let mut base_ts_us: Option<u64> = None;
        let mut wall_start: Option<Instant> = None;
        let mut first_frame_size: Option<(u32, u32)> = None;

        event_loop.run(|event, elwt| {
            elwt.set_control_flow(ControlFlow::Poll);
            match event {
                Event::WindowEvent { event, .. } => match event {
                    WindowEvent::CloseRequested => {
                        video_player.stop_playback();
                        elwt.exit();
                    }
                    WindowEvent::Resized(size) => renderer.resize(size),
                    WindowEvent::KeyboardInput {
                        event:
                            KeyEvent {
                                logical_key: Key::Named(NamedKey::Escape),
                                ..
                            },
                        ..
                    } => {
                        video_player.stop_playback();
                        elwt.exit();
                    }
                    WindowEvent::RedrawRequested => {
                        if let Some(frame) = video_player.get_latest_frame() {
                            if frame.ts_us != last_ts_us {
                                // First real frame → set video size + resize window to match
                                if first_frame_size.is_none() {
                                    first_frame_size = Some((frame.width, frame.height));
                                    renderer.set_video_size(frame.width, frame.height);

                                    let logical =
                                        LogicalSize::new(frame.width as f64, frame.height as f64);
                                    let _ = window.request_inner_size(logical);
                                }

                                renderer.set_frame_data(frame.width, frame.height, &frame.data);

                                last_ts_us = frame.ts_us;
                                if base_ts_us.is_none() {
                                    base_ts_us = Some(frame.ts_us);
                                    wall_start = Some(Instant::now());
                                }
                            }
                        }

                        if let Err(e) = renderer.render() {
                            eprint!("Render error: {e}");
                        }

                        //window.request_redraw();
                        if let (Some(bts), Some(ws)) = (base_ts_us, wall_start) {
                            let target = ws + Duration::from_micros(last_ts_us.saturating_sub(bts));
                            let now = Instant::now();
                            if target > now {
                                elwt.set_control_flow(ControlFlow::WaitUntil(target));
                            } else {
                                // Behind → redraw ASAP to catch up
                                elwt.set_control_flow(ControlFlow::Poll);
                                window.request_redraw();
                            }
                        } else {
                            // No baseline yet → keep polling
                            elwt.set_control_flow(ControlFlow::Poll);
                            window.request_redraw();
                        }
                    }
                    _ => {}
                },
                Event::AboutToWait => {
                    window.request_redraw();
                }
                _ => {}
            }
        })?;
        Ok(())
    }
}
