use std::cell::{Cell, RefCell};

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, BlitFrame, Frame, FrameContext, Offscreen, Renderer, Texture, TextureFilter,
            element::{
                AsRenderElements, Element, Id, Kind, RenderElement,
                memory::MemoryRenderBufferRenderElement,
                solid::SolidColorRenderElement,
                surface::WaylandSurfaceRenderElement,
                utils::{CropRenderElement, RelocateRenderElement, RescaleRenderElement},
            },
            gles::{
                GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, GlesTexProgram, GlesTexture,
                Uniform, UniformName, UniformType, element::PixelShaderElement,
            },
            utils::{CommitCounter, DamageSet, OpaqueRegions},
        },
    },
    desktop::space::{
        ConstrainBehavior, ConstrainReference, SpaceRenderElements, constrain_space_element,
        space_render_elements,
    },
    output::{Output, OutputNoMode},
    utils::{Buffer, Logical, Physical, Rectangle, Scale, Size, Transform, user_data::UserDataMap},
    wayland::{
        background_effect::BackgroundEffectSurfaceCachedState,
        compositor::{RectangleKind, RegionAttributes, with_states},
        shell::wlr_layer::{KeyboardInteractivity, Layer},
    },
};

use crate::{config::AppearanceConfig, layout::is_exclusive_keyboard_layer, state::Paw2};

const DOWNSAMPLE_SHADER: &str = r#"#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
uniform vec2 texel_size;
uniform float offset;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

