use {
    super::spirv,
    crate::gpu::driver::{
        descriptor_range_desc, descriptor_set_layout_binding, ComputePipeline, DescriptorPool,
        DescriptorSetLayout, Driver, Sampler, ShaderModule,
    },
    gfx_hal::{
        pso::{
            BufferDescriptorFormat, BufferDescriptorType, DescriptorPool as _, DescriptorRangeDesc,
            DescriptorSetLayoutBinding, DescriptorType, ImageDescriptorType, ShaderStageFlags,
        },
        Backend,
    },
    gfx_impl::Backend as _Backend,
    std::{
        borrow::Borrow,
        iter::{empty, once},
        ops::Range,
    },
};

pub struct Compute {
    desc_pool: DescriptorPool,
    desc_sets: Vec<<_Backend as Backend>::DescriptorSet>,
    pipeline: ComputePipeline,
    set_layout: DescriptorSetLayout,
    samplers: Vec<Sampler>,
    shader: ShaderModule,
}

impl Compute {
    #[allow(clippy::too_many_arguments)]
    fn new<I, IR, ID, IS>(
        #[cfg(debug_assertions)] name: &str,
        driver: &Driver,
        spirv: &[u32],
        consts: IR,
        max_sets: usize,
        desc_ranges: ID,
        bindings: I,
        samplers: IS,
    ) -> Self
    where
        I: IntoIterator,
        I::Item: Borrow<DescriptorSetLayoutBinding>,
        IR: IntoIterator,
        IR::IntoIter: ExactSizeIterator,
        IR::Item: Borrow<(ShaderStageFlags, Range<u32>)>,
        ID: IntoIterator,
        ID::IntoIter: ExactSizeIterator,
        ID::Item: Borrow<DescriptorRangeDesc>,
        IS: Iterator<Item = Sampler>,
    {
        let shader = unsafe { ShaderModule::new(Driver::clone(&driver), spirv) };
        let set_layout = DescriptorSetLayout::new(
            #[cfg(debug_assertions)]
            name,
            Driver::clone(&driver),
            bindings,
        );
        let pipeline = unsafe {
            ComputePipeline::new(
                #[cfg(debug_assertions)]
                name,
                Driver::clone(&driver),
                ShaderModule::entry_point(&shader),
                once(&*set_layout),
                consts,
            )
        };

        let mut desc_pool = DescriptorPool::new(Driver::clone(&driver), max_sets, desc_ranges);
        let desc_sets = (0..max_sets)
            .map(|_| unsafe { desc_pool.allocate_set(&*set_layout).unwrap() })
            .collect();

        let samplers = samplers.collect();

        Compute {
            desc_pool,
            desc_sets,
            pipeline,
            set_layout,
            samplers,
            shader,
        }
    }

    pub fn calc_vertex_attrs(#[cfg(debug_assertions)] name: &str, driver: &Driver) -> Self {
        Self::new(
            #[cfg(debug_assertions)]
            name,
            driver,
            &spirv::compute::CALC_VERTEX_ATTRS_COMP,
            &[(ShaderStageFlags::COMPUTE, 0..4)],
            1,
            &[descriptor_range_desc(
                1,
                DescriptorType::Buffer {
                    format: BufferDescriptorFormat::Structured {
                        dynamic_offset: false,
                    },
                    ty: BufferDescriptorType::Storage { read_only: false },
                },
            )],
            &[descriptor_set_layout_binding(
                0,
                1,
                ShaderStageFlags::COMPUTE,
                DescriptorType::Buffer {
                    format: BufferDescriptorFormat::Structured {
                        dynamic_offset: false,
                    },
                    ty: BufferDescriptorType::Storage { read_only: false },
                },
            )],
            empty(),
        )
    }

    pub fn decode_rgb_rgba(#[cfg(debug_assertions)] name: &str, driver: &Driver) -> Self {
        Self::new(
            #[cfg(debug_assertions)]
            name,
            driver,
            &spirv::compute::DECODE_RGB_RGBA_COMP,
            &[(ShaderStageFlags::COMPUTE, 0..4)],
            1,
            &[
                descriptor_range_desc(
                    1,
                    DescriptorType::Buffer {
                        format: BufferDescriptorFormat::Structured {
                            dynamic_offset: false,
                        },
                        ty: BufferDescriptorType::Storage { read_only: true },
                    },
                ),
                descriptor_range_desc(
                    1,
                    DescriptorType::Image {
                        ty: ImageDescriptorType::Storage { read_only: false },
                    },
                ),
            ],
            &[
                descriptor_set_layout_binding(
                    0,
                    1,
                    ShaderStageFlags::COMPUTE,
                    DescriptorType::Buffer {
                        format: BufferDescriptorFormat::Structured {
                            dynamic_offset: false,
                        },
                        ty: BufferDescriptorType::Storage { read_only: true },
                    },
                ),
                descriptor_set_layout_binding(
                    1,
                    1,
                    ShaderStageFlags::COMPUTE,
                    DescriptorType::Image {
                        ty: ImageDescriptorType::Storage { read_only: false },
                    },
                ),
            ],
            empty(),
        )
    }

    pub fn pipeline(&self) -> &ComputePipeline {
        &self.pipeline
    }

    fn reset(&mut self) {
        unsafe {
            self.desc_pool.reset();
        }

        for set in &mut self.desc_sets {
            *set = unsafe { self.desc_pool.allocate_set(&*self.set_layout).unwrap() }
        }
    }

    pub fn desc_set(&self, idx: usize) -> &<_Backend as Backend>::DescriptorSet {
        &self.desc_sets[idx]
    }
}