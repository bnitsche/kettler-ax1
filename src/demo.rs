//! Demo database for screenshots: Bob and Alice with four weeks of rides.

use std::path::Path;

use anyhow::{Result, bail};

use crate::db::{self, PersonUpdate, Recovery, RecoverySource, RideSummary};
use crate::program::{FlatSegment, target_at};
use crate::protocol::Status;
use crate::session::Sample;
use crate::sim::HeartModel;

const DAY: i64 = 86_400;
/// Rides of the last two weeks have a chest strap pulse (the strap came with 1.5.0).
const STRAP_DAYS: i64 = 14;
/// Maximum change of the actual watts per second, as on the device.
const RAMP_WATTS_PER_SEC: f64 = 15.0;

struct Rider {
    name: &'static str,
    weight_kg: f64,
    flat_watts: u16,
    rest_pulse: f64,
    /// Pulse drop over the four weeks at the same power (training progress).
    progress_bpm: f64,
    /// Days ago and program (`None` = free ride), oldest first.
    rides: &'static [(i64, Option<&'static str>)],
}

const RIDERS: [Rider; 2] = [
    Rider {
        name: "Bob",
        weight_kg: 85.0,
        flat_watts: 70,
        rest_pulse: 60.0,
        progress_bpm: 4.0,
        rides: &[
            (27, Some("Grundlage 30")),
            (25, Some("Pyramide")),
            (22, Some("4 × 4")),
            (20, Some("Grundlage 30")),
            (17, Some("Pyramide")),
            (14, Some("Rampentest")),
            (12, Some("4 × 4")),
            (9, Some("Grundlage 30")),
            (6, None),
            (3, Some("Pyramide")),
        ],
    },
    Rider {
        name: "Alice",
        weight_kg: 62.0,
        flat_watts: 55,
        rest_pulse: 64.0,
        progress_bpm: 3.0,
        rides: &[
            (26, Some("Grundlage 30")),
            (21, Some("Pyramide")),
            (15, Some("Grundlage 30")),
            (11, None),
            (7, Some("Pyramide")),
            (2, Some("Grundlage 30")),
        ],
    },
];

/// Small deterministic generator (xorshift64*), so two runs give the same database.
struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in `-amp..amp`.
    fn noise(&mut self, amp: f64) -> f64 {
        (self.unit() * 2.0 - 1.0) * amp
    }
}

/// The free ride: 30 minutes around 100 W with a step up and back down.
fn free_ride() -> Vec<FlatSegment> {
    [(0, 100), (600, 120), (1200, 100)]
        .into_iter()
        .map(|(start_secs, watts)| FlatSegment {
            name: String::new(),
            label: String::new(),
            start_secs,
            duration_secs: 600,
            start_watts: watts,
            end_watts: watts,
        })
        .collect()
}

