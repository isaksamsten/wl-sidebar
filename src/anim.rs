//! Slide animations: fixed-duration easing curves and damped springs.
//!
//! Both are evaluated in closed form from `(from, velocity, start)`, so any thread can
//! sample the global slide state at any time without stepping a simulation. Positions
//! are in "slide units": 1.0 = fully shown, 0.0 = fully hidden.

use {serde::Deserialize, std::time::Duration};

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum Animation {
    /// `curve` over a fixed `duration`. A zero duration is instant.
    Easing {
        curve: Curve,
        duration: Duration,
    },
    Spring(Spring),
}

/// A damped harmonic oscillator with unit mass.
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct Spring {
    /// < 1 oscillates (bouncy), 1 = critically damped, > 1 = sluggish.
    pub damping_ratio: f64,
    pub stiffness: f64,
    /// Settled once both distance to the target and movement per 60 Hz frame are below
    /// this.
    pub epsilon: f64,
}

impl Default for Spring {
    fn default() -> Self {
        Self {
            damping_ratio: 1.0,
            stiffness: 1000.0,
            epsilon: 0.001,
        }
    }
}

impl Default for Animation {
    fn default() -> Self {
        Self::Spring(Spring::default())
    }
}

#[derive(Deserialize, Debug, Default, Copy, Clone, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Curve {
    Linear,
    EaseInQuad,
    EaseInCubic,
    EaseOutQuad,
    #[default]
    EaseOutCubic,
    EaseOutExpo,
    EaseInOutCubic,
    EaseOutBack,
    EaseOutElastic,
    EaseOutBounce,
}

impl Curve {
    /// Maps progress `t` in [0, 1] to eased progress (0 at 0, 1 at 1, may leave [0, 1]
    /// in between).
    fn eval(self, t: f64) -> f64 {
        use std::f64::consts::PI;
        match self {
            Curve::Linear => t,
            Curve::EaseInQuad => t * t,
            Curve::EaseInCubic => t * t * t,
            Curve::EaseOutQuad => 1.0 - (1.0 - t).powi(2),
            Curve::EaseOutCubic => 1.0 - (1.0 - t).powi(3),
            Curve::EaseOutExpo => {
                if t >= 1.0 {
                    1.0
                } else {
                    1.0 - 2f64.powf(-10.0 * t)
                }
            }
            Curve::EaseInOutCubic => {
                if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
                }
            }
            Curve::EaseOutBack => {
                const C1: f64 = 1.70158;
                const C3: f64 = C1 + 1.0;
                1.0 + C3 * (t - 1.0).powi(3) + C1 * (t - 1.0).powi(2)
            }
            Curve::EaseOutElastic => {
                if t <= 0.0 || t >= 1.0 {
                    t.clamp(0.0, 1.0)
                } else {
                    2f64.powf(-10.0 * t) * ((10.0 * t - 0.75) * (2.0 * PI / 3.0)).sin() + 1.0
                }
            }
            Curve::EaseOutBounce => {
                const N: f64 = 7.5625;
                const D: f64 = 2.75;
                if t < 1.0 / D {
                    N * t * t
                } else if t < 2.0 / D {
                    let t = t - 1.5 / D;
                    N * t * t + 0.75
                } else if t < 2.5 / D {
                    let t = t - 2.25 / D;
                    N * t * t + 0.9375
                } else {
                    let t = t - 2.625 / D;
                    N * t * t + 0.984375
                }
            }
        }
    }

    /// d eval / dt, numerically.
    fn slope(self, t: f64) -> f64 {
        const H: f64 = 1e-4;
        let (a, b) = ((t - H).max(0.0), (t + H).min(1.0));
        (self.eval(b) - self.eval(a)) / (b - a)
    }
}

/// What to do when an animation overshoots the fully-shown position. The app keeps its
/// size, so overshooting would open a gap between the screen edge and the sidebar.
#[derive(Deserialize, Debug, Default, Copy, Clone, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Overshoot {
    /// Reflect off the screen edge, as if it were a wall.
    #[default]
    Bounce,
    /// Stop at the edge.
    Clamp,
}

/// The animations of one sidebar.
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct Animations {
    pub show: Animation,
    pub hide: Animation,
    pub overshoot: Overshoot,
}

/// An animation sampled at some point in time.
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct Sample {
    /// Position in [0, 1].
    pub pos: f64,
    /// Velocity in units per second.
    pub vel: f64,
    pub done: bool,
}

impl Animations {
    /// Samples the show (`visible`) or hide animation that started at position `from`
    /// with velocity `v0`, `t` after its start.
    pub fn sample(&self, visible: bool, from: f64, v0: f64, t: Duration) -> Sample {
        let (anim, to) = if visible {
            (self.show, 1.0)
        } else {
            (self.hide, 0.0)
        };
        let mut s = anim.sample(from, to, v0, t.as_secs_f64());
        if s.pos > 1.0 {
            match self.overshoot {
                Overshoot::Bounce => {
                    s.pos = 2.0 - s.pos;
                    s.vel = -s.vel;
                }
                Overshoot::Clamp => {
                    s.pos = 1.0;
                    s.vel = 0.0;
                }
            }
        }
        // Past fully hidden is offscreen anyway.
        if s.pos < 0.0 {
            s.pos = 0.0;
            s.vel = 0.0;
        }
        s
    }
}

