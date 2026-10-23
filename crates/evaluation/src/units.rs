//! A unit registry by dimension, for the UCUM case-sensitive codes the SDR
//! uses (README, Schemas).
//!
//! A code is a product of components joined by `.` (multiply) or `/` (divide
//! by the next component), for example `kg/m3`, `N.m` or `m/s2`. A component
//! is an atom, a metric prefix on a metric atom, and an optional integer
//! exponent. Annotations (`{..}`), special units in brackets (`[in_i]`) and
//! units with an offset (`Cel`) are not supported and are reported as unknown,
//! never guessed.

use std::f64::consts::PI;
use std::fmt;

/// Exponents of the base dimensions: length, mass, time, electric current,
/// temperature, amount of substance, luminous intensity and plane angle.
/// UCUM treats plane angle as a base dimension, so radians never compare
/// equal to a plain ratio.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dimension([i32; 8]);

const BASE_CODES: [&str; 8] = ["m", "kg", "s", "A", "K", "mol", "cd", "rad"];

impl Dimension {
    const NONE: Dimension = Dimension([0; 8]);

    const fn base(index: usize) -> Dimension {
        let mut exponents = [0; 8];
        exponents[index] = 1;
        Dimension(exponents)
    }

    const fn of(exponents: [i32; 8]) -> Dimension {
        Dimension(exponents)
    }

    fn times(self, other: Dimension, power: i32) -> Dimension {
        let mut exponents = self.0;
        for (e, o) in exponents.iter_mut().zip(other.0) {
            *e += o * power;
        }
        Dimension(exponents)
    }

    /// The coherent SI unit of this dimension, as a UCUM code, for example
    /// `kg.m-3`; `1` when dimensionless.
    pub fn si_code(self) -> String {
        let parts: Vec<String> = BASE_CODES
            .iter()
            .zip(self.0)
            .filter(|(_, e)| *e != 0)
            .map(|(code, e)| {
                if e == 1 {
                    (*code).to_owned()
                } else {
                    format!("{code}{e}")
                }
            })
            .collect();
        if parts.is_empty() {
            "1".to_owned()
        } else {
            parts.join(".")
        }
    }
}

impl fmt::Display for Dimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.si_code())
    }
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum UnitError {
    #[error("{code:?} is not a supported UCUM unit: {reason}")]
    Unknown { code: String, reason: String },
    #[error("cannot convert {from} ({from_dimension}) to {to} ({to_dimension})")]
    Mismatch {
        from: String,
        from_dimension: Dimension,
        to: String,
        to_dimension: Dimension,
    },
}

/// A parsed unit: its dimension and the factor that converts a value in it
/// to the coherent SI unit of that dimension.
#[derive(Clone, Debug, PartialEq)]
pub struct Unit {
    code: String,
    dimension: Dimension,
    factor: f64,
}

struct Atom {
    code: &'static str,
    dimension: Dimension,
    factor: f64,
    /// Whether metric prefixes apply.
    metric: bool,
}

const LENGTH: Dimension = Dimension::base(0);
const MASS: Dimension = Dimension::base(1);
const TIME: Dimension = Dimension::base(2);
const ANGLE: Dimension = Dimension::base(7);

