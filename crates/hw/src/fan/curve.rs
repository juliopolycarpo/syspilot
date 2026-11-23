//! The fan-curve engine: temperature in, duty out. Pure arithmetic, no I/O and
//! no clock; the caller passes the elapsed time. Always compiled, because it
//! cannot write anything.
//!
//! - [`Curve`]: points with linear interpolation, temperature °C to duty %.
//! - [`TemperatureSource`]: one sensor or the maximum of several, refusing a
//!   missing, `None`, non-finite or implausible reading instead of skipping it.
//! - [`CurveFollower`]: applies hysteresis, the channel's minimum duty and a
//!   ramp-rate limit to the curve's answer.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::RangeInclusive;

/// The largest duty, in percent.
pub const MAX_DUTY: f64 = 100.0;

/// Readings outside this range (°C) are not temperatures: a floating input
/// reads very low, a failed bus read very high. The curve never follows them.
pub const PLAUSIBLE_TEMPERATURE: RangeInclusive<f64> = -20.0..=125.0;

/// Sensor readings by sensor key; `None` is a sensor that did not answer.
pub type Readings = BTreeMap<String, Option<f64>>;

/// A curve, source or setting that breaks a rule of the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurveError {
    what: &'static str,
    expected: String,
    received: String,
}

impl CurveError {
    fn new(what: &'static str, expected: impl Into<String>, received: impl Into<String>) -> Self {
        Self { what, expected: expected.into(), received: received.into() }
    }
}

impl fmt::Display for CurveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}): expected {} | received: {}", self.what, self.expected, self.received)
    }
}

impl std::error::Error for CurveError {}

// ---------------------------------------------------------------------------
// Curve
// ---------------------------------------------------------------------------

/// One point of a curve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CurvePoint {
    /// °C.
    pub temperature: f64,
    /// Percent of full duty, `0..=100`.
    pub duty: f64,
}

/// A validated curve. Between points the duty is interpolated linearly;
/// below the first point it is the first duty and above the last point it is
/// the last duty.
#[derive(Debug, Clone, PartialEq)]
pub struct Curve {
    points: Vec<CurvePoint>,
}

impl Curve {
    /// A curve through `points`: at least two, temperatures finite, plausible
    /// and strictly increasing, duties in `0..=100` and never decreasing (a
    /// fan that slows down as the machine heats up is a mistake, not a
    /// preference).
    pub fn new(points: Vec<CurvePoint>) -> Result<Self, CurveError> {
        if points.len() < 2 {
            return Err(CurveError::new(
                "fan curve",
                "at least 2 points",
                format!("{} point(s)", points.len()),
            ));
        }
        for (index, point) in points.iter().enumerate() {
            if !point.temperature.is_finite() || !PLAUSIBLE_TEMPERATURE.contains(&point.temperature)
            {
                return Err(CurveError::new(
                    "fan curve",
                    format!(
                        "point {index} temperature within {}..={} °C",
                        PLAUSIBLE_TEMPERATURE.start(),
                        PLAUSIBLE_TEMPERATURE.end()
                    ),
                    format!("{} °C", point.temperature),
                ));
            }
            if !point.duty.is_finite() || !(0.0..=MAX_DUTY).contains(&point.duty) {
                return Err(CurveError::new(
                    "fan curve",
                    format!("point {index} duty within 0..=100 %"),
                    format!("{} %", point.duty),
                ));
            }
        }
        for (index, pair) in points.windows(2).enumerate() {
            if pair[1].temperature <= pair[0].temperature {
                return Err(CurveError::new(
                    "fan curve",
                    format!(
                        "point {} temperature above point {index} ({} °C)",
                        index + 1,
                        pair[0].temperature
                    ),
                    format!("{} °C", pair[1].temperature),
                ));
            }
            if pair[1].duty < pair[0].duty {
                return Err(CurveError::new(
                    "fan curve",
                    format!(
                        "point {} duty not below point {index} ({} %)",
                        index + 1,
                        pair[0].duty
                    ),
                    format!("{} %", pair[1].duty),
                ));
            }
        }
        Ok(Self { points })
    }