impl Animation {
    /// Animates from `from` (moving at `v0` units/s) to `to`, sampled `t` seconds after
    /// the start.
    fn sample(&self, from: f64, to: f64, v0: f64, t: f64) -> Sample {
        let done = Sample {
            pos: to,
            vel: 0.0,
            done: true,
        };
        match *self {
            Animation::Easing { curve, duration } => {
                let d = duration.as_secs_f64();
                if d <= 0.0 || t >= d {
                    return done;
                }
                let p = t / d;
                Sample {
                    pos: from + (to - from) * curve.eval(p),
                    vel: (to - from) * curve.slope(p) / d,
                    done: false,
                }
            }
            Animation::Spring(spring) => {
                let (x, v) = spring.eval(from - to, v0, t);
                // Also give up eventually, in case of a degenerate configuration.
                if t > 10.0 || (x.abs() < spring.epsilon && (v / 60.0).abs() < spring.epsilon) {
                    return done;
                }
                Sample {
                    pos: to + x,
                    vel: v,
                    done: false,
                }
            }
        }
    }
}

impl Spring {
    /// Displacement and velocity at `t`, starting at displacement `x0` with velocity `v0`.
    fn eval(&self, x0: f64, v0: f64, t: f64) -> (f64, f64) {
        let w = self.stiffness.sqrt();
        let z = self.damping_ratio;
        if (z - 1.0).abs() < 1e-6 {
            // Critically damped.
            let e = (-w * t).exp();
            let b = v0 + w * x0;
            ((x0 + b * t) * e, (v0 - w * b * t) * e)
        } else if z < 1.0 {
            // Underdamped.
            let wd = w * (1.0 - z * z).sqrt();
            let e = (-z * w * t).exp();
            let (sin, cos) = (wd * t).sin_cos();
            let x = e * (x0 * cos + (v0 + z * w * x0) / wd * sin);
            let v = e * (v0 * cos - (z * w * v0 + w * w * x0) / wd * sin);
            (x, v)
        } else {
            // Overdamped.
            let s = (z * z - 1.0).sqrt();
            let (r1, r2) = (-w * (z - s), -w * (z + s));
            let c1 = (v0 - r2 * x0) / (r1 - r2);
            let c2 = x0 - c1;
            let (e1, e2) = ((r1 * t).exp(), (r2 * t).exp());
            (c1 * e1 + c2 * e2, c1 * r1 * e1 + c2 * r2 * e2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn springs() -> [Spring; 3] {
        [0.4, 1.0, 2.0].map(|damping_ratio| Spring {
            damping_ratio,
            ..Spring::default()
        })
    }

    #[test]
    fn spring_initial_conditions() {
        for s in springs() {
            let (x, v) = s.eval(-1.0, 3.0, 0.0);
            assert!((x + 1.0).abs() < 1e-9 && (v - 3.0).abs() < 1e-9, "{s:?}");
        }
    }

    #[test]
    fn spring_velocity_is_derivative() {
        for s in springs() {
            for t in [0.01, 0.05, 0.1, 0.3] {
                let h = 1e-6;
                let num = (s.eval(-1.0, 2.0, t + h).0 - s.eval(-1.0, 2.0, t - h).0) / (2.0 * h);
                let (_, v) = s.eval(-1.0, 2.0, t);
                assert!((num - v).abs() < 1e-4, "{s:?} t={t}: {num} vs {v}");
            }
        }
    }

    #[test]
    fn spring_settles() {
        let a = Animations::default();
        let s = a.sample(true, 0.0, 0.0, Duration::from_secs(2));
        assert!(s.done && s.pos == 1.0);
    }

    #[test]
    fn curves_hit_endpoints() {
        use Curve::*;
        for c in [
            Linear,
            EaseInQuad,
            EaseInCubic,
            EaseOutQuad,
            EaseOutCubic,
            EaseInOutCubic,
            EaseOutBack,
            EaseOutElastic,
            EaseOutBounce,
        ] {
            assert!(c.eval(0.0).abs() < 1e-9, "{c:?}");
            assert!((c.eval(1.0) - 1.0).abs() < 1e-9, "{c:?}");
        }
    }

    #[test]
    fn overshoot_stays_in_range() {
        for overshoot in [Overshoot::Bounce, Overshoot::Clamp] {
            let a = Animations {
                show: Animation::Spring(Spring {
                    damping_ratio: 0.2,
                    ..Spring::default()
                }),
                overshoot,
                ..Animations::default()
            };
            for ms in 0..1000 {
                let s = a.sample(true, 0.0, 0.0, Duration::from_millis(ms));
                assert!(
                    (0.0..=1.0).contains(&s.pos),
                    "{overshoot:?} {ms}ms: {}",
                    s.pos
                );
            }
        }
    }
}
