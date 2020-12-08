mod command;
mod compiler;

/// This module houses all the dynamically created meshes used by the drawing code to fulfill user commands.
mod geom;

mod geom_buf;
mod instruction;
mod key;

pub use self::{command::Command, compiler::Compiler};

use {
    self::{
        geom::LINE_STRIDE,
        geom_buf::GeometryBuffer,
        instruction::{Instruction, MeshBind},
    },
    super::Op,
    crate::{
        camera::Camera,
        color::{AlphaColor, Color, TRANSPARENT_BLACK},
        gpu::{
            data::CopyRange,
            driver::{
                bind_graphics_descriptor_set, CommandPool, Device, Driver, Fence, Framebuffer2d,
                PhysicalDevice,
            },
            model::MeshIter,
            pool::{DrawRenderPassMode, Graphics, GraphicsMode, Lease, Pool, RenderPassMode},
            BitmapRef, Data, MeshFilter, ModelRef, Pose, Texture2d, TextureRef,
        },
        math::{Cone, Coord, CoordF, Extent, Mat4, Sphere, Vec3},
    },
    gfx_hal::{
        buffer::{Access as BufferAccess, IndexBufferView, SubRange},
        command::{CommandBuffer as _, CommandBufferFlags, ImageCopy, Level, SubpassContents},
        device::Device as _,
        format::Aspects,
        image::{
            Access as ImageAccess, Layout, Offset, SubresourceLayers, SubresourceRange, ViewKind,
        },
        pool::CommandPool as _,
        pso::{Descriptor, DescriptorSetWrite, PipelineStage, ShaderStageFlags, Viewport},
        queue::{CommandQueue as _, Submission},
        Backend,
    },
    gfx_impl::Backend as _Backend,
    std::{
        cmp::Ordering,
        hash::{Hash, Hasher},
        iter::{empty, once},
        ops::Range,
    },
};

// TODO: Remove!
const _0: BufferAccess = BufferAccess::MEMORY_WRITE;
const _1: Extent = Extent::ZERO;
const _2: SubRange = SubRange::WHOLE;

pub struct DrawOp<'a> {
    cmd_buf: <_Backend as Backend>::CommandBuffer,
    cmd_pool: Lease<CommandPool>,
    compiler: Lease<Compiler>,
    driver: Driver,
    dst: Texture2d,
    dst_preserve: bool,
    fence: Lease<Fence>,
    frame_buf: Framebuffer2d,
    geom_buf: GeometryBuffer,
    graphics_line: Option<Lease<Graphics>>,
    graphics_mesh: Option<Lease<Graphics>>,
    graphics_mesh_anim: Option<Lease<Graphics>>,
    graphics_spotlight: Option<Lease<Graphics>>,
    graphics_sunlight: Option<Lease<Graphics>>,
    mode: DrawRenderPassMode,

    #[cfg(debug_assertions)]
    name: String,

    pool: &'a mut Pool,
}

