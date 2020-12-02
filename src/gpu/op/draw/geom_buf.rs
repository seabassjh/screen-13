use {
    crate::{
        gpu::{
            pool::{Lease, Pool},
            Texture2d,
        },
        math::Extent,
    },
    gfx_hal::{
        format::Format,
        image::{Layout, Tiling, Usage as ImageUsage},
    },
};

pub struct GeometryBuffer {
    pub albedo: Lease<Texture2d>,
    pub depth: Lease<Texture2d>,
    pub light: Lease<Texture2d>,
    pub material: Lease<Texture2d>,
    pub normal: Lease<Texture2d>,
}

impl GeometryBuffer {
    pub fn new(
        #[cfg(debug_assertions)] name: &str,
        pool: &mut Pool,
        dims: Extent,
        albedo_fmt: Format,
    ) -> Self {
        let albedo = pool.texture(
            #[cfg(debug_assertions)]
            &format!("{} (Albedo)", name),
            dims,
            Tiling::Optimal,
            albedo_fmt,
            &[],
            Layout::Undefined,
            ImageUsage::COLOR_ATTACHMENT
                | ImageUsage::INPUT_ATTACHMENT
                | ImageUsage::SAMPLED
                | ImageUsage::TRANSFER_DST
                | ImageUsage::TRANSFER_SRC,
            1,
            1,
            1,
        );
        let depth = pool.texture(
            #[cfg(debug_assertions)]
            &format!("{} (Depth)", name),
            dims,
            Tiling::Optimal,
            Format::R32Sfloat,
            &[],
            Layout::Undefined,
            ImageUsage::DEPTH_STENCIL_ATTACHMENT
                | ImageUsage::INPUT_ATTACHMENT
                | ImageUsage::SAMPLED,
            1,
            1,
            1,
        );
        let light = pool.texture(
            #[cfg(debug_assertions)]
            &format!("{} (Light)", name),
            dims,
            Tiling::Optimal,
            Format::R32Uint,
            &[],
            Layout::Undefined,
            ImageUsage::COLOR_ATTACHMENT | ImageUsage::INPUT_ATTACHMENT | ImageUsage::SAMPLED,
            1,
            1,
            1,
        );
        let material = pool.texture(
            #[cfg(debug_assertions)]
            &format!("{} (Material)", name),
            dims,
            Tiling::Optimal,
            Format::Rg8Unorm,
            &[],
            Layout::Undefined,
            ImageUsage::COLOR_ATTACHMENT | ImageUsage::INPUT_ATTACHMENT | ImageUsage::SAMPLED,
            1,
            1,
            1,
        );
        let normal = pool.texture(
            #[cfg(debug_assertions)]
            &format!("{} (Normal)", name),
            dims,
            Tiling::Optimal,
            Format::Rgb32Sfloat,
            &[],
            Layout::Undefined,
            ImageUsage::COLOR_ATTACHMENT | ImageUsage::INPUT_ATTACHMENT | ImageUsage::SAMPLED,
            1,
            1,
            1,
        );

        Self {
            albedo,
            depth,
            light,
            material,
            normal,
        }
    }
}
