//! LilyGO T5 2.13" e-paper: shows which bus to take to Oerlikon, built to run on
//! battery.
//!
//! Every wake-up:
//!   1. Load today's schedule from flash (NVS).
//!   2. Sync over Wi-Fi only when needed: no schedule/clock yet, the morning window
//!      is starting, or the data is more than 30 min old while buses are still on screen.
//!      The Worker's response also sets the clock, so no NTP is needed.
//!   3. Redraw the e-paper only if what's on screen would change.
//!   4. Deep-sleep until the first bus on screen is gone (or until the next morning).
//!
//! Build-time env vars (see ../.env.example): WIFI_SSID, WIFI_PASS, WORKER_URL, WORKER_TOKEN

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};
use embedded_graphics::{
    prelude::*,
    primitives::{Line, PrimitiveStyle},
};
use epd_waveshare::{
    color::Color,
    epd2in13_v2::{Display2in13, Epd2in13},
    graphics::DisplayRotation,
    prelude::*,
};
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    hal::{
        delay::Delay,
        gpio::{AnyIOPin, PinDriver, Pull},
        modem::Modem,
        peripherals::Peripherals,
        spi::{config, SpiDeviceDriver},
        units::Hertz,
    },
    http::{
        client::{Configuration as HttpConfig, EspHttpConnection},
        Method,
    },
    nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault},
    sys,
    wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi},
};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use u8g2_fonts::{
    fonts,
    types::{FontColor, HorizontalAlignment, VerticalPosition},
    FontRenderer,
};

const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PASS: &str = env!("WIFI_PASS");
const WORKER_URL: &str = env!("WORKER_URL");
const WORKER_TOKEN: &str = env!("WORKER_TOKEN");

/// While buses are on screen, refresh delays/clock at most this often.
const RESYNC_EVERY: i64 = 30 * 60;
/// After a failed sync, try again after this long.
const RETRY_AFTER: i64 = 5 * 60;
/// Wake this long after the first bus's leave time before dropping it.
const DROP_GRACE: i64 = 5;
/// Anything before this means the clock was never set (cold boot).
const CLOCK_VALID_AFTER: i64 = 1_700_000_000;

#[derive(Deserialize, Serialize, Clone, PartialEq)]
struct Opt {
    leave_ts: i64,
    leave: String,
    walk_min: i64,
    stop: String,
    line: String,
    depart: String,
    delay: i64,
    arrive: String,
    transfers: u32,
}

#[derive(Deserialize, Serialize, Clone)]
struct Day {
    to: String,
    now: i64,
    next_sync: i64,
    next_sync_label: String,
    options: Vec<Opt>,
    /// Second trip (Zug, arrive by 08:00), latest good departure first.
    #[serde(default)]
    trip2: Option<Trip>,
}

#[derive(Deserialize, Serialize, Clone)]
struct Trip {
    label: String,
    options: Vec<RouteOpt>,
}

#[derive(Deserialize, Serialize, Clone, PartialEq)]
struct RouteOpt {
    leave_ts: i64,
    leave: String,
    walk_min: i64,
    stop: String,
    chain: String,
    /// Compact chain ("T7 · B50 · IR · B611") for when `chain` doesn't fit.
    #[serde(default)]
    chain_short: String,
    arrive: String,
    transfers: u32,
}

/// What's on screen, so we only redraw (and flash the e-paper) when it changes.
#[derive(PartialEq)]
enum Screen<'a> {
    Plan(&'a [Opt]),
    /// Second trip: label + its still-reachable options, latest first.
    Trip(&'a str, Vec<&'a RouteOpt>),
    Idle(&'a str),
    Message(&'a str, &'a str),
}

impl Screen<'_> {
    fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        match self {
            Screen::Plan(opts) => {
                "plan".hash(&mut h);
                for o in *opts {
                    (o.leave_ts, &o.line, &o.depart, o.delay, &o.arrive).hash(&mut h);
                }
            }
            Screen::Trip(label, opts) => {
                ("trip", label).hash(&mut h);
                for o in opts {
                    (o.leave_ts, &o.chain, &o.arrive).hash(&mut h);
                }
            }
            Screen::Idle(label) => ("idle", label).hash(&mut h),
            Screen::Message(a, b) => ("msg", a, b).hash(&mut h),
        }
        h.finish()
    }
}

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let sleep_for = match run() {
        Ok(secs) => secs,
        Err(e) => {
            error!("run failed: {e:?}");
            RETRY_AFTER
        }
    };
    deep_sleep(sleep_for);
}

