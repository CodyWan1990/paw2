use std::cell::{Cell, RefCell};

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, BlitFrame, Frame, FrameContext, Offscreen, Renderer, Texture, TextureFilter,
            element::{
                AsRenderElements, Element, Id, Kind, RenderElement,
                solid::{SolidColorBuffer, SolidColorRenderElement},
                surface::WaylandSurfaceRenderElement,
                utils::{CropRenderElement, RelocateRenderElement, RescaleRenderElement},
            },
            gles::{
                GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture, Uniform,
                UniformName, UniformType,
            },
            utils::{CommitCounter, DamageSet},
        },
    },
    desktop::space::{
        ConstrainBehavior, ConstrainReference, SpaceRenderElements, constrain_space_element,
        space_render_elements,
    },
    output::{Output, OutputNoMode},
    reexports::wayland_server::Resource,
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

smithay::backend::renderer::element::render_elements! {
    pub PawRenderElements<=GlesRenderer>;
    Space=SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
    Blur=BlurElement,
    Cursor=WaylandSurfaceRenderElement<GlesRenderer>,
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
    ring: [SolidColorBuffer; 4],
    blur_programs: Option<BlurPrograms>,
    blur_commit: CommitCounter,
    appearance: AppearanceConfig,
}

impl Effects {
    pub fn new(renderer: &mut GlesRenderer, appearance: AppearanceConfig) -> Self {
        let blur_programs = (appearance.blur_radius > 0)
            .then(|| compile_blur(renderer))
            .flatten();
        let ring =
            std::array::from_fn(|_| SolidColorBuffer::new((0, 0), appearance.focus_ring_color));
        Self {
            ring,
            blur_programs,
            blur_commit: CommitCounter::default(),
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
            if self.blur_programs.is_none() && appearance.blur_radius > 0 {
                self.blur_programs = compile_blur(renderer);
            }
            self.appearance = appearance;
            if self.blur_programs.is_some() {
                state.enable_background_effects();
            }
        }
        let scale = Scale::from(output.current_scale().fractional_scale());
        if let Some(window) = state.fullscreen_window(output).cloned() {
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
            let output_size = state.output_geometry(output).unwrap().size;
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

        state.blur_surfaces.retain(Resource::is_alive);
        let space = state.space_for_output(output).unwrap();
        let space_elements = space_render_elements(renderer, [space], output, 1.0)?;
        let output_bounds = Rectangle::from_size(
            output
                .current_transform()
                .transform_size(output.current_mode().unwrap().size),
        );
        let mut elements = Vec::with_capacity(space_elements.len() + 4);
        elements.extend(
            self.focus_ring(state, output, scale)
                .map(PawRenderElements::Solid),
        );

        for element in space_elements {
            let blur = self.blur_elements(state, output, &element, scale, output_bounds);
            elements.push(PawRenderElements::Space(element));
            elements.extend(blur.into_iter().map(PawRenderElements::Blur));
        }
        Ok(elements)
    }

    fn blur_elements(
        &self,
        state: &Paw2,
        output: &Output,
        element: &SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
        scale: Scale<f64>,
        output_bounds: Rectangle<i32, Physical>,
    ) -> Vec<BlurElement> {
        if self.appearance.blur_radius == 0 {
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
                let Some(window_geometry) = state.window_geometry_for_element(output, element.id())
                else {
                    return Vec::new();
                };
                let window_geometry = window_geometry.to_f64().to_physical(scale).to_i32_round();
                uncovered_rectangles(
                    Rectangle::from_size(geometry.size),
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

    fn focus_ring(
        &mut self,
        state: &Paw2,
        output: &Output,
        scale: Scale<f64>,
    ) -> impl Iterator<Item = SolidColorRenderElement> + '_ {
        let width = self.appearance.focus_ring_width;
        let geometry = (!state.has_exclusive_keyboard_layer())
            .then(|| state.focused_window_geometry(output))
            .flatten()
            .zip(state.output_geometry(output))
            .map(|(mut window, output)| {
                window.loc -= output.loc;
                window
            });
        let rectangles = geometry
            .filter(|_| width > 0)
            .map(|geometry| ring_rectangles(geometry, width))
            .unwrap_or_default();

        let color = self.appearance.focus_ring_color;
        self.ring
            .iter_mut()
            .zip(rectangles)
            .map(move |(buffer, rectangle)| {
                buffer.update(rectangle.size, color);
                SolidColorRenderElement::from_buffer(
                    buffer,
                    rectangle.loc.to_physical_precise_round(scale),
                    scale,
                    1.0,
                    Kind::Unspecified,
                )
            })
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

fn uncovered_rectangles(
    bounds: Rectangle<i32, Physical>,
    opaque: impl IntoIterator<Item = Rectangle<i32, Physical>>,
) -> Vec<Rectangle<i32, Physical>> {
    Rectangle::subtract_rects_many_in_place(vec![bounds], opaque)
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

fn ring_rectangles(geometry: Rectangle<i32, Logical>, width: i32) -> [Rectangle<i32, Logical>; 4] {
    let outer = Rectangle::new(
        (geometry.loc.x - width, geometry.loc.y - width).into(),
        geometry.size + (width * 2, width * 2).into(),
    );
    [
        Rectangle::new(outer.loc, (outer.size.w, width).into()),
        Rectangle::new(
            (outer.loc.x, outer.loc.y + outer.size.h - width).into(),
            (outer.size.w, width).into(),
        ),
        Rectangle::new(
            (outer.loc.x, outer.loc.y + width).into(),
            (width, outer.size.h - width * 2).into(),
        ),
        Rectangle::new(
            (outer.loc.x + outer.size.w - width, outer.loc.y + width).into(),
            (width, outer.size.h - width * 2).into(),
        ),
    ]
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
    fn automatic_blur_uses_only_uncovered_regions() {
        let bounds = Rectangle::from_size((20, 20).into());
        assert_eq!(uncovered_rectangles(bounds, []), [bounds]);
        assert!(uncovered_rectangles(bounds, [bounds]).is_empty());

        let opaque = Rectangle::new((0, 0).into(), (10, 20).into());
        let uncovered = uncovered_rectangles(bounds, [opaque]);
        assert_eq!(uncovered, [Rectangle::new((10, 0).into(), (10, 20).into())]);
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
    fn focus_ring_stays_outside_the_window() {
        let window = Rectangle::new((10, 10).into(), (100, 80).into());
        assert!(
            ring_rectangles(window, 3)
                .into_iter()
                .all(|rectangle| !rectangle.overlaps(window))
        );
    }
}
