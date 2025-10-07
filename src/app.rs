use crate::{renderer::Renderer, video_player::VideoPlayer};
use std::error::Error;
use winit::{
    dpi::{LogicalSize, PhysicalSize},
    event::{Event, KeyEvent, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
    keyboard::{Key, NamedKey},
    window::WindowBuilder,
};

pub struct App {}

impl App {
    pub async fn run(&self) -> Result<(), Box<dyn Error>> {
        let event_loop = EventLoop::new()?;
        event_loop.set_control_flow(ControlFlow::Poll);

        let mut video_player = VideoPlayer::new();
        video_player
            .start_playback(
                "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8",
                Default::default(),
            )
            .await?;

        let window = WindowBuilder::new()
            .with_title("WGPU Video Player")
            .with_inner_size(LogicalSize::new(1270, 720))
            .with_resizable(false)
            .build(&event_loop)?;

        let mut renderer = Renderer::new(&window).await?;

        let mut last_ts: u64 = 0;

        event_loop.run(|event, elwt| match event {
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => {
                    video_player.stop_playback();
                    elwt.exit();
                }
                WindowEvent::Resized(size) => renderer.resize(PhysicalSize {
                    width: size.width,
                    height: size.height,
                }),
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
                        if frame.ts_us != last_ts {
                            last_ts = frame.ts_us;
                            renderer.set_frame_data(frame.width, frame.height, &frame.data);
                        }
                    }
                    let _ = renderer.render();
                    window.request_redraw();
                }
                _ => {}
            },
            Event::AboutToWait => {
                window.request_redraw();
            }
            _ => {}
        })?;
        Ok(())
    }
}