/// One wake-up. Returns how many seconds to sleep.
fn run() -> Result<i64> {
    let p = Peripherals::take()?;
    let nvs_part = EspDefaultNvsPartition::take()?;
    let nvs = EspNvs::new(nvs_part.clone(), "sbb", true)?;

    let mut day = load_day(&nvs);
    let now = unix_now();
    let clock_ok = now > CLOCK_VALID_AFTER;
    let last_sync = nvs.get_i64("last_sync")?.unwrap_or(0);
    let upcoming_now = day
        .as_ref()
        .map(|d| {
            d.options.iter().any(|o| o.leave_ts >= now)
                || d.trip2.iter().flat_map(|t| &t.options).any(|o| o.leave_ts >= now)
        })
        .unwrap_or(false);

    let need_sync = match &day {
        _ if !clock_ok => true,
        None => true,
        Some(d) if upcoming_now => now - last_sync >= RESYNC_EVERY,
        Some(d) => now >= d.next_sync - 60,
    };

    let mut sync_failed = false;
    if need_sync {
        info!("syncing (clock_ok={clock_ok}, have_data={})", day.is_some());
        match sync(p.modem, nvs_part) {
            Ok(fresh) => {
                set_clock(fresh.now);
                nvs.set_i64("last_sync", fresh.now)?;
                save_day(&nvs, &fresh)?;
                day = Some(fresh);
            }
            Err(e) => {
                error!("sync failed: {e:?}");
                sync_failed = true;
            }
        }
    }

    let now = unix_now();
    let (screen, mut sleep_until) = match &day {
        Some(d) if now > CLOCK_VALID_AFTER && trip_upcoming(d, now).is_some() => {
            // The second trip (Zug) takes the screen until its latest departure has gone.
            let (label, opts) = trip_upcoming(d, now).unwrap();
            let wake = opts.iter().map(|o| o.leave_ts).min().unwrap() + DROP_GRACE;
            (Screen::Trip(label, opts), wake)
        }
        Some(d) if now > CLOCK_VALID_AFTER => {
            let first = d.options.iter().position(|o| o.leave_ts >= now);
            match first {
                Some(i) => {
                    let shown = &d.options[i..(i + 3).min(d.options.len())];
                    (Screen::Plan(shown), shown[0].leave_ts + DROP_GRACE)
                }
                None => (Screen::Idle(&d.next_sync_label), d.next_sync),
            }
        }
        _ => (
            Screen::Message("Offline", "Can't reach the timetable"),
            now + RETRY_AFTER,
        ),
    };
    if sync_failed {
        sleep_until = sleep_until.min(now + RETRY_AFTER);
    }

    // Redraw only when the content changes.
    let fp = screen.fingerprint();
    if nvs.get_u64("screen")? != Some(fp) {
        draw(
            p.spi2,
            p.pins.gpio18,
            p.pins.gpio23,
            p.pins.gpio5,
            p.pins.gpio4,
            p.pins.gpio17,
            p.pins.gpio16,
            &screen,
        )?;
        nvs.set_u64("screen", fp)?;
    } else {
        info!("screen unchanged, not redrawing");
    }

    if let Screen::Plan(opts) = &screen {
        for o in *opts {
            info!(
                "Leave {} | {} | {} {} | arrive {}",
                o.leave, o.stop, o.line, o.depart, o.arrive
            );
        }
    }
    Ok((sleep_until - unix_now()).max(10))
}