void main() {
    vec2 step = texel_size * offset;
    vec4 color = texture2D(tex, v_coords) * 0.5;
    color += texture2D(tex, v_coords + vec2(-step.x, -step.y)) * 0.125;
    color += texture2D(tex, v_coords + vec2( step.x, -step.y)) * 0.125;
    color += texture2D(tex, v_coords + vec2(-step.x,  step.y)) * 0.125;
    color += texture2D(tex, v_coords + vec2( step.x,  step.y)) * 0.125;

#if defined(NO_ALPHA)
    color.a = 1.0;
#endif
    color *= alpha;

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
"#;

const UPSAMPLE_SHADER: &str = r#"#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
uniform vec2 texel_size;
uniform float offset;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

void main() {
    vec2 step = texel_size * offset;
    vec4 color = texture2D(tex, v_coords + vec2(-step.x, 0.0));
    color += texture2D(tex, v_coords + vec2( step.x, 0.0));
    color += texture2D(tex, v_coords + vec2(0.0, -step.y));
    color += texture2D(tex, v_coords + vec2(0.0,  step.y));
    color += texture2D(tex, v_coords + vec2(-step.x, -step.y)) * 2.0;
    color += texture2D(tex, v_coords + vec2( step.x, -step.y)) * 2.0;
    color += texture2D(tex, v_coords + vec2(-step.x,  step.y)) * 2.0;
    color += texture2D(tex, v_coords + vec2( step.x,  step.y)) * 2.0;
    color /= 12.0;

#if defined(NO_ALPHA)
    color.a = 1.0;
#endif
    color *= alpha;

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
"#;

const WINDOW_SHADER: &str = r#"#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
// All four are physical pixels. `v_coords` spans 0..1 across this element's
// destination rectangle, so `v_coords * elem_size + geo_offset` lands in
// window-local pixels even when the element is a sub-rectangle of the window.
uniform vec2 elem_size;
uniform vec2 geo_offset;
uniform vec2 geo_size;
uniform float corner_radius;
uniform float opacity;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

// Signed distance from a point to a rounded rectangle centred on the origin.
float rounded_box(vec2 point, vec2 half_size, float radius) {
    vec2 q = abs(point) - half_size + radius;
    return min(max(q.x, q.y), 0.0) + length(max(q, 0.0)) - radius;
}

void main() {
    vec4 color = texture2D(tex, v_coords);

#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0);
#endif

    if (corner_radius > 0.0) {
        vec2 half_size = geo_size * 0.5;
        vec2 point = v_coords * elem_size + geo_offset - half_size;
        float radius = min(corner_radius, min(half_size.x, half_size.y));
        // A pixel of falloff keeps the curve from stair-stepping.
        color *= 1.0 - smoothstep(-1.0, 0.0, rounded_box(point, half_size, radius));
    }

    color *= opacity * alpha;

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
"#;

/// Drawn over the window's rectangle grown by the ring width, so the outer
/// curve is the window's corner radius plus that width. `size` and `alpha` are
/// supplied by Smithay; `v_coords` spans 0..1 across the area, and `size` is
/// logical, so this works entirely in logical pixels.
const RING_SHADER: &str = r#"
precision highp float;

uniform vec2 size;
uniform float alpha;
uniform float corner_radius;
uniform float ring_width;
uniform vec4 ring_color;
varying vec2 v_coords;

float rounded_box(vec2 point, vec2 half_size, float radius) {
    vec2 q = abs(point) - half_size + radius;
    return min(max(q.x, q.y), 0.0) + length(max(q, 0.0)) - radius;
}

void main() {
    vec2 half_size = size * 0.5;
    vec2 point = v_coords * size - half_size;

    float outer_radius = min(corner_radius + ring_width, min(half_size.x, half_size.y));
    float outer = rounded_box(point, half_size, outer_radius);

    vec2 inner_half = max(half_size - ring_width, vec2(0.0, 0.0));
    float inner_radius = min(corner_radius, min(inner_half.x, inner_half.y));
    float inner = rounded_box(point, inner_half, inner_radius);

    // Inside the outer curve but outside the inner one: that band is the ring.
    float coverage = (1.0 - smoothstep(-1.0, 0.0, outer)) * smoothstep(-1.0, 0.0, inner);
    gl_FragColor = ring_color * coverage * alpha;
}
"#;

/// Space elements as this compositor builds them.
type SpaceElements = SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>;

smithay::backend::renderer::element::render_elements! {
    pub PawRenderElements<=GlesRenderer>;
    Space=SpaceElements,
    Window=WindowShaderElement,
    Blur=BlurElement,
    Ring=PixelShaderElement,
    Cursor=WaylandSurfaceRenderElement<GlesRenderer>,
    Memory=MemoryRenderBufferRenderElement<GlesRenderer>,
    Fullscreen=FullscreenElement,
    Solid=SolidColorRenderElement,
}

type FullscreenElement = CropRenderElement<
    RelocateRenderElement<RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>>,
>;

pub fn clear_color(fullscreen: bool) -> [f32; 4] {
    if fullscreen {
        [0.0, 0.0, 0.0, 1.0]
    } else {
        [0.04, 0.04, 0.05, 1.0]
    }
}

pub struct Effects {
    /// Kept between frames so its id and commit counter stay stable; rebuilding
    /// it every frame would damage the ring area forever and the compositor
    /// would never idle.
    ring: Option<PixelShaderElement>,
    ring_program: Option<GlesPixelProgram>,
    blur_programs: Option<BlurPrograms>,
    blur_commit: CommitCounter,
    window_program: Option<GlesTexProgram>,
    /// Bumped on every appearance change so window elements take a fresh id and
    /// the damage tracker repaints them, even when nothing has moved.
    generation: usize,
    appearance: AppearanceConfig,
}

impl Effects {
    pub fn new(renderer: &mut GlesRenderer, appearance: AppearanceConfig) -> Self {
        let blur_programs = appearance
            .blur_enabled()
            .then(|| compile_blur(renderer))
            .flatten();
        let ring_program = (appearance.focus_ring_width > 0)
            .then(|| compile_ring(renderer))
            .flatten();
        let window_program = window_effects(&appearance)
            .then(|| compile_window(renderer))
            .flatten();
        Self {
            ring: None,
            ring_program,
            blur_programs,
            blur_commit: CommitCounter::default(),
            window_program,
            generation: 0,
            appearance,
        }
    }

    pub fn blur_available(&self) -> bool {
        self.blur_programs.is_some()
    }

    pub fn output_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        state: &mut Paw2,
        output: &Output,
    ) -> Result<Vec<PawRenderElements>, OutputNoMode> {
        let appearance = state.config.appearance;
        if self.appearance != appearance {
            if self.appearance.blur_radius != appearance.blur_radius {
                self.blur_commit.increment();
            }
            if self.blur_programs.is_none() && appearance.blur_enabled() {
                self.blur_programs = compile_blur(renderer);
            }
            if self.window_program.is_none() && window_effects(&appearance) {
                self.window_program = compile_window(renderer);
            }
            if self.ring_program.is_none() && appearance.focus_ring_width > 0 {
                self.ring_program = compile_ring(renderer);
            }
            self.generation = self.generation.wrapping_add(1);
            if let Some(ring) = &mut self.ring {
                ring.update_uniforms(ring_uniforms(&appearance));
            }
            self.appearance = appearance;
            if self.blur_programs.is_some() {
                state.enable_background_effects();
            }
        }
        state.advance_animations();
        let scale = Scale::from(output.current_scale().fractional_scale());
        if let Some(window) = state.fullscreen_window(output).cloned() {
            // A suspended output is unmapped from its space but still owns the
            // fullscreen window, so its geometry can be gone.
            let Some(output_size) = state.output_geometry(output).map(|geometry| geometry.size)
            else {
                return Ok(Vec::new());
            };
            let layers = smithay::desktop::layer_map_for_output(output);
            let mut elements = layers
                .layers()
                .filter(|surface| {
                    let state = surface.cached_state();
                    layer_visible_over_fullscreen(state.layer, state.keyboard_interactivity)
                })
                .flat_map(|layer| {
                    let location = layers
                        .layer_geometry(layer)
                        .map(|geometry| geometry.loc)
                        .unwrap_or_default()
                        .to_physical_precise_round(scale);
                    AsRenderElements::<GlesRenderer>::render_elements::<
                        WaylandSurfaceRenderElement<GlesRenderer>,
                    >(layer, renderer, location, scale, 1.0)
                })
                .map(PawRenderElements::Cursor)
                .collect::<Vec<_>>();
            elements.extend(
                state
                    .x11_override_windows(output)
                    .into_iter()
                    .flat_map(|(window, location)| {
                        AsRenderElements::<GlesRenderer>::render_elements::<
                            WaylandSurfaceRenderElement<GlesRenderer>,
                        >(
                            &window,
                            renderer,
                            location.to_physical_precise_round(scale),
                            scale,
                            1.0,
                        )
                    })
                    .map(PawRenderElements::Cursor),
            );
            elements.extend(
                constrain_space_element::<_, _, FullscreenElement, _, _>(
                    renderer,
                    &window,
                    (0, 0),
                    1.0,
                    scale,
                    Rectangle::from_size(output_size),
                    ConstrainBehavior {
                        reference: ConstrainReference::Geometry,
                        behavior:
                            smithay::backend::renderer::element::utils::ConstrainScaleBehavior::Fit,
                        align: smithay::backend::renderer::element::utils::ConstrainAlign::CENTER,
                    },
                )
                .map(PawRenderElements::Fullscreen),
            );
            return Ok(elements);
        }

        let Some(space) = state.space_for_output(output) else {
            return Ok(Vec::new());
        };
        let space_elements = space_render_elements(renderer, [space], output, 1.0)?;
        let output_bounds = Rectangle::from_size(
            output
                .current_transform()
                .transform_size(output.current_mode().unwrap().size),
        );
        let mut elements = Vec::with_capacity(space_elements.len() + 4);
        elements.extend(self.focus_ring(state, output).map(PawRenderElements::Ring));

        for element in space_elements {
            let blur = self.blur_elements(state, output, &element, scale, output_bounds);
            elements.push(self.window_element(state, output, element, scale));
            elements.extend(blur.into_iter().map(PawRenderElements::Blur));
        }
        Ok(elements)
    }

    /// Wraps a window in the corner/opacity shader. Anything that is not a
    /// window - layer surfaces, the cursor - and any window with no effect to
    /// apply is passed through untouched, so the opaque fast path survives.
    fn window_element(
        &self,
        state: &Paw2,
        output: &Output,
        element: SpaceElements,
        scale: Scale<f64>,
    ) -> PawRenderElements {
        let radius = self.appearance.corner_radius as f64 * scale.x;
        if !matches!(element, SpaceElements::Element(_)) || !window_effects(&self.appearance) {
            return PawRenderElements::Space(element);
        }
        let Some(program) = self.window_program.clone() else {
            return PawRenderElements::Space(element);
        };
        let Some((window, opacity)) = state.window_render_state(output, element.id()) else {
            return PawRenderElements::Space(element);
        };
        let window = window.to_f64().to_physical(scale).to_i32_round();
        let (elem_size, geo_offset, geo_size) =
            window_shader_geometry(window, element.geometry(scale));
        PawRenderElements::Window(WindowShaderElement {
            id: element.id().namespaced(window_namespace(self.generation)),
            inner: element,
            program,
            uniforms: vec![
                Uniform::new("elem_size", elem_size),
                Uniform::new("geo_offset", geo_offset),
                Uniform::new("geo_size", geo_size),
                Uniform::new("corner_radius", radius as f32),
                Uniform::new("opacity", opacity),
            ],
            opaque_inset: (opacity >= 1.0).then_some((window, radius.ceil() as i32)),
        })
    }

    fn blur_elements(
        &self,
        state: &Paw2,
        output: &Output,
        element: &SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
        scale: Scale<f64>,
        output_bounds: Rectangle<i32, Physical>,
    ) -> Vec<BlurElement> {
        if !self.appearance.blur_enabled() {
            return Vec::new();
        }
        let Some(programs) = &self.blur_programs else {
            return Vec::new();
        };
        let geometry = element.geometry(scale);
        let rectangles: Vec<Rectangle<i32, Physical>> = if let Some(region) =
            requested_blur_region(state, element.id())
        {
            region_rectangles(
                &region,
                geometry.size.to_f64().to_logical(scale).to_i32_round(),
            )
            .into_iter()
            .map(|rectangle| {
                Rectangle::new(
                    geometry.loc + rectangle.loc.to_physical_precise_round(scale),
                    rectangle.size.to_physical_precise_round(scale),
                )
            })
            .collect()
        } else if matches!(element, SpaceRenderElements::Element(_)) {
            let Some((window_geometry, _)) = state.window_render_state(output, element.id()) else {
                return Vec::new();
            };
            let window_geometry = window_geometry.to_f64().to_physical(scale).to_i32_round();
            Rectangle::subtract_rects_many_in_place(
                vec![Rectangle::from_size(geometry.size)],
                element.opaque_regions(scale),
            )
            .into_iter()
            .map(|mut rectangle| {
                rectangle.loc += geometry.loc;
                rectangle
            })
            .filter_map(|rectangle| rectangle.intersection(window_geometry))
            .collect()
        } else {
            return Vec::new();
        };

        let radius = (self.appearance.blur_radius as f64 * scale.x).ceil() as i32;
        rectangles
            .into_iter()
            .enumerate()
            .filter_map(|(index, area)| {
                let area = area.intersection(output_bounds)?;
                Some(BlurElement {
                    id: element.id().namespaced(index),
                    commit: self.blur_commit,
                    capture: padded_rectangle(area, radius, output_bounds)?,
                    area,
                    radius: radius as u32,
                    programs: programs.clone(),
                })
            })
            .collect()
    }

    fn focus_ring(&mut self, state: &Paw2, output: &Output) -> Option<PixelShaderElement> {
        let appearance = self.appearance;
        let width = appearance.focus_ring_width;
        if width <= 0 || state.has_exclusive_keyboard_layer() {
            return None;
        }
        let program = self.ring_program.clone()?;
        let mut window = state.focused_window_geometry(output)?;
        window.loc -= state.output_geometry(output)?.loc;
        let area = ring_area(window, width);
        let ring = self.ring.get_or_insert_with(|| {
            PixelShaderElement::new(
                program,
                area,
                None,
                1.0,
                ring_uniforms(&appearance),
                Kind::Unspecified,
            )
        });
        // Only bumps the commit counter when the area actually moved.
        ring.resize(area, None);
        Some(ring.clone())
    }
}

fn layer_visible_over_fullscreen(layer: Layer, interactivity: KeyboardInteractivity) -> bool {
    layer == Layer::Overlay || is_exclusive_keyboard_layer(layer, interactivity)
}

#[derive(Debug, Clone)]
struct BlurPrograms {
    downsample: GlesTexProgram,
    upsample: GlesTexProgram,
}

/// True when a window needs the shader at all. Kept in one place so the "no
/// effects configured" path stays byte-identical to having no shader support.
fn window_effects(appearance: &AppearanceConfig) -> bool {
    appearance.corner_radius > 0 || appearance.window_opacity() < 1.0
}

/// Shader inputs placing `element` inside `window`; both are physical and
/// output-relative. Split out so the coordinate maths is testable without a GPU.
fn window_shader_geometry(
    window: Rectangle<i32, Physical>,
    element: Rectangle<i32, Physical>,
) -> ([f32; 2], [f32; 2], [f32; 2]) {
    let offset = element.loc - window.loc;
    (
        [element.size.w as f32, element.size.h as f32],
        [offset.x as f32, offset.y as f32],
        [window.size.w as f32, window.size.h as f32],
    )
}

fn compile_window(renderer: &mut GlesRenderer) -> Option<GlesTexProgram> {
    let uniforms = [
        UniformName::new("elem_size", UniformType::_2f),
        UniformName::new("geo_offset", UniformType::_2f),
        UniformName::new("geo_size", UniformType::_2f),
        UniformName::new("corner_radius", UniformType::_1f),
        UniformName::new("opacity", UniformType::_1f),
    ];
    renderer
        .compile_custom_texture_shader(WINDOW_SHADER, &uniforms)
        .map_err(|error: GlesError| {
            tracing::warn!(?error, "rounded corners and window opacity unavailable")
        })
        .ok()
}

/// A window drawn through the corner/opacity shader. Smithay honours a frame's
/// texture-program override for any element that does not bring its own
/// program, which is exactly what surface elements do.
#[derive(Debug)]
pub struct WindowShaderElement {
    id: Id,
    inner: SpaceElements,
    program: GlesTexProgram,
    uniforms: Vec<Uniform<'static>>,
    /// Window rectangle and corner inset, while the window is still opaque.
    opaque_inset: Option<(Rectangle<i32, Physical>, i32)>,
}

impl Element for WindowShaderElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn location(&self, scale: Scale<f64>) -> smithay::utils::Point<i32, Physical> {
        self.inner.location(scale)
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }

    fn transform(&self) -> Transform {
        self.inner.transform()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }

    /// Rounded corners cut holes in the window, so only the rectangle inside
    /// the corner arcs may still be treated as opaque. A translucent window
    /// reports nothing, which is also what makes blur show through it.
    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        let Some((window, inset)) = self.opaque_inset else {
            return OpaqueRegions::default();
        };
        let mut inner = Rectangle::new(
            (window.loc.x + inset, window.loc.y + inset).into(),
            (
                (window.size.w - inset * 2).max(0),
                (window.size.h - inset * 2).max(0),
            )
                .into(),
        );
        inner.loc -= self.geometry(scale).loc;
        self.inner
            .opaque_regions(scale)
            .iter()
            .filter_map(|region| region.intersection(inner))
            .collect()
    }
}