const ATOMS: &[Atom] = &[
    atom("m", LENGTH, 1.0, true),
    atom("g", MASS, 1e-3, true),
    atom("t", MASS, 1e3, true),
    atom("s", TIME, 1.0, true),
    atom("A", Dimension::base(3), 1.0, true),
    atom("K", Dimension::base(4), 1.0, true),
    atom("mol", Dimension::base(5), 1.0, true),
    atom("cd", Dimension::base(6), 1.0, true),
    atom("rad", ANGLE, 1.0, true),
    atom("N", Dimension::of([1, 1, -2, 0, 0, 0, 0, 0]), 1.0, true),
    atom("Pa", Dimension::of([-1, 1, -2, 0, 0, 0, 0, 0]), 1.0, true),
    atom("J", Dimension::of([2, 1, -2, 0, 0, 0, 0, 0]), 1.0, true),
    atom("W", Dimension::of([2, 1, -3, 0, 0, 0, 0, 0]), 1.0, true),
    atom("Hz", Dimension::of([0, 0, -1, 0, 0, 0, 0, 0]), 1.0, true),
    atom("L", Dimension::of([3, 0, 0, 0, 0, 0, 0, 0]), 1e-3, true),
    atom("l", Dimension::of([3, 0, 0, 0, 0, 0, 0, 0]), 1e-3, true),
    atom("min", TIME, 60.0, false),
    atom("h", TIME, 3600.0, false),
    atom("d", TIME, 86400.0, false),
    atom("deg", ANGLE, PI / 180.0, false),
    atom("%", Dimension::NONE, 1e-2, false),
    atom("1", Dimension::NONE, 1.0, false),
];

const fn atom(code: &'static str, dimension: Dimension, factor: f64, metric: bool) -> Atom {
    Atom {
        code,
        dimension,
        factor,
        metric,
    }
}

/// Longest first, so `da` wins over `d`.
const PREFIXES: &[(&str, f64)] = &[
    ("da", 1e1),
    ("Y", 1e24),
    ("Z", 1e21),
    ("E", 1e18),
    ("P", 1e15),
    ("T", 1e12),
    ("G", 1e9),
    ("M", 1e6),
    ("k", 1e3),
    ("h", 1e2),
    ("d", 1e-1),
    ("c", 1e-2),
    ("m", 1e-3),
    ("u", 1e-6),
    ("n", 1e-9),
    ("p", 1e-12),
    ("f", 1e-15),
    ("a", 1e-18),
];

