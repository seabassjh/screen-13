use {
    super::{
        geom::{
            gen_line, gen_rect_light, gen_spotlight, LINE_STRIDE, POINT_LIGHT, RECT_LIGHT_STRIDE,
            SPOTLIGHT_STRIDE,
        },
        instruction::Instruction,
        key::{LineKey, RectLightKey, SpotlightKey},
        Command,
    },
    crate::{
        camera::Camera,
        gpu::{Data, Lease, Mesh, PoolRef, Texture2d},
    },
    bitflags::bitflags,
    std::{cmp::Ordering, mem::take, ops::Range, ptr::copy_nonoverlapping},
};

// Always ask for a bigger cache capacity than needed; it reduces the need to completely replace
// the existing cache and then have to copy all the old data over.
const CACHE_CAPACITY_FACTOR: f32 = 2.0;

// TODO: Maybe store 'LRU' as a number, 4 or so? Right now it's a bool so if you don't use something each frame it gets removed.
// TODO: Also stop compaction after a certain number of cycles or % complete, maybe only 10%.

enum Asm {
    /// LRU index and scale
    RectLight(usize, f32),

    /// LRU index and scale
    Spotlight(usize, f32),
}

pub struct Compilation<'c, 'm> {
    cmds: &'m [Command<'m>],
    compiler: &'c mut Compiler, //TODO: Mutable for mut access to vertex_buf, let's revisit this if can be made read-only
    idx: usize,
    mesh_sets: MeshSets,
    stages: Stages,
}

impl Compilation<'_, '_> {
    /// Returns the accumulated vertex buffer for this compilation. It is a blob of all requested lights and lines jammed together.
    pub fn vertex_buf_mut(&mut self) -> &mut Data {
        // This can be unwrapped because it is set by the compiler
        &mut self.compiler.vertex_buf.as_mut().unwrap().0
    }

    pub fn mesh_sets_required(&self) -> &MeshSets {
        &self.mesh_sets
    }

    pub fn stages_required(&self) -> Stages {
        self.stages
    }
}

impl<'c> Iterator for Compilation<'c, '_> {
    type Item = Instruction<'c>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(Self::Item::Stop)
    }
}

/// Compiles a series of drawing commands into renderable instructions. The purpose of this structure is
/// two-fold:
/// - Reduce per-draw allocations with line and light caches (they are not cleared after each use)
/// - Store references to the in-use mesh textures during rendering (this cache is cleared after use)
#[derive(Default)]
pub struct Compiler {
    code: Vec<Asm>,
    line_lru: Vec<Lru<LineKey>>,
    mesh_textures: Vec<Texture2d>,
    point_light_lru: bool,
    rect_light_lru: Vec<Lru<RectLightKey>>,
    spotlight_lru: Vec<Lru<SpotlightKey>>,
    vertex_buf: Option<(Lease<Data>, Option<Range<u64>>, Vec<Range<u64>>)>, // Tuple of the data and dirty CPU and GPU regions
}

impl Compiler {
    /// Moves cache items into clumps so future items can be appended onto the end without needing to
    /// resize the cache buffer. As a side effect this causes dirty regions to be moved on the GPU.
    fn compact_cache(&mut self) {}