impl RenderElement<GlesRenderer> for WindowShaderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        frame.override_default_tex_program(self.program.clone(), self.uniforms.clone());
        let result = self
            .inner
            .draw(frame, src, dst, damage, opaque_regions, cache);
        // Must be cleared even when the inner draw failed: the override outlives
        // this call and would otherwise leak onto every later element.
        frame.clear_tex_program_override();
        result
    }
}

fn compile_blur(renderer: &mut GlesRenderer) -> Option<BlurPrograms> {
    let uniforms = [
        UniformName::new("texel_size", UniformType::_2f),
        UniformName::new("offset", UniformType::_1f),
    ];
    let result = (|| {
        Ok(BlurPrograms {
            downsample: renderer.compile_custom_texture_shader(DOWNSAMPLE_SHADER, &uniforms)?,
            upsample: renderer.compile_custom_texture_shader(UPSAMPLE_SHADER, &uniforms)?,
        })
    })();
    result
        .map_err(|error: GlesError| tracing::warn!(?error, "background blur unavailable"))
        .ok()
}

fn requested_blur_region(state: &Paw2, id: &Id) -> Option<RegionAttributes> {
    state.blur_surfaces.iter().find_map(|surface| {
        (Id::from_wayland_resource(surface) == *id).then(|| {
            with_states(surface, |states| {
                states
                    .cached_state
                    .get::<BackgroundEffectSurfaceCachedState>()
                    .current()
                    .blur_region
                    .clone()
            })
        })?
    })
}

