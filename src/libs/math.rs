//! BT mathematics standard library.
//!
//! `Math` is a global stateless object whose methods read arguments directly and return numeric values.
//! Integral results become `Int`; non-integral results remain `Float`, keeping script output natural.

use crate::value::Value;
use std::time::{SystemTime, UNIX_EPOCH};

/// Math library object.
#[derive(Debug, Clone, PartialEq)]
pub struct BtMath;

impl BtMath {
    /// Reads mathematical constant attributes.
    pub fn get_property(&self, name: &str) -> Option<Value> {
        let value = match name {
            "E" => std::f64::consts::E,
            "LN2" => std::f64::consts::LN_2,
            "LN10" => std::f64::consts::LN_10,
            "LOG2E" => std::f64::consts::LOG2_E,
            "LOG10E" => std::f64::consts::LOG10_E,
            "PI" => std::f64::consts::PI,
            "SQRT1_2" => std::f64::consts::FRAC_1_SQRT_2,
            "SQRT2" => std::f64::consts::SQRT_2,
            "TAU" => std::f64::consts::TAU,
            _ => return None,
        };
        Some(number_value(value))
    }

    /// Calls a mathematical method.
    pub fn call_method(&self, method: &str, args: Vec<Value>) -> Result<Value, String> {
        if method == "random" {
            return Ok(Value::Float(random_unit()));
        }

        let first = || required_number_arg(&args, 0, method);
        let second = || required_number_arg(&args, 1, method);
        let third = || required_number_arg(&args, 2, method);
        let value = match method {
            "abs" => first()?.abs(),
            "pow" => first()?.powf(second()?),
            "sqrt" => first()?.sqrt(),
            "cbrt" => first()?.cbrt(),
            "hypot" => first()?.hypot(second()?),
            "exp" => first()?.exp(),
            "exp2" => first()?.exp2(),
            "expm1" => first()?.exp_m1(),
            "ln" => first()?.ln(),
            "log" => first()?.log(second()?),
            "log10" => first()?.log10(),
            "log2" => first()?.log2(),
            "log1p" => first()?.ln_1p(),
            "sin" => first()?.sin(),
            "cos" => first()?.cos(),
            "tan" => first()?.tan(),
            "asin" => first()?.asin(),
            "acos" => first()?.acos(),
            "atan" => first()?.atan(),
            "atan2" => first()?.atan2(second()?),
            "sinh" => first()?.sinh(),
            "cosh" => first()?.cosh(),
            "tanh" => first()?.tanh(),
            "asinh" => first()?.asinh(),
            "acosh" => first()?.acosh(),
            "atanh" => first()?.atanh(),
            "round" => first()?.round(),
            "ceil" => first()?.ceil(),
            "floor" => first()?.floor(),
            "trunc" => first()?.trunc(),
            "rad" => first()?.to_radians(),
            "deg" => first()?.to_degrees(),
            "sign" => {
                let value = first()?;
                if value > 0.0 {
                    1.0
                } else if value < 0.0 {
                    -1.0
                } else {
                    0.0
                }
            }
            "clamp" => {
                let value = first()?;
                let min = second()?;
                let max = third()?;
                if min > max {
                    return Err("Math.clamp(): minimum cannot be greater than maximum".to_string());
                }
                if value < min {
                    min
                } else if value > max {
                    max
                } else {
                    value
                }
            }
            "min" => {
                if args.is_empty() {
                    return Err("Math.min() requires at least 1 numeric argument".to_string());
                }
                args.iter()
                    .map(Value::to_f64_lossy)
                    .fold(f64::INFINITY, f64::min)
            }
            "max" => {
                if args.is_empty() {
                    return Err("Math.max() requires at least 1 numeric argument".to_string());
                }
                args.iter()
                    .map(Value::to_f64_lossy)
                    .fold(f64::NEG_INFINITY, f64::max)
            }
            _ => return Err(format!("Math has no method `{}`", method)),
        };
        Ok(number_value(value))
    }
}

/// Reads the numeric argument at the specified position.
fn required_number_arg(args: &[Value], index: usize, method: &str) -> Result<f64, String> {
    args.get(index).map(Value::to_f64_lossy).ok_or_else(|| {
        format!(
            "Math.{}() is missing numeric argument {}",
            method,
            index + 1
        )
    })
}

/// Squeeze floating point results into a more natural numeric type on the script side.
fn number_value(value: f64) -> Value {
    if value.is_finite()
        && value.fract() == 0.0
        && value >= i64::MIN as f64
        && value < -(i64::MIN as f64)
    {
        Value::Int(value as i64)
    } else {
        Value::Float(value)
    }
}

/// Generates a lightweight random floating-point value in `0 <= n < 1`.
fn random_unit() -> f64 {
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_nanos() as u64)
        .unwrap_or(0);
    let mut x = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    ((x >> 11) as f64) * (1.0 / ((1u64 << 53) as f64))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Check that Math constants are returned as attributes.
    #[test]
    fn math_constants_are_properties() {
        let math = BtMath;
        assert_eq!(
            math.get_property("E"),
            Some(Value::Float(std::f64::consts::E))
        );
        assert_eq!(
            math.get_property("TAU"),
            Some(Value::Float(std::f64::consts::TAU))
        );
        assert_eq!(math.get_property("NOPE"), None);
    }

    /// Verification static method returns expected number.
    #[test]
    fn static_methods_return_expected_numbers() {
        let math = BtMath;
        assert_eq!(
            math.call_method("pow", vec![Value::Int(2), Value::Int(3)]),
            Ok(Value::Int(8))
        );
        assert_eq!(
            math.call_method("sqrt", vec![Value::Int(9)]),
            Ok(Value::Int(3))
        );
        assert_eq!(
            math.call_method(
                "clamp",
                vec![Value::Int(120), Value::Int(0), Value::Int(100)]
            ),
            Ok(Value::Int(100))
        );
        assert_eq!(
            math.call_method("hypot", vec![Value::Int(3), Value::Int(4)]),
            Ok(Value::Int(5))
        );
    }

    /// Uses the Math object name when reporting a missing argument.
    #[test]
    fn missing_arguments_report_math_errors() {
        let math = BtMath;
        assert_eq!(
            math.call_method("sqrt", Vec::new()),
            Err("Math.sqrt() is missing numeric argument 1".to_string())
        );
        assert_eq!(
            math.call_method("min", Vec::new()),
            Err("Math.min() requires at least 1 numeric argument".to_string())
        );
        assert_eq!(
            math.call_method("log", vec![Value::Int(8)]),
            Err("Math.log() is missing numeric argument 2".to_string())
        );
    }

    /// Check that the random number always returns a Float and falls within the half-open interval.
    #[test]
    fn random_returns_float_unit_value() {
        let math = BtMath;
        for _ in 0..8 {
            let value = math.call_method("random", Vec::new()).unwrap();
            let Value::Float(value) = value else {
                panic!("Math.random() should return a Float");
            };
            assert!((0.0..1.0).contains(&value));
        }
    }

    /// Checks that integral floating-point values outside the i64 range are not narrowed to Int.
    #[test]
    fn number_value_keeps_out_of_range_integer_float() {
        assert_eq!(
            number_value((i64::MAX as f64) * 2.0),
            Value::Float((i64::MAX as f64) * 2.0)
        );
    }
}
