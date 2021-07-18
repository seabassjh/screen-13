use {
    super::vector_font::{VectorFont, VectorGlyph},
    crate::{
        gpu::{
            adapter, align_up,
            pool::{Lease, Pool},
            Data, Mapping, Texture2d,
        },
        math::{CoordF, Rect, RectF},
        ptr::Shared,
    },
    archery::SharedPointerKind,
    etagere::{AtlasAllocator, Size},
    fontdue::OutlineBounds,
    gfx_hal::{
        adapter::PhysicalDevice as _,
        format::Format,
        image::{Layout as ImageLayout, Usage as ImageUsage},
    },
    std::{collections::HashMap, ops::Range, ptr::copy_nonoverlapping},
};

struct Buffer<P>
where
    P: SharedPointerKind,
{
    data: Lease<Data, P>,
    offset: u64,
}

pub(super) struct DynamicAtlas<P>
where
    P: SharedPointerKind,
{
    bufs: Vec<Buffer<P>>,
    glyphs: HashMap<Key, Value>,
    font: Shared<VectorFont, P>,
    pages: Vec<Page<P>>,
    pending_glyphs: Vec<Glyph>,
}

impl<P> DynamicAtlas<P>
where
    P: SharedPointerKind,
{
    pub fn new(font: &Shared<VectorFont, P>) -> Self {
        Self {
            bufs: Default::default(),
            glyphs: Default::default(),
            font: Shared::clone(font),
            pages: Default::default(),
            pending_glyphs: Default::default(),
        }
    }

    pub fn font(&self) -> &Shared<VectorFont, P> {
        &self.font
    }

    pub fn page(&self, idx: usize) -> &Texture2d {
        self.pages[idx].as_ref()
    }

    pub fn pages(&self) -> impl ExactSizeIterator<Item = &Texture2d> {
        self.pages.iter().map(|page| page.as_ref())
    }

    pub(super) fn parse<'a>(
        &'a mut self,
        pool: &'a mut Pool<P>,
        buf_len: u64,
        dims: u32,
        size: f32,
        text: &'a str,
    ) -> impl Iterator<Item = (char, VectorGlyph)> + 'a {
        Parser {
            atlas: self,
            buf_len,
            chars: text.chars(),
            dims,
            pool,
            pos: CoordF::ZERO,
            size,
        }
    }

    /// Work-around for pop_pending_glyph not being in the form of an ExactSizeIterator
    pub(super) fn has_pending_glyphs(&self) -> bool {
        !self.pending_glyphs.is_empty()
    }

    /// Pops a glyph off the pending list and returns a reference to the data. I would love for this
    /// to be an Iterator however the mutable Data reference would live longer than the iterator,
    /// unless there is something I'm missing. So we call it one-by-one no biggie.
    pub(super) fn pop_pending_glyph<'a>(&'a mut self) -> Option<GlyphRef<'a>> {
        let bufs = &mut self.bufs;
        let pages = &self.pages;
        self.pending_glyphs.pop().map(move |glyph| GlyphRef {
            buf: &mut bufs[glyph.buf_idx].data,
            buf_range: glyph.buf_range,
            page: pages[glyph.page_idx].as_ref(),
            page_rect: glyph.page_rect,
        })
    }
}

struct Glyph {
    buf_idx: usize,
    buf_range: Range<u64>,
    page_idx: usize,
    page_rect: Rect,
}

pub struct GlyphRef<'a> {
    pub buf: &'a mut Data,
    pub buf_range: Range<u64>,
    pub page: &'a Texture2d,
    pub page_rect: Rect,
}

// TODO: Better name
#[derive(Eq, Hash, PartialEq)]
pub struct Key {
    char: char,
    scale: u32, // u32 bits of a f32 because we only care about uniqueness
}

struct Page<P>
where
    P: SharedPointerKind,
{
    allocator: AtlasAllocator,
    texture: Lease<Shared<Texture2d, P>, P>,
}

impl<P> AsRef<Texture2d> for Page<P>
where
    P: SharedPointerKind,
{
    fn as_ref(&self) -> &Texture2d {
        &self.texture
    }
}

struct Parser<'a, C, P>
where
    C: Iterator<Item = char>,
    P: 'static + SharedPointerKind,
{
    atlas: &'a mut DynamicAtlas<P>,
    buf_len: u64,
    chars: C,
    dims: u32,
    pool: &'a mut Pool<P>,
    pos: CoordF,
    size: f32,
}