fn region_rectangles(
    region: &RegionAttributes,
    surface_size: smithay::utils::Size<i32, Logical>,
) -> Vec<Rectangle<i32, Logical>> {
    let bounds = Rectangle::from_size(surface_size);
    let mut rectangles = Vec::new();
    for (kind, rectangle) in &region.rects {
        let Some(rectangle) = rectangle.intersection(bounds) else {
            continue;
        };
        match kind {
            RectangleKind::Add => {
                let additions = Rectangle::subtract_rects_many_in_place(
                    vec![rectangle],
                    rectangles.iter().copied(),
                );
                rectangles.extend(additions);
            }
            RectangleKind::Subtract => {
                rectangles = Rectangle::subtract_rects_many_in_place(rectangles, [rectangle]);
            }
        }
    }
    rectangles
}

fn padded_rectangle(
    area: Rectangle<i32, Physical>,
    padding: i32,
    bounds: Rectangle<i32, Physical>,
) -> Option<Rectangle<i32, Physical>> {
    Rectangle::new(
        (area.loc.x - padding, area.loc.y - padding).into(),
        (area.size.w + padding * 2, area.size.h + padding * 2).into(),
    )
    .intersection(bounds)
}

fn blur_parameters(radius: u32) -> (usize, f32) {
    if radius == 0 {
        return (0, 0.0);
    }
    let levels = radius.div_ceil(16).clamp(1, 4) as usize;
    let offset = 0.5 + 1.5 * radius.saturating_sub(1).min(63) as f32 / 63.0;
    (levels, offset)
}