impl<'a> DrawOp<'a> {
    /// # Safety
    /// None
    pub fn new(
        #[cfg(debug_assertions)] name: &str,
        driver: Driver,
        pool: &'a mut Pool,
        dst: &Texture2d,
    ) -> Self {
        // Allocate the command buffer
        let family = Device::queue_family(&driver.borrow());
        let mut cmd_pool = pool.cmd_pool(&driver, family);

        // The g-buffer will share size and format with the destination texture
        let (dims, fmt) = {
            let dst = dst.borrow();
            (dst.dims(), dst.format())
        };
        let geom_buf = GeometryBuffer::new(
            #[cfg(debug_assertions)]
            name,
            &driver,
            pool,
            dims,
            fmt,
        );

        let (frame_buf, mode) = {
            let albedo = geom_buf.albedo.borrow();
            let depth = geom_buf.depth.borrow();
            let light = geom_buf.light.borrow();
            let material = geom_buf.material.borrow();
            let normal = geom_buf.normal.borrow();
            let output = geom_buf.output.borrow();

            let mode = DrawRenderPassMode {
                albedo: fmt,
                depth: depth.format(),
                light: light.format(),
                material: material.format(),
                normal: normal.format(),
            };

            // Setup the framebuffer
            let frame_buf = Framebuffer2d::new(
                #[cfg(debug_assertions)]
                &name,
                Driver::clone(&driver),
                pool.render_pass(&driver, RenderPassMode::Draw(mode)),
                vec![
                    albedo.as_default_view().as_ref(),
                    depth
                        .as_view(
                            ViewKind::D2,
                            mode.depth,
                            Default::default(),
                            SubresourceRange {
                                aspects: Aspects::DEPTH,
                                ..Default::default()
                            },
                        )
                        .as_ref(),
                    light.as_default_view().as_ref(),
                    material.as_default_view().as_ref(),
                    normal.as_default_view().as_ref(),
                    output.as_default_view().as_ref(),
                ],
                dims,
            );

            (frame_buf, mode)
        };
        let fence = pool.fence(
            #[cfg(debug_assertions)]
            name,
            &driver,
        );

        Self {
            cmd_buf: unsafe { cmd_pool.allocate_one(Level::Primary) },
            cmd_pool,
            compiler: pool.compiler(),
            driver,
            dst: TextureRef::clone(dst),
            dst_preserve: false,
            fence,
            frame_buf,
            geom_buf,
            graphics_line: None,
            graphics_mesh: None,
            graphics_mesh_anim: None,
            graphics_spotlight: None,
            graphics_sunlight: None,
            mode,

            #[cfg(debug_assertions)]
            name: name.to_owned(),

            pool,
        }
    }

    /// Preserves the contents of the destination texture. Without calling this function the existing
    /// contents of the destination texture will not be composited into the final result.
    pub fn with_preserve(&mut self) -> &mut Self {
        self.dst_preserve = true;
        self
    }

