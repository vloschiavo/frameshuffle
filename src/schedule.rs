//! Per-client time windows. A client outside its window is served the current image (or 204)
//! and told, via X-Next-Refresh, when to come back.

use crate::config::WindowCfg;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, Duration, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

#[derive(Debug, Clone)]
struct Win {
    days: [bool; 7], // Monday = 0
    start: NaiveTime,
    end: NaiveTime,
}

#[derive(Debug, Clone)]
pub struct Windows {
    tz: Tz,
    wins: Vec<Win>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Open,
    /// Seconds until the window next opens.
    Closed {
        opens_in: u64,
    },
}

fn parse_time(s: &str) -> Result<NaiveTime> {
    if s == "24:00" {
        return Ok(NaiveTime::from_hms_nano_opt(23, 59, 59, 999_999_999).unwrap());
    }
    NaiveTime::parse_from_str(s, "%H:%M").with_context(|| format!("bad time '{s}', expected HH:MM"))
}

fn parse_day(s: &str) -> Result<usize> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "mon" | "monday" => 0,
        "tue" | "tuesday" => 1,
        "wed" | "wednesday" => 2,
        "thu" | "thursday" => 3,
        "fri" | "friday" => 4,
        "sat" | "saturday" => 5,
        "sun" | "sunday" => 6,
        _ => bail!("bad day '{s}'"),
    })
}

impl Windows {
    pub fn new(tz: &str, cfg: &[WindowCfg]) -> Result<Windows> {
        let tz: Tz = tz
            .parse()
            .map_err(|_| anyhow::anyhow!("unknown timezone '{tz}'"))?;
        let mut wins = Vec::new();
        for w in cfg {
            let mut days = [w.days.is_empty(); 7];
            for d in &w.days {
                days[parse_day(d)?] = true;
            }
            let (start, end) = (parse_time(&w.start)?, parse_time(&w.end)?);
            if start == end {
                bail!("window start and end are identical ({})", w.start);
            }
            wins.push(Win { days, start, end });
        }
        Ok(Windows { tz, wins })
    }

    pub fn is_unrestricted(&self) -> bool {
        self.wins.is_empty()
    }

    pub fn state_at(&self, at: DateTime<Utc>) -> State {
        if self.wins.is_empty() {
            return State::Open;
        }
        let local = at.with_timezone(&self.tz);
        let t = local.time();
        let dow = local.date_naive().weekday().num_days_from_monday() as usize;
        for w in &self.wins {
            let open = if w.start < w.end {
                w.days[dow] && t >= w.start && t < w.end
            } else {
                // Overnight: either the evening part (today) or the morning tail of yesterday's window.
                (w.days[dow] && t >= w.start) || (w.days[(dow + 6) % 7] && t < w.end)
            };
            if open {
                return State::Open;
            }
        }
        // Closed: find the earliest upcoming start.
        let mut best: Option<DateTime<Utc>> = None;
        for off in 0..=7i64 {
            let date = local.date_naive() + Duration::days(off);
            let d = date.weekday().num_days_from_monday() as usize;
            for w in &self.wins {
                if !w.days[d] {
                    continue;
                }
                if let Some(cand) = self
                    .tz
                    .from_local_datetime(&date.and_time(w.start))
                    .earliest()
                {
                    let cand = cand.with_timezone(&Utc);
                    if cand > at && best.is_none_or(|b| cand < b) {
                        best = Some(cand);
                    }
                }
            }
        }
        match best {
            Some(b) => State::Closed {
                opens_in: (b - at).num_seconds().max(1) as u64,
            },
            // Unreachable in practice (a window with no days). Treat as open rather than lock the frame out.
            None => State::Open,
        }
    }

    /// Seconds from `now` until the client should ask again, given it wants to refresh after `wait` seconds.
    /// If that moment falls outside the window, it is pushed to the next opening.
    pub fn next_refresh(&self, now: DateTime<Utc>, wait: u64) -> u64 {
        let target = now + Duration::seconds(wait as i64);
        match self.state_at(target) {
            State::Open => wait,
            State::Closed { opens_in } => wait + opens_in,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(days: &[&str], s: &str, e: &str) -> WindowCfg {
        WindowCfg {
            days: days.iter().map(|d| d.to_string()).collect(),
            start: s.into(),
            end: e.into(),
        }
    }
    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn daytime_window() {
        let ws = Windows::new("UTC", &[w(&[], "07:00", "22:00")]).unwrap();
        assert_eq!(ws.state_at(utc("2026-11-09T12:00:00Z")), State::Open);
        assert_eq!(
            ws.state_at(utc("2026-11-09T23:00:00Z")),
            State::Closed { opens_in: 8 * 3600 }
        );
        assert_eq!(
            ws.state_at(utc("2026-11-09T03:00:00Z")),
            State::Closed { opens_in: 4 * 3600 }
        );
    }

    #[test]
    fn overnight_window() {
        let ws = Windows::new("UTC", &[w(&["fri"], "22:00", "02:00")]).unwrap();
        // 2026-11-13 is a Friday.
        assert_eq!(ws.state_at(utc("2026-11-13T23:00:00Z")), State::Open);
        assert_eq!(ws.state_at(utc("2026-11-14T01:00:00Z")), State::Open); // Saturday morning tail
        assert!(matches!(
            ws.state_at(utc("2026-11-14T03:00:00Z")),
            State::Closed { .. }
        ));
    }

    #[test]
    fn weekday_only_skips_weekend() {
        let ws = Windows::new(
            "UTC",
            &[w(&["mon", "tue", "wed", "thu", "fri"], "08:00", "18:00")],
        )
        .unwrap();
        // Friday 19:00 -> next opening Monday 08:00 = 61h
        assert_eq!(
            ws.state_at(utc("2026-11-13T19:00:00Z")),
            State::Closed {
                opens_in: 61 * 3600
            }
        );
    }

    #[test]
    fn timezone_and_next_refresh() {
        let ws = Windows::new("America/Los_Angeles", &[w(&[], "07:00", "22:00")]).unwrap();
        // 2026-11-09 06:00 UTC = Sun 22:00 PST -> closed, opens 07:00 PST = 15:00 UTC (9h)
        let now = utc("2026-11-09T06:00:00Z");
        assert_eq!(ws.state_at(now), State::Closed { opens_in: 9 * 3600 });
        // Wants a refresh in 1h, but that is still closed -> 1h + remaining 8h
        assert_eq!(ws.next_refresh(now, 3600), 9 * 3600);
    }

    #[test]
    fn rejects_garbage() {
        assert!(Windows::new("Nowhere/City", &[]).is_err());
        assert!(Windows::new("UTC", &[w(&["funday"], "07:00", "08:00")]).is_err());
        assert!(Windows::new("UTC", &[w(&[], "7am", "08:00")]).is_err());
        assert!(Windows::new("UTC", &[w(&[], "07:00", "07:00")]).is_err());
    }
}