fn pyramid_sizes(size: Size<i32, Buffer>, levels: usize) -> Vec<Size<i32, Buffer>> {
    let mut sizes = Vec::with_capacity(levels + 1);
    sizes.push(size);
    for _ in 0..levels {
        let previous = *sizes.last().unwrap();
        sizes.push(((previous.w / 2).max(1), (previous.h / 2).max(1)).into());
    }
    sizes
}

/// The window rectangle grown by the ring width on every side.
fn ring_area(window: Rectangle<i32, Logical>, width: i32) -> Rectangle<i32, Logical> {
    Rectangle::new(
        (window.loc.x - width, window.loc.y - width).into(),
        window.size + (width * 2, width * 2).into(),
    )
}

fn ring_uniforms(appearance: &AppearanceConfig) -> Vec<Uniform<'static>> {
    vec![
        Uniform::new("corner_radius", appearance.corner_radius as f32),
        Uniform::new("ring_width", appearance.focus_ring_width as f32),
        Uniform::new("ring_color", appearance.focus_ring_color),
    ]
}

/// Blur rectangles namespace their base id by rectangle index, so window
/// elements namespace down from the top of the range and can never collide.
fn window_namespace(generation: usize) -> usize {
    usize::MAX - generation
}

fn compile_ring(renderer: &mut GlesRenderer) -> Option<GlesPixelProgram> {
    let uniforms = [
        UniformName::new("corner_radius", UniformType::_1f),
        UniformName::new("ring_width", UniformType::_1f),
        UniformName::new("ring_color", UniformType::_4f),
    ];
    renderer
        .compile_custom_pixel_shader(RING_SHADER, &uniforms)
        .map_err(|error: GlesError| tracing::warn!(?error, "focus ring shader unavailable"))
        .ok()
}

