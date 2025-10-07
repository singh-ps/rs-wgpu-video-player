use std::error::Error;
use wgpu::{
    AddressMode, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingResource, BindingType, Color,
    CommandEncoderDescriptor, Device, DeviceDescriptor, Extent3d, FilterMode, FragmentState,
    Instance, LoadOp, MultisampleState, Operations, Origin3d, PipelineCompilationOptions,
    PipelineLayoutDescriptor, PresentMode, PrimitiveState, Queue, RenderPassColorAttachment,
    RenderPassDescriptor, RenderPipeline, RenderPipelineDescriptor, RequestAdapterOptions, Sampler,
    SamplerBindingType, SamplerDescriptor, ShaderModuleDescriptor, ShaderStages, StoreOp, Surface,
    SurfaceConfiguration, TexelCopyBufferLayout, TexelCopyTextureInfo, Texture, TextureAspect,
    TextureDescriptor, TextureDimension, TextureFormat, TextureSampleType, TextureUsages,
    TextureView, TextureViewDescriptor, TextureViewDimension, VertexState,
};
use winit::{dpi::PhysicalSize, window::Window};

struct FrameTexture {
    texture: Texture,
    view: TextureView,
    bind: BindGroup,
    width: u32,
    height: u32,
}

pub struct Renderer<'r> {
    surface: Surface<'r>,
    device: Device,
    queue: Queue,
    config: SurfaceConfiguration,
    sampler: Sampler,
    pipeline: RenderPipeline,
    bind_layout: BindGroupLayout,
    frame_tex: Option<FrameTexture>,
}

impl<'r> Renderer<'r> {
    pub async fn new(window: &'r Window) -> Result<Self, Box<dyn Error>> {
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

        let present_mode = if surface_caps.present_modes.contains(&PresentMode::Mailbox) {
            PresentMode::Mailbox
        } else {
            PresentMode::Immediate
        };

        let size = window.inner_size();
        let config = SurfaceConfiguration {
            desired_maximum_frame_latency: 2,
            usage: TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width,
            height: size.height,
            present_mode,
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
        };

        surface.configure(&device, &config);

        let sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("Texture Sampler"),
            address_mode_u: AddressMode::ClampToEdge,
            address_mode_v: AddressMode::ClampToEdge,
            address_mode_w: AddressMode::ClampToEdge,
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            mipmap_filter: FilterMode::Nearest,
            ..Default::default()
        });

        let bind_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("Texture Bind Group Layout"),
            entries: &[
                BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Texture {
                        sample_type: TextureSampleType::Float { filterable: true },
                        view_dimension: TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Sampler(SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("Render Pipeline Layout"),
            bind_group_layouts: &[&bind_layout],
            push_constant_ranges: &[],
        });

        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("Shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("Render Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: PipelineCompilationOptions::default(),
            },
            fragment: Some(FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(surface_format.into())],
                compilation_options: PipelineCompilationOptions::default(),
            }),
            primitive: PrimitiveState::default(),
            depth_stencil: None,
            multisample: MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        Ok(Self {
            surface,
            device,
            queue,
            config,
            sampler,
            pipeline,
            bind_layout,
            frame_tex: None,
        })
    }

    pub fn resize(&mut self, new_size: PhysicalSize<u32>) {
        if new_size.width > 0 && new_size.height > 0 {
            self.config.width = new_size.width;
            self.config.height = new_size.height;
            self.surface.configure(&self.device, &self.config);
        }
    }

    pub fn set_frame_data(&mut self, width: u32, height: u32, data: &[u8]) {
        let needs_new_tex = match &self.frame_tex {
            Some(tex) => tex.width != width || tex.height != height,
            None => true,
        };

        if needs_new_tex {
            let tex = self.device.create_texture(&TextureDescriptor {
                label: Some("Frame Texture"),
                size: Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: TextureDimension::D2,
                format: TextureFormat::Rgba8UnormSrgb,
                usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
                view_formats: &[],
            });

            let view = tex.create_view(&TextureViewDescriptor::default());
            let bind = self.device.create_bind_group(&BindGroupDescriptor {
                label: Some("Frame Texture Bind Group"),
                layout: &self.bind_layout,
                entries: &[
                    BindGroupEntry {
                        binding: 0,
                        resource: BindingResource::TextureView(&view),
                    },
                    BindGroupEntry {
                        binding: 1,
                        resource: BindingResource::Sampler(&self.sampler),
                    },
                ],
            });

            self.frame_tex = Some(FrameTexture {
                texture: tex,
                view,
                bind,
                width,
                height,
            });

            if let Some(frame_tex) = &self.frame_tex {
                self.queue.write_texture(
                    TexelCopyTextureInfo {
                        texture: &frame_tex.texture,
                        mip_level: 0,
                        origin: Origin3d::ZERO,
                        aspect: TextureAspect::All,
                    },
                    data,
                    TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(4 * width),
                        rows_per_image: Some(height),
                    },
                    Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                );
            }
        }
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
        {
            let rp_color_attachment = RenderPassColorAttachment {
                depth_slice: None,
                view: &frame_view,
                resolve_target: None,
                ops: Operations {
                    load: LoadOp::Clear(Color {
                        r: 0.1,
                        g: 0.1,
                        b: 0.1,
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

            let mut render_pass = command_encoder.begin_render_pass(&rp_desc);

            if let Some(v) = &self.frame_tex {
                render_pass.set_pipeline(&self.pipeline);
                render_pass.set_bind_group(0, &v.bind, &[]);
                render_pass.draw(0..3, 0..1);
            }
        }

        self.queue.submit(Some(command_encoder.finish()));
        frame.present();

        Ok(())
    }
}
