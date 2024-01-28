use std::thread::sleep;
use std::time::Duration;
use std::{error::Error, str::FromStr};
use std::{fs::File, thread};
use std::{
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    sync::mpsc::channel,
};
use std::{path::PathBuf, sync::mpsc};

use clap::Parser;
use env_logger::{Builder, Env};
use global_hotkey::{hotkey::HotKey, GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use image::DynamicImage;
use imgui_winit_glow_renderer_viewports::Renderer;
use log::{debug, error, info, warn};
use notify::{watcher, RecursiveMode, Watcher};
use xcap::Window;

use wfinfo::{
    database::Database,
    ocr::{normalize_string, reward_image_to_reward_names, OCR},
    utils::fetch_prices_and_items,
};

use std::{ffi::CString, num::NonZeroU32, time::Instant};

use glow::{Context, HasContext};
use glutin::{
    config::ConfigTemplateBuilder,
    context::ContextAttributesBuilder,
    display::GetGlDisplay,
    prelude::{
        GlDisplay, NotCurrentGlContextSurfaceAccessor, PossiblyCurrentContextGlSurfaceAccessor,
    },
    surface::{GlSurface, SurfaceAttributesBuilder, WindowSurface},
};
use glutin_winit::DisplayBuilder;
use imgui::ConfigFlags;
use raw_window_handle::HasRawWindowHandle;
use winit::{
    dpi::{LogicalSize, PhysicalPosition},
    event::WindowEvent,
    event_loop::EventLoop,
    platform::unix::{WindowBuilderExtUnix, XWindowType},
    window::WindowBuilder,
};

use imgui::*;

fn run_detection(capturer: &Window, db: &Database) -> Vec<wfinfo::database::Item> {
    let frame = capturer.capture_image().unwrap();
    info!("Captured");
    let image = DynamicImage::ImageRgba8(frame);
    info!("Converted");
    let text = reward_image_to_reward_names(image, None);
    let text = text.iter().map(|s| normalize_string(s));
    println!("{:#?}", text);
    let db = Database::load_from_file(None, None);
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

    // for (index, item) in items.iter().enumerate() {
    //     if let Some(item) = item {
    //         println!(
    //             "{}\n\t{}\t{}\t{}",
    //             item.drop_name,
    //             item.platinum,
    //             item.ducats as f32 / 10.0,
    //             if Some(index) == best { "<----" } else { "" }
    //         );
    //     } else {
    //         println!("Unknown item\n\tUnknown");
    //     }
    // }
}

fn main() {
    let event_loop = EventLoop::new();

    let window_builder = WindowBuilder::new()
        .with_inner_size(LogicalSize::new(1.0, 1.0))
        .with_position(PhysicalPosition::new(0, 1))
        .with_visible(true)
        .with_resizable(true)
        .with_transparent(true)
        .with_decorations(false)
        .with_always_on_top(true)
        .with_x11_window_type(vec![XWindowType::Notification])
        .with_title("DO NOT CLOSE THIS WINDOW");

    let template_builder = ConfigTemplateBuilder::new();
    let (window, gl_config) = DisplayBuilder::new()
        .with_window_builder(Some(window_builder))
        .build(&event_loop, template_builder, |mut configs| {
            configs.next().unwrap()
        })
        .expect("Failed to create main window");

    let window = window.unwrap();

    window
        .set_cursor_grab(winit::window::CursorGrabMode::None)
        .expect("cannot set cursor grab!");

    let context_attribs = ContextAttributesBuilder::new().build(Some(window.raw_window_handle()));
    let context = unsafe {
        gl_config
            .display()
            .create_context(&gl_config, &context_attribs)
            .expect("Failed to create main context")
    };

    let size = window.inner_size();
    let surface_attribs = SurfaceAttributesBuilder::<WindowSurface>::new().build(
        window.raw_window_handle(),
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

    let mut rewards = String::from("A");
    let mut items: Vec<wfinfo::database::Item> = Vec::new();

    let mut best: Option<usize> = Some(0 as usize);

    let path = std::env::args().nth(1).unwrap();
    println!("Path: {}", path);
    let (tx, rx) = mpsc::channel();
    let mut watcher = watcher(tx, Duration::from_millis(100)).unwrap();
    watcher
        .watch(&path, RecursiveMode::NonRecursive)
        .unwrap_or_else(|_| panic!("Failed to open EE.log file: {path}"));

    let mut position = File::open(&path).unwrap().seek(SeekFrom::End(0)).unwrap();
    println!("Position: {}", position);
    // rewards.push_str(format!("Position: {}", position).as_str());

    let mut capturer = Capturer::new(0).unwrap();
    println!("Capture source resolution: {:?}", capturer.geometry());

    // run_detection(&mut capturer);

    event_loop.run(move |event, window_target, control_flow| {
        control_flow.set_poll();

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
                    // println!("> {:?}", line);
                    if line.contains("Pause countdown done")
                        || line.contains("Got rewards")
                        || line.contains("Created /Lotus/Interface/ProjectionRewardChoice.swf")
                    {
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
                    rewards.clear();

                    items = run_detection(&mut capturer).clone();

                    best = items
                        .iter()
                        .map(|item| {
                            item.platinum
                                .max(item.ducats as f32 / 10.0 + item.platinum / 100.0)
                        })
                        .enumerate()
                        .max_by(|a, b| a.1.total_cmp(&b.1))
                        .map(|best| best.0);

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
                    rewards.clear();
                    items.clear();
                    window.request_redraw();
                }

                position = f.metadata().unwrap().len();
                println!("Log position: {}", position);
                // rewards.push_str(format!("Log Position: {}\n", position).as_str());
            }
            Ok(_) => {}
            Err(err) => {
                // eprintln!("Error: {:?}", err);
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
                control_flow.set_exit();
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
            winit::event::Event::MainEventsCleared => {
                window.request_redraw();
            }
            winit::event::Event::RedrawRequested(_) => {
                let ui = imgui.frame();
                if items.len() > 0 {
                    ui.window("RelicRewards")
                        .size([400.0, 200.0], Condition::FirstUseEver)
                        .resizable(true)
                        .focused(false)
                        .nav_focus(false)
                        .focus_on_appearing(false)
                        .draw_background(false)
                        .bg_alpha(0.5)
                        .flags(WindowFlags::NO_INPUTS | WindowFlags::NO_NAV_FOCUS)
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
                                for (index, item) in items.iter().enumerate() {
                                    ui.table_next_column();
                                    ui.text(item.drop_name.clone());

                                    ui.table_next_column();
                                    ui.text(format!("{}", item.platinum));

                                    ui.table_next_column();
                                    ui.text(format!("{}", item.ducats));
                                    ui.table_next_row();
                                }

                                // note you MUST call `next_column` at least to START
                                // Let's walk through a table like it's an iterator...

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
}