#[derive(Debug, Clone)]
pub struct BlurElement {
    id: Id,
    commit: CommitCounter,
    capture: Rectangle<i32, Physical>,
    area: Rectangle<i32, Physical>,
    radius: u32,
    programs: BlurPrograms,
}

#[derive(Default)]
struct BlurCache {
    textures: RefCell<Vec<GlesTexture>>,
    failed: Cell<bool>,
}

impl Element for BlurElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size((self.capture.size.w as f64, self.capture.size.h as f64).into())
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.capture
    }

    fn damage_since(
        &self,
        _scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        if commit == Some(self.commit) {
            DamageSet::default()
        } else {
            DamageSet::from_slice(&[Rectangle::from_size(self.capture.size)])
        }
    }

    fn is_framebuffer_effect(&self) -> bool {
        true
    }
}

impl RenderElement<GlesRenderer> for BlurElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let Some(cache) = cache.and_then(|cache| cache.get::<BlurCache>()) else {
            return Ok(());
        };
        let textures = cache.textures.borrow();
        let Some(texture) = textures.first() else {
            return Ok(());
        };
        let local_area = Rectangle::new(self.area.loc - self.capture.loc, self.area.size);
        let clipped_damage: Vec<_> = damage
            .iter()
            .filter_map(|rectangle| rectangle.intersection(local_area))
            .map(|mut rectangle| {
                rectangle.loc -= local_area.loc;
                rectangle
            })
            .collect();
        if clipped_damage.is_empty() {
            return Ok(());
        }
        frame.render_texture_from_to(
            texture,
            Rectangle::new(
                (local_area.loc.x as f64, local_area.loc.y as f64).into(),
                (local_area.size.w as f64, local_area.size.h as f64).into(),
            ),
            Rectangle::new(dst.loc + local_area.loc, local_area.size),
            &clipped_damage,
            &[],
            Transform::Normal,
            1.0,
            None,
            &[],
        )
    }

    fn capture_framebuffer(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), GlesError> {
        let cache = cache.get_or_insert(BlurCache::default);
        if cache.failed.get() {
            return Ok(());
        }
        if let Err(error) = self.capture_and_blur(frame, dst, cache) {
            tracing::warn!(?error, "background blur disabled for a window");
            cache.textures.borrow_mut().clear();
            cache.failed.set(true);
        }
        Ok(())
    }
}