    /// Compiles a given set of commands into a ready-to-draw list of instructions. Performs these steps:
    /// - Cull commands which might not be visible to the camera
    /// - Sort commands into predictable groupings (opaque meshes, lights, transparent meshes, lines)
    /// - Sort mesh commands further by texture(s) in order to reduce descriptor set switching/usage
    /// - Prepare a single buffer of all line and light vertices which can be copied to the GPU all at once
    pub fn compile<'a, 'c>(
        &'a mut self,
        #[cfg(debug_assertions)] name: &str,
        pool: &PoolRef,
        camera: &impl Camera,
        mut cmds: &'c mut [Command<'c>],
    ) -> Compilation<'a, 'c> {
        // TODO: This function has been normalized to be easier to read, may want to combine steps for perf after design stabilizes
        //assert!(self.line_buf.is_empty());
        assert!(self.mesh_textures.is_empty());

        #[cfg(debug_assertions)]
        if let Some((_, dirty, _)) = self.vertex_buf.as_ref() {
            assert!(dirty.is_none());
            // TODO: Add some assertions here
        }

        // Remove non-visible commands
        Self::cull(camera, &mut cmds);

        // Keep track of the stages needed to draw these commands
        // TODO: Roll this into one of the other loops or kill with fire completely, probably not needed
        let mut stages = Stages::empty();
        for cmd in cmds.iter() {
            match cmd {
                Command::Line(_) => stages |= Stages::LINE,
                Command::Mesh(_) => {
                    // TODO: Actual logic!
                    stages |= Stages::MESH_SINGLE_TEX;
                }
                Command::PointLight(_) => stages |= Stages::POINTLIGHT,
                _ => todo!(),
            }
        }

        // Rearrange the commands so draw order doesn't cause unnecessary resource-switching
        self.sort(cmds);

        // Fill the vertex cache buffer for all requested lines and lights
        self.fill_cache(
            #[cfg(debug_assertions)]
            name,
            pool,
            cmds,
        );

        Compilation {
            cmds,
            compiler: self,
            idx: 0,
            mesh_sets: Default::default(),
            stages,
        }
    }

    // TODO: Could return counts here and put a tiny bit of speed-up into the `fill_cache` function
    /// Cull any commands which are not within the camera frustum. Also adds z-order to meshes.
    fn cull(camera: &impl Camera, cmds: &mut &mut [Command]) {
        let eye = -camera.eye();
        let mut idx = 0;
        let mut len = cmds.len();

        while idx < len {
            if match &mut cmds[idx] {
                Command::Mesh(cmd) => {
                    let res = camera.overlaps_sphere(cmd.mesh.bounds);
                    if res {
                        // Assign a relative measure of distance from the camera for all mesh commands which allows us to submit draw commands
                        // in the best order for the z-buffering algorithm (we use a depth map with comparisons that discard covered fragments)
                        cmd.camera_z = cmd.transform.transform_vector3(eye).length_squared();
                    }

                    res
                }
                Command::PointLight(cmd) => camera.overlaps_sphere(cmd.bounds()),
                Command::RectLight(cmd) => camera.overlaps_sphere(cmd.bounds()),
                Command::Spotlight(cmd) => camera.overlaps_cone(cmd.bounds()),
                _ => {
                    // Lines and Sunlight do not get culled; we assume they are visible and draw them
                    // TODO: Test the effect of adding in line culling with lots and lots of lines, make it a feature or argument?
                    true
                }
            } {
                // The command at `idx` has been culled and won't be drawn (put it at the end of the list/no-mans land)
                len -= 1;
                cmds.swap(idx, len);
            } else {
                // The command at `idx` is visible and will draw normally
                idx += 1;
            }
        }

        // Safely replace `cmds` with a subslice, this drops the references to the culled commands but not their values
        *cmds = &mut take(cmds)[0..len];
    }

    /// Gets this compiler ready to use the given commands by pre-filling vertex cache buffers. Also records the range of vertex data
    /// which must be copied from CPU to the GPU.
    fn fill_cache(
        &mut self,
        #[cfg(debug_assertions)] name: &str,
        pool: &PoolRef,
        cmds: &mut [Command],
    ) {
        #[cfg(debug_assertions)]
        if self.vertex_buf.is_some() {
            let (_, cpu_dirty, gpu_dirty) = self.vertex_buf.as_ref().unwrap();

            assert!(cpu_dirty.is_none());
            assert!(gpu_dirty.is_empty());
        }

        // Locate the groups - we know these `SearchIdx` values will not be found as they are gaps in between the groups
        let point_light_idx = cmds
            .binary_search_by(|probe| {
                (Self::group_idx(probe) as isize).cmp(&(SearchIdx::PointLight as _))
            })
            .unwrap_err();
        let rect_light_idx = cmds[point_light_idx..]
            .binary_search_by(|probe| {
                (Self::group_idx(probe) as isize).cmp(&(SearchIdx::RectLight as _))
            })
            .unwrap_err();
        let spotlight_idx = cmds[rect_light_idx..]
            .binary_search_by(|probe| {
                (Self::group_idx(probe) as isize).cmp(&(SearchIdx::Spotlight as _))
            })
            .unwrap_err();
        let line_idx = cmds[spotlight_idx..]
            .binary_search_by(|probe| {
                (Self::group_idx(probe) as isize).cmp(&(SearchIdx::Line as _))
            })
            .unwrap_err();

        // Count how many items of each group we found
        let point_light_count = rect_light_idx - point_light_idx;
        let rect_light_count = spotlight_idx - rect_light_idx;
        let spotlight_count = line_idx - spotlight_idx;
        let line_count = cmds.len() - line_idx;

        // Early out if there is no filling of new vertices to be done
        if point_light_count == 0
            && rect_light_count == 0
            && spotlight_count == 0
            && line_count == 0
        {
            return;
        }

        // Note that the vertex buffer will always reserve space for a point light icosphere
        let len = (POINT_LIGHT.len()
            + line_count * LINE_STRIDE
            + rect_light_count * RECT_LIGHT_STRIDE
            + spotlight_count * SPOTLIGHT_STRIDE) as u64;
        let capacity = (len as f32 * CACHE_CAPACITY_FACTOR) as u64;
        let mut end = (POINT_LIGHT.len()
            + self.line_lru.len() * LINE_STRIDE
            + self.rect_light_lru.len() * RECT_LIGHT_STRIDE
            + self.spotlight_lru.len() * SPOTLIGHT_STRIDE) as u64;

        #[cfg(debug_assertions)]
        let name = format!("{} vertex buffer", name);

        // Resize the vertex buffer as needed
        if self.vertex_buf.is_none() || len > self.vertex_buf.as_ref().unwrap().0.capacity() {
            #[cfg(debug_assertions)]
            let mut should_rename = true;

            if let Some((old_buf, _, _)) = self.vertex_buf.replace((
                pool.borrow_mut().data(
                    #[cfg(debug_assertions)]
                    &name,
                    capacity,
                ),
                Some(Range { start: 0, end }),
                Default::default(),
            )) {
                #[cfg(debug_assertions)]
                {
                    should_rename = false;
                }

                let new_buf = &mut self.vertex_buf.as_mut().unwrap().0;
                let start = if self.point_light_lru {
                    POINT_LIGHT.len() as u64
                } else {
                    0
                };
                let count = end - start;

                unsafe {
                    // TODO: Is it faster to just re-gen all the vertices? Maybe store both buffers and do a GPU copy instead?
                    // Preserve the contents of the old buffer in the new buffer
                    copy_nonoverlapping(
                        old_buf.map_range(start..end).as_ptr(),
                        new_buf.map_range_mut(start..end).as_mut_ptr(),
                        count as _,
                    );
                }
            } else {
                end = POINT_LIGHT.len() as _;
            }

            #[cfg(debug_assertions)]
            if should_rename {
                self.vertex_buf.as_mut().unwrap().0.rename(&name);
            }
        }

        let mut start = end;
        let (buf, dirty, _) = &mut self.vertex_buf.as_mut().unwrap();

        // When we resize the buffer there will be pre-existing dirty info from above
        if let Some(dirty) = dirty {
            start = start.min(dirty.start);
        }

        // Add the point light mesh to the buffer as needed (the spot is reserved for it)
        if point_light_count > 0 && !self.point_light_lru {
            self.point_light_lru = true;

            unsafe {
                copy_nonoverlapping(
                    POINT_LIGHT.as_ptr(),
                    buf.map_range_mut(0..POINT_LIGHT.len() as _).as_mut_ptr(),
                    POINT_LIGHT.len() as _,
                );
            }
        }

        // Produce the assembly code that will draw all rectangular lights
        for cmd in cmds[rect_light_idx..rect_light_idx + rect_light_count].iter() {
            let (key, scale) = RectLightKey::quantize(cmd.as_rect_light().unwrap());
            self.code.push(Asm::Spotlight(
                match self
                    .rect_light_lru
                    .binary_search_by(|probe| probe.key.cmp(&key))
                {
                    Err(idx) => {
                        // Cache the normalized mesh for this rectangular light
                        let new_end = end + SPOTLIGHT_STRIDE as u64;
                        let vertices = gen_rect_light(key.dims(), key.range(), key.radius());

                        unsafe {
                            copy_nonoverlapping(
                                vertices.as_ptr(),
                                buf.map_range_mut(end..new_end).as_mut_ptr(),
                                SPOTLIGHT_STRIDE,
                            );
                        }

                        // Create a new cache entry for this rectangular light
                        self.rect_light_lru.insert(
                            idx,
                            Lru {
                                key,
                                offset: end,
                                recently_used: true,
                            },
                        );
                        end = new_end;

                        idx
                    }
                    Ok(idx) => {
                        self.spotlight_lru[idx].recently_used = true;

                        idx
                    }
                },
                scale,
            ));
        }

        // Produce the assembly code that will draw all spotlights
        for cmd in cmds[spotlight_idx..spotlight_idx + spotlight_count].iter() {
            let (key, scale) = SpotlightKey::quantize(cmd.as_spotlight().unwrap());
            self.code.push(Asm::Spotlight(
                match self
                    .spotlight_lru
                    .binary_search_by(|probe| probe.key.cmp(&key))
                {
                    Err(idx) => {
                        // Cache the normalized mesh for this spotlight
                        let new_end = end + SPOTLIGHT_STRIDE as u64;
                        let vertices = gen_spotlight(key.radius(), key.range());

                        unsafe {
                            copy_nonoverlapping(
                                vertices.as_ptr(),
                                buf.map_range_mut(end..new_end).as_mut_ptr(),
                                SPOTLIGHT_STRIDE,
                            );
                        }

                        // Create a new cache entry for this spotlight
                        self.spotlight_lru.insert(
                            idx,
                            Lru {
                                key,
                                offset: end,
                                recently_used: true,
                            },
                        );
                        end = new_end;

                        idx
                    }
                    Ok(idx) => {
                        self.spotlight_lru[idx].recently_used = true;

                        idx
                    }
                },
                scale,
            ));
        }

        // Cache all line vertices
        for cmd in cmds[line_idx..].iter() {
            let line = cmd.as_line().unwrap();
            let key = LineKey::hash(line);
            match self.line_lru.binary_search_by(|probe| probe.key.cmp(&key)) {
                Err(idx) => {
                    // Cache the vertices for this line segment
                    let new_end = end + LINE_STRIDE as u64;
                    let vertices = gen_line(&line.vertices);

                    unsafe {
                        copy_nonoverlapping(
                            vertices.as_ptr(),
                            buf.map_range_mut(end..new_end).as_mut_ptr(),
                            SPOTLIGHT_STRIDE,
                        );
                    }

                    // Create a new cache entry for this line segment
                    self.line_lru.insert(
                        idx,
                        Lru {
                            key,
                            offset: end,
                            recently_used: true,
                        },
                    );
                    end = new_end;
                }
                Ok(idx) => {
                    self.line_lru[idx].recently_used = true;
                }
            }
        }

        // We need to copy these vertices from the CPU to the GPU
        *dirty = Some(Range { start, end });
    }

    /// All commands sort into groups: first meshes, then lights, followed by lines.
    fn group_idx(cmd: &Command) -> GroupIdx {
        // TODO: Transparencies?
        match cmd {
            Command::Mesh(_) => GroupIdx::Mesh,
            Command::PointLight(_) => GroupIdx::PointLight,
            Command::RectLight(_) => GroupIdx::RectLight,
            Command::Spotlight(_) => GroupIdx::Spotlight,
            Command::Sunlight(_) => GroupIdx::Sunlight,
            Command::Line(_) => GroupIdx::Line,
        }
    }

    /// Meshes sort into sub-groups: first animated, then single texture, followed by dual texture.
    fn mesh_group_idx(mesh: &Mesh) -> usize {
        // TODO: Transparencies?
        if mesh.is_animated() {
            0
        } else if mesh.is_single_texture() {
            1
        } else {
            2
        }
    }

    /// Returns the index of a given texture in our `mesh texture` list, adding it as needed.
    fn mesh_texture_idx(&mut self, tex: &Texture2d) -> usize {
        let tex_ptr = tex.as_ptr();
        match self
            .mesh_textures
            .binary_search_by(|probe| probe.as_ptr().cmp(&tex_ptr))
        {
            Err(idx) => {
                // Not in the list - add and return the new index
                self.mesh_textures.insert(idx, Texture2d::clone(tex));

                idx
            }
            Ok(idx) => idx,
        }
    }

    /// Resets the internal caches so that this compiler may be reused by calling the `compile` function.
    pub fn reset(&mut self) {
        self.mesh_textures.clear();

        // Reset the CPU/GPU dirty regions
        if let Some((_, cpu_dirty, gpu_dirty)) = self.vertex_buf.as_mut() {
            *cpu_dirty = None;
            gpu_dirty.clear(); // TODO: Do this after compaction?
        }

        // Remove the least recently used line and light from the cache (LRU == Not used this frame)
        self.compact_cache();

        // Finally, reset the "recently used" flags
        self.point_light_lru = false;

        for item in self.line_lru.iter_mut() {
            item.recently_used = false;
        }

        for item in self.rect_light_lru.iter_mut() {
            item.recently_used = false;
        }

        for item in self.spotlight_lru.iter_mut() {
            item.recently_used = false;
        }
    }

    // Sorts commands into a predictable and efficient order for drawing.
    fn sort(&mut self, cmds: &mut [Command]) {
        // TODO: Sorting meshes by material also - helpful or not?
        cmds.sort_unstable_by(|lhs, rhs| {
            // Shorthand - we only care about equal or not-equal here
            use Ordering::Equal as eq;

            let lhs_idx = Self::group_idx(lhs) as isize;
            let rhs_idx = Self::group_idx(rhs) as isize;

            // Compare group indices
            match lhs_idx.cmp(&rhs_idx) {
                eq => match lhs {
                    Command::Line(lhs) => {
                        let rhs = rhs.as_line().unwrap();

                        // Compare line widths
                        lhs.width.partial_cmp(&rhs.width).unwrap_or(eq)
                    }
                    Command::Mesh(lhs) => {
                        let rhs = rhs.as_mesh().unwrap();
                        let lhs_idx = Self::mesh_group_idx(lhs.mesh);
                        let rhs_idx = Self::mesh_group_idx(rhs.mesh);

                        // Compare mesh group indices
                        match lhs_idx.cmp(&rhs_idx) {
                            eq => {
                                for (lhs_tex, rhs_tex) in
                                    lhs.mesh.textures().zip(rhs.mesh.textures())
                                {
                                    let lhs_idx = self.mesh_texture_idx(lhs_tex);
                                    let rhs_idx = self.mesh_texture_idx(rhs_tex);

                                    // Compare mesh texture indices
                                    match lhs_idx.cmp(&rhs_idx) {
                                        eq => continue,
                                        ne => return ne,
                                    }
                                }

                                // Compare z-order (sorting in closer to further)
                                lhs.camera_z.partial_cmp(&rhs.camera_z).unwrap_or(eq)
                            }
                            ne => ne,
                        }
                    }
                    _ => eq,
                },
                ne => ne,
            }
        });
    }
}

