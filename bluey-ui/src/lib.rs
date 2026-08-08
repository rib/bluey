//#![allow(unused)]
// There's some kind of compiler bug going on, causing a crazy amount of false
// positives atm :(
#![allow(dead_code)]

use std::num::NonZeroU32;
use std::sync::Arc;
use egui::ViewportId;
use winit::event_loop::{ActiveEventLoop, EventLoop};

use winit::{
    event_loop::{ControlFlow},
};

use egui_wgpu::winit::Painter;
use egui_winit::State;
use winit::event::Event::*;

#[cfg(target_os="android")]
use android_activity::AndroidApp;

mod ble;
mod ui;
mod tokio_runtime;

const INITIAL_WIDTH: u32 = 1920;
const INITIAL_HEIGHT: u32 = 1080;

// Needs to match whatever is used in
const COMPANION_CHOOSER_REQUEST_CODE: u32 = 0;


/// Enable egui to request redraws via a custom Winit event...
#[derive(Clone)]
struct RepaintSignal(std::sync::Arc<std::sync::Mutex<winit::event_loop::EventLoopProxy<ui::Event>>>);

struct Window {
    window: Arc<winit::window::Window>,
    state: egui_winit::State,
}

fn create_window(event_loop: &ActiveEventLoop, ctx: egui::Context, painter: &mut Painter) -> Option<Window> {
    let window_attributes = winit::window::Window::default_attributes()
        .with_decorations(true)
        .with_resizable(true)
        .with_transparent(false)
        .with_title("Bluey-UI")
        .with_inner_size(winit::dpi::PhysicalSize {
            width: INITIAL_WIDTH,
            height: INITIAL_HEIGHT,
        });

    let window = Arc::new(event_loop.create_window(window_attributes).unwrap());

    if let Err(err) =
        pollster::block_on(painter.set_window(egui::ViewportId::ROOT, Some(window.clone())))
    {
        log::error!("Failed to associate new Window with Painter: {err:?}");
        return None;
    }

    let native_pixels_per_point = Some(window.scale_factor() as f32);
    let mut state = State::new(
        ctx.clone(),
        ViewportId::ROOT,
        &window,
        native_pixels_per_point,
        None,
        None,
    );

    // NB: calling set_window will lazily initialize render state which
    // means we will be able to query the maximum supported texture
    // dimensions
    if let Some(max_size) = painter.max_texture_side() {
        state.set_max_texture_side(max_size);
    }

    window.request_redraw();

    Some(Window { window, state })
}