impl BlurElement {
    fn capture_and_blur(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        dst: Rectangle<i32, Physical>,
        cache: &BlurCache,
    ) -> Result<(), GlesError> {
        let (levels, offset) = blur_parameters(self.radius);
        let buffer_size: Size<i32, Buffer> = (dst.size.w, dst.size.h).into();
        let sizes = pyramid_sizes(buffer_size, levels);
        let mut textures = cache.textures.borrow_mut();

        if textures.len() != sizes.len()
            || textures
                .iter()
                .zip(&sizes)
                .any(|(texture, size)| texture.size() != *size)
        {
            let mut renderer = frame.renderer();
            let mut replacements = Vec::with_capacity(sizes.len());
            for size in &sizes {
                replacements.push(renderer.as_mut().create_buffer(Fourcc::Abgr8888, *size)?);
            }
            *textures = replacements;
        }

        let mut target = {
            let mut renderer = frame.renderer();
            renderer.as_mut().bind(&mut textures[0])?
        };
        let _sync = frame.blit_to(
            &mut target,
            dst,
            Rectangle::from_size(dst.size),
            TextureFilter::Linear,
        )?;
        drop(target);

        let mut guard = frame.renderer();
        let renderer = guard.as_mut();
        for level in 1..textures.len() {
            let (larger, smaller) = textures.split_at_mut(level);
            render_blur_pass(
                renderer,
                &larger[level - 1],
                &mut smaller[0],
                &self.programs.downsample,
                offset,
            )?;
        }
        for level in (1..textures.len()).rev() {
            let (larger, smaller) = textures.split_at_mut(level);
            render_blur_pass(
                renderer,
                &smaller[0],
                &mut larger[level - 1],
                &self.programs.upsample,
                offset,
            )?;
        }
        Ok(())
    }
}

