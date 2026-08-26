use smithay::{
    backend::{
        renderer::damage::OutputDamageTracker,
        winit::{self, WinitEvent},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::EventLoop,
        wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
    },
    utils::{Rectangle, Transform},
    wayland::presentation::Refresh,
};

use crate::{
    render::{Effects, clear_color},
    state::Paw2,
};

pub fn init(
    event_loop: &mut EventLoop<Paw2>,
    state: &mut Paw2,
) -> Result<(), Box<dyn std::error::Error>> {
    let (mut backend, source) = winit::init()?;
    let mode = Mode {
        size: backend.window_size(),
        refresh: 60_000,
    };
    let output = Output::new(
        "paw2".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Paw2".into(),
            model: "Nested".into(),
            serial_number: "Unknown".into(),
        },
    );

    output.create_global::<Paw2>(&state.display_handle);
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    state.output_added(&output, (0, 0).into());

    let mut damage_tracker = OutputDamageTracker::from_output(&output);
    let mut effects = {
        let (renderer, _framebuffer) = backend.bind()?;
        Effects::new(renderer, state.config.appearance)
    };
    if effects.blur_available() {
        state.enable_background_effects();
    }

    let mut sequence = 0;
    event_loop
        .handle()
        .insert_source(source, move |event, _, state| match event {
            WinitEvent::Resized { size, .. } => {
                output.change_current_state(
                    Some(Mode {
                        size,
                        refresh: 60_000,
                    }),
                    None,
                    None,
                    None,
                );
                smithay::desktop::layer_map_for_output(&output).arrange();
                state.relayout(true);
            }
            WinitEvent::Input(event) => state.process_input(event),
            WinitEvent::Redraw => {
                let frame_duration = std::time::Duration::from_secs_f64(1.0 / 60.0);
                let frame_target = state.clock.now() + frame_duration;
                state.pre_repaint(&output, frame_target);
                let damage = Rectangle::from_size(backend.window_size());
                // Skip the frame rather than the session. The DRM path already
                // warns and returns on a failed frame; this one used to panic,
                // which turns one bad frame into a lost nested session.
                let feedback = (|| {
                    let (renderer, mut framebuffer) = backend
                        .bind()
                        .map_err(|error| tracing::warn!(?error, "failed to bind the window"))
                        .ok()?;
                    let elements = effects
                        .output_elements(renderer, state, &output)
                        .map_err(|error| {
                            tracing::warn!(?error, "failed to collect render elements")
                        })
                        .ok()?;
                    let result = damage_tracker
                        .render_output(
                            renderer,
                            &mut framebuffer,
                            0,
                            &elements,
                            clear_color(state, &output),
                        )
                        .map_err(|error| tracing::warn!(?error, "failed to render frame"))
                        .ok()?;
                    Some(state.take_presentation_feedback(&output, &result.states))
                })();
                if let Err(error) = backend.submit(Some(&[damage])) {
                    tracing::warn!(?error, "failed to submit frame");
                }
                sequence += 1;
                if let Some(mut feedback) = feedback {
                    feedback.presented(
                        state.clock.now(),
                        Refresh::fixed(frame_duration),
                        sequence,
                        wp_presentation_feedback::Kind::Vsync,
                    );
                }

                state.send_frames(&output);
                state.finish_frame();
                backend.window().request_redraw();
            }
            WinitEvent::CloseRequested => state.begin_shutdown(),
            _ => {}
        })?;

    Ok(())
}
