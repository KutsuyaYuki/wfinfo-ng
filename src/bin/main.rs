use std::{
    borrow::Borrow,
    error::Error,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    sync::mpsc,
    thread::sleep,
    time::Duration
};

use glutin::{display::GetGlDisplay, prelude::{GlDisplay, NotCurrentGlContext, PossiblyCurrentGlContext}, surface::GlSurface};
use image::DynamicImage;
use imgui_winit_glow_renderer_viewports::Renderer;
use log::info;
use notify::{watcher, RecursiveMode, Watcher};

use wfinfo::{
    database::Database,
    ocr::{normalize_string, reward_image_to_reward_names},
    utils::fetch_prices_and_items,
};

use std::{ffi::CString, num::NonZeroU32, time::Instant};

use glow::{Context, HasContext};
use glutin::{
    config::ConfigTemplateBuilder,
    context::ContextAttributesBuilder,
    surface::{SurfaceAttributesBuilder, WindowSurface},
};
use glutin_winit::DisplayBuilder;
use imgui::ConfigFlags;
use raw_window_handle::HasWindowHandle;
use winit::{
    dpi::{LogicalSize, PhysicalPosition},
    event::WindowEvent,
    event_loop::EventLoop,
};

use imgui::*;

use image::RgbaImage;
use dbus::{
    arg::{AppendAll, Iter, IterAppend, PropMap, ReadAll, RefArg, TypeMismatchError, Variant},
    blocking::{Connection, Proxy},
    message::{MatchRule, SignalArgs},
};
use std::{
    collections::HashMap,
    fs::{self},
    sync::{Arc, Mutex},
};
use percent_encoding::percent_decode;

static DBUS_LOCK: Mutex<()> = Mutex::new(());
use image::open;
#[derive(Debug)]
struct OrgFreedesktopPortalRequestResponse {
    status: u32,
    results: PropMap,
}

impl AppendAll for OrgFreedesktopPortalRequestResponse {
    fn append(&self, i: &mut IterAppend) {
        RefArg::append(&self.status, i);
        RefArg::append(&self.results, i);
    }
}

impl ReadAll for OrgFreedesktopPortalRequestResponse {
    fn read(i: &mut Iter) -> Result<Self, TypeMismatchError> {
        Ok(OrgFreedesktopPortalRequestResponse {
            status: i.read()?,
            results: i.read()?,
        })
    }
}

