//#![allow(unused)]
// There's some kind of compiler bug going on, causing a crazy amount of false
// positives atm :(
#![allow(dead_code)]

use log::{error, warn, info, debug, trace};

use std::thread;
use winit::event_loop::{EventLoopWindowTarget, EventLoopBuilder, EventLoopProxy};

use winit::{
    event_loop::{ControlFlow},
};

use egui_wgpu::winit::Painter;
use egui_winit::State;
//use egui_winit_platform::{Platform, PlatformDescriptor};
use winit::event::Event::*;

#[cfg(target_os="android")]
use jni::objects::JObject;

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


fn create_window<T>(event_loop: &EventLoopWindowTarget<T>, state: &mut State, painter: &mut Painter) -> winit::window::Window {
    let window = winit::window::WindowBuilder::new()
        .with_decorations(true)
        .with_resizable(true)
        .with_transparent(false)
        .with_title("Bluey-UI")
        .with_inner_size(winit::dpi::PhysicalSize {
            width: INITIAL_WIDTH,
            height: INITIAL_HEIGHT,
        })
            .build(&event_loop)
            .unwrap();

    unsafe { painter.set_window(Some(&window)) };

    // NB: calling set_window will lazily initialize render state which
    // means we will be able to query the maximum supported texture
    // dimensions
    if let Some(max_size) = painter.max_texture_side() {
        state.set_max_texture_side(max_size);
    }

    let pixels_per_point = window.scale_factor() as f32;
    state.set_pixels_per_point(pixels_per_point);

    window.request_redraw();

    window
}


fn _main() {
    let event_loop: winit::event_loop::EventLoop<ui::Event> = EventLoopBuilder::with_user_event().build();

    let ctx = egui::Context::default();
    let repaint_signal = RepaintSignal(std::sync::Arc::new(std::sync::Mutex::new(
        event_loop.create_proxy()
    )));
    ctx.set_request_repaint_callback(move || {
        repaint_signal.0.lock().unwrap().send_event(ui::Event::RequestRedraw).ok();
    });

    let mut winit_state = egui_winit::State::new(&event_loop);
    let mut painter = egui_wgpu::winit::Painter::new(
        wgpu::Backends::all(),
        wgpu::PowerPreference::LowPower,
        wgpu::DeviceDescriptor {
            label: None,
            features: wgpu::Features::default(),
            limits: wgpu::Limits::default()
        },
        wgpu::PresentMode::Fifo,
        1);
    let mut window: Option<winit::window::Window> = None;
    let mut ui_state = ui::State::default();

    let bluetooth_event_proxy = event_loop.create_proxy();
    //let (tx, rx) = std::sync::mpsc::channel::<ble::BleRequest>();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ble::BleRequest>();
    let _background_service = ble::BleService::new(bluetooth_event_proxy,
        rx,
        None);
        //Some(COMPANION_CHOOSER_REQUEST_CODE));

    // On most platforms we can immediately create a winit window. On Android we manage
    // window + surface state according to Resumed/Paused events.
    #[cfg(not(target_os="android"))]
    {
        window = Some(create_window(&event_loop, &mut winit_state, &mut painter));
    }

    event_loop.run(move |event, event_loop, control_flow| {

        match event {
            #[cfg(target_os="android")]
            Resumed => {
                match window {
                    None => {
                        window = Some(create_window(event_loop, &mut winit_state, &mut painter));
                    }
                    Some(ref window) => {
                        unsafe { painter.set_window(Some(window)) };
                        window.request_redraw();
                    }
                }
            }

            #[cfg(target_os="android")]
            Suspended => {
                window = None;
            }

            RedrawRequested(..) => {
                if let Some(window) = window.as_ref() {
                    let mut raw_input = winit_state.take_egui_input(window);

                    #[cfg(target_os="android")]
                    if let Some(ppp) = raw_input.pixels_per_point {
                        raw_input.pixels_per_point = Some(ppp * 4.0);
                    }

                    let full_output = ctx.run(raw_input, |ctx| {
                        ui_state.draw(ctx, &tx);
                    });
                    winit_state.handle_platform_output(window, &ctx, full_output.platform_output);

                    painter.paint_and_update_textures(winit_state.pixels_per_point(),
                        egui::Rgba::default(),
                        &ctx.tessellate(full_output.shapes),
                        &full_output.textures_delta);

                    if full_output.needs_repaint {
                        window.request_redraw();
                    }
                }
            }
            MainEventsCleared | UserEvent(ui::Event::RequestRedraw) => {
                if let Some(window) = window.as_ref() {
                    window.request_redraw();
                }
            }
            UserEvent(user_event) => {
                ui_state.handle_event(user_event, &tx);
            }
            WindowEvent { event, .. } => {
                if winit_state.on_event(&ctx, &event) == false {
                    match event {
                        winit::event::WindowEvent::Resized(size) => {
                            painter.on_window_resized(size.width, size.height);
                        }
                        winit::event::WindowEvent::CloseRequested => {
                            *control_flow = ControlFlow::Exit;
                        }
                        _ => {}
                    }
                }
            },
            _ => (),
        }
    });
}

fn _tokio_main() {
    // We create a tokio runtime manually because on Android we need to hook into
    // thread spawning to attach them to the JVM.
    let runtime = tokio_runtime::build_tokio_runtime().unwrap();
    runtime.block_on(async {
        _main()
    })
}

#[cfg(target_os="android")]
#[no_mangle]
extern "C" fn android_main() {
    android_logger::init_once(
        android_logger::Config::default()
            .with_tag("blueyui")
            .with_min_level(log::Level::Debug)
            .with_filter(android_logger::FilterBuilder::new().parse("debug,naga=warn,wgpu=warn").build()
        )
    );

    _tokio_main();
}
// Stop rust-analyzer from complaining that this file doesn't have a main() function...
#[cfg(target_os="android")]
#[cfg(allow_unused)]
fn main() {}

#[cfg(not(target_os="android"))]
fn main() {
    env_logger::builder().filter_level(log::LevelFilter::Trace) // Default Log Level
        .filter(Some("naga"), log::LevelFilter::Warn)
        .filter(Some("wgpu"), log::LevelFilter::Warn)
        .parse_default_env()
        .init();

    _tokio_main();
}