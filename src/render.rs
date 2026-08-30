use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
};

use fontdue::{
    Font, FontSettings,
    layout::{CoordinateSystem, HorizontalAlign, Layout, LayoutSettings, TextStyle, VerticalAlign},
};

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, BlitFrame, Frame, FrameContext, Offscreen, Renderer, Texture, TextureFilter,
            element::{
                AsRenderElements, Element, Id, Kind, RenderElement,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                solid::SolidColorRenderElement,
                surface::WaylandSurfaceRenderElement,
                utils::{
                    ConstrainAlign, ConstrainScaleBehavior, CropRenderElement, Relocate,
                    RelocateRenderElement, RescaleRenderElement,
                },
            },
            gles::{
                GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, GlesTexProgram, GlesTexture,
                Uniform, UniformName, UniformType, element::PixelShaderElement,
            },
            utils::{CommitCounter, DamageSet, OpaqueRegions},
        },
    },
    desktop::{
        layer_map_for_output,
        space::{
            ConstrainBehavior, ConstrainReference, SpaceRenderElements, constrain_space_element,
            space_render_elements,
        },
    },
    output::{Output, OutputNoMode},
    utils::{
        Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform, user_data::UserDataMap,
    },
    wayland::{
        background_effect::BackgroundEffectSurfaceCachedState,
        compositor::{RectangleKind, RegionAttributes, with_states},
        shell::wlr_layer::{KeyboardInteractivity, Layer},
    },
};

use crate::{
    config::AppearanceConfig,
    layout::{WindowRenderState, is_exclusive_keyboard_layer, overview_screen_rect},
    state::Paw2,
};

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

/// Painted behind each workspace card so a desktop with no wallpaper client
/// still shows a card rather than a hole onto the backdrop.
const OVERVIEW_CARD: [f32; 4] = [0.08, 0.08, 0.1, 1.0];
/// The card shadow, as fractions of a card's height. Keeping them relative is
/// what makes the shadow shrink with the card instead of swamping it.
const OVERVIEW_SHADOW_SPREAD: f64 = 0.03;
const OVERVIEW_SHADOW_OFFSET: f64 = 0.012;
const OVERVIEW_SHADOW_COLOR: [f32; 4] = [0.0, 0.0, 0.0, 0.45];
/// Logical pixels, held constant across zoom levels so the selection stays
/// legible however far the workspaces are shrunk.
const OVERVIEW_RING_WIDTH: i32 = 3;

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

/// Signed distance from a point to a rounded rectangle centred on the origin.
///
/// The window, the focus ring and the shadow all have to trace the *same*
/// curve or they will not line up, so it is written once and spliced into each
/// source by [`with_rounded_box`] - the same shape as Smithay's own
/// `//_DEFINES_` placeholder above.
const ROUNDED_BOX: &str = "\
float rounded_box(vec2 point, vec2 half_size, float radius) {
    vec2 q = abs(point) - half_size + radius;
    return min(max(q.x, q.y), 0.0) + length(max(q, 0.0)) - radius;
}
";

/// Splice [`ROUNDED_BOX`] into a shader that asked for it.
fn with_rounded_box(source: &str) -> String {
    let spliced = source.replace("//_ROUNDED_BOX_", ROUNDED_BOX);
    debug_assert!(
        spliced.contains("float rounded_box"),
        "placeholder not found"
    );
    spliced
}

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

//_ROUNDED_BOX_

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

//_ROUNDED_BOX_

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

/// Drawn behind a window: the same rounded rectangle, blurred outwards and
/// shifted by the configured offset. The element covers the window grown by
/// `spread` on every side and moved by that offset, so the shadow's own shape
/// sits dead centre and the window it belongs to sits `window_offset` away.
/// Logical pixels throughout, like the ring.
const SHADOW_SHADER: &str = r#"
precision highp float;

uniform vec2 size;
uniform float alpha;
uniform float corner_radius;
uniform float spread;
uniform vec4 shadow_color;
uniform vec2 window_offset;
varying vec2 v_coords;

//_ROUNDED_BOX_

void main() {
    vec2 half_size = max(size * 0.5 - spread, vec2(0.0, 0.0));
    float radius = min(corner_radius, min(half_size.x, half_size.y));
    vec2 point = v_coords * size - size * 0.5;

    // Half the falloff falls inside the shape and half outside, which is what
    // makes the edge read as blurred rather than as a bigger rectangle.
    float coverage = 1.0 - smoothstep(-spread, spread, rounded_box(point, half_size, radius));
    // Punch the window back out. Windows are usually translucent here, and a
    // shadow left underneath one would darken the window itself.
    coverage *= smoothstep(-1.0, 0.0, rounded_box(point + window_offset, half_size, radius));

    gl_FragColor = shadow_color * coverage * alpha;
}
"#;

/// Space elements as this compositor builds them.
type SpaceElements = SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>;