impl<C, P> Iterator for Parser<'_, C, P>
where
    C: Iterator<Item = char>,
    P: SharedPointerKind,
{
    type Item = (char, VectorGlyph);

    fn next(&mut self) -> Option<Self::Item> {
        self.chars.next().map(|char| {
            let buf_len = self.buf_len;
            let dims = self.dims;
            let size = self.size;
            let bufs = &mut self.atlas.bufs;
            let font = &self.atlas.font;
            let pages = &mut self.atlas.pages;
            let pending_glyphs = &mut self.atlas.pending_glyphs;
            let pool = &mut self.pool;
            let pos = &mut self.pos;
            let glyph = self
                .atlas
                .glyphs
                .entry(Key {
                    char,
                    scale: self.size.to_bits(),
                })
                .or_insert_with(|| {
                    let (mut metrics, mut raster) = font.0.rasterize(char, size);

                    // Whitespace characters have no rasterized pixels - we use a single blank pixel
                    if raster.is_empty() {
                        metrics.height = 1;
                        metrics.width = 1;
                        raster.push(0);
                    }

                    // TODO: Assert width and height are reasonable values?
                    let raster_size = Size::new(metrics.width as i32, metrics.height as i32);

                    // Get a page and allocation either by finding the first usable page or allocating
                    // from a new page
                    let (page_idx, allocation) = pages
                        .iter_mut()
                        .enumerate()
                        .find_map(|(page_idx, page)| {
                            page.allocator
                                .allocate(raster_size)
                                .map(|allocation| (page_idx, allocation))
                        })
                        .unwrap_or_else(|| {
                            let mut allocator =
                                AtlasAllocator::new(Size::new(dims as i32, dims as i32));
                            let allocation = allocator.allocate(raster_size).unwrap();

                            let texture = unsafe {
                                pool.texture(
                                    #[cfg(feature = "debug-names")]
                                    "Vector font atlas",
                                    (dims, dims).into(),
                                    Format::R8Unorm,
                                    ImageLayout::Undefined,
                                    ImageUsage::SAMPLED
                                        | ImageUsage::TRANSFER_DST
                                        | ImageUsage::TRANSFER_SRC,
                                    1,
                                    1,
                                    1,
                                )
                            };
                            let page_idx = pages.len();
                            pages.push(Page { allocator, texture });

                            (page_idx, allocation)
                        });

                    let (non_coherent_atom_size, optimal_buffer_copy_offset_alignment) = unsafe {
                        let limits = adapter().physical_device.properties().limits;

                        (
                            limits.non_coherent_atom_size,
                            limits.optimal_buffer_copy_offset_alignment,
                        )
                    };

                    // Get a large enough buffer (optimization: must be the last buffer) or a new one
                    let bufs_len = bufs.len();
                    let (buf, buf_idx) = if let Some(buf) = bufs.last_mut().filter(|buf| {
                        buf.data.capacity() as i64
                            - align_up(buf.offset, optimal_buffer_copy_offset_alignment) as i64
                            >= raster.len() as _
                    }) {
                        (buf, bufs_len - 1)
                    } else {
                        bufs.push(Buffer {
                            data: unsafe {
                                pool.data(
                                    #[cfg(feature = "debug-names")]
                                    "Vector font buffer",
                                    buf_len.max(raster.len() as _),
                                    true,
                                )
                            },
                            offset: 0,
                        });
                        (bufs.last_mut().unwrap(), bufs_len)
                    };

                    // Copy this rasterized character into the buffer
                    unsafe {
                        let mut mapped_range = buf
                            .data
                            .map_range_mut(buf.offset..buf.offset + raster.len() as u64)
                            .unwrap();
                        copy_nonoverlapping(
                            raster.as_ptr(),
                            mapped_range.as_mut_ptr(),
                            raster.len() as _,
                        );
                        debug!("Copied {} bytes", raster.len());
                        Mapping::flush(&mut mapped_range).unwrap();
                    }

                    debug!(
                        "Rasterized '{}' ({} bytes, metrics={}x{}, buf={}..{} page={} buf={})",
                        char,
                        raster.len(),
                        metrics.width,
                        metrics.height,
                        buf.offset,
                        buf.offset + raster.len() as u64,
                        page_idx,
                        buf_idx,
                    );

                    // Keep track of the need to copy this buffer data to the page
                    let page_rect = Rect::new(
                        allocation.rectangle.min.x,
                        allocation.rectangle.min.y,
                        metrics.width as _,
                        metrics.height as _,
                    );
                    pending_glyphs.push(Glyph {
                        buf_idx,
                        buf_range: buf.offset..buf.offset + raster.len() as u64,
                        page_idx,
                        page_rect,
                    });
                    buf.offset += align_up(raster.len(), non_coherent_atom_size) as u64;

                    Value {
                        advance: CoordF::new(metrics.advance_width, metrics.advance_height),
                        bounds: metrics.bounds,
                        page_idx,
                        page_rect,
                    }
                });

            let res = (
                char,
                VectorGlyph {
                    page_idx: glyph.page_idx,
                    page_rect: glyph.page_rect,
                    screen_rect: RectF::new(
                        pos.x,
                        glyph.bounds.height + glyph.bounds.ymin,
                        glyph.bounds.width,
                        glyph.bounds.height,
                    ),
                },
            );

            pos.x += glyph.advance.x;
            pos.y += glyph.advance.y;

            res
        })
    }
}

// TODO: Better name
pub struct Value {
    pub advance: CoordF,
    pub bounds: OutlineBounds,
    pub page_idx: usize,
    pub page_rect: Rect,
}
