//#![allow(unused)]
// There's some kind of compiler bug going on, causing a crazy amount of false
// positives atm :(
#![allow(dead_code)]

use egui::ViewportId;
use egui_wgpu::RendererOptions;
use std::num::NonZeroU32;
use std::sync::Arc;
use tracing::{error, trace};
use winit::event_loop::{ActiveEventLoop, EventLoop};

use winit::event_loop::ControlFlow;

use egui_wgpu::winit::Painter;
use egui_winit::State;
use winit::event::Event::*;

#[cfg(target_os = "android")]
use android_activity::AndroidApp;

mod ble;
mod tokio_runtime;
mod ui;

const INITIAL_WIDTH: u32 = 1920;
const INITIAL_HEIGHT: u32 = 1080;

const COMPANION_CHOOSER_REQUEST_CODE: u32 = 1002;

/// Enable egui to request redraws via a custom Winit event...
#[derive(Clone)]
struct RepaintSignal(
    std::sync::Arc<std::sync::Mutex<winit::event_loop::EventLoopProxy<ui::Event>>>,
);

struct Window {
    window: Arc<winit::window::Window>,
    state: egui_winit::State,
}

fn create_window(
    event_loop: &ActiveEventLoop, ctx: egui::Context, painter: &mut Painter,
) -> Option<Window> {
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
        error!("Failed to associate new Window with Painter: {err:?}");
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

fn _main(event_loop: winit::event_loop::EventLoop<ui::Event>, config: ble::BleServiceConfig) {
    let ctx = egui::Context::default();
    let repaint_signal = RepaintSignal(std::sync::Arc::new(std::sync::Mutex::new(
        event_loop.create_proxy(),
    )));
    ctx.set_request_repaint_callback(move |_info| {
        trace!("Request Repaint Callback");
        repaint_signal
            .0
            .lock()
            .unwrap()
            .send_event(ui::Event::RequestRedraw)
            .ok();
    });

    let mut painter = pollster::block_on(Painter::new(
        ctx.clone(),
        egui_wgpu::WgpuConfiguration::default(),
        false, // don't require transparent backbuffer
        RendererOptions::default(),
    ));

    let mut window: Option<Window> = None;
    let mut egui_demo_windows = egui_demo_lib::DemoWindows::default();

    let mut ui_state = ui::State::default();

    if cfg!(target_os = "android") || std::env::var("BLUEY_UI_MOBILE").is_ok() {
        ui_state.is_mobile = true;
    }

    let bluetooth_event_proxy = event_loop.create_proxy();
    //let (tx, rx) = std::sync::mpsc::channel::<ble::BleRequest>();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ble::BleRequest>();
    let _background_service = ble::BleService::new(bluetooth_event_proxy, rx, config);

    #[allow(deprecated)]
    event_loop
        .run(move |event, event_loop| {
            event_loop.set_control_flow(ControlFlow::Wait);

            trace!("handling winit event: {event:?}");

            match (&mut window, event) {
                (None, Resumed) => {
                    window = create_window(event_loop, ctx.clone(), &mut painter);
                }
                (Some(ref mut w), Resumed) => {
                    pollster::block_on(
                        painter.set_window(egui::ViewportId::ROOT, Some(w.window.clone())),
                    )
                    .unwrap_or_else(|err| {
                        error!("Failed to associate Window with Painter: {err:?}");
                    });
                    w.window.request_redraw();
                }
                (_, Suspended) => {
                    window = None;
                    pollster::block_on(painter.set_window(egui::ViewportId::ROOT, None))
                        .unwrap_or_else(|err| {
                            error!("Failed to disassociate Window from Painter: {err:?}");
                        });
                }
                (_, UserEvent(ui::Event::RequestRedraw)) => {
                    if let Some(window) = window.as_ref() {
                        trace!("Winit request redraw, user event");
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
                    trace!("Window Event: {event:?}");

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
                        trace!("Window Event was not consumed by egui");
                        match event {
                            winit::event::WindowEvent::RedrawRequested => {
                                trace!("Handling RedrawRequested Event");
                                let raw_input = window.state.take_egui_input(&window.window);
                                let full_output = ctx.run(raw_input, |ctx| {
                                    //egui_demo_windows.ui(ctx);
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
                    } else {
                        trace!("Window Event was consumed by egui");
                    }
                }
                _ => {}
            }
        })
        .unwrap();
}

fn _tokio_main(event_loop: winit::event_loop::EventLoop<ui::Event>, config: ble::BleServiceConfig) {
    // We create a tokio runtime manually because on Android we need to hook into
    // thread spawning to attach them to the JVM.
    let runtime = tokio_runtime::build_tokio_runtime().unwrap();
    runtime.block_on(async { _main(event_loop, config) })
}

const DEFAULT_ENV_FILTER: &str = "trace,wgpu_hal=info,wgpu_core=info,winit=info,naga=info";

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: AndroidApp) {
    use std::sync::OnceLock;
    use winit::platform::android::EventLoopBuilderExtAndroid;

    std::env::set_var("RUST_BACKTRACE", "full");
    std::env::set_var("WGPU_BACKEND", "vulkan");

    // NB: android_main can be called multiple times if the application Activity
    // is destroyed and recreated so we use a OnceLock to ensure that we only
    // initialize our global state once (otherwise tracing_subscriber will panic
    // if we try to initialize it multiple times).
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        use tracing_subscriber::prelude::*;

        let filter_layer = tracing_subscriber::EnvFilter::new(DEFAULT_ENV_FILTER);
        let android_layer = paranoid_android::layer(env!("CARGO_PKG_NAME"))
            .with_ansi(false)
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
            .with_thread_names(true);
        tracing_subscriber::registry()
            .with(filter_layer)
            .with(android_layer)
            .init();
    });

    // Initialize jni::JavaVM::singleton()
    // SAFETY: We know that the AndroidApp JVM pointer is valid as long as we are running
    let _jvm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) };

    std::env::set_var("RUST_BACKTRACE", "full");

    let event_loop = EventLoop::with_user_event()
        .with_android_app(app.clone())
        .build()
        .unwrap();

    let mut config = ble::BleServiceConfig::android_new(app.clone());
    config.companion_chooser_request_code = Some(COMPANION_CHOOSER_REQUEST_CODE);

    _tokio_main(event_loop, config);
}

// Stop rust-analyzer from complaining that this file doesn't have a main() function...
#[cfg(target_os = "android")]
#[cfg(allow_unused)]
fn main() {}

#[cfg(not(target_os = "android"))]
fn main() {
    if !std::option_env!("RUST_LOG").is_some() {
        std::env::set_var("RUST_LOG", DEFAULT_ENV_FILTER);
    }
    tracing_subscriber::fmt::init();

    let event_loop = EventLoop::with_user_event().build().unwrap();
    let config = ble::BleServiceConfig::new();
    _tokio_main(event_loop, config);
}