fn render_blur_pass(
    renderer: &mut GlesRenderer,
    source: &GlesTexture,
    destination: &mut GlesTexture,
    program: &GlesTexProgram,
    offset: f32,
) -> Result<(), GlesError> {
    let source_size = source.size();
    let destination_size = destination.size();
    let output_size: Size<i32, Physical> = (destination_size.w, destination_size.h).into();
    let area = Rectangle::from_size(output_size);
    let mut target = renderer.bind(destination)?;
    let mut frame = renderer.render(&mut target, output_size, Transform::Normal)?;
    frame.clear([0.0, 0.0, 0.0, 0.0].into(), &[area])?;
    frame.render_texture_from_to(
        source,
        Rectangle::from_size(source_size.to_f64()),
        area,
        &[area],
        &[],
        Transform::Normal,
        1.0,
        Some(program),
        &[
            Uniform::new(
                "texel_size",
                (1.0 / source_size.w as f32, 1.0 / source_size.h as f32),
            ),
            Uniform::new("offset", offset),
        ],
    )?;
    let _sync = frame.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fullscreen_keeps_overlay_and_exclusive_launchers_visible() {
        assert!(layer_visible_over_fullscreen(
            Layer::Overlay,
            KeyboardInteractivity::None
        ));
        assert!(layer_visible_over_fullscreen(
            Layer::Top,
            KeyboardInteractivity::Exclusive
        ));
        assert!(!layer_visible_over_fullscreen(
            Layer::Top,
            KeyboardInteractivity::None
        ));
    }

    #[test]
    fn resolves_and_clips_blur_regions() {
        let region = RegionAttributes {
            rects: vec![
                (
                    RectangleKind::Add,
                    Rectangle::new((-10, 0).into(), (30, 20).into()),
                ),
                (
                    RectangleKind::Subtract,
                    Rectangle::new((5, 5).into(), (5, 5).into()),
                ),
            ],
        };
        let rectangles = region_rectangles(&region, (20, 20).into());
        assert!(
            rectangles.iter().all(|rectangle| {
                Rectangle::from_size((20, 20).into()).contains_rect(*rectangle)
            })
        );
        assert!(
            !rectangles
                .iter()
                .any(|rectangle| rectangle.contains((7, 7)))
        );
    }

    #[test]
    fn blur_strength_and_capture_are_bounded() {
        assert_eq!(blur_parameters(0), (0, 0.0));
        assert_eq!(blur_parameters(1).0, 1);
        assert_eq!(blur_parameters(16).0, 1);
        assert_eq!(blur_parameters(17).0, 2);
        assert_eq!(blur_parameters(64).0, 4);
        assert!(blur_parameters(32).1 < blur_parameters(64).1);

        let sizes = pyramid_sizes((64, 32).into(), 3);
        assert_eq!(
            sizes,
            [
                (64, 32).into(),
                (32, 16).into(),
                (16, 8).into(),
                (8, 4).into()
            ]
        );

        let area = Rectangle::new((10, 10).into(), (20, 20).into());
        let bounds = Rectangle::from_size((100, 100).into());
        assert_eq!(
            padded_rectangle(area, 8, bounds),
            Some(Rectangle::new((2, 2).into(), (36, 36).into()))
        );
        let edge = Rectangle::new((0, 0).into(), (20, 20).into());
        assert_eq!(
            padded_rectangle(edge, 8, bounds),
            Some(Rectangle::new((0, 0).into(), (28, 28).into()))
        );
        let offscreen = Rectangle::new((200, 0).into(), (20, 20).into());
        assert_eq!(padded_rectangle(offscreen, 8, bounds), None);

        let partial = Rectangle::new((-10, 10).into(), (20, 20).into());
        let partial = partial.intersection(bounds).unwrap();
        assert_eq!(
            padded_rectangle(partial, 8, bounds),
            Some(Rectangle::new((0, 2).into(), (18, 36).into()))
        );
    }

    #[test]
    fn window_shader_maps_elements_into_window_space() {
        let window = Rectangle::new((100, 50).into(), (800, 600).into());

        // An element covering the whole window sits at the origin of window space.
        let (size, offset, geo) = window_shader_geometry(window, window);
        assert_eq!(size, [800.0, 600.0]);
        assert_eq!(offset, [0.0, 0.0]);
        assert_eq!(geo, [800.0, 600.0]);

        // A subsurface inset inside the window is offset by that inset, so the
        // shader still measures corners from the window's edges, not its own.
        let subsurface = Rectangle::new((150, 90).into(), (200, 100).into());
        let (size, offset, geo) = window_shader_geometry(window, subsurface);
        assert_eq!(size, [200.0, 100.0]);
        assert_eq!(offset, [50.0, 40.0]);
        assert_eq!(geo, [800.0, 600.0]);

        // An element starting above/left of the window geometry offsets negative.
        let overhang = Rectangle::new((80, 40).into(), (900, 700).into());
        let (_, offset, _) = window_shader_geometry(window, overhang);
        assert_eq!(offset, [-20.0, -10.0]);
    }

    #[test]
    fn window_effects_engage_only_when_configured() {
        let mut appearance = AppearanceConfig {
            gaps: 15,
            focus_ring_width: 1,
            focus_ring_color: [0.0, 0.0, 0.0, 1.0],
            corner_radius: 0,
            blur: false,
            blur_radius: 0,
            transparency: false,
            opacity: 0.5,
        };
        // Transparency off pins opacity to 1.0, so nothing to do.
        assert!(!window_effects(&appearance));

        appearance.corner_radius = 8;
        assert!(window_effects(&appearance));

        appearance.corner_radius = 0;
        appearance.transparency = true;
        assert!(window_effects(&appearance));

        appearance.opacity = 1.0;
        assert!(!window_effects(&appearance));
    }

    #[test]
    fn focus_ring_surrounds_the_window() {
        let window = Rectangle::new((10, 10).into(), (100, 80).into());
        let area = ring_area(window, 3);
        assert_eq!(area, Rectangle::new((7, 7).into(), (106, 86).into()));
        // The shader punches the window back out of the middle, so the area has
        // to contain the window on every side for the band to be even.
        assert!(area.contains_rect(window));
        assert_eq!(window.loc.x - area.loc.x, 3);
        assert_eq!(
            (area.loc.x + area.size.w) - (window.loc.x + window.size.w),
            3
        );
    }
}