/// Evenly numbered because we use `SearchIdx` to quickly locate these groups while filling the cache.
#[derive(Clone, Copy)]
enum GroupIdx {
    Mesh = 0,
    Sunlight = 2,
    PointLight = 4,
    RectLight = 6,
    Spotlight = 8,
    Line = 10,
}

struct Lru<T> {
    key: T,
    offset: u64,
    recently_used: bool, // TODO: Should this hold a number instead?
}

#[derive(Default)]
pub struct MeshSets {
    pub dual_tex: usize,
    pub single_tex: usize,
    pub trans: usize,
}

/// These oddly numbered indices are the spaces in between the `GroupIdx` values. This was more efficient than
/// finding the actual group index because we would have to walk to the front and back of each group after any
/// binary search in order to find the whole group.
#[derive(Clone, Copy)]
enum SearchIdx {
    PointLight = 3,
    RectLight = 5,
    Spotlight = 7,
    Line = 9,
}

bitflags! {
    pub struct Stages: usize {
        const LINE = Self::bit(0);
        const MESH_ANIMATED = Self::bit(1);
        const MESH_DUAL_TEX = Self::bit(2);
        const MESH_SINGLE_TEX = Self::bit(3);
        const MESH_TRANSPARENT = Self::bit(4);
        const POINTLIGHT = Self::bit(5);
        const RECTLIGHT = Self::bit(6);
        const SPOTLIGHT = Self::bit(7);
        const SUNLIGHT = Self::bit(8);
    }
}

impl Stages {
    /// Returns a usize with the given zero-indexed bit set to one
    const fn bit(b: usize) -> usize {
        1 << b
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::{camera::Perspective, math::vec3},
    };

    #[test]
    fn test_no_commands() {
        let camera = {
            let eye = vec3(-10.0, 0.0, 0.0);
            let target = vec3(10.0, 0.0, 0.0);
            let width = 320.0;
            let height = 200.0;
            let fov = 45.0;
            let near = 1.0;
            let far = 100.0;
            Perspective::new_view(eye, target, near..far, fov, (width, height))
        };
        let mut compiler = Compiler::default();
        let mut cmds: Vec<Command> = vec![];
        // let res = compiler.compile(&camera, &mut cmds);

        // assert!(res.stages_required().is_empty());
        // assert_eq!(res.mesh_sets_required().dual_tex, 0);
        // assert_eq!(res.mesh_sets_required().single_tex, 0);
        // assert_eq!(res.mesh_sets_required().trans, 0);
    }
}