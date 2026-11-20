//! Cloudflare Worker: "which bus should I take to Oerlikon?"
//!
//! All requests need `x-token: <TOKEN>` (or `?token=<TOKEN>`).
//!
//! GET /                 plain-text plan from now (for humans / curl)
//! GET /?format=json     same, as JSON
//! GET /?format=day      the e-paper board's schedule: every option left in today's
//!                       morning window, the current time (the board sets its clock
//!                       from it), and when the board should next wake to sync.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Weekday};
use serde::{Deserialize, Serialize};
use worker::*;

const API: &str = "https://transport.opendata.ch/v1/connections";

// ---------- SBB API types ----------

#[derive(Deserialize)]
struct Resp {
    #[serde(default)]
    connections: Vec<Connection>,
}

#[derive(Deserialize)]
struct Connection {
    from: Stop,
    to: Stop,
    transfers: Option<u32>,
    #[serde(default)]
    sections: Vec<Section>,
}

#[derive(Deserialize)]
struct Section {
    journey: Option<Journey>,
    walk: Option<Walk>,
    departure: Stop,
}

#[derive(Deserialize)]
struct Journey {
    category: Option<String>,
    operator: Option<String>,
    number: Option<String>,
    to: Option<String>,
}

#[derive(Deserialize)]
struct Walk {
    duration: Option<i64>,
}

#[derive(Deserialize)]
struct Stop {
    station: Option<Station>,
    departure: Option<String>,
    #[serde(rename = "departureTimestamp")]
    departure_ts: Option<i64>,
    arrival: Option<String>,
    #[serde(rename = "arrivalTimestamp")]
    arrival_ts: Option<i64>,
    delay: Option<i64>,
}

#[derive(Deserialize)]
struct Station {
    name: Option<String>,
    coordinate: Option<Coord>,
}

#[derive(Deserialize)]
struct Coord {
    x: Option<f64>, // latitude (the API's naming)
    y: Option<f64>, // longitude
}

// ---------- our output ----------

#[derive(Serialize, Clone)]
struct Opt {
    leave_ts: i64,
    leave: String,
    leave_in: i64,
    walk_dir: String,
    walk_min: i64,
    stop: String,
    line: String,
    towards: String,
    depart: String,
    delay: i64,
    arrive: String,
    transfers: u32,
}

#[derive(Serialize)]
struct Plan {
    to: String,
    options: Vec<Opt>,
}

#[derive(Serialize)]
struct Day {
    to: String,
    /// Current unix time (seconds). The board sets its clock from this.
    now: i64,
    /// Unix time the board should next wake and sync once today's options run out.
    next_sync: i64,
    /// e.g. "Mon 06:20", shown on the screen while idle.
    next_sync_label: String,
    /// Remaining options in today's window (empty outside the window).
    options: Vec<Opt>,
    /// Second trip (e.g. Zug, arrive by 08:00): latest good connection first.
    #[serde(skip_serializing_if = "Option::is_none")]
    trip2: Option<Trip>,
}

#[derive(Serialize)]
struct Trip {
    label: String,
    options: Vec<RouteOpt>,
}

/// A multi-leg route, summarised for a small screen.
#[derive(Serialize, Clone)]
struct RouteOpt {
    leave_ts: i64,
    leave: String,
    walk_min: i64,
    stop: String,
    /// e.g. "Bus 62 · S8 · IR · S2"
    chain: String,
    /// Same, compact, for when `chain` doesn't fit: "B62 · S8 · IR · S2"
    chain_short: String,
    arrive: String,
    transfers: u32,
}

// ---------- time helpers (Europe/Zurich without a tz database) ----------

fn last_sunday(year: i32, month: u32) -> NaiveDate {
    let mut d = NaiveDate::from_ymd_opt(year, month + 1, 1).unwrap() - Duration::days(1);
    while d.weekday() != Weekday::Sun {
        d -= Duration::days(1);
    }
    d
}

/// Europe/Zurich UTC offset (seconds) at unix time `ts`. EU rules: summer time from
/// the last Sunday of March 01:00 UTC to the last Sunday of October 01:00 UTC.
fn zurich_offset(ts: i64) -> i64 {
    let utc = DateTime::from_timestamp(ts, 0).unwrap().naive_utc();
    let one = NaiveTime::from_hms_opt(1, 0, 0).unwrap();
    let start = last_sunday(utc.year(), 3).and_time(one);
    let end = last_sunday(utc.year(), 10).and_time(one);
    if utc >= start && utc < end {
        7200
    } else {
        3600
    }
}

fn local(ts: i64) -> NaiveDateTime {
    DateTime::from_timestamp(ts + zurich_offset(ts), 0).unwrap().naive_utc()
}