    // TODO: Returns concrete type instead of impl Op because https://github.com/rust-lang/rust/issues/42940
    pub fn record<'c>(mut self, camera: &impl Camera, cmds: &'c mut [Command]) -> DrawOpSubmission {
        let dims: Coord = self.dst.borrow().dims().into();
        let viewport = Viewport {
            rect: dims.as_rect_at(Coord::ZERO),
            depth: 0.0..1.0,
        };
        let view_projection = camera.view() * camera.projection();

        // Use a compiler to figure out rendering instructions without allocating
        // memory per rendering command. The compiler caches code between frames.
        let mut compiler = self.pool.compiler();
        let mut instrs = compiler.compile(
            #[cfg(debug_assertions)]
            &self.name,
            &self.driver,
            &mut self.pool,
            camera,
            cmds,
        );

        // Setup graphics pipelines and descriptor sets
        {
            let materials = instrs.mesh_materials();
            let descriptor_sets = materials.len();
            if descriptor_sets > 0 {
                self.graphics_mesh = Some(self.pool.graphics_sets(
                    #[cfg(debug_assertions)]
                    &self.name,
                    &self.driver,
                    GraphicsMode::DrawMesh,
                    RenderPassMode::Draw(self.mode),
                    0,
                    descriptor_sets,
                ));

                let device = self.driver.borrow();

                unsafe {
                    Self::write_mesh_material_descriptors(
                        &device,
                        self.graphics_mesh.as_ref().unwrap(),
                        materials,
                    );
                }
            }
        }

        if !instrs.is_empty() {
            unsafe {
                self.submit_begin(&viewport);

                while let Some(instr) = instrs.next() {
                    match instr {
                        Instruction::DataCopy((buf, ranges)) => {
                            self.submit_vertex_copies(buf, ranges)
                        }
                        Instruction::DataTransfer((src, dst)) => {
                            self.submit_data_transfer(src, dst)
                        }
                        Instruction::DataWrite((buf, range)) => {
                            self.submit_vertex_write(buf, range)
                        }
                        Instruction::LineDraw((buf, count)) => {
                            self.submit_lines(buf, count, &viewport, view_projection)
                        }
                        Instruction::MeshBegin => self.submit_mesh_begin(&viewport),
                        Instruction::MeshBind(bind) => self.submit_mesh_bind(bind),
                        Instruction::MeshDescriptorSet(set) => self.submit_mesh_descriptor_set(set),
                        Instruction::MeshDraw((meshes, world)) => {
                            self.submit_mesh(meshes, world, view_projection)
                        }
                        _ => panic!(),
                    }
                }

                self.submit_finish();
            }
        }

        debug!("Done drawing");

        DrawOpSubmission {
            cmd_buf: self.cmd_buf,
            cmd_pool: self.cmd_pool,
            compiler: self.compiler,
            dst: self.dst,
            fence: self.fence,
            frame_buf: self.frame_buf,
            geom_buf: self.geom_buf,
            graphics_line: self.graphics_line,
            graphics_mesh: self.graphics_mesh,
            graphics_mesh_anim: self.graphics_mesh_anim,
            graphics_spotlight: self.graphics_spotlight,
            graphics_sunlight: self.graphics_sunlight,
        }
    }

    unsafe fn submit_begin(&mut self, viewport: &Viewport) {
        let mut dst = self.dst.borrow_mut();
        let mut albedo = self.geom_buf.albedo.borrow_mut();
        let mut depth = self.geom_buf.depth.borrow_mut();
        let mut light = self.geom_buf.depth.borrow_mut();
        let mut material = self.geom_buf.material.borrow_mut();
        let mut normal = self.geom_buf.normal.borrow_mut();
        let mut output = self.geom_buf.output.borrow_mut();
        let dims = dst.dims();
        // let fmt = dst.format();

        // Begin
        self.cmd_buf
            .begin_primary(CommandBufferFlags::ONE_TIME_SUBMIT);

        // Optional Step 1: Copy dst into the albedo render target
        if self.dst_preserve {
            dst.set_layout(
                &mut self.cmd_buf,
                Layout::TransferSrcOptimal,
                PipelineStage::TRANSFER,
                ImageAccess::TRANSFER_READ,
            );
            albedo.set_layout(
                &mut self.cmd_buf,
                Layout::TransferDstOptimal,
                PipelineStage::TRANSFER,
                ImageAccess::TRANSFER_WRITE,
            );
            self.cmd_buf.copy_image(
                dst.as_ref(),
                Layout::TransferSrcOptimal,
                albedo.as_ref(),
                Layout::TransferDstOptimal,
                once(ImageCopy {
                    src_subresource: SubresourceLayers {
                        aspects: Aspects::COLOR,
                        level: 0,
                        layers: 0..1,
                    },
                    src_offset: Offset::ZERO,
                    dst_subresource: SubresourceLayers {
                        aspects: Aspects::COLOR,
                        level: 0,
                        layers: 0..1,
                    },
                    dst_offset: Offset::ZERO,
                    extent: dims.as_extent_depth(1),
                }),
            );
        }

        // Prepare the render pass for mesh rendering
        albedo.set_layout(
            &mut self.cmd_buf,
            Layout::ColorAttachmentOptimal,
            PipelineStage::COLOR_ATTACHMENT_OUTPUT,
            ImageAccess::COLOR_ATTACHMENT_WRITE,
        );
        depth.set_layout(
            &mut self.cmd_buf,
            Layout::DepthStencilAttachmentOptimal,
            PipelineStage::LATE_FRAGMENT_TESTS, // TODO: VK_PIPELINE_STAGE_LATE_FRAGMENT_TESTS_BIT or VK_PIPELINE_STAGE_EARLY_FRAGMENT_TESTS_BIT
            ImageAccess::DEPTH_STENCIL_ATTACHMENT_WRITE,
        );
        light.set_layout(
            &mut self.cmd_buf,
            Layout::ColorAttachmentOptimal,
            PipelineStage::COLOR_ATTACHMENT_OUTPUT,
            ImageAccess::COLOR_ATTACHMENT_WRITE,
        );
        material.set_layout(
            &mut self.cmd_buf,
            Layout::ColorAttachmentOptimal,
            PipelineStage::COLOR_ATTACHMENT_OUTPUT,
            ImageAccess::COLOR_ATTACHMENT_WRITE,
        );
        normal.set_layout(
            &mut self.cmd_buf,
            Layout::ColorAttachmentOptimal,
            PipelineStage::COLOR_ATTACHMENT_OUTPUT,
            ImageAccess::COLOR_ATTACHMENT_WRITE,
        );
        output.set_layout(
            &mut self.cmd_buf,
            Layout::ColorAttachmentOptimal,
            PipelineStage::COLOR_ATTACHMENT_OUTPUT,
            ImageAccess::COLOR_ATTACHMENT_WRITE,
        );
        self.cmd_buf.begin_render_pass(
            self.pool
                .render_pass(&self.driver, RenderPassMode::Draw(self.mode)),
            self.frame_buf.as_ref(),
            viewport.rect,
            vec![&TRANSPARENT_BLACK.into()],
            SubpassContents::Inline,
        );
    }

    unsafe fn submit_data_transfer(&mut self, src: &mut Data, dst: &mut Data) {
        src.transfer_range(
            &mut self.cmd_buf,
            dst,
            CopyRange {
                dst: 0,
                src: 0..src.capacity(),
            },
        );
    }

    unsafe fn submit_lines(
        &mut self,
        buf: &mut Data,
        count: u32,
        viewport: &Viewport,
        transform: Mat4,
    ) {
        let render_pass_mode = RenderPassMode::Draw(self.mode);
        let graphics = self.pool.graphics(
            #[cfg(debug_assertions)]
            &format!("{} line", &self.name),
            &self.driver,
            GraphicsMode::DrawLine,
            render_pass_mode,
            0,
        );

        self.cmd_buf.set_scissors(0, &[viewport.rect]);
        self.cmd_buf.set_viewports(0, &[viewport.clone()]);
        self.cmd_buf.bind_graphics_pipeline(graphics.pipeline());
        self.cmd_buf.push_graphics_constants(
            graphics.layout(),
            ShaderStageFlags::VERTEX,
            0,
            LineVertexConsts { transform }.as_ref(),
        );
        self.cmd_buf.bind_vertex_buffers(
            0,
            Some((
                buf.as_ref(),
                SubRange {
                    offset: 0,
                    size: Some((count * LINE_STRIDE as u32) as _),
                },
            )),
        );
        self.cmd_buf.draw(0..count, 0..1);

        self.graphics_line = Some(graphics);
    }

    unsafe fn submit_vertex_copies(&mut self, buf: &mut Data, ranges: &[CopyRange]) {
        buf.copy_ranges(
            &mut self.cmd_buf,
            PipelineStage::VERTEX_INPUT,
            BufferAccess::VERTEX_BUFFER_READ,
            ranges,
        );
    }

    unsafe fn submit_vertex_write(&mut self, buf: &mut Data, range: Range<u64>) {
        debug!("Submitting vertex write");
        buf.write_range(
            &mut self.cmd_buf,
            PipelineStage::VERTEX_INPUT,
            BufferAccess::VERTEX_BUFFER_READ,
            range,
        );
    }

    unsafe fn submit_light_begin(&mut self) {}

    //unsafe fn submit_light(&mut self, _instr: &LightInstruction) {
    //   let _ = ShaderStageFlags::VERTEX;

    // Step 3: Render sunlight
    // self.cmd_buf.next_subpass(SubpassContents::Inline);
    // if self.cmds[idx].is_sunlight() {
    //     let sunlight = self.sunlight.as_ref().unwrap();

    //     self.cmd_buf.bind_graphics_pipeline(sunlight.pipeline());
    //     bind_graphics_descriptor_set(
    //         &mut self.cmd_buf,
    //         sunlight.layout(),
    //         sunlight.desc_set(0),
    //     );
    //     self.cmd_buf.set_scissors(0, &[self.rect()]);
    //     self.cmd_buf.set_viewports(0, &[self.viewport()]);
    //     loop {
    //         let _ = self.cmds.pop_front();
    //         // self.cmd_buf.push_graphics_constants(
    //         //     self.sunlight.layout(),
    //         //     ShaderStageFlags::VERTEX,
    //         //     0,
    //         //     &mat4_to_u32_array(cmd.world * self.view_proj),
    //         // );
    //         self.cmd_buf.draw(0..6, 0..1);

    //         if !self.cmds[0].is_sunlight() {
    //             break;
    //         }
    //     }
    // }

    // // Step 4: Render spotlights
    // if self.cmds[0].is_spotlight() {
    //     let spotlight = self.spotlight.as_ref().unwrap();

    //     self.cmd_buf.bind_graphics_pipeline(spotlight.pipeline());
    //     bind_graphics_descriptor_set(
    //         &mut self.cmd_buf,
    //         spotlight.layout(),
    //         spotlight.desc_set(0),
    //     );
    //     self.cmd_buf.set_scissors(0, &[self.rect()]);
    //     self.cmd_buf.set_viewports(0, &[self.viewport()]);
    //     loop {
    //         let _ = self.cmds.pop_front();
    //         // self.cmd_buf.push_graphics_constants(
    //         //     self.sunlight.layout(),
    //         //     ShaderStageFlags::VERTEX,
    //         //     0,
    //         //     &mat4_to_u32_array(cmd.world * self.view_proj),
    //         // );
    //         self.cmd_buf.draw(0..6, 0..1);

    //         if !self.cmds[0].is_spotlight() {
    //             break;
    //         }
    //     }
    // }

    // self.cmd_buf.next_subpass(SubpassContents::Inline);
    // idx
    //}

    unsafe fn submit_mesh_begin(&mut self, viewport: &Viewport) {
        let graphics = self.graphics_mesh.as_ref().unwrap();

        self.cmd_buf.bind_graphics_pipeline(graphics.pipeline());
        self.cmd_buf.set_scissors(0, &[viewport.rect]);
        self.cmd_buf.set_viewports(0, &[viewport.clone()]);
    }

    unsafe fn submit_mesh_bind(&mut self, bind: MeshBind<'_>) {
        self.cmd_buf.bind_index_buffer(IndexBufferView {
            buffer: bind.index.as_ref(),
            index_type: bind.index_ty.into(),
            range: SubRange::WHOLE,
        });
        self.cmd_buf
            .bind_vertex_buffers(0, once((bind.vertex.as_ref(), SubRange::WHOLE)));
    }

    unsafe fn submit_mesh_descriptor_set(&mut self, set: usize) {
        let graphics = self.graphics_mesh.as_ref().unwrap();

        bind_graphics_descriptor_set(&mut self.cmd_buf, graphics.layout(), graphics.desc_set(set));
    }

    unsafe fn submit_mesh(&mut self, meshes: MeshIter<'_>, world: Mat4, view_projection: Mat4) {
        let graphics = self.graphics_mesh.as_ref().unwrap();
        let world_view_proj = world * view_projection;

        for mesh in meshes {
            let world_view_proj = if let Some(transform) = mesh.transform() {
                transform * world_view_proj
            } else {
                world_view_proj
            };

            self.cmd_buf.push_graphics_constants(
                graphics.layout(),
                ShaderStageFlags::VERTEX,
                0,
                MeshVertexConsts { world_view_proj }.as_ref(),
            );

            for batch in mesh.batches() {
                self.cmd_buf.draw(batch, 0..1);
            }
        }
    }

    unsafe fn submit_finish(&mut self) {
        let mut device = self.driver.borrow_mut();
        let mut dst = self.dst.borrow_mut();
        let mut output = self.geom_buf.output.borrow_mut();
        let dims = dst.dims();

        // Step 6: Copy the output graphics buffer into dst
        self.cmd_buf.end_render_pass();
        output.set_layout(
            &mut self.cmd_buf,
            Layout::TransferSrcOptimal,
            PipelineStage::TRANSFER,
            ImageAccess::TRANSFER_READ,
        );
        dst.set_layout(
            &mut self.cmd_buf,
            Layout::TransferDstOptimal,
            PipelineStage::TRANSFER,
            ImageAccess::TRANSFER_WRITE,
        );
        self.cmd_buf.copy_image(
            output.as_ref(),
            Layout::TransferSrcOptimal,
            dst.as_ref(),
            Layout::TransferDstOptimal,
            once(ImageCopy {
                src_subresource: SubresourceLayers {
                    aspects: Aspects::COLOR,
                    level: 0,
                    layers: 0..1,
                },
                src_offset: Offset::ZERO,
                dst_subresource: SubresourceLayers {
                    aspects: Aspects::COLOR,
                    level: 0,
                    layers: 0..1,
                },
                dst_offset: Offset::ZERO,
                extent: dims.as_extent_depth(1),
            }),
        );

        // Finish
        self.cmd_buf.finish();

        // Submit
        Device::queue_mut(&mut device).submit(
            Submission {
                command_buffers: once(&self.cmd_buf),
                wait_semaphores: empty(),
                signal_semaphores: empty::<&<_Backend as Backend>::Semaphore>(),
            },
            Some(self.fence.as_ref()),
        );
    }

    unsafe fn write_mesh_material_descriptors<'m>(
        device: &Device,
        graphics: &Graphics,
        materials: impl ExactSizeIterator<Item = &'m Material>,
    ) {
        // TODO: Update other write-descriptor functions to use this `default view doesn't borrow device` way of doing things
        for (idx, material) in materials.enumerate() {
            device.write_descriptor_sets(vec![
                DescriptorSetWrite {
                    set: graphics.desc_set(idx),
                    binding: 0,
                    array_offset: 0,
                    descriptors: once(Descriptor::CombinedImageSampler(
                        material.albedo.borrow().as_default_view().as_ref(),
                        Layout::ShaderReadOnlyOptimal,
                        graphics.sampler(0).as_ref(),
                    )),
                },
                DescriptorSetWrite {
                    set: graphics.desc_set(idx),
                    binding: 0,
                    array_offset: 0,
                    descriptors: once(Descriptor::CombinedImageSampler(
                        material.metal_rough.borrow().as_default_view().as_ref(),
                        Layout::ShaderReadOnlyOptimal,
                        graphics.sampler(0).as_ref(),
                    )),
                },
                DescriptorSetWrite {
                    set: graphics.desc_set(idx),
                    binding: 0,
                    array_offset: 0,
                    descriptors: once(Descriptor::CombinedImageSampler(
                        material.normal.borrow().as_default_view().as_ref(),
                        Layout::ShaderReadOnlyOptimal,
                        graphics.sampler(0).as_ref(),
                    )),
                },
            ]);
        }
    }
}