impl SignalArgs for OrgFreedesktopPortalRequestResponse {
    const NAME: &'static str = "Response";
    const INTERFACE: &'static str = "org.freedesktop.portal.Request";
}
fn png_to_rgba_image(
    filename: &String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> xcap::XCapResult<RgbaImage> {
    let mut dynamic_image = open(filename)?;
    dynamic_image = dynamic_image.crop(x as u32, y as u32, width as u32, height as u32);
    Ok(dynamic_image.to_rgba8())
}

fn org_freedesktop_portal_screenshot(
    conn: &Connection,
    proxy: &Proxy<'_, &Connection>,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> xcap::XCapResult<RgbaImage> {
    let status: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
    let status_res = status.clone();
    let path: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let path_res = path.clone();

    let match_rule = MatchRule::new_signal("org.freedesktop.portal.Request", "Response");
    conn.add_match(
        match_rule,
        move |response: OrgFreedesktopPortalRequestResponse, _conn, _msg| {
            if let Ok(mut status) = status.lock() {
                *status = Some(response.status);
            }

            let uri = response.results.get("uri").and_then(|str| str.as_str());
            if let (Some(uri_str), Ok(mut path)) = (uri, path.lock()) {
                *path = uri_str[7..].to_string();
            }

            true
        },
    )?;

    let mut options: PropMap = HashMap::new();
    options.insert(
        String::from("handle_token"),
        Variant(Box::new(String::from("1234"))),
    );
    options.insert(String::from("modal"), Variant(Box::new(true)));
    options.insert(String::from("interactive"), Variant(Box::new(false)));

    proxy.method_call::<(), (&str, PropMap), &str, &str>(
        "org.freedesktop.portal.Screenshot",
        "Screenshot",
        ("", options),
    )?;

    // wait 60 seconds for user interaction
    for _ in 0..60 {
        let result = conn.process(Duration::from_millis(1000))?;
        let status = status_res
            .lock()
            .map_err(|_| xcap::XCapError::new("Get status lock failed"))?;

        if result && status.is_some() {
            break;
        }
    }

    let status = status_res
        .lock()
        .map_err(|_| xcap::XCapError::new("Get status lock failed"))?;
    let status = *status;

    let path = path_res
        .lock()
        .map_err(|_| xcap::XCapError::new("Get path lock failed"))?;
    let path = &*path;

    if status.ne(&Some(0)) || path.is_empty() {
        if !path.is_empty() {
            fs::remove_file(path)?;
        }
        return Err(xcap::XCapError::new("Screenshot failed or canceled"));
    }

    let filename = percent_decode(path.as_bytes()).decode_utf8()?.to_string();
    let rgba_image = png_to_rgba_image(&filename, x, y, width, height)?;

    fs::remove_file(&filename)?;

    Ok(rgba_image)
}

fn run_detection(
    conn: &Connection,
    proxy: &Proxy<'_, &Connection>,
    monitor: &xcap::Monitor,
    db: &Database
    ) -> Vec<wfinfo::database::Item> {
    let x = ((monitor.x() as f32) * monitor.scale_factor()) as i32;
    let y = ((monitor.y() as f32) * monitor.scale_factor()) as i32;
    let width = ((monitor.width() as f32) * monitor.scale_factor()) as i32;
    let height = ((monitor.height() as f32) * monitor.scale_factor()) as i32;

    let lock = DBUS_LOCK.lock();
    let frame = org_freedesktop_portal_screenshot(&conn, &proxy, x, y, width, height).unwrap();

    drop(lock);
    info!("Captured");
    let image = DynamicImage::ImageRgba8(frame);
    info!("Converted");
    let text = reward_image_to_reward_names(image, None);
    let text = text.iter().map(|s| normalize_string(s));
    println!("{:#?}", text);
    let items: Vec<_> = text
        .map(move |s| {
            db.find_item(&s, None)
                .unwrap_or(&wfinfo::database::Item {
                    drop_name: ("".to_owned()),
                    ducats: (0),
                    name: ("".to_owned()),
                    platinum: (0.0 as f32),
                })
                .to_owned()
        })
        .collect();

    return items;
}

fn main() -> Result<(), Box<dyn Error>> {
    let event_loop = EventLoop::new().unwrap();
    let window_builder = winit::window::Window::default_attributes()
        .with_inner_size(LogicalSize::new(400.0, 200.0))
        .with_position(PhysicalPosition::new(1, 1))
        .with_visible(true)
        .with_resizable(true)
        .with_transparent(true)
        .with_decorations(false)
        .with_maximized(true)
        //.with_x11_window_type(vec![XWindowType::Notification])
        .with_title("DO NOT CLOSE THIS WINDOW");

    let template_builder = ConfigTemplateBuilder::new();
    let (window, gl_config) = DisplayBuilder::new()
        .with_window_attributes(Some(window_builder))
        .build(&event_loop, template_builder, |mut configs| {
            configs.next().unwrap()
        })
        .expect("Failed to create main window");

    let window = window.unwrap();

    window.focus_window();

    window
        .set_cursor_grab(winit::window::CursorGrabMode::None)
        .expect("cannot set cursor grab!");

    let context_attribs = ContextAttributesBuilder::new().build(Some(window.window_handle()?.as_raw()));
    let context = unsafe {
        gl_config
            .display()
            .create_context(&gl_config, &context_attribs)
            .expect("Failed to create main context")
    };

    let size = window.inner_size();
    let surface_attribs = SurfaceAttributesBuilder::<WindowSurface>::new().build(
        window.window_handle()?.as_raw(),
        NonZeroU32::new(size.width).unwrap(),
        NonZeroU32::new(size.height).unwrap(),
    );
    let surface = unsafe {
        gl_config
            .display()
            .create_window_surface(&gl_config, &surface_attribs)
            .expect("Failed to create main surface")
    };

    let context = context
        .make_current(&surface)
        .expect("Failed to make current");

    let glow = unsafe {
        Context::from_loader_function(|name| {
            let name = CString::new(name).unwrap();
            context.display().get_proc_address(&name)
        })
    };

    let mut imgui = imgui::Context::create();
    imgui
        .io_mut()
        .config_flags
        .insert(ConfigFlags::DOCKING_ENABLE);
    imgui
        .io_mut()
        .config_flags
        .insert(ConfigFlags::VIEWPORTS_ENABLE);
    imgui.io_mut().config_flags.insert(ConfigFlags::NO_MOUSE);
    imgui.set_ini_filename(None);

    let mut renderer = Renderer::new(&mut imgui, &window, &glow).expect("Failed to init Renderer");

    let mut last_frame = Instant::now();

    let mut items: Vec<wfinfo::database::Item> = Vec::new();

    let path = std::env::args().nth(1).unwrap();
    println!("Path: {}", path);
    let (tx, rx) = mpsc::channel();
    let mut watcher = watcher(tx, Duration::from_millis(100)).unwrap();
    watcher
        .watch(&path, RecursiveMode::NonRecursive)
        .unwrap_or_else(|_| panic!("Failed to open EE.log file: {path}"));

    let mut position = File::open(&path).unwrap().seek(SeekFrom::End(0)).unwrap();
    println!("Position: {}", position);

    let monitors = xcap::Monitor::all().unwrap();
    
    let conn = &Connection::new_session()?;
    let proxy = conn.with_proxy(
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        Duration::from_millis(10000),
    );
    
    let (prices, dbitems) = fetch_prices_and_items()?;
    let db = Database::load_from_file(Some(&prices), Some(&dbitems));

    let _ = event_loop.run(move |event, window_target | {
        window_target.set_control_flow(winit::event_loop::ControlFlow::Poll);

        renderer.handle_event(&mut imgui, &window, &event);

        match rx.try_recv() {
            Ok(notify::DebouncedEvent::Write(_)) => {
                let mut f = File::open(&path).unwrap();
                f.seek(SeekFrom::Start(position)).unwrap();

                let mut reward_screen_detected = false;
                let mut end_of_match_detected = false;

                let reader = BufReader::new(f.by_ref());
                for line in reader.lines() {
                    let line = match line {
                        Ok(line) => line,
                        Err(err) => {
                            println!("Error reading line: {}", err);
                            continue;
                        }
                    };
                    if line.contains("Pause countdown done")
                        || line.contains("Got rewards")
                        || line.contains("Created /Lotus/Interface/ProjectionRewardChoice.swf")
                    {
                        println!("> {:?}", line);
                        reward_screen_detected = true;
                    }
                    if line.contains("Created /Lotus/Interface/EndOfMatch.swf") {
                        end_of_match_detected = true;
                    }
                }

                if reward_screen_detected {
                    println!("Detected, waiting...");
                    sleep(Duration::from_millis(1500));
                    println!("Capturing");
                    let mut rewards = String::new();

                    items = run_detection(conn, &proxy.borrow(), monitors[0].borrow(), &db).clone();

                    let best = items
                        .iter()
                        .map(|item| {
                            item.platinum
                                .max(item.ducats as f32 / 10.0 + item.platinum / 100.0)
                        })
                        .enumerate()
                        .max_by(|a, b| a.1.total_cmp(&b.1))
                        .map(|b: (usize, f32)| b.0);

                    let mut result = String::new();

                    for (index, item) in items.iter().enumerate() {
                        result.push_str(&format!(
                            "{}\t{}\t{}\t{}\n",
                            item.drop_name,
                            item.platinum,
                            item.ducats as f32 / 10.0,
                            if Some(index) == best { "<----" } else { "" }
                        ));
                    }
                    rewards.push_str(result.as_str());
                    println!("rewards: {}", rewards);
                    window.request_redraw();
                }

                if end_of_match_detected {
                    println!("Match ended!");
                    items.clear();
                    window.request_redraw();
                }

                position = f.metadata().unwrap().len();
                info!("Log position: {}", position);
            }
            Ok(_) => {}
            Err(_err) => {
                //eprintln!("Error: {:?}", err);
            }
        }

        match event {
            winit::event::Event::NewEvents(_) => {
                let now = Instant::now();
                imgui.io_mut().update_delta_time(now - last_frame);
                last_frame = now;
            }
            winit::event::Event::WindowEvent {
                window_id,
                event: WindowEvent::CloseRequested,
            } if window_id == window.id() => {
                window_target.set_control_flow(winit::event_loop::ControlFlow::Poll);
            }
            winit::event::Event::WindowEvent {
                window_id,
                event: WindowEvent::Resized(new_size),
            } if window_id == window.id() => {
                surface.resize(
                    &context,
                    NonZeroU32::new(new_size.width).unwrap(),
                    NonZeroU32::new(new_size.height).unwrap(),
                );
            }
            winit::event::Event::AboutToWait => {
                window.request_redraw();
                window.set_maximized(false);
                let ui = imgui.frame();
                if items.len() > 0 {
                    ui.window("RelicRewards")
                        .size([400.0, 200.0], Condition::FirstUseEver)
                        .position([0.0, 0.0], Condition::FirstUseEver)
                        .resizable(true)
                        .focused(false)
                        .nav_focus(false)
                        .focus_on_appearing(false)
                        .draw_background(false)
                        .bring_to_front_on_focus(true)
                        .bg_alpha(0.5)
                        .flags(WindowFlags::NO_INPUTS | WindowFlags::NO_NAV_FOCUS)
                        .nav_inputs(false)
                        .build(|| {
                            ui.text("rewards:");
                            if let Some(_t) = ui.begin_table_header_with_flags(
                                "Basic-Table",
                                [
                                    TableColumnSetup::new("Name"),
                                    TableColumnSetup::new("Platinum"),
                                    TableColumnSetup::new("Ducats"),
                                ],
                                TableFlags::BORDERS | TableFlags::SIZING_FIXED_FIT,
                            ) {
                                items.sort_by(|a, b| b.platinum.total_cmp(&a.platinum));
                                for (_index, item) in items.iter().enumerate() {
                                    ui.table_next_column();
                                    ui.text(item.drop_name.clone());

                                    ui.table_next_column();
                                    ui.text(format!("{}", item.platinum));

                                    ui.table_next_column();
                                    ui.text(format!("{}", item.ducats));
                                    ui.table_next_row();
                                }
                                ui.new_line();
                            }
                        });
                }

                ui.end_frame_early();

                renderer.prepare_render(&mut imgui, &window);

                imgui.update_platform_windows();
                renderer
                    .update_viewports(&mut imgui, window_target, &glow)
                    .expect("Failed to update viewports");

                let draw_data = imgui.render();

                if let Err(e) = context.make_current(&surface) {
                    // For some reason make_current randomly throws errors on windows.
                    // Until the reason for this is found, we just print it out instead of panicing.
                    eprintln!("Failed to make current: {e}");
                }

                unsafe {
                    glow.disable(glow::SCISSOR_TEST);
                    glow.clear(glow::COLOR_BUFFER_BIT);
                }

                renderer
                    .render(&window, &glow, draw_data)
                    .expect("Failed to render main viewport");

                surface
                    .swap_buffers(&context)
                    .expect("Failed to swap buffers");

                renderer
                    .render_viewports(&glow, &mut imgui)
                    .expect("Failed to render viewports");
            }
            _ => {}
        }
    });
    Ok(())
}