fn trip_upcoming(d: &Day, now: i64) -> Option<(&str, Vec<&RouteOpt>)> {
    let t = d.trip2.as_ref()?;
    let opts: Vec<&RouteOpt> = t.options.iter().filter(|o| o.leave_ts >= now).collect();
    (!opts.is_empty()).then(|| (t.label.as_str(), opts))
}

// ---------- storage / clock / sleep ----------

fn load_day(nvs: &EspNvs<NvsDefault>) -> Option<Day> {
    let len = nvs.blob_len("day").ok()??;
    let mut buf = vec![0u8; len];
    let bytes = nvs.get_blob("day", &mut buf).ok()??;
    serde_json::from_slice(bytes).ok()
}

fn save_day(nvs: &EspNvs<NvsDefault>, day: &Day) -> Result<()> {
    nvs.set_blob("day", &serde_json::to_vec(day)?)?;
    Ok(())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn set_clock(unix: i64) {
    let tv = sys::timeval {
        tv_sec: unix as _,
        tv_usec: 0,
    };
    unsafe { sys::settimeofday(&tv, std::ptr::null()) };
    info!("clock set to {unix}");
}

fn deep_sleep(secs: i64) -> ! {
    info!("deep sleep for {secs} s");
    unsafe {
        sys::esp_sleep_enable_timer_wakeup(secs as u64 * 1_000_000);
        sys::esp_deep_sleep_start();
    }
}

// ---------- network ----------

fn sync(modem: Modem<'static>, nvs: EspDefaultNvsPartition) -> Result<Day> {
    let sysloop = EspSystemEventLoop::take()?;
    let mut wifi = BlockingWifi::wrap(EspWifi::new(modem, sysloop.clone(), Some(nvs))?, sysloop)?;
    let auth_method = if WIFI_PASS.is_empty() {
        AuthMethod::None
    } else {
        AuthMethod::WPA2Personal
    };
    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: WIFI_SSID.try_into().map_err(|_| anyhow!("SSID too long"))?,
        password: WIFI_PASS
            .try_into()
            .map_err(|_| anyhow!("password too long"))?,
        auth_method,
        ..Default::default()
    }))?;
    wifi.start()?;
    info!("connecting to Wi-Fi '{WIFI_SSID}'...");
    wifi.connect()?;
    wifi.wait_netif_up()?;

    let result = fetch_day();
    if let Err(e) = wifi.stop() {
        warn!("wifi stop: {e:?}");
    }
    result
}

fn fetch_day() -> Result<Day> {
    let mut conn = EspHttpConnection::new(&HttpConfig {
        crt_bundle_attach: Some(sys::esp_crt_bundle_attach),
        timeout: Some(Duration::from_secs(20)),
        ..Default::default()
    })?;
    let url = format!("{WORKER_URL}?format=day");
    conn.initiate_request(Method::Get, &url, &[("x-token", WORKER_TOKEN)])?;
    conn.initiate_response()?;
    let status = conn.status();

    let mut body = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let n = conn.read(&mut buf)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    if status != 200 {
        bail!("HTTP {status}: {}", String::from_utf8_lossy(&body));
    }
    let day: Day = serde_json::from_slice(&body)?;
    info!(
        "synced: {} options, next sync {}",
        day.options.len(),
        day.next_sync_label
    );
    Ok(day)
}

// ---------- e-paper (LilyGO T5 V2.3 2.13", SSD1680) ----------