smithay::backend::renderer::element::render_elements! {
    pub PawRenderElements<=GlesRenderer>;
    Space=SpaceElements,
    Window=WindowShaderElement,
    Blur=BlurElement,
    Shader=PixelShaderElement,
    Cursor=WaylandSurfaceRenderElement<GlesRenderer>,
    Memory=MemoryRenderBufferRenderElement<GlesRenderer>,
    Constrained=ConstrainedWindowElement,
    Solid=SolidColorRenderElement,
}

type ConstrainedWindowElement = CropRenderElement<
    RelocateRenderElement<RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>>,
>;

/// Shrinks output-relative surface elements into a workspace card: scaled about
/// the output origin, moved onto the card, then clipped to `crop`. One uniform
/// transform for wallpaper and windows alike, which is why the preview keeps the
/// proportions of the real desktop.
/// Every render element a surface tree contributes, at full opacity.
///
/// The turbofish this saves - naming both the renderer and the element type -
/// was written out at five call sites, and is the same every time.
fn surface_elements<T>(
    source: &T,
    renderer: &mut GlesRenderer,
    location: Point<i32, Physical>,
    scale: Scale<f64>,
) -> Vec<WaylandSurfaceRenderElement<GlesRenderer>>
where
    T: AsRenderElements<GlesRenderer>,
    WaylandSurfaceRenderElement<GlesRenderer>: From<T::RenderElement>,
{
    source.render_elements(renderer, location, scale, 1.0)
}

fn zoomed(
    elements: impl IntoIterator<Item = WaylandSurfaceRenderElement<GlesRenderer>>,
    zoom: f64,
    card: Point<i32, Physical>,
    crop: Rectangle<i32, Physical>,
    scale: Scale<f64>,
) -> impl Iterator<Item = ConstrainedWindowElement> {
    elements.into_iter().filter_map(move |element| {
        let scaled = RescaleRenderElement::from_element(element, Point::default(), zoom);
        let moved = RelocateRenderElement::from_element(scaled, card, Relocate::Relative);
        CropRenderElement::from_element(moved, scale, crop)
    })
}

fn tab_buffer(
    font: &Font,
    size: Size<i32, Physical>,
    scale: f32,
    titles: &[String],
    active: usize,
    focus_color: [f32; 4],
) -> MemoryRenderBuffer {
    let width = size.w as usize;
    let height = size.h as usize;
    let mut pixels = vec![0_u8; width * height * 4];
    let font_size = 13.0 * scale;
    let padding = (8.0 * scale).round() as usize;
    for (index, title) in titles.iter().enumerate() {
        let start = index * width / titles.len();
        let end = (index + 1) * width / titles.len();
        let color = if index == active {
            focus_color.map(|channel| (channel.clamp(0.0, 1.0) * 255.0).round() as u8)
        } else {
            [31, 31, 36, 255]
        };
        for y in 0..height {
            for x in start..end {
                let pixel = (y * width + x) * 4;
                pixels[pixel..pixel + 4].copy_from_slice(&[color[0], color[1], color[2], 255]);
            }
        }

        let available = end.saturating_sub(start + padding * 2) as f32;
        let title = fitted_tab_title(font, title, font_size, available);
        let mut layout = Layout::new(CoordinateSystem::PositiveYDown);
        layout.reset(&LayoutSettings {
            x: (start + padding) as f32,
            y: 0.0,
            max_width: Some(available),
            max_height: Some(height as f32),
            horizontal_align: HorizontalAlign::Center,
            vertical_align: VerticalAlign::Middle,
            ..LayoutSettings::default()
        });
        layout.append(&[font], &TextStyle::new(&title, font_size, 0));
        for glyph in layout.glyphs() {
            let (_, coverage) = font.rasterize_config(glyph.key);
            for glyph_y in 0..glyph.height {
                for glyph_x in 0..glyph.width {
                    let x = glyph.x.round() as i32 + glyph_x as i32;
                    let y = glyph.y.round() as i32 + glyph_y as i32;
                    if x < start as i32 || x >= end as i32 || y < 0 || y >= size.h {
                        continue;
                    }
                    let alpha = coverage[glyph_y * glyph.width + glyph_x] as u16;
                    let pixel = (y as usize * width + x as usize) * 4;
                    for channel in &mut pixels[pixel..pixel + 3] {
                        *channel = (((*channel as u16) * (255 - alpha) + 255 * alpha) / 255) as u8;
                    }
                }
            }
        }
    }
    MemoryRenderBuffer::from_slice(
        &pixels,
        Fourcc::Argb8888,
        (size.w, size.h),
        1,
        Transform::Normal,
        None,
    )
}