pub struct DrawOpSubmission {
    cmd_buf: <_Backend as Backend>::CommandBuffer,
    cmd_pool: Lease<CommandPool>,
    compiler: Lease<Compiler>,
    dst: Texture2d,
    fence: Lease<Fence>,
    frame_buf: Framebuffer2d,
    geom_buf: GeometryBuffer,
    graphics_line: Option<Lease<Graphics>>,
    graphics_mesh: Option<Lease<Graphics>>,
    graphics_mesh_anim: Option<Lease<Graphics>>,
    graphics_spotlight: Option<Lease<Graphics>>,
    graphics_sunlight: Option<Lease<Graphics>>,
}

impl Drop for DrawOpSubmission {
    fn drop(&mut self) {
        self.wait();

        // Causes the compiler to drop internal caches which store texture refs; they were being held
        // alive there so that they could not be dropped until we finished GPU execution
        self.compiler.reset();
    }
}

impl Op for DrawOpSubmission {
    fn wait(&self) {
        Fence::wait(&self.fence);
    }
}

struct LineInstruction(u32);

#[derive(Clone, Debug)]
pub struct LineCommand([LineVertex; 2]);

#[derive(Clone, Debug)]
struct LineVertex {
    color: AlphaColor,
    pos: Vec3,
}

#[repr(C)]
struct LineVertexConsts {
    transform: Mat4,
}