impl Unit {
    pub fn parse(code: &str) -> Result<Unit, UnitError> {
        let unknown = |reason: &str| UnitError::Unknown {
            code: code.to_owned(),
            reason: reason.to_owned(),
        };
        if code.is_empty() {
            return Err(unknown("empty"));
        }
        if code.contains(['{', '[', '(', ' ']) {
            return Err(unknown(
                "annotations, brackets and spaces are not supported",
            ));
        }
        let mut dimension = Dimension::NONE;
        let mut factor = 1.0;
        let mut rest = code;
        let mut divide = false;
        if let Some(stripped) = rest.strip_prefix('/') {
            rest = stripped;
            divide = true;
        }
        loop {
            let end = rest.find(['.', '/']).unwrap_or(rest.len());
            let (component, tail) = rest.split_at(end);
            let (atom_dimension, atom_factor, exponent) = component_of(component)
                .ok_or_else(|| unknown(&format!("unknown component {component:?}")))?;
            let power = if divide { -exponent } else { exponent };
            dimension = dimension.times(atom_dimension, power);
            factor *= atom_factor.powi(power);
            let Some(operator) = tail.chars().next() else {
                break;
            };
            divide = operator == '/';
            rest = &tail[1..];
        }
        if !factor.is_finite() || factor == 0.0 {
            return Err(unknown("the conversion factor is out of range"));
        }
        Ok(Unit {
            code: code.to_owned(),
            dimension,
            factor,
        })
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn dimension(&self) -> Dimension {
        self.dimension
    }

    /// `value` in this unit, expressed in the coherent SI unit.
    pub fn to_si(&self, value: f64) -> f64 {
        value * self.factor
    }

    /// `value` in this unit, expressed in `target`.
    pub fn convert(&self, value: f64, target: &Unit) -> Result<f64, UnitError> {
        if self.dimension != target.dimension {
            return Err(UnitError::Mismatch {
                from: self.code.clone(),
                from_dimension: self.dimension,
                to: target.code.clone(),
                to_dimension: target.dimension,
            });
        }
        Ok(value * self.factor / target.factor)
    }
}

/// An atom, with or without a metric prefix, and its exponent.
fn component_of(component: &str) -> Option<(Dimension, f64, i32)> {
    let digits_start = component
        .char_indices()
        .find(|(i, c)| c.is_ascii_digit() || (*i > 0 && (*c == '-' || *c == '+')))
        .map_or(component.len(), |(i, _)| i);
    // `1` is an atom, not an exponent.
    let (symbol, exponent) = if digits_start == 0 {
        (component, 1)
    } else {
        let (symbol, digits) = component.split_at(digits_start);
        let exponent = if digits.is_empty() {
            1
        } else {
            digits.parse::<i32>().ok()?
        };
        (symbol, exponent)
    };
    if let Some(atom) = ATOMS.iter().find(|a| a.code == symbol) {
        return Some((atom.dimension, atom.factor, exponent));
    }
    PREFIXES.iter().find_map(|(prefix, scale)| {
        let rest = symbol.strip_prefix(prefix)?;
        let atom = ATOMS.iter().find(|a| a.metric && a.code == rest)?;
        Some((atom.dimension, atom.factor * scale, exponent))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-12 * a.abs().max(b.abs()).max(1.0)
    }

    #[test]
    fn common_codes_parse_with_the_right_dimension_and_factor() {
        let cases: &[(&str, &str, f64)] = &[
            ("kg", "kg", 1.0),
            ("g", "kg", 1e-3),
            ("mg", "kg", 1e-6),
            ("t", "kg", 1e3),
            ("mm", "m", 1e-3),
            ("km", "m", 1e3),
            ("um", "m", 1e-6),
            ("dam", "m", 10.0),
            ("mm2", "m2", 1e-6),
            ("mm3", "m3", 1e-9),
            ("cm3", "m3", 1e-6),
            ("L", "m3", 1e-3),
            ("mL", "m3", 1e-6),
            ("kg/m3", "m-3.kg", 1.0),
            ("g/cm3", "m-3.kg", 1e3),
            ("N.m", "m2.kg.s-2", 1.0),
            ("m/s2", "m.s-2", 1.0),
            ("kg.m/s2", "m.kg.s-2", 1.0),
            ("MPa", "m-1.kg.s-2", 1e6),
            ("min", "s", 60.0),
            ("h", "s", 3600.0),
            ("/s", "s-1", 1.0),
            ("m-1", "m-1", 1.0),
            ("cd", "cd", 1.0),
            ("deg", "rad", PI / 180.0),
            ("1", "1", 1.0),
            ("%", "1", 0.01),
        ];
        for (code, si, factor) in cases {
            let unit = Unit::parse(code).unwrap_or_else(|e| panic!("{code}: {e}"));
            assert_eq!(unit.dimension().si_code(), *si, "{code}");
            assert!(
                close(unit.to_si(1.0), *factor),
                "{code}: {}",
                unit.to_si(1.0)
            );
        }
    }

    #[test]
    fn unsupported_codes_are_reported_not_guessed() {
        for code in [
            "",
            "kgg",
            "[in_i]",
            "Cel",
            "m{length}",
            "xm",
            "m^2",
            "k",
            "m.",
            "foo/s",
        ] {
            assert!(
                matches!(Unit::parse(code), Err(UnitError::Unknown { .. })),
                "{code:?} parsed"
            );
        }
    }

    #[test]
    fn conversion_needs_the_same_dimension() {
        let g = Unit::parse("g").unwrap();
        let kg = Unit::parse("kg").unwrap();
        let mm = Unit::parse("mm").unwrap();
        assert!(close(g.convert(1500.0, &kg).unwrap(), 1.5));
        assert!(matches!(
            g.convert(1.0, &mm),
            Err(UnitError::Mismatch { .. })
        ));
        let rad = Unit::parse("rad").unwrap();
        let one = Unit::parse("1").unwrap();
        assert!(rad.convert(1.0, &one).is_err(), "an angle is not a ratio");
    }
}