/// Local wall-clock time on `date` -> unix time (exact except during the 02:00-03:00 DST jump).
fn unix_at(date: NaiveDate, t: NaiveTime) -> i64 {
    let naive = date.and_time(t).and_utc().timestamp();
    naive - zurich_offset(naive - 3600)
}

fn parse_hhmm(s: &str) -> NaiveTime {
    NaiveTime::parse_from_str(s, "%H:%M").unwrap_or(NaiveTime::from_hms_opt(6, 30, 0).unwrap())
}

fn parse_days(s: &str) -> Vec<Weekday> {
    s.split(',').filter_map(|d| d.trim().parse::<Weekday>().ok()).collect()
}

// ---------- SBB lookups ----------

/// "2026-11-18T07:34:00+0100" -> "07:34"
fn hhmm(s: &Option<String>) -> &str {
    s.as_deref().and_then(|s| s.get(11..16)).unwrap_or("--:--")
}

/// Compass direction (8 points) from (lat1, lon1) to (lat2, lon2).
fn compass(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> &'static str {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dl = (lon2 - lon1).to_radians();
    let y = dl.sin() * p2.cos();
    let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos();
    let deg = (y.atan2(x).to_degrees() + 360.0) % 360.0;
    const DIRS: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    DIRS[(((deg + 22.5) / 45.0) as usize) % 8]
}

/// Kind of vehicle for one ride. The API sometimes omits the category for VBZ
/// (Zurich city) lines; there, lines 2-17 are trams and the rest are buses.
fn leg_kind(j: &Journey) -> (&'static str, String) {
    let num = j.number.as_deref().unwrap_or("").trim_start_matches('0').to_string();
    let kind = match j.category.as_deref().unwrap_or("") {
        "B" | "BUS" => "Bus",
        "T" => "Tram",
        "S" | "SN" => "S",
        "" => {
            let vbz = j.operator.as_deref().unwrap_or("").starts_with("VBZ");
            match num.parse::<u32>() {
                Ok(n) if vbz && (2..=17).contains(&n) => "Tram",
                Ok(_) => "Bus",
                Err(_) => "",
            }
        }
        _ => "Long", // IR, IC, EC, RE...
    };
    (kind, num)
}

/// "Bus 62", "Tram 7", "S8", "IR" (long-distance: the number doesn't help).
fn leg_label(j: &Journey) -> String {
    match leg_kind(j) {
        ("Bus", n) => format!("Bus {n}"),
        ("Tram", n) => format!("Tram {n}"),
        ("S", n) => format!("S{n}"),
        ("Long", _) => j.category.clone().unwrap_or_default(),
        (_, n) => n,
    }
}

/// Compact form: "B62", "T7", "S8", "IR".
fn leg_label_short(j: &Journey) -> String {
    match leg_kind(j) {
        ("Bus", n) => format!("B{n}"),
        ("Tram", n) => format!("T{n}"),
        ("S", n) => format!("S{n}"),
        ("Long", _) => j.category.clone().unwrap_or_default(),
        (_, n) => n,
    }
}

/// Connections that arrive by `arrive_by` and leave at or after `now`,
/// latest departure first.
async fn fetch_arrive_by(
    from: &str,
    walk_min: i64,
    to: &str,
    date: NaiveDate,
    arrive_by: NaiveTime,
    arrive_by_ts: i64,
    now: i64,
) -> Result<Vec<RouteOpt>> {
    let mut api = Url::parse(API)?;
    api.query_pairs_mut()
        .append_pair("from", from)
        .append_pair("to", to)
        .append_pair("date", &date.format("%Y-%m-%d").to_string())
        .append_pair("time", &arrive_by.format("%H:%M").to_string())
        .append_pair("isArrivalTime", "1")
        .append_pair("limit", "6");
    let mut r = Fetch::Url(api).send().await?;
    if r.status_code() != 200 {
        return Err(Error::RustError(format!("SBB API error {}", r.status_code())));
    }
    let resp: Resp = r.json().await?;

    let mut out: Vec<RouteOpt> = resp
        .connections
        .iter()
        .filter_map(|c| {
            // Planned from a fixed stop; she walks `walk_min` from home to reach it.
            let leave_ts = c.from.departure_ts? - walk_min * 60;
            if leave_ts < now || c.to.arrival_ts? > arrive_by_ts {
                return None;
            }
            let rides: Vec<&Section> = c.sections.iter().filter(|s| s.journey.is_some()).collect();
            let first = rides.first()?;
            Some(RouteOpt {
                leave_ts,
                leave: local(leave_ts).format("%H:%M").to_string(),
                walk_min,
                stop: first
                    .departure
                    .station
                    .as_ref()
                    .and_then(|s| s.name.as_deref())
                    .map(short)
                    .unwrap_or("?")
                    .to_string(),
                chain: rides
                    .iter()
                    .map(|s| leg_label(s.journey.as_ref().unwrap()))
                    .collect::<Vec<_>>()
                    .join(" · "),
                chain_short: rides
                    .iter()
                    .map(|s| leg_label_short(s.journey.as_ref().unwrap()))
                    .collect::<Vec<_>>()
                    .join(" · "),
                arrive: hhmm(&c.to.arrival).to_string(),
                transfers: c.transfers.unwrap_or(0),
            })
        })
        .collect();
    out.sort_by_key(|o| std::cmp::Reverse(o.leave_ts));
    out.truncate(3);
    Ok(out)
}

