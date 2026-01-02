use std::collections::HashMap;
use std::error::Error;
use std::thread; // Added for pause functionality
use std::time::Duration; // Added for pause functionality

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use image::{Rgba, RgbaImage};
use x11rb::connection::Connection;
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    self, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask,
    WindowClass,
};

// !!! FONT PATH !!!
const FONT_PATH: &str = "/usr/share/fonts/TTF/DejaVuSans.ttf";
const ICON_SIZE: u16 = 24;

fn main() -> Result<(), Box<dyn Error>> {
    // 1. Connecting to X11
    let (conn, screen_num) = x11rb::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let root_window = screen.root;

    // Enable XKB extension (keyboard)
    conn.xkb_use_extension(1, 0)?;
    conn.xkb_select_events(
        xkb::ID::USE_CORE_KBD.into(),
        0u16.into(),
        xkb::EventType::STATE_NOTIFY,
        0u16.into(),
        0u16.into(),
        &xkb::SelectEventsAux::default(),
    )?;

    // 2. Loading font
    let layout_names = get_layout_names(&conn)?;
    println!("Detected layouts: {:?}", layout_names);

    let font_data = std::fs::read(FONT_PATH)
        .map_err(|_| format!("ERROR: Font not found at '{}'", FONT_PATH))?;
    let font = FontRef::try_from_slice(&font_data)?;

    let mut icon_cache = HashMap::new();
    for name in &layout_names {
        let short = shorten_name(name);
        let pixels = render_icon_bgra(&short, &font);
        icon_cache.insert(name.clone(), pixels);
    }

    // 3. Creating window
    let win_id = conn.generate_id()?;
    let white_pixel = screen.white_pixel;

    let win_aux = CreateWindowAux::new()
        .background_pixel(white_pixel)
        .override_redirect(1)
        .event_mask(EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY);

    conn.create_window(
        x11rb::COPY_FROM_PARENT as u8,
        win_id,
        root_window,
        0, 0, ICON_SIZE, ICON_SIZE,
        0,
        WindowClass::INPUT_OUTPUT,
        x11rb::COPY_FROM_PARENT,
        &win_aux,
    )?;

    // 4. Docking (WITH CHANGES: RETRY ATTEMPTS)
    let max_retries = 10; // 10 attempts
    let mut docked = false;

    println!("Attempting to dock into System Tray...");
    for i in 1..=max_retries {
        match dock_window_to_tray(&conn, screen_num, win_id) {
            Ok(_) => {
                docked = true;
                println!("Successfully docked on attempt #{}", i);
                break;
            }
            Err(_) => {
                if i < max_retries {
                    println!("Tray not found (attempt {}/{}), retrying in 500ms...", i, max_retries);
                    thread::sleep(Duration::from_millis(500));
                }
            }
        }
    }

    if !docked {
        return Err("Could not find System Tray after waiting. Is tint2/panel running?".into());
    }

    // Show window
    conn.map_window(win_id)?;
    conn.flush()?;

    // 5. Initial rendering
    let state_cookie = conn.xkb_get_state(xkb::ID::USE_CORE_KBD.into())?;
    let state_reply = state_cookie.reply()?;
    let mut current_group: u8 = state_reply.group.into();

    if let Some(name) = layout_names.get(current_group as usize) {
        if let Some(pixels) = icon_cache.get(name) {
            draw_icon(&conn, win_id, screen, pixels)?;
        }
    }

    println!("App started. Icon should now be IN the tray.");

    // 6. Main loop
    loop {
        let event = conn.wait_for_event()?;
        match event {
            x11rb::protocol::Event::XkbStateNotify(e) => {
                let new_group: u8 = e.group.into();
                if new_group != current_group {
                    current_group = new_group;
                    if let Some(name) = layout_names.get(current_group as usize) {
                        if let Some(pixels) = icon_cache.get(name) {
                            draw_icon(&conn, win_id, screen, pixels)?;
                        }
                    }
                }
            }
            x11rb::protocol::Event::Expose(e) => {
                if e.count == 0 {
                    if let Some(name) = layout_names.get(current_group as usize) {
                        if let Some(pixels) = icon_cache.get(name) {
                            draw_icon(&conn, win_id, screen, pixels)?;
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

// --- Helpers ---

fn dock_window_to_tray(conn: &impl Connection, screen_num: usize, win_id: xproto::Window) -> Result<(), Box<dyn Error>> {
    let tray_atom_name = format!("_NET_SYSTEM_TRAY_S{}", screen_num);
    let tray_atom = conn.intern_atom(false, tray_atom_name.as_bytes())?.reply()?.atom;

    // Check if there is an owner of the tray atom
    let manager_reply = conn.get_selection_owner(tray_atom)?.reply()?;
    let manager_win = manager_reply.owner;

    if manager_win == x11rb::NONE {
        return Err("No system tray detected".into());
    }

    let opcode_atom = conn.intern_atom(false, b"_NET_SYSTEM_TRAY_OPCODE")?.reply()?.atom;

    let event = ClientMessageEvent {
        response_type: xproto::CLIENT_MESSAGE_EVENT,
        format: 32,
        window: manager_win,
        type_: opcode_atom,
        data: xproto::ClientMessageData::from([0, 0, win_id, 0, 0]),
        sequence: 0,
    };

    conn.send_event(false, manager_win, EventMask::NO_EVENT, event)?;
    Ok(())
}

fn draw_icon(
    conn: &impl Connection,
    win: xproto::Window,
    screen: &xproto::Screen,
    pixels: &[u8]
) -> Result<(), Box<dyn Error>> {
    let gc = conn.generate_id()?;
    conn.create_gc(gc, win, &xproto::CreateGCAux::new())?;

    conn.put_image(
        xproto::ImageFormat::Z_PIXMAP,
        win,
        gc,
        ICON_SIZE, ICON_SIZE,
        0, 0,
        0,
        screen.root_depth,
        pixels,
    )?;

    conn.free_gc(gc)?;
    conn.flush()?;
    Ok(())
}

fn shorten_name(name: &str) -> String {
    let lower = name.to_lowercase();
    if lower.contains("ru") || lower.contains("russian") { return "RU".to_string(); }
    if lower.contains("us") || lower.contains("english") { return "EN".to_string(); }
    if lower.contains("ua") { return "UA".to_string(); }
    name.chars().take(2).collect::<String>().to_uppercase()
}

fn get_layout_names(conn: &impl Connection) -> Result<Vec<String>, Box<dyn Error>> {
    let names = conn.xkb_get_names(xkb::ID::USE_CORE_KBD.into(), xkb::NameDetail::GROUP_NAMES)?.reply()?;
    let mut res = Vec::new();
    if let Some(groups) = names.value_list.groups {
        for atom in groups {
            if atom == 0 { break; }
            let name = String::from_utf8(conn.get_atom_name(atom)?.reply()?.name)?;
            res.push(name);
        }
    }
    if res.is_empty() { res.push("US".to_string()); }
    Ok(res)
}

fn render_text_icon(text: &str, font: &FontRef) -> RgbaImage {
    // Colors
    let bg_color = [35u8, 35u8, 35u8];
    let fg_color = [255u8, 255u8, 255u8];

    let mut image = RgbaImage::from_pixel(
        ICON_SIZE as u32,
        ICON_SIZE as u32,
        Rgba([bg_color[0], bg_color[1], bg_color[2], 255])
    );

    let scale = PxScale { x: 16.0, y: 16.0 };
    let scaled_font = font.as_scaled(scale);

    let mut text_width = 0.0;
    for c in text.chars() {
        text_width += scaled_font.h_advance(scaled_font.glyph_id(c));
    }

    let start_x = ((ICON_SIZE as f32 - text_width) / 2.0).round() - 2.0;

    let v_metrics = scaled_font.ascent() - scaled_font.descent();
    let start_y = ((ICON_SIZE as f32 - v_metrics) / 2.0 + scaled_font.ascent()).round() - 1.0;

    let mut current_x = start_x;

    for c in text.chars() {
        let glyph_id = scaled_font.glyph_id(c);
        let glyph = glyph_id.with_scale_and_position(scale, ab_glyph::point(current_x, start_y));

        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();

            outlined.draw(|x, y, coverage| {
                let px = x as u32 + bounds.min.x as u32;
                let py = y as u32 + bounds.min.y as u32;

                if px < ICON_SIZE as u32 && py < ICON_SIZE as u32 {
                    let pixel = image.get_pixel_mut(px, py);

                    let blend = |bg: u8, fg: u8, v: f32| -> u8 {
                        ((bg as f32 * (1.0 - v)) + (fg as f32 * v)) as u8
                    };

                    let r = blend(bg_color[0], fg_color[0], coverage);
                    let g = blend(bg_color[1], fg_color[1], coverage);
                    let b = blend(bg_color[2], fg_color[2], coverage);

                    *pixel = Rgba([r, g, b, 255]);
                }
            });
        }
        current_x += scaled_font.h_advance(glyph_id);
    }
    image
}

fn render_icon_bgra(text: &str, font: &FontRef) -> Vec<u8> {
    let img = render_text_icon(text, font);
    let mut data = Vec::with_capacity((ICON_SIZE * ICON_SIZE * 4) as usize);
    for pixel in img.pixels() {
        let [r, g, b, _a] = pixel.0;
        data.push(b);
        data.push(g);
        data.push(r);
        data.push(0);
    }
    data
}