    /// The points, in order.
    pub fn points(&self) -> &[CurvePoint] {
        &self.points
    }

    /// The duty for `temperature`, in `0..=100`. A non-finite temperature
    /// yields the last duty: the engine never turns bad input into less
    /// cooling.
    pub fn duty_at(&self, temperature: f64) -> f64 {
        let first = self.points[0];
        let last = self.points[self.points.len() - 1];
        if temperature.is_nan() || temperature >= last.temperature {
            return last.duty;
        }
        if temperature <= first.temperature {
            return first.duty;
        }
        for pair in self.points.windows(2) {
            let (low, high) = (pair[0], pair[1]);
            if temperature <= high.temperature {
                let fraction =
                    (temperature - low.temperature) / (high.temperature - low.temperature);
                return low.duty + fraction * (high.duty - low.duty);
            }
        }
        last.duty
    }
}

// ---------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------

/// Why a [`TemperatureSource`] could not produce a temperature.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceFault {
    /// The readings hold no entry for the sensor.
    Missing {
        /// The sensor key.
        sensor: String,
    },
    /// The sensor did not answer (`None`).
    NoReading {
        /// The sensor key.
        sensor: String,
    },
    /// The value is not a temperature: not finite, or outside
    /// [`PLAUSIBLE_TEMPERATURE`].
    Implausible {
        /// The sensor key.
        sensor: String,
        /// What it read.
        value: f64,
    },
}

impl SourceFault {
    /// The sensor at fault.
    pub fn sensor(&self) -> &str {
        match self {
            Self::Missing { sensor }
            | Self::NoReading { sensor }
            | Self::Implausible { sensor, .. } => sensor,
        }
    }
}

impl fmt::Display for SourceFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { sensor } => write!(
                f,
                "(temperature source): expected a reading for `{sensor}` | received: no entry"
            ),
            Self::NoReading { sensor } => write!(
                f,
                "(temperature source): expected a reading for `{sensor}` | received: none"
            ),
            Self::Implausible { sensor, value } => write!(
                f,
                "(temperature source): expected `{sensor}` within {}..={} °C | received: {value}",
                PLAUSIBLE_TEMPERATURE.start(),
                PLAUSIBLE_TEMPERATURE.end()
            ),
        }
    }
}

impl std::error::Error for SourceFault {}

/// Where a channel's temperature comes from: one sensor, or the hottest of
/// several. Every sensor must read correctly: with several, a failing one is
/// not ignored, because the hottest part may be the one that stopped
/// answering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemperatureSource {
    sensors: Vec<String>,
}

impl TemperatureSource {
    /// The temperature of one sensor.
    pub fn single(sensor: impl Into<String>) -> Self {
        Self { sensors: vec![sensor.into()] }
    }

    /// The maximum over `sensors`: at least one, without repeats.
    pub fn maximum_of(sensors: Vec<String>) -> Result<Self, CurveError> {
        if sensors.is_empty() {
            return Err(CurveError::new("temperature source", "at least 1 sensor", "none"));
        }
        for (index, sensor) in sensors.iter().enumerate() {
            if sensors[..index].contains(sensor) {
                return Err(CurveError::new(
                    "temperature source",
                    "each sensor listed once",
                    format!("`{sensor}` twice"),
                ));
            }
        }
        Ok(Self { sensors })
    }

    /// The sensor keys.
    pub fn sensors(&self) -> &[String] {
        &self.sensors
    }

    /// The temperature now: the largest reading, after checking that every
    /// sensor has a finite, plausible one. `f64::max` would silently drop a
    /// NaN, so each value is checked before any is compared.
    pub fn resolve(&self, readings: &Readings) -> Result<f64, SourceFault> {
        let mut hottest = f64::NEG_INFINITY;
        for sensor in &self.sensors {
            let value = match readings.get(sensor) {
                None => return Err(SourceFault::Missing { sensor: sensor.clone() }),
                Some(None) => return Err(SourceFault::NoReading { sensor: sensor.clone() }),
                Some(Some(value)) => *value,
            };
            if !value.is_finite() || !PLAUSIBLE_TEMPERATURE.contains(&value) {
                return Err(SourceFault::Implausible { sensor: sensor.clone(), value });
            }
            hottest = hottest.max(value);
        }
        Ok(hottest)
    }
}