fn short(name: &str) -> &str {
    name.strip_prefix("Zürich, ").unwrap_or(name)
}

struct Ctx {
    home: String,
    dest: String,
    lat: f64,
    lon: f64,
}

/// One connections query (up to 16 results). Keeps options leaving at or after `now`.
async fn fetch_options(
    ctx: &Ctx,
    at: Option<NaiveDateTime>,
    now: i64,
) -> Result<Vec<Opt>> {
    let mut api = Url::parse(API)?;
    {
        let mut q = api.query_pairs_mut();
        q.append_pair("from", &ctx.home)
            .append_pair("to", &ctx.dest)
            .append_pair("limit", "16");
        if let Some(at) = at {
            q.append_pair("date", &at.format("%Y-%m-%d").to_string())
                .append_pair("time", &at.format("%H:%M").to_string());
        }
    }
    let mut r = Fetch::Url(api).send().await?;
    if r.status_code() != 200 {
        return Err(Error::RustError(format!("SBB API error {}", r.status_code())));
    }
    let resp: Resp = r.json().await?;

    let mut out = Vec::new();
    for c in &resp.connections {
        let Some(leave_ts) = c.from.departure_ts else { continue };
        if leave_ts < now {
            continue;
        }
        let walk_min = c
            .sections
            .iter()
            .find_map(|s| s.walk.as_ref()?.duration)
            .map(|s| (s + 59) / 60)
            .unwrap_or(0);
        let Some(ride) = c.sections.iter().find(|s| s.journey.is_some()) else { continue };
        let j = ride.journey.as_ref().unwrap();
        let stop = ride.departure.station.as_ref();
        let category = match j.category.as_deref().unwrap_or("") {
            "B" | "BUS" => "Bus",
            "T" => "Tram",
            other => other,
        };
        out.push(Opt {
            leave_ts,
            leave: hhmm(&c.from.departure).to_string(),
            leave_in: (leave_ts - now) / 60,
            walk_dir: stop
                .and_then(|s| s.coordinate.as_ref())
                .and_then(|c| Some(compass(ctx.lat, ctx.lon, c.x?, c.y?)))
                .unwrap_or("?")
                .to_string(),
            walk_min,
            stop: stop.and_then(|s| s.name.as_deref()).map(short).unwrap_or("?").to_string(),
            line: format!("{category} {}", j.number.as_deref().unwrap_or("")),
            towards: short(j.to.as_deref().unwrap_or("?")).to_string(),
            depart: hhmm(&ride.departure.departure).to_string(),
            delay: ride.departure.delay.unwrap_or(0).max(0),
            arrive: hhmm(&c.to.arrival).to_string(),
            transfers: c.transfers.unwrap_or(0),
        });
    }
    Ok(out)
}

