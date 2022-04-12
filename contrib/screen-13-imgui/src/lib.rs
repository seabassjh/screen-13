pub mod prelude_arc {
    pub use super::*;

    use screen_13::ptr::ArcK;

    pub type ImGui = super::ImGui<ArcK>;
}

pub mod prelude_rc {
    pub use super::*;

    use screen_13::ptr::RcK;

    pub type ImGui = super::ImGui<RcK>;
}

pub use imgui::{self, Condition, Ui};

use {
    imgui::{Context, DrawCmd, DrawCmdParams},
    imgui_winit_support::{HiDpiMode, WinitPlatform},
    screen_13::prelude_all::*,
    std::time::Duration,
};

#[derive(Debug)]
pub struct ImGui<P>
where
    P: SharedPointerKind,
{
    context: Context,
    font_atlas_image: Option<ImageLeaseBinding<P>>,
    pipeline: Shared<GraphicPipeline<P>, P>,
    platform: WinitPlatform,
    pool: HashPool<P>,
}

impl<P> ImGui<P>
where
    P: SharedPointerKind + Send + 'static,
{
    pub fn new(device: &Shared<Device<P>, P>) -> Self {
        let mut context = Context::create();
        let platform = WinitPlatform::init(&mut context);
        let pool = HashPool::new(device);
        let pipeline = Shared::new(
            GraphicPipeline::create(
                device,
                GraphicPipelineInfo::new()
                    .blend(BlendMode::Alpha)
                    .cull_mode(vk::CullModeFlags::NONE),
                [
                    Shader::new_vertex(*include_bytes!(concat!(
                        env!("OUT_DIR"),
                        "/imgui.vert.spirv"
                    ))),
                    Shader::new_fragment(*include_bytes!(concat!(
                        env!("OUT_DIR"),
                        "/imgui.frag.spirv"
                    ))),
                ],
            )
            .unwrap(),
        );

        Self {
            context,
            font_atlas_image: None,
            pipeline,
            platform,
            pool,
        }
    }

    pub fn draw(
        &mut self,
        dt: f32,
        events: &[Event<'_, ()>],
        window: &Window,
        render_graph: &mut RenderGraph<P>,
        resolution: UVec2,
        ui_func: impl FnOnce(&mut Ui),
    ) -> ImageLeaseNode<P> {
        let hidpi = self.platform.hidpi_factor();

        self.platform
            .attach_window(self.context.io_mut(), window, HiDpiMode::Default);

        if self.font_atlas_image.is_none() || self.platform.hidpi_factor() != hidpi {
            self.lease_font_atlas_image(render_graph);
        }

        let io = self.context.io_mut();
        io.update_delta_time(Duration::from_secs_f32(dt));

        for event in events {
            self.platform.handle_event(io, window, event);
        }

        self.platform
            .prepare_frame(io, window)
            .expect("Unable to prepare ImGui frame");

        // Let the caller draw the GUI
        let mut ui = self.context.frame();

        ui_func(&mut ui);

        self.platform.prepare_render(&ui, window);
        let draw_data = ui.render();

        let image = render_graph.bind_node(
            self.pool
                .lease(
                    ImageInfo::new_2d(vk::Format::R8G8B8A8_SRGB, resolution.x, resolution.y).usage(
                        vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
                    ),
                )
                .unwrap(),
        );
        let font_atlas_image = render_graph.bind_node(self.font_atlas_image.take().unwrap());
        let display_pos = draw_data.display_pos;
        let framebuffer_scale = draw_data.framebuffer_scale;

        for draw_list in draw_data.draw_lists() {
            let indices_u16 = draw_list.idx_buffer();
            let indices = into_u8_slice(indices_u16);
            let mut idx_buf = self
                .pool
                .lease(BufferInfo {
                    size: indices.len() as _,
                    usage: vk::BufferUsageFlags::INDEX_BUFFER,
                    can_map: true,
                })
                .unwrap();

            {
                Buffer::mapped_slice_mut(idx_buf.get_mut().unwrap())[0..indices.len()]
                    .copy_from_slice(indices);
            }

            let idx_buf = render_graph.bind_node(idx_buf);

            let vertices_slice = draw_list.vtx_buffer();
            let vertices = into_u8_slice(vertices_slice);

            let mut vertex_buf = self
                .pool
                .lease(BufferInfo {
                    size: vertices.len() as _,
                    usage: vk::BufferUsageFlags::VERTEX_BUFFER,
                    can_map: true,
                })
                .unwrap();

            {
                Buffer::mapped_slice_mut(vertex_buf.get_mut().unwrap())[0..vertices.len()]
                    .copy_from_slice(vertices);
            }

            let vertex_buf = render_graph.bind_node(vertex_buf);

            let draw_cmds = draw_list
                .commands()
                .map(|draw_cmd| match draw_cmd {
                    DrawCmd::Elements {
                        count,
                        cmd_params:
                            DrawCmdParams {
                                clip_rect,
                                idx_offset,
                                vtx_offset,
                                ..
                            },
                    } => (count, clip_rect, idx_offset, vtx_offset),
                    _ => unimplemented!(),
                })
                .collect::<Vec<_>>();

            render_graph
                .record_pass("imgui")
                .access_node(idx_buf, AccessType::IndexBuffer)
                .access_node(vertex_buf, AccessType::VertexBuffer)
                .bind_pipeline(&self.pipeline)
                .read_descriptor(0, font_atlas_image)
                .clear_color(0)
                .store_color(0, image)
                .push_constants([
                    self.platform.hidpi_factor() as f32 / resolution.x as f32,
                    self.platform.hidpi_factor() as f32 / resolution.y as f32,
                    f32::NAN, // Required padding
                    f32::NAN, // Required padding
                ])
                .draw(move |device, cmd_buf, bindings| unsafe {
                    use std::slice::from_ref;

                    device.cmd_bind_index_buffer(
                        cmd_buf,
                        *bindings[idx_buf],
                        0,
                        vk::IndexType::UINT16,
                    );
                    device.cmd_bind_vertex_buffers(
                        cmd_buf,
                        0,
                        from_ref(&bindings[vertex_buf]),
                        from_ref(&0),
                    );

                    for (count, clip_rect, idx_offset, vtx_offset) in draw_cmds {
                        let clip_rect = [
                            (clip_rect[0] - display_pos[0]) * framebuffer_scale[0],
                            (clip_rect[1] - display_pos[1]) * framebuffer_scale[1],
                            (clip_rect[2] - display_pos[0]) * framebuffer_scale[0],
                            (clip_rect[3] - display_pos[1]) * framebuffer_scale[1],
                        ];
                        let scissor = vk::Rect2D {
                            offset: vk::Offset2D {
                                x: clip_rect[0].floor() as i32,
                                y: clip_rect[1].floor() as i32,
                            },
                            extent: vk::Extent2D {
                                width: (clip_rect[2] - clip_rect[0]).ceil() as u32,
                                height: (clip_rect[3] - clip_rect[1]).ceil() as u32,
                            },
                        };
                        let count = count as u32;
                        device.cmd_set_scissor(cmd_buf, 0, from_ref(&scissor));
                        device.cmd_draw_indexed(
                            cmd_buf,
                            count,
                            1,
                            idx_offset as _,
                            vtx_offset as _,
                            0,
                        );
                    }
                });
        }

        self.font_atlas_image = Some(render_graph.unbind_node(font_atlas_image));

        image
    }

    pub fn draw_frame(
        &mut self,
        frame: &mut FrameContext<'_, P>,
        ui_func: impl FnOnce(&mut Ui),
    ) -> ImageLeaseNode<P> {
        self.draw(
            frame.dt,
            frame.events,
            frame.window,
            frame.render_graph,
            frame.resolution,
            ui_func,
        )
    }

    fn lease_font_atlas_image(&mut self, render_graph: &mut RenderGraph<P>) {
        use imgui::{FontConfig, FontGlyphRanges, FontSource};

        let hidpi_factor = self.platform.hidpi_factor();
        self.context.io_mut().font_global_scale = (1.0 / hidpi_factor) as f32;

        let font_size = (14.0 * hidpi_factor) as f32;
        let mut fonts = self.context.fonts();
        fonts.clear_fonts();
        fonts.add_font(&[
            FontSource::TtfData {
                data: include_bytes!("../res/font/roboto/roboto-regular.ttf"),
                size_pixels: font_size,
                config: Some(FontConfig {
                    rasterizer_multiply: 2.0,
                    glyph_ranges: FontGlyphRanges::japanese(),
                    ..FontConfig::default()
                }),
            },
            FontSource::TtfData {
                data: include_bytes!("../res/font/mplus-1p/mplus-1p-regular.ttf"),
                size_pixels: font_size,
                config: Some(FontConfig {
                    oversample_h: 2,
                    oversample_v: 2,
                    // Range of glyphs to rasterize
                    glyph_ranges: FontGlyphRanges::japanese(),
                    ..FontConfig::default()
                }),
            },
        ]);

        let texture = fonts.build_rgba32_texture(); // TODO: Fix fb channel writes and use alpha8!
        let temp_buf_len = texture.data.len();
        let mut temp_buf = self
            .pool
            .lease(BufferInfo {
                size: temp_buf_len as _,
                usage: vk::BufferUsageFlags::TRANSFER_SRC,
                can_map: true,
            })
            .unwrap();

        {
            let temp_buf = temp_buf.get_mut().unwrap();
            let temp_buf = Buffer::mapped_slice_mut(temp_buf);
            temp_buf[0..temp_buf_len].copy_from_slice(texture.data);
        }

        let temp_buf = render_graph.bind_node(temp_buf);
        let image = render_graph.bind_node(
            self.pool
                .lease(
                    ImageInfo::new_2d(vk::Format::R8G8B8A8_UNORM, texture.width, texture.height)
                        .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST),
                )
                .unwrap(),
        );

        render_graph.copy_buffer_to_image(temp_buf, image);

        self.font_atlas_image = Some(render_graph.unbind_node(image));
    }
}