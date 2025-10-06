use std::error::Error;
use wgpu::{
    Color, CommandEncoderDescriptor, Device, DeviceDescriptor, Instance, LoadOp, Operations,
    PresentMode, Queue, RenderPassColorAttachment, RenderPassDescriptor, RequestAdapterOptions,
    StoreOp, Surface, SurfaceConfiguration, TextureUsages, TextureViewDescriptor,
};
use winit::window::Window;

pub struct Renderer<'r> {
    surface: Surface<'r>,
    device: Device,
    queue: Queue,
}

impl<'r> Renderer<'r> {
    pub async fn new(window: &'r Window) -> Result<Self, Box<dyn Error>> {
        let size = window.inner_size();
        let instance = Instance::default();

        let surface = instance.create_surface(window)?;

        let adapter_options = RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        };

        let adapter = instance
            .request_adapter(&adapter_options)
            .await
            .map_err(|e| format!("Failed to find an appropriate adapter: {}", e))?;

        let device_desc = DeviceDescriptor {
            label: Some("GPU Device"),
            ..Default::default()
        };

        let (device, queue) = adapter
            .request_device(&device_desc)
            .await
            .map_err(|e| format!("Failed to create device: {}", e))?;

        let surface_caps = surface.get_capabilities(&adapter);
        let surface_format = surface_caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(surface_caps.formats[0]);

        let config = SurfaceConfiguration {
            desired_maximum_frame_latency: 2,
            usage: TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width,
            height: size.height,
            present_mode: PresentMode::Immediate,
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
        };

        surface.configure(&device, &config);

        Ok(Self {
            surface,
            device,
            queue,
        })
    }

    pub fn render(&mut self) -> Result<(), Box<dyn Error>> {
        let frame = self
            .surface
            .get_current_texture()
            .expect("Failed to acquire next swap chain texture");

        let texture_desc = TextureViewDescriptor::default();
        let frame_view = frame.texture.create_view(&texture_desc);

        let command_encoder_desc = CommandEncoderDescriptor {
            label: Some("Render Encoder"),
        };

        let mut command_encoder = self.device.create_command_encoder(&command_encoder_desc);

        let rp_color_attachment = RenderPassColorAttachment {
            depth_slice: None,
            view: &frame_view,
            resolve_target: None,
            ops: Operations {
                load: LoadOp::Clear(Color {
                    r: 0.1,
                    g: 0.2,
                    b: 0.3,
                    a: 1.0,
                }),
                store: StoreOp::Store,
            },
        };

        let rp_desc = RenderPassDescriptor {
            label: Some("Render Pass"),
            color_attachments: &[Some(rp_color_attachment)],
            depth_stencil_attachment: None,
            ..Default::default()
        };

        command_encoder.begin_render_pass(&rp_desc);
        self.queue.submit(Some(command_encoder.finish()));
        frame.present();
        Ok(())
    }
}
