use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::protocol::DeviceLimits;

const MAX_REPEAT: u16 = 50;
const MAX_STEP_SECS: u32 = 7200;
const MAX_TOTAL_SECS: u32 = 21600;
/// Bounds the expanded program, which travels with every live view.
const MAX_SEGMENTS: u64 = 500;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Program {
    pub id: Option<i64>,
    pub name: String,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    pub repeat: u16,
    pub steps: Vec<Step>,
}

/// A ramp when `start_watts != end_watts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub name: String,
    pub duration_secs: u32,
    pub start_watts: u16,
    pub end_watts: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlatSegment {
    pub name: String,
    pub label: String,
    pub start_secs: u32,
    pub duration_secs: u32,
    pub start_watts: u16,
    pub end_watts: u16,
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum ProgramError {
    #[error("Programm braucht einen Namen")]
    MissingName,
    #[error("Programm braucht mindestens einen Block")]
    NoBlocks,
    #[error("Block {0}: Wiederholungen müssen zwischen 1 und {max} liegen", max = MAX_REPEAT)]
    BadRepeat(usize),
    #[error("Block {0} braucht mindestens einen Abschnitt")]
    EmptyBlock(usize),
    #[error("Block {block}: Abschnitt braucht einen Namen")]
    MissingStepName { block: usize },
    #[error("Block {block}, Abschnitt „{name}“: Dauer muss zwischen 1 s und {h} h liegen", h = MAX_STEP_SECS / 3600)]
    BadDuration { block: usize, name: String },
    #[error("Block {block}, Abschnitt „{name}“: Watt müssen zwischen {min} und {max} liegen")]
    BadWatts {
        block: usize,
        name: String,
        min: u16,
        max: u16,
    },
    #[error("Programm ist zu lang (höchstens {h} h)", h = MAX_TOTAL_SECS / 3600)]
    TooLong,
    #[error("Programm hat zu viele Abschnitte (höchstens {MAX_SEGMENTS})")]
    TooManySegments,
}

impl Program {
    /// `expand` and `total_secs` assume a program that passed `validate`.
    pub fn validate(&self, limits: &DeviceLimits) -> Result<(), ProgramError> {
        if self.name.trim().is_empty() {
            return Err(ProgramError::MissingName);
        }
        if self.blocks.is_empty() {
            return Err(ProgramError::NoBlocks);
        }
        let mut total: u64 = 0;
        let mut segments: u64 = 0;
        for (i, block) in self.blocks.iter().enumerate() {
            let n = i + 1;
            if !(1..=MAX_REPEAT).contains(&block.repeat) {
                return Err(ProgramError::BadRepeat(n));
            }
            if block.steps.is_empty() {
                return Err(ProgramError::EmptyBlock(n));
            }
            for step in &block.steps {
                if step.name.trim().is_empty() {
                    return Err(ProgramError::MissingStepName { block: n });
                }
                if !(1..=MAX_STEP_SECS).contains(&step.duration_secs) {
                    return Err(ProgramError::BadDuration {
                        block: n,
                        name: step.name.clone(),
                    });
                }
                let range = limits.min..=limits.max;
                if !range.contains(&step.start_watts) || !range.contains(&step.end_watts) {
                    return Err(ProgramError::BadWatts {
                        block: n,
                        name: step.name.clone(),
                        min: limits.min,
                        max: limits.max,
                    });
                }
                total += u64::from(step.duration_secs) * u64::from(block.repeat);
            }
            segments += block.steps.len() as u64 * u64::from(block.repeat);
        }
        if total > u64::from(MAX_TOTAL_SECS) {
            return Err(ProgramError::TooLong);
        }
        if segments > MAX_SEGMENTS {
            return Err(ProgramError::TooManySegments);
        }
        Ok(())
    }

    /// Assumes a validated program.
    pub fn expand(&self) -> Vec<FlatSegment> {
        let mut out = Vec::new();
        let mut t = 0u32;
        for block in &self.blocks {
            for i in 1..=block.repeat {
                for step in &block.steps {
                    let label = if block.repeat > 1 {
                        format!("{} {}/{}", step.name, i, block.repeat)
                    } else {
                        step.name.clone()
                    };
                    out.push(FlatSegment {
                        name: step.name.clone(),
                        label,
                        start_secs: t,
                        duration_secs: step.duration_secs,
                        start_watts: step.start_watts,
                        end_watts: step.end_watts,
                    });
                    t = t.saturating_add(step.duration_secs);
                }
            }
        }
        out
    }

    /// Assumes a validated program.
    pub fn total_secs(&self) -> u32 {
        self.blocks
            .iter()
            .map(|b| {
                let steps: u32 = b.steps.iter().map(|s| s.duration_secs).sum();
                steps.saturating_mul(u32::from(b.repeat))
            })
            .fold(0, u32::saturating_add)
    }
}

/// Programs inserted when a database is created.
pub fn templates() -> Vec<Program> {
    fn step(name: &str, duration_secs: u32, start_watts: u16, end_watts: u16) -> Step {
        Step { name: name.into(), duration_secs, start_watts, end_watts }
    }
    fn block(repeat: u16, steps: Vec<Step>) -> Block {
        Block { repeat, steps }
    }
    fn stage(watts: u16, secs: u32) -> Step {
        step(&format!("Stufe {watts} W"), secs, watts, watts)
    }
    fn program(name: &str, blocks: Vec<Block>) -> Program {
        Program { id: None, name: name.into(), blocks }
    }
    vec![
        program(
            "Grundlage 30",
            vec![
                block(1, vec![step("Aufwärmen", 300, 60, 100)]),
                block(1, vec![step("Grundlage", 1200, 110, 110)]),
                block(1, vec![step("Ausfahren", 300, 70, 70)]),
            ],
        ),
        program(
            "Pyramide",
            vec![
                block(1, vec![step("Aufwärmen", 300, 60, 100)]),
                block(1, [110, 130, 150, 170, 150, 130, 110].map(|w| stage(w, 120)).into()),
                block(1, vec![step("Ausfahren", 300, 70, 70)]),
            ],
        ),
        program(
            "4 × 4",
            vec![
                block(1, vec![step("Aufwärmen", 600, 60, 110)]),
                block(4, vec![step("Intervall", 240, 170, 170), step("Erholung", 180, 80, 80)]),
                block(1, vec![step("Ausfahren", 300, 70, 70)]),
            ],
        ),
        program(
            "Rampentest",
            vec![
                block(1, vec![step("Einfahren", 300, 80, 80)]),
                block(1, (100..=300).step_by(10).map(|w| stage(w, 60)).collect()),
            ],
        ),
    ]
}

/// Index of the segment containing `t_secs` and the target watts (linear ramp, rounded,
/// not clamped to device limits). `None` past the end.
pub fn target_at(segments: &[FlatSegment], t_secs: u32) -> Option<(usize, u16)> {
    let idx = segments.iter().position(|s| {
        t_secs >= s.start_secs
            && u64::from(t_secs) < u64::from(s.start_secs) + u64::from(s.duration_secs)
    })?;
    let s = &segments[idx];
    let (a, b) = (f64::from(s.start_watts), f64::from(s.end_watts));
    let frac = f64::from(t_secs - s.start_secs) / f64::from(s.duration_secs);
    Some((idx, (a + (b - a) * frac).round() as u16))
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    pub(crate) fn intervals() -> Program {
        Program {
            id: None,
            name: "Intervall".into(),
            blocks: vec![
                Block {
                    repeat: 1,
                    steps: vec![Step {
                        name: "Aufwärmen".into(),
                        duration_secs: 600,
                        start_watts: 60,
                        end_watts: 100,
                    }],
                },
                Block {
                    repeat: 5,
                    steps: vec![
                        Step {
                            name: "Intervall".into(),
                            duration_secs: 120,
                            start_watts: 150,
                            end_watts: 150,
                        },
                        Step {
                            name: "Erholung".into(),
                            duration_secs: 120,
                            start_watts: 80,
                            end_watts: 80,
                        },
                    ],
                },
                Block {
                    repeat: 1,
                    steps: vec![Step {
                        name: "Ausfahren".into(),
                        duration_secs: 300,
                        start_watts: 60,
                        end_watts: 60,
                    }],
                },
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::intervals;
    use super::*;

    #[test]
    fn expands_repeats_with_labels() {
        let s = intervals().expand();
        assert_eq!(s.len(), 12);
        assert_eq!(s[5].label, "Intervall 3/5");
        assert_eq!(s[5].start_secs, 600 + 4 * 120);
        assert_eq!(intervals().total_secs(), 600 + 1200 + 300);
    }

    #[test]
    fn ramps_linearly() {
        let s = intervals().expand();
        assert_eq!(target_at(&s, 0), Some((0, 60)));
        assert_eq!(target_at(&s, 300), Some((0, 80)));
        assert_eq!(target_at(&s, 600), Some((1, 150)));
        assert_eq!(target_at(&s, 2100), None);
    }

    #[test]
    fn rejects_invalid_programs() {
        let l = DeviceLimits::default();
        let mut p = intervals();
        p.blocks.clear();
        assert!(matches!(p.validate(&l), Err(ProgramError::NoBlocks)));
        let mut p = intervals();
        p.blocks[0].steps[0].duration_secs = 0;
        assert!(matches!(
            p.validate(&l),
            Err(ProgramError::BadDuration { block: 1, .. })
        ));
        let mut p = intervals();
        p.blocks[1].steps[0].start_watts = 500;
        assert!(matches!(
            p.validate(&l),
            Err(ProgramError::BadWatts { block: 2, .. })
        ));
        let mut p = intervals();
        p.blocks[1].repeat = 0;
        assert!(matches!(p.validate(&l), Err(ProgramError::BadRepeat(2))));
        let mut p = intervals();
        p.name = "  ".into();
        assert!(matches!(p.validate(&l), Err(ProgramError::MissingName)));
        assert!(intervals().validate(&l).is_ok());
    }

    #[test]
    fn rejects_more_cases_with_german_messages() {
        let l = DeviceLimits::default();
        let mut p = intervals();
        p.blocks[1].steps[1].end_watts = 10;
        assert_eq!(
            p.validate(&l).unwrap_err().to_string(),
            "Block 2, Abschnitt „Erholung“: Watt müssen zwischen 25 und 400 liegen"
        );
        let mut p = intervals();
        p.blocks[1].steps.clear();
        assert!(matches!(p.validate(&l), Err(ProgramError::EmptyBlock(2))));
        let mut p = intervals();
        p.blocks[1].steps[0].name = " ".into();
        assert!(matches!(
            p.validate(&l),
            Err(ProgramError::MissingStepName { block: 2 })
        ));
        let mut p = intervals();
        p.blocks[1].repeat = 51;
        assert!(matches!(p.validate(&l), Err(ProgramError::BadRepeat(2))));
        let mut p = intervals();
        p.blocks[0].steps[0].duration_secs = 7201;
        assert!(matches!(
            p.validate(&l),
            Err(ProgramError::BadDuration { block: 1, .. })
        ));
    }

    fn single(repeat: u16, secs: u32, start: u16, end: u16) -> Program {
        Program {
            id: None,
            name: "P".into(),
            blocks: vec![Block {
                repeat,
                steps: vec![Step {
                    name: "S".into(),
                    duration_secs: secs,
                    start_watts: start,
                    end_watts: end,
                }],
            }],
        }
    }

    #[test]
    fn accepts_boundary_values() {
        let l = DeviceLimits::default();
        assert!(single(1, 60, 25, 400).validate(&l).is_ok());
        assert!(single(50, 60, 400, 25).validate(&l).is_ok());
        assert!(single(1, 7200, 100, 100).validate(&l).is_ok());
    }

    #[test]
    fn rejects_too_long_programs() {
        let l = DeviceLimits::default();
        let step = Step {
            name: "S".into(),
            duration_secs: 7200,
            start_watts: 100,
            end_watts: 100,
        };
        let p = Program {
            id: None,
            name: "P".into(),
            blocks: vec![Block {
                repeat: 50,
                steps: vec![step; 10],
            }],
        };
        assert_eq!(p.validate(&l), Err(ProgramError::TooLong));
        assert!(single(3, 7200, 100, 100).validate(&l).is_ok());
        assert_eq!(
            single(4, 7200, 100, 100).validate(&l),
            Err(ProgramError::TooLong)
        );
    }

    #[test]
    fn rejects_too_many_segments() {
        let l = DeviceLimits::default();
        let step = |i: u32| Step { name: format!("S{i}"), duration_secs: 1, start_watts: 100, end_watts: 100 };
        let program = |steps: u32| Program {
            id: None,
            name: "P".into(),
            blocks: vec![Block { repeat: 50, steps: (0..steps).map(step).collect() }],
        };
        assert!(program(10).validate(&l).is_ok());
        assert_eq!(program(10).expand().len(), 500);
        assert_eq!(program(11).validate(&l), Err(ProgramError::TooManySegments));
        assert_eq!(
            ProgramError::TooManySegments.to_string(),
            "Programm hat zu viele Abschnitte (höchstens 500)"
        );
    }

    #[test]
    fn label_without_repeat_is_name_and_last_segments() {
        let s = intervals().expand();
        assert_eq!(s[0].label, s[0].name);
        assert_eq!(target_at(&s, 1799), Some((10, 80)));
        assert_eq!(target_at(&s, 1800), Some((11, 60)));
    }

    #[test]
    fn templates_are_valid_with_expected_length() {
        let l = DeviceLimits::default();
        let t = templates();
        let got: Vec<_> = t.iter().map(|p| (p.name.as_str(), p.total_secs())).collect();
        assert_eq!(
            got,
            [("Grundlage 30", 1800), ("Pyramide", 1440), ("4 × 4", 2580), ("Rampentest", 1560)]
        );
        for p in &t {
            assert_eq!(p.validate(&l), Ok(()), "{}", p.name);
        }
    }
}
