use std::error::Error;
use winit::{
    dpi::LogicalSize,
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

        let _window = WindowBuilder::new()
            .with_title("WGPU Video Player")
            .with_inner_size(LogicalSize::new(1270, 720))
            .with_resizable(false)
            .build(&event_loop)?;

        event_loop.run(|event, elwt| match event {
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => elwt.exit(),
                WindowEvent::KeyboardInput {
                    event:
                        KeyEvent {
                            logical_key: Key::Named(NamedKey::Escape),
                            ..
                        },
                    ..
                } => elwt.exit(),
                _ => {}
            },
            _ => {}
        })?;
        Ok(())
    }
}
