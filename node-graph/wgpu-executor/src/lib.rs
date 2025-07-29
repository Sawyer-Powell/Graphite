mod context;
pub mod texture_upload;

use anyhow::Result;
pub use context::Context;
use dyn_any::StaticType;
use futures::lock::Mutex;
use glam::{IVec2, UVec2};
use graphene_application_io::{ApplicationIo, EditorApi, SurfaceHandle, SurfaceId};
use graphene_core::num_traits::{clamp_max, clamp_min};
use graphene_core::transform::{Footprint, Transform};
use graphene_core::{Color, Ctx};
pub use graphene_svg_renderer::RenderContext;
use std::sync::{Arc, MutexGuard};
use vello::low_level::Render;
use vello::{AaConfig, AaSupport, RenderParams, Renderer, RendererOptions, Scene};
use wgpu::util::{DeviceExt, TextureBlitter};
use wgpu::wgt::TextureViewDescriptor;
use wgpu::{Origin3d, PipelineCompilationOptions, SurfaceConfiguration, TextureAspect};

#[derive(dyn_any::DynAny)]
pub struct WgpuExecutor {
	pub context: Context,
	vello_renderer: Mutex<Renderer>,
}

impl std::fmt::Debug for WgpuExecutor {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("WgpuExecutor").field("context", &self.context).finish()
	}
}

impl<'a, T: ApplicationIo<Executor = WgpuExecutor>> From<&'a EditorApi<T>> for &'a WgpuExecutor {
	fn from(editor_api: &'a EditorApi<T>) -> Self {
		editor_api.application_io.as_ref().unwrap().gpu_executor().unwrap()
	}
}

pub type WgpuSurface = Arc<SurfaceHandle<Surface>>;
pub type WgpuWindow = Arc<SurfaceHandle<WindowHandle>>;

pub struct Surface {
	pub inner: wgpu::Surface<'static>,
	pub target_texture: Mutex<Option<TargetTexture>>,
	pub blitter: TextureBlitter,
}

pub struct TargetTexture {
	view: wgpu::TextureView,
	size: UVec2,
}

#[cfg(target_family = "wasm")]
pub type Window = web_sys::HtmlCanvasElement;
#[cfg(not(target_family = "wasm"))]
pub type Window = Arc<winit::window::Window>;

unsafe impl StaticType for Surface {
	type Static = Surface;
}

const VELLO_SURFACE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