#[allow(clippy::too_many_arguments)]
fn draw(
    spi2: esp_idf_svc::hal::spi::SPI2<'static>,
    sck: esp_idf_svc::hal::gpio::Gpio18<'static>,
    mosi: esp_idf_svc::hal::gpio::Gpio23<'static>,
    cs: esp_idf_svc::hal::gpio::Gpio5<'static>,
    busy: esp_idf_svc::hal::gpio::Gpio4<'static>,
    dc: esp_idf_svc::hal::gpio::Gpio17<'static>,
    rst: esp_idf_svc::hal::gpio::Gpio16<'static>,
    screen: &Screen,
) -> Result<()> {
    let mut spi = SpiDeviceDriver::new_single(
        spi2,
        sck,
        mosi,
        Option::<AnyIOPin>::None,
        Some(cs),
        &config::DriverConfig::new(),
        &config::Config::new().baudrate(Hertz(4_000_000)),
    )?;
    let busy = PinDriver::input(busy, Pull::Floating)?;
    let dc = PinDriver::output(dc)?;
    let rst = PinDriver::output(rst)?;
    let mut delay = Delay::new_default();
    let mut epd = Epd2in13::new(&mut spi, busy, dc, rst, &mut delay, None)
        .map_err(|e| anyhow!("e-paper init: {e:?}"))?;

    let mut d = Box::new(Display2in13::default());
    d.set_rotation(DisplayRotation::Rotate90); // 250 x 122 landscape
    match screen {
        Screen::Plan(opts) => draw_plan(&mut d, opts),
        Screen::Trip(label, opts) => draw_trip(&mut d, label, opts),
        Screen::Idle(label) => draw_message(&mut d, "Next buses", &format!("from {label}")),
        Screen::Message(title, detail) => draw_message(&mut d, title, detail),
    }
    epd.update_and_display_frame(&mut spi, d.buffer(), &mut delay)
        .map_err(|e| anyhow!("{e:?}"))?;
    epd.sleep(&mut spi, &mut delay)
        .map_err(|e| anyhow!("{e:?}"))?;
    Ok(())
}

fn text<F: u8g2_fonts::Font>(d: &mut Display2in13, s: &str, x: i32, y: i32) {
    let _ = FontRenderer::new::<F>().render(
        s,
        Point::new(x, y),
        VerticalPosition::Top,
        FontColor::Transparent(Color::Black),
        d,
    );
}

fn text_right<F: u8g2_fonts::Font>(d: &mut Display2in13, s: &str, right_x: i32, y: i32) {
    let _ = FontRenderer::new::<F>().render_aligned(
        s,
        Point::new(right_x, y),
        VerticalPosition::Top,
        HorizontalAlignment::Right,
        FontColor::Transparent(Color::Black),
        d,
    );
}

fn draw_message(d: &mut Display2in13, title: &str, detail: &str) {
    let _ = d.clear(Color::White);
    text::<fonts::u8g2_font_helvB18_tf>(d, title, 4, 30);
    text::<fonts::u8g2_font_helvR12_tf>(d, detail, 4, 64);
}

/// Right edge of the "arrives" time column (and the big leave time).
const RIGHT: i32 = 246;
/// Right edge of the "bus departs" time column. Shared by the top and bottom
/// rows so the columns line up even though the fonts differ in size.
const DEPART_COL: i32 = 198;
/// Left edge of the bus column in the small rows (after the longest stop name,
/// "Friedrichstrasse").
const BUS_COL: i32 = 108;

fn delay_suffix(delay: i64) -> String {
    if delay > 0 {
        format!(" +{delay}")
    } else {
        String::new()
    }
}

/// Second-trip view: latest good connection big, earlier fallbacks below.
fn draw_trip(d: &mut Display2in13, label: &str, opts: &[&RouteOpt]) {
    let _ = d.clear(Color::White);
    let Some(best) = opts.first() else { return };
    const GAP: i32 = 8;

    text::<fonts::u8g2_font_helvB18_tf>(d, &format!("{label}: leave"), 4, 2);
    text_right::<fonts::u8g2_font_helvB18_tf>(d, &best.leave, RIGHT, 2);
    text::<fonts::u8g2_font_helvR12_tf>(
        d,
        &format!("to {} ({}')", pretty_stop(&best.stop), best.walk_min),
        4,
        26,
    );
    let room = RIGHT - width::<fonts::u8g2_font_helvB12_tf>(&best.arrive) - GAP - 4;
    let chain = fit::<fonts::u8g2_font_helvB12_tf>(&best.chain, &best.chain_short, room);
    text::<fonts::u8g2_font_helvB12_tf>(d, chain, 4, 46);
    text_right::<fonts::u8g2_font_helvB12_tf>(d, &best.arrive, RIGHT, 46);

    let _ = Line::new(Point::new(0, 68), Point::new(249, 68))
        .into_styled(PrimitiveStyle::with_stroke(Color::Black, 1))
        .draw(d);

    // Earlier fallbacks: "Or 06:32 | Tram 7 · IR · S2 | 07:52", in fixed columns.
    let chain_col = 4 + width::<fonts::u8g2_font_helvB10_tf>("Or 00:00") + GAP;
    for (i, o) in opts.iter().skip(1).take(2).enumerate() {
        let y = 74 + 17 * i as i32;
        text::<fonts::u8g2_font_helvB10_tf>(d, &format!("Or {}", o.leave), 4, y);
        let room = RIGHT - width::<fonts::u8g2_font_helvB10_tf>(&o.arrive) - GAP - chain_col;
        let chain = fit::<fonts::u8g2_font_helvR10_tf>(&o.chain, &o.chain_short, room);
        text::<fonts::u8g2_font_helvR10_tf>(d, chain, chain_col, y);
        text_right::<fonts::u8g2_font_helvB10_tf>(d, &o.arrive, RIGHT, y);
    }
}