impl AsRef<[u32; 16]> for LineVertexConsts {
    #[inline]
    fn as_ref(&self) -> &[u32; 16] {
        unsafe { &*(self as *const _ as *const _) }
    }
}

#[derive(Clone)]
pub struct Material {
    pub albedo: BitmapRef,
    pub metal_rough: BitmapRef,
    pub normal: BitmapRef,
}

impl Eq for Material {}

impl Hash for Material {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.albedo.as_ptr().hash(state);
        self.metal_rough.as_ptr().hash(state);
        self.normal.as_ptr().hash(state);
    }
}

impl Ord for Material {
    fn cmp(&self, other: &Self) -> Ordering {
        let mut res = BitmapRef::as_ptr(&self.albedo).cmp(&BitmapRef::as_ptr(&other.albedo));
        if res != Ordering::Less {
            return res;
        }

        res = BitmapRef::as_ptr(&self.metal_rough).cmp(&BitmapRef::as_ptr(&other.metal_rough));
        if res != Ordering::Less {
            return res;
        }

        BitmapRef::as_ptr(&self.normal).cmp(&BitmapRef::as_ptr(&other.normal))
    }
}

impl PartialEq for Material {
    fn eq(&self, other: &Self) -> bool {
        BitmapRef::ptr_eq(&self.albedo, &other.albedo)
            && BitmapRef::ptr_eq(&self.normal, &other.normal)
            && BitmapRef::ptr_eq(&self.metal_rough, &other.metal_rough)
    }
}