// ---------------------------------------------------------------------------
// Follower
// ---------------------------------------------------------------------------

/// The channel-specific rules applied after the curve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FollowerSettings {
    /// °C a falling temperature must drop below the last followed one before
    /// the duty goes down. Rising temperatures are followed at once.
    pub hysteresis: f64,
    /// The duty never goes below this, percent.
    pub min_duty: f64,
    /// The most the duty may rise per second, percent; `None` for no limit.
    pub max_rise_per_second: Option<f64>,
    /// The most the duty may fall per second, percent; `None` for no limit.
    pub max_fall_per_second: Option<f64>,
}

impl FollowerSettings {
    /// Checks every value: hysteresis finite and not negative, minimum duty
    /// in `0..=100`, ramp limits finite and positive.
    pub fn validate(&self) -> Result<(), CurveError> {
        if !self.hysteresis.is_finite() || self.hysteresis < 0.0 {
            return Err(CurveError::new(
                "fan curve settings",
                "a hysteresis of 0 °C or more",
                format!("{}", self.hysteresis),
            ));
        }
        if !self.min_duty.is_finite() || !(0.0..=MAX_DUTY).contains(&self.min_duty) {
            return Err(CurveError::new(
                "fan curve settings",
                "a minimum duty within 0..=100 %",
                format!("{}", self.min_duty),
            ));
        }
        for (name, limit) in
            [("rise", self.max_rise_per_second), ("fall", self.max_fall_per_second)]
        {
            if let Some(limit) = limit
                && (!limit.is_finite() || limit <= 0.0)
            {
                return Err(CurveError::new(
                    "fan curve settings",
                    format!("a ramp-{name} limit above 0 %/s, or none"),
                    format!("{limit}"),
                ));
            }
        }
        Ok(())
    }
}

/// A curve followed over time: holds the hysteresis and ramp state of one
/// channel.
///
/// Hysteresis: the temperature the curve is evaluated at is the last one
/// followed. A reading at or above it is followed at once; a lower one is
/// followed only when it is more than [`FollowerSettings::hysteresis`] lower,
/// and then the evaluated temperature stays that much above the reading. So
/// while the readings stay within one hysteresis band the duty never
/// decreases and cannot oscillate.
#[derive(Debug, Clone)]
pub struct CurveFollower {
    curve: Curve,
    settings: FollowerSettings,
    /// The temperature the curve was last evaluated at.
    followed: Option<f64>,
    /// The duty last returned.
    output: Option<f64>,
}

impl CurveFollower {
    /// A follower of `curve` under `settings`.
    pub fn new(curve: Curve, settings: FollowerSettings) -> Result<Self, CurveError> {
        settings.validate()?;
        Ok(Self { curve, settings, followed: None, output: None })
    }

    /// The settings.
    pub fn settings(&self) -> &FollowerSettings {
        &self.settings
    }

    /// The duty last returned, if any.
    pub fn output(&self) -> Option<f64> {
        self.output
    }

    /// Forgets the temperature followed and continues from `duty`, so the
    /// ramp limits apply from where the fan actually is. Used after a
    /// fail-safe or a resume.
    pub fn restart_from(&mut self, duty: f64) {
        self.followed = None;
        self.output = Some(duty.clamp(self.settings.min_duty, MAX_DUTY));
    }