/// Creates a new database at `path` with Bob, Alice and their rides in the four weeks
/// before `now_unix`. Refuses to touch an existing file.
pub fn generate(path: &Path, now_unix: i64) -> Result<()> {
    if path.exists() {
        bail!("{} exists already; demo data only goes into a new database", path.display());
    }
    let db = db::open(path)?;
    let strap = db.add_strap(crate::strap::SIM_ADDRESS, "HRM-Dual")?;
    let programs = db.list_programs()?;
    let today = now_unix.div_euclid(DAY) * DAY;
    let mut rng = Rng(0x4B45_5454_4C45_5221);
    for (index, rider) in RIDERS.iter().enumerate() {
        let person = db.add_person(rider.name)?;
        db.update_person(
            person,
            &PersonUpdate {
                weight_kg: Some(rider.weight_kg),
                flat_watts: Some(rider.flat_watts),
                strap_id: (index == 0).then_some(Some(strap)),
                ..PersonUpdate::default()
            },
        )?;
        for &(days_ago, name) in rider.rides {
            let program = name
                .map(|n| programs.iter().find(|p| p.name == n).ok_or_else(|| anyhow::anyhow!("no template {n}")))
                .transpose()?;
            let segments = program.map_or_else(free_ride, |p| p.expand());
            // Evenings between 17:00 and 19:00 UTC.
            let started_at = today - days_ago * DAY + 17 * 3600 + (rng.unit() * 7200.0) as i64;
            let (ride_id, params) = db.start_ride(person, program, started_at)?;
            let fitness = rider.progress_bpm * days_ago as f64 / 28.0;
            let strap_pulse = days_ago <= STRAP_DAYS;
            let samples = ride_samples(&segments, rider.rest_pulse + fitness, strap_pulse, started_at, &mut rng);
            db.add_samples(ride_id, &samples)?;
            let last = samples.last().expect("rides are not empty");
            // Every pyramid of Bob, the last one of Alice.
            if name == Some("Pyramide") && (index == 0 || days_ago == 7) {
                let p1 = last.effective_pulse().expect("demo samples have a pulse");
                // The pulse drops by 15 % of P1 a month ago and by 21 % now (grade about F3,8 → F1,6).
                let share = 0.15 + 0.06 * (28 - days_ago) as f64 / 28.0 + rng.noise(0.01);
                let difference = (f64::from(p1) * share).round() as u16;
                db.add_recovery(
                    ride_id,
                    &Recovery {
                        ride_secs: last.ride_secs,
                        pulse_start: p1,
                        difference,
                        grade_x10: crate::recovery::grade_x10(p1, difference),
                        at_unix_ms: (started_at + i64::from(last.ride_secs) + 60) * 1000,
                        source: if strap_pulse { RecoverySource::Strap } else { RecoverySource::Cockpit },
                    },
                )?;
            }
            let stats = db::summarize(&samples, &params);
            db.finish_ride(
                ride_id,
                &RideSummary {
                    id: ride_id,
                    person_id: person,
                    program_name: name.map(String::from),
                    started_at,
                    ride_secs: stats.ride_secs,
                    avg_watts: stats.avg_watts,
                    max_watts: stats.max_watts,
                    avg_pulse: stats.avg_pulse,
                    max_pulse: stats.max_pulse,
                    distance_m: stats.distance_m,
                    energy_kj: stats.energy_kj,
                    finished: true,
                    ended_at: Some(started_at + i64::from(stats.ride_secs)),
                    avg_rpm: stats.avg_rpm,
                    max_rpm: stats.max_rpm,
                    weight_kg: params.weight_kg,
                    flat_watts: params.flat_watts,
                    elevation_gain_m: stats.elevation_gain_m,
                },
            )?;
        }
    }
    Ok(())
}