fn _main(event_loop: winit::event_loop::EventLoop<ui::Event>) {
    let ctx = egui::Context::default();
    let repaint_signal = RepaintSignal(std::sync::Arc::new(std::sync::Mutex::new(
        event_loop.create_proxy()
    )));
    ctx.set_request_repaint_callback(move |_info| {
        log::trace!("Request Repaint Callback");
        repaint_signal.0.lock().unwrap().send_event(ui::Event::RequestRedraw).ok();
    });

    let mut painter = pollster::block_on(Painter::new(
        ctx.clone(),
        egui_wgpu::WgpuConfiguration::default(),
        1, // msaa samples
        Some(wgpu::TextureFormat::Depth24Plus),
        false, // don't require transparent backbuffer
        false, // no dithering
    ));

    let mut window: Option<Window> = None;
    let mut ui_state = ui::State::default();

    let bluetooth_event_proxy = event_loop.create_proxy();
    //let (tx, rx) = std::sync::mpsc::channel::<ble::BleRequest>();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ble::BleRequest>();
    let _background_service = ble::BleService::new(bluetooth_event_proxy,
        rx,
        None);
        //Some(COMPANION_CHOOSER_REQUEST_CODE));


    #[allow(deprecated)]
    event_loop.run(move |event, event_loop| {
        event_loop.set_control_flow(ControlFlow::Wait);

        log::trace!("handling winit event");

        match (&mut window, event) {
            (None, Resumed) => {
                window = create_window(event_loop, ctx.clone(), &mut painter);
            }
            (Some(ref mut w), Resumed) => {
                pollster::block_on(painter.set_window(egui::ViewportId::ROOT, Some(w.window.clone())))
                    .unwrap_or_else(|err| {
                        log::error!("Failed to associate Window with Painter: {err:?}");
                    });
                w.window.request_redraw();
            }
            (_, Suspended) => {
                window = None;
            }
            (_, UserEvent(ui::Event::RequestRedraw)) => {
                if let Some(window) = window.as_ref() {
                    log::trace!("Winit request redraw, user event");
                    window.window.request_redraw();
                }
            }
            (_, UserEvent(user_event)) => {
                ui_state.handle_event(user_event, &tx);
            }
            (
                Some(window),
                WindowEvent {
                    window_id, event, ..
                },
            ) if window.window.id() == window_id => {
                log::trace!("Window Event: {event:?}");

                let response = window.state.on_window_event(&window.window, &event);
                // egui_winit probably shouldn't be returning repaint=true for RedrawRequested
                // events but in any case we special case RedrawRequested events here so we can
                // avoid creating an infinite repaint cycle.
                if !matches!(event, winit::event::WindowEvent::RedrawRequested)
                    && response.repaint
                {
                    window.window.request_redraw();
                }

                if !response.consumed {
                    match event {
                        winit::event::WindowEvent::RedrawRequested => {
                            let raw_input = window.state.take_egui_input(&window.window);
                            let full_output = ctx.run(raw_input, |ctx| {
                                ui_state.draw(ctx, &tx);
                            });
                            window.state.handle_platform_output(
                                &window.window,
                                full_output.platform_output,
                            );
                            painter.paint_and_update_textures(
                                ViewportId::ROOT,
                                full_output.pixels_per_point,
                                [0.0, 0.0, 0.0, 0.0],
                                &ctx.tessellate(
                                    full_output.shapes,
                                    full_output.pixels_per_point,
                                ),
                                &full_output.textures_delta,
                                vec![],
                            );
                        }
                        winit::event::WindowEvent::Resized(size) => {
                            let width = NonZeroU32::new(size.width).unwrap_or(NonZeroU32::MIN);
                            let height =
                                NonZeroU32::new(size.height).unwrap_or(NonZeroU32::MIN);
                            painter.on_window_resized(ViewportId::ROOT, width, height);
                        }
                        winit::event::WindowEvent::CloseRequested => {
                            event_loop.exit();
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }

    }).unwrap();
}

fn _tokio_main(event_loop: winit::event_loop::EventLoop<ui::Event>) {
    // We create a tokio runtime manually because on Android we need to hook into
    // thread spawning to attach them to the JVM.
    let runtime = tokio_runtime::build_tokio_runtime().unwrap();
    runtime.block_on(async {
        _main(event_loop)
    })
}

#[cfg(target_os="android")]
#[no_mangle]
fn android_main(app: AndroidApp) {
    use winit::platform::android::EventLoopBuilderExtAndroid;

    android_logger::init_once(
        android_logger::Config::default()
            .with_tag("blueyui")
            .with_min_level(log::Level::Debug)
            .with_filter(android_logger::FilterBuilder::new().parse("debug,naga=warn,wgpu=warn").build()
        )
    );

    let event_loop = EventLoop::with_user_event()
        .with_android_app(app)
        .build()
        .unwrap();
    _tokio_main(event_loop);
}
// Stop rust-analyzer from complaining that this file doesn't have a main() function...
#[cfg(target_os="android")]
#[cfg(allow_unused)]
fn main() {}

#[cfg(not(target_os="android"))]
fn main() {
    env_logger::builder().filter_level(log::LevelFilter::Debug) // Default Log Level
        .filter(Some("naga"), log::LevelFilter::Warn)
        .filter(Some("wgpu"), log::LevelFilter::Warn)
        .parse_default_env()
        .init();

    let event_loop = EventLoop::with_user_event().build().unwrap();
    _tokio_main(event_loop);
}