async fn day_schedule(ctx: &Ctx, env: &Env, now: i64) -> Result<Day> {
    let start = parse_hhmm(&env.var("WINDOW_START")?.to_string());
    let end = parse_hhmm(&env.var("WINDOW_END")?.to_string());
    let lead_min: i64 = env.var("SYNC_LEAD_MIN")?.to_string().parse().unwrap_or(10);
    let days = parse_days(&env.var("ACTIVE_DAYS")?.to_string());

    let today = local(now).date();
    let start_ts = unix_at(today, start);
    let end_ts = unix_at(today, end);

    // Every option left in today's window, paging 16 at a time.
    let mut options: Vec<Opt> = Vec::new();
    if days.contains(&today.weekday()) && now < end_ts {
        let mut cursor = now.max(start_ts);
        for _ in 0..6 {
            let batch = fetch_options(ctx, Some(local(cursor)), now).await?;
            let mut added = false;
            for o in batch {
                if o.leave_ts > end_ts
                    || options.iter().any(|p| p.leave_ts == o.leave_ts && p.line == o.line)
                {
                    continue;
                }
                cursor = cursor.max(o.leave_ts + 60);
                options.push(o);
                added = true;
            }
            if !added || cursor > end_ts {
                break;
            }
        }
        options.sort_by_key(|o| o.leave_ts);
    }

    // Next sync: the window start minus the lead, on the next active day still ahead.
    let (sync_time, _) = start.overflowing_sub_signed(Duration::minutes(lead_min));
    let mut d = today;
    let next_sync = loop {
        let ts = unix_at(d, sync_time);
        if days.contains(&d.weekday()) && ts > now {
            break ts;
        }
        d = d.succ_opt().unwrap();
        if d > today + Duration::days(8) {
            break now + 86_400; // no active days configured
        }
    };
    // Label shows when buses start (window start), not the earlier sync time.
    let next_sync_label = local(next_sync + lead_min * 60).format("%a %H:%M").to_string();

    // Second trip: only on active days, until its arrive-by time.
    let mut trip2 = None;
    if let (Ok(to), Ok(label), Ok(by)) = (
        env.secret("TRIP2_TO"),
        env.var("TRIP2_LABEL"),
        env.var("TRIP2_ARRIVE_BY"),
    ) {
        let by = parse_hhmm(&by.to_string());
        let by_ts = unix_at(today, by);
        if days.contains(&today.weekday()) && now < by_ts {
            let from = env.var("TRIP2_FROM")?.to_string();
            let walk: i64 = env.var("TRIP2_WALK_MIN")?.to_string().parse().unwrap_or(0);
            let options =
                fetch_arrive_by(&from, walk, &to.to_string(), today, by, by_ts, now).await?;
            if !options.is_empty() {
                trip2 = Some(Trip { label: label.to_string(), options });
            }
        }
    }

    Ok(Day {
        to: short(&ctx.dest).to_string(),
        now,
        next_sync,
        next_sync_label,
        options,
        trip2,
    })
}

fn no_store(mut res: Response) -> Result<Response> {
    res.headers_mut().set("cache-control", "no-store")?;
    Ok(res)
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let token = env.secret("TOKEN")?.to_string();
    let url = req.url()?;
    let given = req.headers().get("x-token")?.or_else(|| {
        url.query_pairs()
            .find(|(k, _)| k == "token")
            .map(|(_, v)| v.into_owned())
    });
    if given.as_deref() != Some(token.as_str()) {
        return Response::error("unauthorized", 401);
    }

    let ctx = Ctx {
        home: env.secret("HOME_ADDRESS")?.to_string(),
        dest: env.var("DESTINATION")?.to_string(),
        lat: env.secret("HOME_LAT")?.to_string().parse().unwrap_or(0.0),
        lon: env.secret("HOME_LON")?.to_string().parse().unwrap_or(0.0),
    };
    let now = (Date::now().as_millis() / 1000) as i64;
    let format = url
        .query_pairs()
        .find(|(k, _)| k == "format")
        .map(|(_, v)| v.into_owned());

    if format.as_deref() == Some("day") {
        // `?at=<unix>` simulates another time (testing only).
        let at = url
            .query_pairs()
            .find(|(k, _)| k == "at")
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(now);
        return match day_schedule(&ctx, &env, at).await {
            Ok(day) => no_store(Response::from_json(&day)?),
            Err(e) => Response::error(format!("{e}"), 502),
        };
    }

    let mut options = fetch_options(&ctx, None, now).await?;
    options.truncate(3);

    if format.as_deref() == Some("json") {
        let plan = Plan { to: short(&ctx.dest).to_string(), options };
        return no_store(Response::from_json(&plan)?);
    }

    let mut out = format!("To {}\n", short(&ctx.dest));
    for (i, o) in options.iter().enumerate() {
        let delay = if o.delay > 0 { format!(" (+{})", o.delay) } else { String::new() };
        let transfers = if o.transfers > 0 { format!(", {} change", o.transfers) } else { String::new() };
        out.push_str(&format!(
            "{}) Leave in {} min ({})\n   Walk {} {} min to {}\n   {} dir {} at {}{delay}\n   Arrive {}{transfers}\n",
            i + 1, o.leave_in, o.leave, o.walk_dir, o.walk_min, o.stop, o.line, o.towards, o.depart, o.arrive,
        ));
    }
    if options.is_empty() {
        out.push_str("No connections found.\n");
    }
    let mut res = Response::ok(out)?;
    res.headers_mut().set("content-type", "text/plain; charset=utf-8")?;
    no_store(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zurich_dst() {
        // 2026-11-17 12:00 UTC -> winter, +1h
        assert_eq!(zurich_offset(1_794_916_800), 3600);
        // 2026-07-01 12:00 UTC -> summer, +2h
        assert_eq!(zurich_offset(1_782_907_200), 7200);
        // 06:20 local on 2026-11-18 = 05:20 UTC
        let d = NaiveDate::from_ymd_opt(2026, 11, 18).unwrap();
        let t = NaiveTime::from_hms_opt(6, 20, 0).unwrap();
        assert_eq!(local(unix_at(d, t)).time(), t);
    }
}