impl WgpuExecutor {
	pub async fn render_vello_scene_view_mode_pixels(
		&self,
		artboard_dimensions: IVec2,
		scene: &Scene,
		surface: &WgpuSurface,
		footprint: Footprint,
		context: &RenderContext,
		background: Color,
	) -> Result<()> {
		// =========================== RENDER TO ARTBOARD ===========================

		let artboard_texture = self.context.device.create_texture(&wgpu::TextureDescriptor {
			label: Some("artboard texture"),
			size: wgpu::Extent3d {
				width: artboard_dimensions.x as u32,
				height: artboard_dimensions.y as u32,
				depth_or_array_layers: 1,
			},
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D2,
			usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
			format: VELLO_SURFACE_FORMAT,
			view_formats: &[],
		});

		let artboard_texture_view = artboard_texture.create_view(&wgpu::TextureViewDescriptor::default());

		let [r, g, b, _] = background.to_rgba8_srgb();
		let artboard_render_params = RenderParams {
			base_color: vello::peniko::Color::from_rgba8(r, g, b, 0xff),
			width: artboard_dimensions.x as u32,
			height: artboard_dimensions.y as u32,
			antialiasing_method: AaConfig::Msaa16,
		};

		{
			let mut renderer = self.vello_renderer.lock().await;
			for (image, texture) in context.resource_overrides.iter() {
				let texture_view = wgpu::TexelCopyTextureInfoBase {
					texture: texture.clone(),
					mip_level: 0,
					origin: Origin3d::ZERO,
					aspect: TextureAspect::All,
				};
				renderer.override_image(image, Some(texture_view));
			}
			renderer.render_to_texture(&self.context.device, &self.context.queue, scene, &artboard_texture_view, &artboard_render_params)?;
			for (image, _) in context.resource_overrides.iter() {
				renderer.override_image(image, None);
			}
		}

		// ========================== /RENDER TO ARTBOARD ===========================

		let surface_inner = &surface.surface.inner;
		let surface_caps = surface_inner.get_capabilities(&self.context.adapter);
		surface_inner.configure(
			&self.context.device,
			&SurfaceConfiguration {
				usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_DST,
				format: VELLO_SURFACE_FORMAT,
				width: footprint.resolution.x,
				height: footprint.resolution.y,
				present_mode: surface_caps.present_modes[0],
				alpha_mode: wgpu::CompositeAlphaMode::Opaque,
				view_formats: vec![],
				desired_maximum_frame_latency: 2,
			},
		);

		// =========================== ARTBOARD TO CANVAS PIPELINE ===========================

		#[repr(C)]
		#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
		struct Vertex {
			positions: [f32; 2],
		}

		#[repr(C)]
		#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
		struct Transform {
			transform: [[f32; 3]; 2],
		}

		impl From<glam::DAffine2> for Transform {
			fn from(affine: glam::DAffine2) -> Self {
				let mat = affine.to_cols_array();
				Self {
					transform: [[mat[0] as f32, mat[1] as f32, mat[2] as f32], [mat[3] as f32, mat[4] as f32, mat[5] as f32]],
				}
			}
		}

		let vs_module = self.context.device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("Transform Vertex"),
			source: wgpu::ShaderSource::Wgsl(
				r#"
				@group(0) @binding(0) var<uniform> affine_transform: mat2x3<f32>;

				struct VertexOutput {
					@builtin(position) clip_position: vec4<f32>,
					@location(0) uv: vec2<f32>,
				}

				@vertex
				fn vs_main(@location(0) position: vec2<f32>) -> VertexOutput {
					var out: VertexOutput;
					let transformed = affine_transform * vec3<f32>(position, 1.0);
					out.clip_position = vec4<f32>(transformed, 0.0, 1.0);
					out.uv = (position + 1.0) * 0.5;
					return out
				}
				"#
				.into(),
			),
		});

		let fs_module = self.context.device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label: Some("Downscale Fragment"),
			source: wgpu::ShaderSource::Wgsl(
				r#"
				@group(0) @binding(0) var artboard_texture: texture_2d<f32>;
				@group(0) @binding(1) var linear_sampler: sampler;

				@fragment
				fn fs_main(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
					return textureSample(artboard_texture, linear_sampler, uv);
				}
			"#
				.into(),
			),
		});

		let bind_group_layout = self.context.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			entries: &[
				wgpu::BindGroupLayoutEntry {
					// affine_transformation
					binding: 0,
					visibility: wgpu::ShaderStages::VERTEX,
					ty: wgpu::BindingType::Buffer {
						ty: wgpu::BufferBindingType::Uniform,
						has_dynamic_offset: false,
						min_binding_size: None,
					},
					count: None,
				},
				wgpu::BindGroupLayoutEntry {
					// artboard_texture
					binding: 1,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Texture {
						multisampled: false,
						view_dimension: wgpu::TextureViewDimension::D2,
						sample_type: wgpu::TextureSampleType::Float { filterable: true },
					},
					count: None,
				},
				wgpu::BindGroupLayoutEntry {
					// linear_sampler
					binding: 2,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
					count: None,
				},
			],
			label: None,
		});

		let pipeline_layout = self.context.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label: None,
			bind_group_layouts: &[&bind_group_layout],
			push_constant_ranges: &[],
		});

		let vertex_buffer_layout = wgpu::VertexBufferLayout {
			array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
			step_mode: wgpu::VertexStepMode::Vertex,
			attributes: &[wgpu::VertexAttribute {
				offset: 0,
				shader_location: 0,
				format: wgpu::VertexFormat::Float32x2,
			}],
		};

		let pipeline = self.context.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
			label: Some("Artboard to Canvas Pipeline"),
			layout: Some(&pipeline_layout),
			vertex: wgpu::VertexState {
				module: &vs_module,
				entry_point: "vs_main".into(),
				buffers: &[vertex_buffer_layout],
				compilation_options: wgpu::PipelineCompilationOptions::default(),
			},
			fragment: Some(wgpu::FragmentState {
				module: &fs_module,
				entry_point: "fs_main".into(),
				targets: &[Some(wgpu::ColorTargetState {
					format: surface_caps.formats[0],
					blend: None,
					write_mask: wgpu::ColorWrites::ALL,
				})],
				compilation_options: wgpu::PipelineCompilationOptions::default(),
			}),
			primitive: wgpu::PrimitiveState::default(),
			depth_stencil: None,
			multisample: wgpu::MultisampleState::default(),
			multiview: None,
			cache: None,
		});

		// ========================= DOWNSCALING PIPELINE ===========================

		let transform = Transform::from(footprint.transform);

		let transform_buffer = self.context.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
			label: Some("Affine transform buffer"),
			contents: bytemuck::cast_slice(&[transform]),
			usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
		});

		let linear_sampler = self.context.device.create_sampler(&wgpu::SamplerDescriptor {
			address_mode_u: wgpu::AddressMode::ClampToEdge,
			address_mode_v: wgpu::AddressMode::ClampToEdge,
			address_mode_w: wgpu::AddressMode::ClampToEdge,
			mag_filter: wgpu::FilterMode::Linear,
			min_filter: wgpu::FilterMode::Linear,
			mipmap_filter: wgpu::FilterMode::Nearest,
			..Default::default()
		});

		let vertices = [
			Vertex { positions: [-1.0, -1.0] }, // Bottom-left
			Vertex { positions: [1.0, -1.0] },  // Bottom-right
			Vertex { positions: [-1.0, 1.0] },  // Top-left
			Vertex { positions: [1.0, 1.0] },   // Top-right
		];

		let vertex_buffer = self.context.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
			label: Some("Vertex Buffer"),
			contents: bytemuck::cast_slice(&vertices),
			usage: wgpu::BufferUsages::VERTEX,
		});

		let bind_group = self.context.device.create_bind_group(&wgpu::BindGroupDescriptor {
			layout: &bind_group_layout,
			entries: &[
				wgpu::BindGroupEntry {
					binding: 0,
					resource: transform_buffer.as_entire_binding(), // Your transform uniform buffer
				},
				wgpu::BindGroupEntry {
					binding: 1,
					resource: wgpu::BindingResource::TextureView(&artboard_texture_view),
				},
				wgpu::BindGroupEntry {
					binding: 2,
					resource: wgpu::BindingResource::Sampler(&linear_sampler),
				},
			],
			label: Some("Bind Group"),
		});

		let translation = footprint.transform.translation;
		let scale = footprint.transform.decompose_scale();

		// Nothing to render
		if (translation.x as u32) >= footprint.resolution.x || (translation.y as u32) >= footprint.resolution.y {
			return Ok(());
		}

		let surface_texture = surface_inner.get_current_texture()?;
		let mut encoder = self.context.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("Surface Blit") });

		{
			let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
				color_attachments: &[Some(wgpu::RenderPassColorAttachment {
					view: &surface_texture.texture.create_view(&TextureViewDescriptor::default()),
					resolve_target: None,
					ops: wgpu::Operations {
						load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
						store: wgpu::StoreOp::Store,
					},
				})],
				occlusion_query_set: None,
				timestamp_writes: None,
				depth_stencil_attachment: None,
				label: None,
			});

			render_pass.set_pipeline(&pipeline);
			render_pass.set_bind_group(0, &bind_group, &[]);
			render_pass.set_vertex_buffer(0, vertex_buffer.slice(..));
			render_pass.draw(0..4, 0..1);
		}

		self.context.queue.submit([encoder.finish()]);
		surface_texture.present();

		Ok(())
	}

	pub async fn render_vello_scene(&self, scene: &Scene, surface: &WgpuSurface, size: UVec2, context: &RenderContext, background: Color) -> Result<()> {
		let mut guard = surface.surface.target_texture.lock().await;
		let target_texture = if let Some(target_texture) = &*guard
			&& target_texture.size == size
		{
			target_texture
		} else {
			let texture = self.context.device.create_texture(&wgpu::TextureDescriptor {
				label: None,
				size: wgpu::Extent3d {
					width: size.x,
					height: size.y,
					depth_or_array_layers: 1,
				},
				mip_level_count: 1,
				sample_count: 1,
				dimension: wgpu::TextureDimension::D2,
				usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
				format: VELLO_SURFACE_FORMAT,
				view_formats: &[],
			});
			let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
			*guard = Some(TargetTexture { size, view });
			guard.as_ref().unwrap()
		};

		let surface_inner = &surface.surface.inner;
		let surface_caps = surface_inner.get_capabilities(&self.context.adapter);
		surface_inner.configure(
			&self.context.device,
			&SurfaceConfiguration {
				usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::STORAGE_BINDING,
				format: VELLO_SURFACE_FORMAT,
				width: size.x,
				height: size.y,
				present_mode: surface_caps.present_modes[0],
				alpha_mode: wgpu::CompositeAlphaMode::Opaque,
				view_formats: vec![],
				desired_maximum_frame_latency: 2,
			},
		);

		let [r, g, b, _] = background.to_rgba8_srgb();
		let render_params = RenderParams {
			// We are using an explicit opaque color here to eliminate the alpha premultiplication step
			// which would be required to support a transparent webgpu canvas
			base_color: vello::peniko::Color::from_rgba8(r, g, b, 0xff),
			width: size.x,
			height: size.y,
			antialiasing_method: AaConfig::Msaa16,
		};

		{
			let mut renderer = self.vello_renderer.lock().await;
			for (image, texture) in context.resource_overrides.iter() {
				let texture_view = wgpu::TexelCopyTextureInfoBase {
					texture: texture.clone(),
					mip_level: 0,
					origin: Origin3d::ZERO,
					aspect: TextureAspect::All,
				};
				renderer.override_image(image, Some(texture_view));
			}
			renderer.render_to_texture(&self.context.device, &self.context.queue, scene, &target_texture.view, &render_params)?;
			for (image, _) in context.resource_overrides.iter() {
				renderer.override_image(image, None);
			}
		}

		let surface_texture = surface_inner.get_current_texture()?;
		let mut encoder = self.context.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("Surface Blit") });
		surface.surface.blitter.copy(
			&self.context.device,
			&mut encoder,
			&target_texture.view,
			&surface_texture.texture.create_view(&wgpu::TextureViewDescriptor::default()),
		);
		self.context.queue.submit([encoder.finish()]);
		surface_texture.present();

		Ok(())
	}

	pub async fn render_vello_scene_to_texture(&self, scene: &Scene, size: UVec2, context: &RenderContext, background: Color) -> Result<wgpu::Texture> {
		let texture = self.context.device.create_texture(&wgpu::TextureDescriptor {
			label: None,
			size: wgpu::Extent3d {
				width: size.x.max(1),
				height: size.y.max(1),
				depth_or_array_layers: 1,
			},
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu::TextureDimension::D2,
			usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
			format: VELLO_SURFACE_FORMAT,
			view_formats: &[],
		});
		let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

		let [r, g, b, a] = background.to_rgba8_srgb();
		let render_params = RenderParams {
			base_color: vello::peniko::Color::from_rgba8(r, g, b, a),
			width: size.x,
			height: size.y,
			antialiasing_method: AaConfig::Msaa16,
		};

		{
			let mut renderer = self.vello_renderer.lock().await;
			for (image, texture) in context.resource_overrides.iter() {
				let texture_view = wgpu::TexelCopyTextureInfoBase {
					texture: texture.clone(),
					mip_level: 0,
					origin: Origin3d::ZERO,
					aspect: TextureAspect::All,
				};
				renderer.override_image(image, Some(texture_view));
			}
			renderer.render_to_texture(&self.context.device, &self.context.queue, scene, &view, &render_params)?;
			for (image, _) in context.resource_overrides.iter() {
				renderer.override_image(image, None);
			}
		}

		Ok(texture)
	}

	#[cfg(target_family = "wasm")]
	pub fn create_surface(&self, canvas: graphene_application_io::WasmSurfaceHandle) -> Result<SurfaceHandle<Surface>> {
		let surface = self.context.instance.create_surface(wgpu::SurfaceTarget::Canvas(canvas.surface))?;
		self.create_surface_inner(surface, canvas.window_id)
	}
	#[cfg(not(target_family = "wasm"))]
	pub fn create_surface(&self, window: SurfaceHandle<Window>) -> Result<SurfaceHandle<Surface>> {
		let surface = self.context.instance.create_surface(wgpu::SurfaceTarget::Window(Box::new(window.surface)))?;
		self.create_surface_inner(surface, window.window_id)
	}

	pub fn create_surface_inner(&self, surface: wgpu::Surface<'static>, window_id: SurfaceId) -> Result<SurfaceHandle<Surface>> {
		let blitter = TextureBlitter::new(&self.context.device, VELLO_SURFACE_FORMAT);
		Ok(SurfaceHandle {
			window_id,
			surface: Surface {
				inner: surface,
				target_texture: Mutex::new(None),
				blitter,
			},
		})
	}
}