/// One sample per second over `segments`, like a ride with the cockpit (and the strap).
fn ride_samples(segments: &[FlatSegment], rest_pulse: f64, strap: bool, started_at: i64, rng: &mut Rng) -> Vec<Sample> {
    let total: u32 = segments.last().map_or(0, |s| s.start_secs + s.duration_secs);
    let mut heart = HeartModel::new(rest_pulse);
    let (mut actual, mut drift, mut distance_km, mut energy_kj) = (0.0_f64, 0.0_f64, 0.0_f64, 0.0_f64);
    (1..=total)
        .map(|secs| {
            let target = target_at(segments, secs - 1).map_or(0, |(_, w)| w);
            actual += (f64::from(target) - actual).clamp(-RAMP_WATTS_PER_SEC, RAMP_WATTS_PER_SEC);
            // The brake holds the power closely: a slow drift of a few watts.
            drift = 0.9 * drift + rng.noise(0.6);
            let watts = (actual + drift).max(0.0);
            let rpm = (89.0 + 3.0 * (f64::from(secs) / 23.0).sin() + rng.noise(1.5)).round().clamp(84.0, 94.0) as u16;
            let speed_x10 = u32::from(rpm) * 35 / 10;
            heart.step(watts, true, 1.0);
            distance_km += f64::from(speed_x10) / 10.0 / 3600.0;
            energy_kj += watts * 4.0 / 1000.0;
            Sample {
                ride_secs: secs,
                status: Status {
                    pulse: (heart.bpm() + rng.noise(1.5)).round() as u16,
                    rpm,
                    speed_x10: speed_x10 as u16,
                    distance: (distance_km * 10.0) as u16,
                    target_watts: target,
                    energy: energy_kj as u16,
                    elapsed_secs: secs,
                    actual_watts: watts.round() as u16,
                    recovery: false,
                },
                target_watts: target,
                at_unix_ms: Some((started_at + i64::from(secs)) * 1000),
                strap_pulse: strap.then(|| (heart.bpm() - 1.0 + rng.noise(0.7)).round() as u16),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{self, RecoverySource};

    /// 2026-10-29 12:00 UTC.
    const NOW: i64 = 1_793_275_200;
    const DAY: i64 = 86_400;

    fn generated(dir: &tempfile::TempDir, name: &str) -> db::Db {
        let path = dir.path().join(name);
        generate(&path, NOW).unwrap();
        db::open(&path).unwrap()
    }

    #[test]
    fn creates_bob_and_alice() {
        let dir = tempfile::tempdir().unwrap();
        let db = generated(&dir, "demo.db");
        let persons = db.list_persons().unwrap();
        let names: Vec<&str> = persons.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Bob", "Alice"]);
        assert_eq!((persons[0].weight_kg, persons[0].flat_watts), (85.0, 70));
        assert_eq!((persons[1].weight_kg, persons[1].flat_watts), (62.0, 55));
        let straps = db.list_straps().unwrap();
        assert_eq!(persons[0].strap_id, Some(straps[0].id));
        assert_eq!(straps[0].address, crate::strap::SIM_ADDRESS);
    }

    #[test]
    fn bob_has_comparable_pyramids() {
        let dir = tempfile::tempdir().unwrap();
        let db = generated(&dir, "demo.db");
        let bob = db.list_persons().unwrap()[0].id;
        let pyramids: Vec<_> = db
            .list_rides(bob)
            .unwrap()
            .into_iter()
            .filter(|r| r.program_name.as_deref() == Some("Pyramide"))
            .collect();
        assert!(pyramids.len() >= 3, "{}", pyramids.len());
        for r in &pyramids {
            assert!(r.finished);
            assert!(r.elevation_gain_m > 0.0, "{}", r.elevation_gain_m);
            assert!((84..=94).contains(&r.avg_rpm.unwrap()), "{:?}", r.avg_rpm);
            assert!(r.avg_pulse.is_some());
        }
        // Newest first: the newest pyramid was ridden with a lower pulse than the oldest.
        assert!(pyramids[0].avg_pulse < pyramids[pyramids.len() - 1].avg_pulse);
    }

    #[test]
    fn ride_detail_has_strap_and_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let db = generated(&dir, "demo.db");
        let bob = db.list_persons().unwrap()[0].id;
        let newest = db
            .list_rides(bob)
            .unwrap()
            .into_iter()
            .find(|r| r.program_name.as_deref() == Some("Pyramide"))
            .unwrap();
        let detail = db.get_ride(newest.id).unwrap().unwrap();
        assert!(detail.samples.iter().all(|s| s.strap_pulse.is_some()));
        assert!(detail.recoveries.iter().any(|r| r.source == RecoverySource::Strap));
    }

    #[test]
    fn recovery_grades_are_plausible() {
        let dir = tempfile::tempdir().unwrap();
        let db = generated(&dir, "demo.db");
        for person in db.list_persons().unwrap() {
            for ride in db.list_rides(person.id).unwrap() {
                for r in db.get_ride(ride.id).unwrap().unwrap().recoveries {
                    assert!((15..=45).contains(&r.grade_x10), "F{}", r.grade_x10);
                }
            }
        }
    }

    #[test]
    fn watts_stay_close_to_target() {
        let dir = tempfile::tempdir().unwrap();
        let db = generated(&dir, "demo.db");
        let bob = db.list_persons().unwrap()[0].id;
        let ride = db.list_rides(bob).unwrap()[0].id;
        let samples = db.get_ride(ride).unwrap().unwrap().samples;
        // Past the start-up from 0 W, and 3 s after a stage change, the power moves by a few
        // watts a second.
        for w in samples[10..].windows(4) {
            if w.iter().all(|s| s.target_watts == w[0].target_watts) {
                let step = w[2].status.actual_watts.abs_diff(w[3].status.actual_watts);
                assert!(step <= 3, "{step} W at {}", w[3].ride_secs);
            }
        }
    }

    #[test]
    fn rides_lie_in_the_past_four_weeks() {
        let dir = tempfile::tempdir().unwrap();
        let db = generated(&dir, "demo.db");
        for person in db.list_persons().unwrap() {
            for r in db.list_rides(person.id).unwrap() {
                assert!((NOW - 28 * DAY..NOW - DAY).contains(&r.started_at), "{}", r.started_at);
            }
        }
    }

    #[test]
    fn deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (generated(&dir, "a.db"), generated(&dir, "b.db"));
        for person in a.list_persons().unwrap() {
            assert_eq!(a.list_rides(person.id).unwrap(), b.list_rides(person.id).unwrap());
        }
    }

    #[test]
    fn refuses_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kettler.db");
        std::fs::write(&path, b"keep me").unwrap();
        assert!(generate(&path, NOW).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep me");
    }
}