    /// The duty for `temperature` after `elapsed_seconds` since the previous
    /// step, always within `min_duty..=100`. The first step has nothing to
    /// ramp from and returns the curve's duty.
    pub fn step(&mut self, temperature: f64, elapsed_seconds: f64) -> Result<f64, CurveError> {
        if !temperature.is_finite() {
            return Err(CurveError::new(
                "fan curve step",
                "a finite temperature",
                format!("{temperature}"),
            ));
        }
        if !elapsed_seconds.is_finite() || elapsed_seconds < 0.0 {
            return Err(CurveError::new(
                "fan curve step",
                "an elapsed time of 0 s or more",
                format!("{elapsed_seconds}"),
            ));
        }
        let hysteresis = self.settings.hysteresis;
        let followed = match self.followed {
            Some(followed) if temperature < followed => {
                if followed - temperature > hysteresis {
                    temperature + hysteresis
                } else {
                    followed
                }
            }
            _ => temperature,
        };
        self.followed = Some(followed);

        let target = self.curve.duty_at(followed).clamp(self.settings.min_duty, MAX_DUTY);
        let next = match self.output {
            None => target,
            Some(previous) => {
                let rise = self
                    .settings
                    .max_rise_per_second
                    .map_or(f64::INFINITY, |limit| limit * elapsed_seconds);
                let fall = self
                    .settings
                    .max_fall_per_second
                    .map_or(f64::INFINITY, |limit| limit * elapsed_seconds);
                previous + (target - previous).clamp(-fall, rise)
            }
        };
        let next = next.clamp(self.settings.min_duty, MAX_DUTY);
        self.output = Some(next);
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small deterministic generator (xorshift64*): the property tests need
    /// repeatable "random" input, not a dependency.
    struct Sequence(u64);

    impl Sequence {
        fn next_u64(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn coin_flip(&mut self) -> bool {
            self.next_u64() >> 63 == 1
        }

        /// Uniform in `[low, high]`.
        fn between(&mut self, low: f64, high: f64) -> f64 {
            low + (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64 * (high - low)
        }
    }

    fn point(temperature: f64, duty: f64) -> CurvePoint {
        CurvePoint { temperature, duty }
    }

    fn standard_curve() -> Curve {
        Curve::new(vec![point(30.0, 20.0), point(50.0, 40.0), point(70.0, 100.0)]).unwrap()
    }

    fn settings(hysteresis: f64, min_duty: f64) -> FollowerSettings {
        FollowerSettings {
            hysteresis,
            min_duty,
            max_rise_per_second: None,
            max_fall_per_second: None,
        }
    }

    fn readings(entries: &[(&str, Option<f64>)]) -> Readings {
        entries.iter().map(|(key, value)| ((*key).to_owned(), *value)).collect()
    }

    #[test]
    fn a_curve_interpolates_linearly_between_points() {
        let curve = standard_curve();
        assert_eq!(curve.duty_at(40.0), 30.0);
        assert_eq!(curve.duty_at(50.0), 40.0);
        assert_eq!(curve.duty_at(60.0), 70.0);
    }

    #[test]
    fn a_curve_holds_its_end_duties_outside_the_points() {
        let curve = standard_curve();
        assert_eq!(curve.duty_at(-10.0), 20.0);
        assert_eq!(curve.duty_at(30.0), 20.0);
        assert_eq!(curve.duty_at(70.0), 100.0);
        assert_eq!(curve.duty_at(500.0), 100.0);
        assert_eq!(curve.duty_at(f64::NAN), 100.0, "bad input never means less cooling");
    }

    #[test]
    fn a_curve_refuses_every_malformed_shape() {
        let cases: [(Vec<CurvePoint>, &str); 6] = [
            (vec![point(30.0, 20.0)], "at least 2 points | received: 1 point(s)"),
            (
                vec![point(30.0, 20.0), point(30.0, 40.0)],
                "point 1 temperature above point 0 (30 °C) | received: 30 °C",
            ),
            (
                vec![point(30.0, 40.0), point(50.0, 20.0)],
                "point 1 duty not below point 0 (40 %) | received: 20 %",
            ),
            (
                vec![point(30.0, 20.0), point(50.0, 101.0)],
                "point 1 duty within 0..=100 % | received: 101 %",
            ),
            (
                vec![point(30.0, 20.0), point(500.0, 90.0)],
                "point 1 temperature within -20..=125 °C | received: 500 °C",
            ),
            (
                vec![point(f64::NAN, 20.0), point(50.0, 90.0)],
                "point 0 temperature within -20..=125 °C | received: NaN °C",
            ),
        ];
        for (points, detail) in cases {
            let error = Curve::new(points).unwrap_err();
            assert_eq!(error.to_string(), format!("(fan curve): expected {detail}"));
        }
    }

    #[test]
    fn a_single_sensor_source_returns_its_reading() {
        let source = TemperatureSource::single("cpu");
        assert_eq!(source.resolve(&readings(&[("cpu", Some(41.5))])), Ok(41.5));
    }

    #[test]
    fn a_maximum_source_returns_the_hottest_sensor() {
        let source = TemperatureSource::maximum_of(vec!["cpu".into(), "gpu".into()]).unwrap();
        let now = readings(&[("cpu", Some(41.5)), ("gpu", Some(63.0))]);
        assert_eq!(source.resolve(&now), Ok(63.0));
    }

    #[test]
    fn a_maximum_source_fails_when_any_sensor_fails() {
        let source = TemperatureSource::maximum_of(vec!["cpu".into(), "gpu".into()]).unwrap();
        let cases = [
            (readings(&[("cpu", Some(40.0))]), SourceFault::Missing { sensor: "gpu".into() }),
            (
                readings(&[("cpu", Some(40.0)), ("gpu", None)]),
                SourceFault::NoReading { sensor: "gpu".into() },
            ),
            (
                readings(&[("cpu", Some(f64::NAN)), ("gpu", Some(60.0))]),
                SourceFault::Implausible { sensor: "cpu".into(), value: f64::NAN },
            ),
            (
                readings(&[("cpu", Some(40.0)), ("gpu", Some(126.0))]),
                SourceFault::Implausible { sensor: "gpu".into(), value: 126.0 },
            ),
            (
                readings(&[("cpu", Some(-21.0)), ("gpu", Some(60.0))]),
                SourceFault::Implausible { sensor: "cpu".into(), value: -21.0 },
            ),
        ];
        for (now, expected) in cases {
            let fault = source.resolve(&now).unwrap_err();
            assert_eq!(fault.sensor(), expected.sensor());
            assert_eq!(fault.to_string(), expected.to_string());
        }
    }

    #[test]
    fn a_source_accepts_the_edges_of_the_plausible_range() {
        let source = TemperatureSource::maximum_of(vec!["a".into(), "b".into()]).unwrap();
        assert_eq!(source.resolve(&readings(&[("a", Some(-20.0)), ("b", Some(125.0))])), Ok(125.0));
    }

    #[test]
    fn a_source_fault_names_the_sensor_and_the_expected_range() {
        let fault = SourceFault::Implausible { sensor: "cpu".into(), value: 130.0 };
        assert_eq!(
            fault.to_string(),
            "(temperature source): expected `cpu` within -20..=125 °C | received: 130"
        );
        let fault = SourceFault::NoReading { sensor: "cpu".into() };
        assert_eq!(
            fault.to_string(),
            "(temperature source): expected a reading for `cpu` | received: none"
        );
    }

    #[test]
    fn a_maximum_source_refuses_no_sensors_and_repeats() {
        assert_eq!(
            TemperatureSource::maximum_of(vec![]).unwrap_err().to_string(),
            "(temperature source): expected at least 1 sensor | received: none"
        );
        assert_eq!(
            TemperatureSource::maximum_of(vec!["cpu".into(), "cpu".into()])
                .unwrap_err()
                .to_string(),
            "(temperature source): expected each sensor listed once | received: `cpu` twice"
        );
        assert_eq!(TemperatureSource::single("cpu").sensors(), ["cpu".to_owned()]);
    }

    #[test]
    fn settings_refuse_values_outside_their_range() {
        let cases = [
            (settings(-1.0, 20.0), "a hysteresis of 0 °C or more | received: -1"),
            (settings(2.0, 101.0), "a minimum duty within 0..=100 % | received: 101"),
            (settings(2.0, f64::NAN), "a minimum duty within 0..=100 % | received: NaN"),
            (
                FollowerSettings { max_rise_per_second: Some(0.0), ..settings(2.0, 20.0) },
                "a ramp-rise limit above 0 %/s, or none | received: 0",
            ),
            (
                FollowerSettings { max_fall_per_second: Some(-3.0), ..settings(2.0, 20.0) },
                "a ramp-fall limit above 0 %/s, or none | received: -3",
            ),
        ];
        for (bad, detail) in cases {
            let error = CurveFollower::new(standard_curve(), bad).unwrap_err();
            assert_eq!(error.to_string(), format!("(fan curve settings): expected {detail}"));
        }
    }

    #[test]
    fn a_step_refuses_a_non_finite_temperature_and_a_negative_interval() {
        let mut follower = CurveFollower::new(standard_curve(), settings(2.0, 20.0)).unwrap();
        assert_eq!(
            follower.step(f64::NAN, 1.0).unwrap_err().to_string(),
            "(fan curve step): expected a finite temperature | received: NaN"
        );
        assert_eq!(
            follower.step(40.0, -1.0).unwrap_err().to_string(),
            "(fan curve step): expected an elapsed time of 0 s or more | received: -1"
        );
        assert_eq!(follower.output(), None, "a refused step changes nothing");
    }

    #[test]
    fn the_minimum_duty_lifts_a_lower_curve_value() {
        let mut follower = CurveFollower::new(standard_curve(), settings(0.0, 35.0)).unwrap();
        assert_eq!(follower.step(30.0, 1.0), Ok(35.0), "the curve says 20, the channel needs 35");
        assert_eq!(follower.step(60.0, 1.0), Ok(70.0));
    }

    #[test]
    fn a_rising_temperature_is_followed_at_once() {
        let mut follower = CurveFollower::new(standard_curve(), settings(5.0, 20.0)).unwrap();
        assert_eq!(follower.step(40.0, 1.0), Ok(30.0));
        assert_eq!(follower.step(41.0, 1.0), Ok(31.0));
    }

    #[test]
    fn a_falling_temperature_inside_the_band_holds_the_duty() {
        let mut follower = CurveFollower::new(standard_curve(), settings(5.0, 20.0)).unwrap();
        assert_eq!(follower.step(50.0, 1.0), Ok(40.0));
        assert_eq!(follower.step(45.0, 1.0), Ok(40.0), "exactly one band down still holds");
        assert_eq!(follower.step(46.0, 1.0), Ok(40.0));
    }

    #[test]
    fn a_falling_temperature_beyond_the_band_lowers_the_duty_with_a_lag() {
        let mut follower = CurveFollower::new(standard_curve(), settings(5.0, 20.0)).unwrap();
        assert_eq!(follower.step(50.0, 1.0), Ok(40.0));
        // 44 is 6 below 50: follow, evaluating at 44 + 5 = 49.
        assert_eq!(follower.step(44.0, 1.0), Ok(39.0));
        // 45 is 4 below the followed 49 °C, inside the band: hold.
        assert_eq!(follower.step(45.0, 1.0), Ok(39.0));
        // 43 is 6 below 49: follow again, evaluating at 43 + 5 = 48.
        assert_eq!(follower.step(43.0, 1.0), Ok(38.0));
    }

    #[test]
    fn a_rise_limit_slows_an_increase_and_a_fall_limit_slows_a_decrease() {
        let limited = FollowerSettings {
            max_rise_per_second: Some(10.0),
            max_fall_per_second: Some(5.0),
            ..settings(0.0, 20.0)
        };
        let mut follower = CurveFollower::new(standard_curve(), limited).unwrap();
        assert_eq!(follower.step(30.0, 1.0), Ok(20.0));
        assert_eq!(follower.step(70.0, 1.0), Ok(30.0), "100 wanted, 10 %/s allowed");
        assert_eq!(follower.step(70.0, 2.5), Ok(55.0), "25 more in 2.5 s");
        assert_eq!(follower.step(30.0, 2.0), Ok(45.0), "20 wanted, 5 %/s down over 2 s");
        assert_eq!(follower.step(30.0, 0.0), Ok(45.0), "no time, no movement");
    }

    #[test]
    fn restarting_from_a_duty_continues_the_ramp_from_there() {
        let limited = FollowerSettings { max_fall_per_second: Some(5.0), ..settings(0.0, 20.0) };
        let mut follower = CurveFollower::new(standard_curve(), limited).unwrap();
        follower.restart_from(100.0);
        assert_eq!(follower.step(30.0, 1.0), Ok(95.0), "from full duty down at 5 %/s");
        follower.restart_from(5.0);
        assert_eq!(follower.output(), Some(20.0), "a restart is clamped to the minimum duty");
    }

    #[test]
    fn the_output_stays_within_the_minimum_duty_and_full_duty_for_any_input() {
        let mut generator = Sequence(0x9E37_79B9_7F4A_7C15);
        for _ in 0..200 {
            let low = generator.between(-20.0, 60.0);
            let high = low + generator.between(1.0, 60.0);
            let first = generator.between(0.0, 60.0);
            let second = first + generator.between(0.0, 40.0);
            let curve =
                Curve::new(vec![point(low, first), point(high, second)]).expect("generated curve");
            let min_duty = generator.between(0.0, 100.0);
            let limited = FollowerSettings {
                hysteresis: generator.between(0.0, 10.0),
                min_duty,
                max_rise_per_second: generator.coin_flip().then(|| generator.between(0.5, 50.0)),
                max_fall_per_second: generator.coin_flip().then(|| generator.between(0.5, 50.0)),
            };
            let mut follower = CurveFollower::new(curve, limited).unwrap();
            for _ in 0..100 {
                let temperature = generator.between(-30.0, 140.0);
                let elapsed = generator.between(0.0, 5.0);
                let duty = follower.step(temperature, elapsed).unwrap();
                assert!(
                    (min_duty..=MAX_DUTY).contains(&duty),
                    "(property): expected duty within {min_duty}..=100 | received: {duty} at {temperature} °C"
                );
            }
        }
    }

    #[test]
    fn the_duty_never_decreases_while_the_temperature_stays_inside_one_band() {
        let mut generator = Sequence(0xD1B5_4A32_D192_ED03);
        for _ in 0..200 {
            let hysteresis = generator.between(0.5, 8.0);
            let base = generator.between(25.0, 85.0);
            let ramped = generator.coin_flip().then(|| generator.between(1.0, 20.0));
            let limited = FollowerSettings {
                hysteresis,
                min_duty: generator.between(0.0, 30.0),
                max_rise_per_second: ramped,
                max_fall_per_second: ramped,
            };
            let mut follower = CurveFollower::new(standard_curve(), limited).unwrap();
            let mut previous = follower.step(base, 1.0).unwrap();
            for _ in 0..100 {
                // Every reading is within `hysteresis` of `base`.
                let temperature = base + generator.between(-hysteresis, 0.0);
                let duty = follower.step(temperature, generator.between(0.0, 3.0)).unwrap();
                assert!(
                    duty >= previous,
                    "(property): expected no decrease inside a {hysteresis} °C band | received: {previous} then {duty} at {temperature} °C (base {base})"
                );
                previous = duty;
            }
        }
    }

    #[test]
    fn a_band_of_readings_never_makes_the_duty_go_up_then_down_then_up() {
        let mut generator = Sequence(0x0123_4567_89AB_CDEF);
        for _ in 0..200 {
            let hysteresis = generator.between(0.5, 8.0);
            let base = generator.between(25.0, 85.0);
            let mut follower =
                CurveFollower::new(standard_curve(), settings(hysteresis, 20.0)).unwrap();
            let mut directions = Vec::new();
            let mut previous = follower.step(base, 1.0).unwrap();
            for _ in 0..100 {
                let temperature = base + generator.between(-hysteresis / 2.0, hysteresis / 2.0);
                let duty = follower.step(temperature, 1.0).unwrap();
                if duty != previous {
                    directions.push(duty > previous);
                }
                previous = duty;
            }
            assert!(
                directions.iter().all(|&up| up),
                "(property): expected only increases inside the band | received: {directions:?}"
            );
        }
    }
}