impl WgpuExecutor {
	pub async fn new() -> Option<Self> {
		let context = Context::new().await?;

		let vello_renderer = Renderer::new(
			&context.device,
			RendererOptions {
				// surface_format: Some(wgpu::TextureFormat::Rgba8Unorm),
				pipeline_cache: None,
				use_cpu: false,
				antialiasing_support: AaSupport::all(),
				num_init_threads: std::num::NonZeroUsize::new(1),
			},
		)
		.map_err(|e| anyhow::anyhow!("Failed to create Vello renderer: {:?}", e))
		.ok()?;

		Some(Self {
			context,
			vello_renderer: vello_renderer.into(),
		})
	}
	pub fn with_context(context: Context) -> Option<Self> {
		let vello_renderer = Renderer::new(
			&context.device,
			RendererOptions {
				pipeline_cache: None,
				use_cpu: false,
				antialiasing_support: AaSupport::all(),
				num_init_threads: std::num::NonZeroUsize::new(1),
			},
		)
		.map_err(|e| anyhow::anyhow!("Failed to create Vello renderer: {:?}", e))
		.ok()?;

		Some(Self {
			context,
			vello_renderer: vello_renderer.into(),
		})
	}
}

pub type WindowHandle = Arc<SurfaceHandle<Window>>;

#[node_macro::node(skip_impl)]
fn create_gpu_surface<'a: 'n, Io: ApplicationIo<Executor = WgpuExecutor, Surface = Window> + 'a + Send + Sync>(_: impl Ctx + 'a, editor_api: &'a EditorApi<Io>) -> Option<WgpuSurface> {
	let canvas = editor_api.application_io.as_ref()?.window()?;
	let executor = editor_api.application_io.as_ref()?.gpu_executor()?;
	Some(Arc::new(executor.create_surface(canvas).ok()?))
}