/// Rendered width of `s` in font `F`, in pixels.
fn width<F: u8g2_fonts::Font>(s: &str) -> i32 {
    FontRenderer::new::<F>()
        .get_rendered_dimensions(s, Point::zero(), VerticalPosition::Top)
        .ok()
        .and_then(|d| d.bounding_box)
        .map(|b| b.size.width as i32)
        .unwrap_or(0)
}

/// `full` if it fits in `room` pixels, otherwise the compact form.
fn fit<'a, F: u8g2_fonts::Font>(full: &'a str, short: &'a str, room: i32) -> &'a str {
    if short.is_empty() || width::<F>(full) <= room {
        full
    } else {
        short
    }
}

/// The API spells umlauts out at the start of stop names ("Ueberlandpark").
fn pretty_stop(name: &str) -> String {
    match name.strip_prefix("Ue") {
        Some(rest) => format!("Ü{rest}"),
        None => name.to_string(),
    }
}

fn draw_plan(d: &mut Display2in13, opts: &[Opt]) {
    let _ = d.clear(Color::White);
    let Some(best) = opts.first() else { return };

    // Best option, big. Labels on the left, times pinned to the right edge.
    text::<fonts::u8g2_font_helvB18_tf>(d, "Leave", 4, 2);
    text_right::<fonts::u8g2_font_helvB18_tf>(d, &best.leave, RIGHT, 2);
    text::<fonts::u8g2_font_helvR12_tf>(
        d,
        &format!("to {} ({}')", best.stop, best.walk_min),
        4,
        26,
    );
    let change = if best.transfers > 0 { " *" } else { "" };
    text::<fonts::u8g2_font_helvB12_tf>(
        d,
        &format!("{}{}{change}", best.line, delay_suffix(best.delay)),
        4,
        46,
    );
    text_right::<fonts::u8g2_font_helvB12_tf>(d, &best.depart, DEPART_COL, 46);
    text_right::<fonts::u8g2_font_helvB12_tf>(d, &best.arrive, RIGHT, 46);

    let _ = Line::new(Point::new(0, 68), Point::new(249, 68))
        .into_styled(PrimitiveStyle::with_stroke(Color::Black, 1))
        .draw(d);

    // Next options, small, in fixed columns: stop | bus | departs | arrives.
    for (i, o) in opts.iter().skip(1).take(2).enumerate() {
        let y = 74 + 17 * i as i32;
        text::<fonts::u8g2_font_helvR10_tf>(d, &o.stop, 4, y);
        text::<fonts::u8g2_font_helvR10_tf>(
            d,
            &format!("{}{}", o.line, delay_suffix(o.delay)),
            BUS_COL,
            y,
        );
        text_right::<fonts::u8g2_font_helvB10_tf>(d, &o.depart, DEPART_COL, y);
        text_right::<fonts::u8g2_font_helvB10_tf>(d, &o.arrive, RIGHT, y);
    }
}