impl PartialOrd for Material {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[repr(C)]
struct MeshVertexConsts {
    world_view_proj: Mat4,
}

impl AsRef<[u32; 16]> for MeshVertexConsts {
    #[inline]
    fn as_ref(&self) -> &[u32; 16] {
        unsafe { &*(self as *const Self as *const [u32; 16]) }
    }
}

pub struct ModelCommand {
    camera_order: f32, // TODO: Could probably be u16?
    material: Material,
    mesh_filter: Option<MeshFilter>,
    model: ModelRef,
    pose: Option<Pose>,
    transform: Mat4,
}

#[derive(Clone, Debug)]
pub struct PointLightCommand {
    core: Sphere,  // full-bright center and radius
    color: Color,  // `core` and penumbra-to-transparent color
    penumbra: f32, // distance after `core` which fades from `color` to transparent
    power: f32, // sRGB power value, normalized to current gamma so 1.0 == a user setting of 1.2 and 2.0 == 2.4
}

impl PointLightCommand {
    /// Returns a tightly fitting sphere around the lit area of this point light, including the penumbra
    pub(self) fn bounds(&self) -> Sphere {
        self.core + self.penumbra
    }
}

#[derive(Clone, Debug)]
pub struct RectLightCommand {
    color: Color, // full-bright and penumbra-to-transparent color
    dims: CoordF,
    radius: f32, // size of the penumbra area beyond the box formed by `pos` and `range` which fades from `color` to transparent
    pos: Vec3,   // top-left corner when viewed from above
    power: f32, // sRGB power value, normalized to current gamma so 1.0 == a user setting of 1.2 and 2.0 == 2.4
    range: f32, // distance from `pos` to the bottom of the rectangular light
}

impl RectLightCommand {
    /// Returns a tightly fitting sphere around the lit area of this rectangular light, including the penumbra
    pub(self) fn bounds(&self) -> Sphere {
        todo!();
    }
}

#[derive(Clone, Debug)]
pub struct SunlightCommand {
    color: Color, // uniform color for any area exposed to the sunlight
    normal: Vec3, // direction which the sunlight shines
    power: f32, // sRGB power value, normalized to current gamma so 1.0 == a user setting of 1.2 and 2.0 == 2.4
}

#[derive(Clone, Debug)]
pub struct SpotlightCommand {
    color: Color,         // `cone` and penumbra-to-transparent color
    cone_radius: f32, // radius of the spotlight cone from the center to the edge of the full-bright area
    normal: Vec3,     // direction from `pos` which the spotlight shines
    penumbra_radius: f32, // Additional radius beyond `cone_radius` which fades from `color` to transparent
    pos: Vec3,            // position of the pointy end
    power: f32, // sRGB power value, normalized to current gamma so 1.0 == a user setting of 1.2 and 2.0 == 2.4
    range: Range<f32>, // lit distance from `pos` and to the bottom of the spotlight (does not account for the lens-shaped end)
    top_radius: f32,
}

impl SpotlightCommand {
    /// Returns a tightly fitting cone around the lit area of this spotlight, including the penumbra and
    /// lens-shaped base.
    pub(self) fn bounds(&self) -> Cone {
        Cone::new(
            self.pos,
            self.normal,
            self.range.end,
            self.cone_radius + self.penumbra_radius,
        )
    }
}