fn fitted_tab_title(font: &Font, title: &str, size: f32, available: f32) -> String {
    let clean = title
        .chars()
        .map(|character| {
            if character.is_control() {
                '�'
            } else {
                character
            }
        })
        .collect::<String>();
    let width = |text: &str| {
        text.chars()
            .map(|character| font.metrics(character, size).advance_width)
            .sum::<f32>()
    };
    if width(&clean) <= available {
        return clean;
    }
    let ellipsis = '…';
    let limit = (available - font.metrics(ellipsis, size).advance_width).max(0.0);
    let mut fitted = String::new();
    let mut used = 0.0;
    for character in clean.chars() {
        let advance = font.metrics(character, size).advance_width;
        if used + advance > limit {
            break;
        }
        fitted.push(character);
        used += advance;
    }
    fitted.push(ellipsis);
    fitted
}

/// What the output is wiped to before anything is drawn: black behind a
/// fullscreen window, the configured backdrop behind the overview's cards, and
/// otherwise the near-black that shows through gaps.
pub fn clear_color(state: &Paw2, output: &Output) -> [f32; 4] {
    if state.renders_fullscreen(output) {
        [0.0, 0.0, 0.0, 1.0]
    } else if state.overview_visible() {
        state.config.overview.backdrop_color
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
    shadow_program: Option<GlesPixelProgram>,
    /// One cached shadow per window per output, kept for the same reason the
    /// ring is: a fresh `PixelShaderElement` takes a fresh id every frame, and
    /// the damage tracker would then repaint it forever.
    shadows: HashMap<(String, Id), PixelShaderElement>,
    blur_programs: Option<BlurPrograms>,
    blur_commit: CommitCounter,
    window_program: Option<GlesTexProgram>,
    /// Bumped on every appearance change so window elements take a fresh id and
    /// the damage tracker repaints them, even when nothing has moved.
    generation: usize,
    appearance: AppearanceConfig,
    /// One stable id per workspace card, kept for the same reason the ring is:
    /// a fresh id every frame would damage the card forever.
    overview_cards: [Id; crate::layout::WORKSPACE_COUNT],
    /// Cached alongside the card height and ring size they were built for.
    /// `update_uniforms` damages unconditionally, so rebuilding these every
    /// frame would keep the compositor repainting for as long as the overview
    /// is open.
    overview_shadows: [Option<(i32, PixelShaderElement)>; crate::layout::WORKSPACE_COUNT],
    overview_ring: Option<((i32, i32), PixelShaderElement)>,
    /// The scratchpad panel's border needs its own slot: sharing one with
    /// `overview_ring` would make the two evict each other every frame.
    panel_ring: Option<((i32, i32), PixelShaderElement)>,
    tab_font: Font,
    tab_buffers: HashMap<(String, usize), TabBuffer>,
}

struct TabBuffer {
    size: Size<i32, Physical>,
    scale: u64,
    active: usize,
    titles: Vec<String>,
    generation: usize,
    buffer: MemoryRenderBuffer,
}

impl Effects {
    pub fn new(renderer: &mut GlesRenderer, appearance: AppearanceConfig) -> Self {
        let blur_programs = appearance
            .blur_enabled()
            .then(|| compile_blur(renderer))
            .flatten();
        // Both of these are compiled up front rather than on demand: the
        // overview needs them for its selection ring and its card shadows even
        // when the desktop's own ring and shadows are switched off.
        let ring_program = compile_ring(renderer);
        let shadow_program = compile_shadow(renderer);
        let window_program = window_effects(&appearance)
            .then(|| compile_window(renderer))
            .flatten();
        Self {
            ring: None,
            ring_program,
            shadow_program,
            shadows: HashMap::new(),
            blur_programs,
            blur_commit: CommitCounter::default(),
            window_program,
            generation: 0,
            appearance,
            overview_cards: std::array::from_fn(|_| Id::new()),
            overview_shadows: std::array::from_fn(|_| None),
            overview_ring: None,
            panel_ring: None,
            tab_font: Font::from_bytes(
                include_bytes!("../assets/NotoSans-Regular.ttf") as &[u8],
                FontSettings::default(),
            )
            .expect("bundled Noto Sans must be a valid font"),
            tab_buffers: HashMap::new(),
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
            if self.shadow_program.is_none() && appearance.shadow_enabled() {
                self.shadow_program = compile_shadow(renderer);
            }
            if appearance.shadow_enabled() {
                for shadow in self.shadows.values_mut() {
                    shadow.update_uniforms(shadow_uniforms(&appearance));
                }
            } else {
                self.shadows.clear();
            }
            self.generation = self.generation.wrapping_add(1);
            self.overview_ring = None;
            self.panel_ring = None;
            self.overview_shadows = std::array::from_fn(|_| None);
            if let Some(ring) = &mut self.ring {
                ring.update_uniforms(ring_uniforms(&appearance));
            }
            self.appearance = appearance;
            if self.blur_programs.is_some() {
                state.enable_background_effects();
            }
        }
        state.advance_animations();
        // Gated on visibility, not on input ownership: the closing animation
        // still has to be drawn after the overview has given focus back.
        if state.overview_visible() {
            return self.overview_elements(renderer, state, output);
        }
        let scale = Scale::from(output.current_scale().fractional_scale());
        if state.renders_fullscreen(output)
            && let Some(window) = state.fullscreen_window(output).cloned()
        {
            // A suspended output is unmapped from its space but still owns the
            // fullscreen window, so its geometry can be gone.
            let Some(output_size) = state.output_geometry(output).map(|geometry| geometry.size)
            else {
                return Ok(Vec::new());
            };
            let layers = layer_map_for_output(output);
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
                    surface_elements(layer, renderer, location, scale)
                })
                .map(PawRenderElements::Cursor)
                .collect::<Vec<_>>();
            elements.extend(
                state
                    .x11_override_windows(output)
                    .into_iter()
                    .flat_map(|(window, location)| {
                        surface_elements(
                            &window,
                            renderer,
                            location.to_physical_precise_round(scale),
                            scale,
                        )
                    })
                    .map(PawRenderElements::Cursor),
            );
            elements.extend(
                constrain_space_element::<_, _, ConstrainedWindowElement, _, _>(
                    renderer,
                    &window,
                    (0, 0),
                    1.0,
                    scale,
                    Rectangle::from_size(output_size),
                    ConstrainBehavior {
                        reference: ConstrainReference::Geometry,
                        behavior: ConstrainScaleBehavior::Fit,
                        align: ConstrainAlign::CENTER,
                    },
                )
                .map(PawRenderElements::Constrained),
            );
            // The scratchpad strip reads this same layer map to find the usable
            // area, and smithay's guard is a mutex: holding it across the call
            // would deadlock.
            drop(layers);
            return self.with_scratchpad_overview(renderer, state, output, elements);
        }

        let Some(space) = state.space_for_output(output) else {
            return Ok(Vec::new());
        };
        let space_elements = space_render_elements(renderer, [space], output, 1.0)?;
        let output_bounds = Rectangle::from_size(
            output
                .current_transform()
                .transform_size(output.current_mode().ok_or(OutputNoMode)?.size),
        );
        let mut elements = Vec::with_capacity(space_elements.len() + 4);
        let mut focus_ring = self.focus_ring(state, output);
        let mut tab_elements = self.tab_elements(renderer, state, output, scale);

        let output_name = output.name();
        let mut shadowed = Vec::new();
        for element in space_elements {
            // Resolved once per element: the lookup walks every window on every
            // workspace, and all three consumers below want the same answer.
            let window_state = state.window_render_state(output, element.id());
            if state.scratchpad_overview_visible_on(output)
                && window_state.is_some_and(|window| window.scratchpad)
            {
                continue;
            }
            let blur = self.blur_elements(state, &element, scale, output_bounds, window_state);
            let shadow = self.shadow_element(&output_name, element.id(), window_state);
            if shadow.is_some() {
                shadowed.push(element.id().clone());
            }
            let focused = focus_ring
                .as_ref()
                .is_some_and(|(_, id)| id == element.id());
            let tab = tab_elements
                .iter()
                .position(|(root, _)| root == element.id())
                .map(|index| tab_elements.remove(index).1);
            elements.push(self.window_element(element, scale, window_state));
            elements.extend(tab.map(PawRenderElements::Memory));
            if focused {
                elements.extend(
                    focus_ring
                        .take()
                        .map(|(ring, _)| PawRenderElements::Shader(ring)),
                );
            }
            elements.extend(blur.into_iter().map(PawRenderElements::Blur));
            // Last, so it draws first and everything else lands on top of it.
            elements.extend(shadow.map(PawRenderElements::Shader));
        }
        // Windows on other outputs keep their shadows; this one's are gone as
        // soon as the window is.
        // ponytail: linear lookup, bounded by the windows on one workspace.
        self.shadows
            .retain(|(name, id), _| *name != output_name || shadowed.contains(id));
        self.with_scratchpad_overview(renderer, state, output, elements)
    }

    fn with_scratchpad_overview(
        &mut self,
        renderer: &mut GlesRenderer,
        state: &Paw2,
        output: &Output,
        mut desktop: Vec<PawRenderElements>,
    ) -> Result<Vec<PawRenderElements>, OutputNoMode> {
        if !state.scratchpad_overview_visible_on(output) {
            return Ok(desktop);
        }
        let mut overview = self.scratchpad_overview_elements(renderer, state, output)?;
        overview.append(&mut desktop);
        Ok(overview)
    }

    fn tab_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        state: &Paw2,
        output: &Output,
        scale: Scale<f64>,
    ) -> Vec<(Id, MemoryRenderBufferRenderElement<GlesRenderer>)> {
        let output_name = output.name();
        let bars = state.tab_bars(output);
        let active_ids = bars.iter().map(|bar| bar.id).collect::<Vec<_>>();
        self.tab_buffers
            .retain(|(name, id), _| *name != output_name || active_ids.contains(id));
        bars.into_iter()
            .filter_map(|bar| {
                let size = bar.geometry.size.to_f64().to_physical(scale).to_i32_round();
                if size.w <= 0 || size.h <= 0 {
                    return None;
                }
                let key = (output_name.clone(), bar.id);
                let scale_bits = scale.x.to_bits();
                let cached = self.tab_buffers.get(&key).is_some_and(|cached| {
                    cached.size == size
                        && cached.scale == scale_bits
                        && cached.active == bar.active
                        && cached.titles == bar.titles
                        && cached.generation == self.generation
                });
                if !cached {
                    let buffer = tab_buffer(
                        &self.tab_font,
                        size,
                        scale.x as f32,
                        &bar.titles,
                        bar.active,
                        self.appearance.focus_ring_color,
                    );
                    self.tab_buffers.insert(
                        key.clone(),
                        TabBuffer {
                            size,
                            scale: scale_bits,
                            active: bar.active,
                            titles: bar.titles,
                            generation: self.generation,
                            buffer,
                        },
                    );
                }
                let buffer = &self.tab_buffers.get(&key)?.buffer;
                let position = bar.geometry.loc.to_f64().to_physical(scale);
                MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    position,
                    buffer,
                    None,
                    None,
                    None,
                    Kind::Unspecified,
                )
                .ok()
                .map(|element| (bar.root, element))
            })
            .collect()
    }

    fn scratchpad_overview_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        state: &Paw2,
        output: &Output,
    ) -> Result<Vec<PawRenderElements>, OutputNoMode> {
        let scale = Scale::from(output.current_scale().fractional_scale());
        let Some(strip) = state.scratchpad_strip(output) else {
            return Ok(Vec::new());
        };
        let mut elements = Vec::new();
        for preview in strip.previews {
            if preview.selected {
                elements.extend(
                    self.overview_ring(preview.card, 1.0)
                        .map(PawRenderElements::Shader),
                );
            }
            elements.extend(
                constrain_space_element::<_, _, ConstrainedWindowElement, _, _>(
                    renderer,
                    &preview.window,
                    preview.card.loc,
                    1.0,
                    scale,
                    preview.card,
                    ConstrainBehavior {
                        reference: ConstrainReference::BoundingBox,
                        behavior: ConstrainScaleBehavior::Fit,
                        align: ConstrainAlign::CENTER,
                    },
                )
                .map(PawRenderElements::Constrained),
            );
        }
        // Border first: earlier elements draw on top, and the ring band overlaps
        // the panel it outlines.
        elements.extend(self.panel_ring(strip.panel).map(PawRenderElements::Shader));
        elements.push(PawRenderElements::Solid(SolidColorRenderElement::new(
            self.overview_cards[0].clone(),
            strip.panel.to_f64().to_physical(scale).to_i32_round(),
            CommitCounter::default(),
            OVERVIEW_CARD,
            Kind::Unspecified,
        )));
        Ok(elements)
    }

    /// The overview: every visible workspace drawn as a shrunken copy of the
    /// real output - its wallpaper, its windows, its layout - floating on the
    /// backdrop. There is one transform per card and everything inside goes
    /// through it, which is what keeps the previews honest.
    fn overview_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        state: &Paw2,
        output: &Output,
    ) -> Result<Vec<PawRenderElements>, OutputNoMode> {
        let scale = Scale::from(output.current_scale().fractional_scale());
        let Some(output_size) = state.output_geometry(output).map(|geometry| geometry.size) else {
            return Ok(Vec::new());
        };

        // Top and overlay stay at full size above the overview; background and
        // bottom shrink with the workspaces, so every card carries the
        // wallpaper the way the real desktop does.
        let layers = layer_map_for_output(output);
        let (above, below): (Vec<_>, Vec<_>) = layers
            .layers()
            .rev()
            .filter_map(|surface| {
                layers
                    .layer_geometry(surface)
                    .map(|geometry| (geometry.loc, surface.clone()))
            })
            .partition(|(_, surface)| matches!(surface.layer(), Layer::Top | Layer::Overlay));
        drop(layers);

        let mut elements = above
            .iter()
            .flat_map(|(location, surface)| {
                surface_elements(
                    surface,
                    renderer,
                    location.to_physical_precise_round(scale),
                    scale,
                )
            })
            .map(PawRenderElements::Cursor)
            .collect::<Vec<_>>();

        let mut shadowed = Vec::new();
        let mut shadows = Vec::new();
        for workspace in state.overview_workspaces(output) {
            let card = workspace.card.to_f64().to_physical(scale).to_i32_round();
            // Windows may scroll out past their card's sides, exactly as they do
            // on the real desktop; the band stops them bleeding into the card
            // above or below.
            let band = Rectangle::new(
                (0, card.loc.y).into(),
                (
                    ((f64::from(output_size.w) * scale.x).round() as i32).max(1),
                    card.size.h,
                )
                    .into(),
            );

            if let Some(selected) = workspace.windows.iter().find(|preview| preview.selected) {
                let ring = self.overview_ring(
                    overview_screen_rect(selected.geometry, workspace.card, workspace.zoom),
                    workspace.zoom,
                );
                elements.extend(ring.map(PawRenderElements::Shader));
            }

            for preview in &workspace.windows {
                // A window's surface can reach past its geometry - client-side
                // shadows do exactly that - so it renders from the bounding box
                // origin, the same correction `Space` makes when it maps one.
                let origin = preview.geometry.loc - preview.window.geometry().loc;
                elements.extend(
                    zoomed(
                        surface_elements(
                            &preview.window,
                            renderer,
                            origin.to_physical_precise_round(scale),
                            scale,
                        ),
                        workspace.zoom,
                        card.loc,
                        band,
                        scale,
                    )
                    .map(PawRenderElements::Constrained),
                );
            }

            for (location, surface) in &below {
                elements.extend(
                    zoomed(
                        surface_elements(
                            surface,
                            renderer,
                            location.to_physical_precise_round(scale),
                            scale,
                        ),
                        workspace.zoom,
                        card.loc,
                        card,
                        scale,
                    )
                    .map(PawRenderElements::Constrained),
                );
            }

            elements.push(PawRenderElements::Solid(SolidColorRenderElement::new(
                self.overview_cards[workspace.workspace].clone(),
                card,
                CommitCounter::default(),
                OVERVIEW_CARD,
                Kind::Unspecified,
            )));
            // Collected rather than pushed: a shadow reaches past its own card,
            // and drawing them all last keeps one card's shadow from landing on
            // top of the next card's wallpaper.
            shadows.extend(
                self.overview_shadow(workspace.workspace, workspace.card)
                    .map(PawRenderElements::Shader),
            );
            shadowed.push(workspace.workspace);
        }
        // ponytail: linear lookup, bounded by the workspaces one output shows.
        for (workspace, shadow) in self.overview_shadows.iter_mut().enumerate() {
            if !shadowed.contains(&workspace) {
                *shadow = None;
            }
        }
        elements.append(&mut shadows);
        Ok(elements)
    }

    /// The ring around the selected preview. Unlike the desktop's focus ring
    /// this keeps a fixed width and ignores `focus_ring_width` entirely: in the
    /// overview the ring is the cursor, not decoration, and a ring scaled down
    /// with the card comes out a single pixel wide and disappears.
    fn overview_ring(
        &mut self,
        window: Rectangle<i32, Logical>,
        zoom: f64,
    ) -> Option<PixelShaderElement> {
        let radius = (f64::from(self.appearance.corner_radius) * zoom).round() as i32;
        let program = self.ring_program.clone()?;
        let color = self.appearance.focus_ring_color;
        Some(cached_ring(
            program,
            &mut self.overview_ring,
            window,
            OVERVIEW_RING_WIDTH,
            radius,
            color,
        ))
    }

    /// The border around the scratchpad panel, the same one the focused window
    /// wears.
    ///
    /// ponytail: square corners, because the panel behind it is a plain solid
    /// and a rounded ring would leave a nub poking out at each corner. Give the
    /// panel the window shader if it ever needs real rounding.
    fn panel_ring(&mut self, panel: Rectangle<i32, Logical>) -> Option<PixelShaderElement> {
        let width = self.appearance.focus_ring_width;
        if width <= 0 {
            return None;
        }
        let program = self.ring_program.clone()?;
        let color = self.appearance.focus_ring_color;
        Some(cached_ring(
            program,
            &mut self.panel_ring,
            panel,
            width,
            0,
            color,
        ))
    }

    /// The shadow that lifts one workspace card off the backdrop. Its spread is
    /// a fraction of the card, so it shrinks with the card during the animation
    /// instead of staying window-sized and swamping it.
    fn overview_shadow(
        &mut self,
        workspace: usize,
        card: Rectangle<i32, Logical>,
    ) -> Option<PixelShaderElement> {
        let program = self.shadow_program.clone()?;
        let spread = ((f64::from(card.size.h) * OVERVIEW_SHADOW_SPREAD).round() as i32).max(1);
        let offset = (f64::from(card.size.h) * OVERVIEW_SHADOW_OFFSET).round() as i32;
        let area = Rectangle::new(
            (card.loc.x - spread, card.loc.y - spread + offset).into(),
            card.size + (spread * 2, spread * 2).into(),
        );
        let shadow = match &mut self.overview_shadows[workspace] {
            Some((cached, shadow)) if *cached == card.size.h => shadow,
            slot => {
                let shadow = PixelShaderElement::new(
                    program,
                    area,
                    None,
                    1.0,
                    vec![
                        Uniform::new("corner_radius", 0.0f32),
                        Uniform::new("spread", spread as f32),
                        Uniform::new("shadow_color", OVERVIEW_SHADOW_COLOR),
                        Uniform::new("window_offset", [0.0f32, offset as f32]),
                    ],
                    Kind::Unspecified,
                );
                &mut slot.insert((card.size.h, shadow)).1
            }
        };
        shadow.resize(area, None);
        Some(shadow.clone())
    }

    /// Wraps a window in the corner/opacity shader. Anything that is not a
    /// window - layer surfaces, the cursor - and any window with no effect to
    /// apply is passed through untouched, so the opaque fast path survives.
    fn window_element(
        &self,
        element: SpaceElements,
        scale: Scale<f64>,
        window_state: Option<WindowRenderState>,
    ) -> PawRenderElements {
        let radius = self.appearance.corner_radius as f64 * scale.x;
        if !matches!(element, SpaceElements::Element(_)) || !window_effects(&self.appearance) {
            return PawRenderElements::Space(element);
        }
        let Some(program) = self.window_program.clone() else {
            return PawRenderElements::Space(element);
        };
        let Some(WindowRenderState {
            client: window,
            opacity,
            ..
        }) = window_state
        else {
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
        element: &SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
        scale: Scale<f64>,
        output_bounds: Rectangle<i32, Physical>,
        window_state: Option<WindowRenderState>,
    ) -> Vec<BlurElement> {
        if !self.appearance.blur_enabled() {
            return Vec::new();
        }
        let Some(programs) = &self.blur_programs else {
            return Vec::new();
        };
        let geometry = element.geometry(scale);
        let rectangles: Vec<Rectangle<i32, Physical>> =
            if let Some(region) = requested_blur_region(state, element.id()) {
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
                let Some(WindowRenderState {
                    client: window_geometry,
                    ..
                }) = window_state
                else {
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

    fn focus_ring(&mut self, state: &Paw2, output: &Output) -> Option<(PixelShaderElement, Id)> {
        let appearance = self.appearance;
        let width = appearance.focus_ring_width;
        if width <= 0 || state.has_exclusive_keyboard_layer() {
            return None;
        }
        let program = self.ring_program.clone()?;
        let (mut window, root) = state.focused_window_render_state(output)?;
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
        Some((ring.clone(), root))
    }

    /// The shadow for one window, or `None` for anything that is not a window's
    /// root surface. Uniforms depend only on the appearance config, so the
    /// cached element only ever has to be moved.
    fn shadow_element(
        &mut self,
        output_name: &str,
        element: &Id,
        window_state: Option<WindowRenderState>,
    ) -> Option<PixelShaderElement> {
        let appearance = self.appearance;
        if !appearance.shadow_enabled() {
            return None;
        }
        let program = self.shadow_program.clone()?;
        let window = window_state?.frame;
        let area = shadow_area(window, &appearance);
        let shadow = self
            .shadows
            .entry((output_name.to_owned(), element.clone()))
            .or_insert_with(|| {
                PixelShaderElement::new(
                    program,
                    area,
                    None,
                    1.0,
                    shadow_uniforms(&appearance),
                    Kind::Unspecified,
                )
            });
        shadow.resize(area, None);
        Some(shadow.clone())
    }
}

fn layer_visible_over_fullscreen(layer: Layer, interactivity: KeyboardInteractivity) -> bool {
    layer == Layer::Overlay || is_exclusive_keyboard_layer(layer, interactivity)
}

fn compile_shadow(renderer: &mut GlesRenderer) -> Option<GlesPixelProgram> {
    let uniforms = [
        UniformName::new("corner_radius", UniformType::_1f),
        UniformName::new("spread", UniformType::_1f),
        UniformName::new("shadow_color", UniformType::_4f),
        UniformName::new("window_offset", UniformType::_2f),
    ];
    renderer
        .compile_custom_pixel_shader(with_rounded_box(SHADOW_SHADER), &uniforms)
        .map_err(|error: GlesError| tracing::warn!(?error, "window shadow shader unavailable"))
        .ok()
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
        .compile_custom_texture_shader(with_rounded_box(WINDOW_SHADER), &uniforms)
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

/// The ring shader drawn around `window`, reusing `slot` whenever the size it
/// was built for still holds. A fresh `PixelShaderElement` carries a fresh id,
/// which damages forever and stops the compositor ever idling, so every ring
/// goes through here rather than being rebuilt per frame.
fn cached_ring(
    program: GlesPixelProgram,
    slot: &mut Option<((i32, i32), PixelShaderElement)>,
    window: Rectangle<i32, Logical>,
    width: i32,
    radius: i32,
    color: [f32; 4],
) -> PixelShaderElement {
    let area = ring_area(window, width);
    let ring = match slot {
        Some((cached, ring)) if *cached == (width, radius) => ring,
        slot => {
            let ring = PixelShaderElement::new(
                program,
                area,
                None,
                1.0,
                vec![
                    Uniform::new("corner_radius", radius as f32),
                    Uniform::new("ring_width", width as f32),
                    Uniform::new("ring_color", color),
                ],
                Kind::Unspecified,
            );
            &mut slot.insert(((width, radius), ring)).1
        }
    };
    ring.resize(area, None);
    ring.clone()
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

/// The window rectangle grown by the shadow radius on every side, then moved by
/// the shadow offset. Everything the shader needs about where the window sits
/// follows from that, which is why the offset is not applied twice.
fn shadow_area(
    window: Rectangle<i32, Logical>,
    appearance: &AppearanceConfig,
) -> Rectangle<i32, Logical> {
    let radius = appearance.shadow_radius;
    Rectangle::new(
        (
            window.loc.x - radius + appearance.shadow_offset_x,
            window.loc.y - radius + appearance.shadow_offset_y,
        )
            .into(),
        window.size + (radius * 2, radius * 2).into(),
    )
}

fn shadow_uniforms(appearance: &AppearanceConfig) -> Vec<Uniform<'static>> {
    vec![
        Uniform::new("corner_radius", appearance.corner_radius as f32),
        Uniform::new("spread", appearance.shadow_radius as f32),
        Uniform::new("shadow_color", appearance.shadow_color),
        Uniform::new(
            "window_offset",
            [
                appearance.shadow_offset_x as f32,
                appearance.shadow_offset_y as f32,
            ],
        ),
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
        .compile_custom_pixel_shader(with_rounded_box(RING_SHADER), &uniforms)
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
    fn every_rounded_shader_gets_the_same_curve() {
        for source in [WINDOW_SHADER, RING_SHADER, SHADOW_SHADER] {
            let spliced = with_rounded_box(source);
            assert!(
                !spliced.contains("//_ROUNDED_BOX_"),
                "placeholder survived splicing"
            );
            assert_eq!(spliced.matches("float rounded_box").count(), 1);
        }
        // `#version` has to stay the very first line of the texture shader.
        assert!(with_rounded_box(WINDOW_SHADER).starts_with("#version 100"));
    }

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
            shadow_radius: 0,
            shadow_color: [0.0, 0.0, 0.0, 0.0],
            shadow_offset_x: 0,
            shadow_offset_y: 0,
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
    fn a_shadow_needs_both_reach_and_colour() {
        let mut appearance = AppearanceConfig {
            gaps: 15,
            focus_ring_width: 1,
            focus_ring_color: [0.0, 0.0, 0.0, 1.0],
            corner_radius: 0,
            shadow_radius: 0,
            shadow_color: [0.0, 0.0, 0.0, 0.5],
            shadow_offset_x: 0,
            shadow_offset_y: 0,
            blur: false,
            blur_radius: 0,
            transparency: false,
            opacity: 1.0,
        };
        assert!(!appearance.shadow_enabled());

        appearance.shadow_radius = 12;
        assert!(appearance.shadow_enabled());

        // A radius with nothing to draw in it is off, not an invisible cost.
        appearance.shadow_color = [0.0, 0.0, 0.0, 0.0];
        assert!(!appearance.shadow_enabled());
    }

    #[test]
    fn the_shadow_area_centres_on_the_window_plus_the_offset() {
        let window = Rectangle::new((100, 100).into(), (200, 160).into());
        let mut appearance = AppearanceConfig {
            gaps: 15,
            focus_ring_width: 1,
            focus_ring_color: [0.0, 0.0, 0.0, 1.0],
            corner_radius: 8,
            shadow_radius: 12,
            shadow_color: [0.0, 0.0, 0.0, 0.45],
            shadow_offset_x: 0,
            shadow_offset_y: 0,
            blur: false,
            blur_radius: 0,
            transparency: false,
            opacity: 1.0,
        };

        // Without an offset the shadow is an even collar around the window,
        // which is what lets the shader treat its own centre as the window's.
        let area = shadow_area(window, &appearance);
        assert_eq!(area, Rectangle::new((88, 88).into(), (224, 184).into()));
        assert!(area.contains_rect(window));

        // With one, the area moves by exactly the offset and nothing else - the
        // shader derives the window's position from that same offset, so any
        // second application here would double it.
        appearance.shadow_offset_x = 3;
        appearance.shadow_offset_y = 4;
        let shifted = shadow_area(window, &appearance);
        assert_eq!(shifted.size, area.size);
        assert_eq!(shifted.loc - area.loc, (3, 4).into());
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

    #[test]
    fn tab_titles_are_ellipsized_to_their_segment() {
        let font = Font::from_bytes(
            include_bytes!("../assets/NotoSans-Regular.ttf") as &[u8],
            FontSettings::default(),
        )
        .unwrap();
        assert_eq!(fitted_tab_title(&font, "Paw2", 13.0, 200.0), "Paw2");
        let fitted = fitted_tab_title(&font, "A very long window title", 13.0, 35.0);
        assert!(fitted.ends_with('…'));
        assert!(fitted.len() < "A very long window title".len());
    }
}
