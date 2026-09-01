//! BT datetime standard library.
//!
//! Date objects keep only their own minimal state. Constructors and method dispatch return `crate::value::Value`,
//! leaving the VM responsible only for routing function and method names.

use crate::value::Value;
use chrono::{
    DateTime, Datelike, Duration, Local, LocalResult, Months, NaiveDateTime, TimeZone, Timelike,
};

/// BT date object.
///
/// Dates use the local time zone and follow value semantics in scripts. Methods that
/// adjust a date return a new object rather than mutating shared state in place.
#[derive(Debug, Clone, PartialEq)]
pub struct BtDate {
    /// The local time held by the current date object.
    time: DateTime<Local>,
}

impl BtDate {
    /// Creates a date object.
    ///
    /// With no arguments, returns the current local time. Integer and floating-point
    /// arguments are Unix timestamps in seconds; strings are parsed as common date literals.
    ///
    /// `date('%Y-%m-%d')` is parsed as a date literal, not treated as a format string.
    /// Use `date().format('%Y-%m-%d')` to format the current time.
    pub fn new(args: Vec<Value>) -> Result<Value, String> {
        let date = match args.first() {
            None => Self { time: Local::now() },
            Some(Value::Int(timestamp)) => Self::from_timestamp(*timestamp)?,
            Some(Value::Float(timestamp)) => Self::from_timestamp(*timestamp as i64)?,
            Some(Value::Str(text)) => Self::parse(text)?,
            Some(value) => {
                return Err(format!(
                    "date() accepts no argument, a timestamp, or a date string; received {}",
                    value.type_name()
                ));
            }
        };
        Ok(Value::Date(date))
    }

    /// Dispatches a date-object method.
    ///
    /// Date-specific capabilities stay behind this dispatch table, so the VM only
    /// needs to route the method name.
    pub fn call_method(&self, method: &str, args: Vec<Value>) -> Result<Value, String> {
        match method {
            "format" => {
                let pattern = args
                    .first()
                    .map(Value::to_string)
                    .unwrap_or_else(|| "%Y-%m-%d %H:%M:%S".to_string());
                Ok(Value::Str(self.format(&pattern)))
            }
            "from_string" => {
                let text = Self::required_string(&args, 0, method)?;
                Ok(Value::Date(Self::parse(&text)?))
            }
            "from_timestamp" => {
                let timestamp = Self::required_i64(&args, 0, method)?;
                Ok(Value::Date(Self::from_timestamp(timestamp)?))
            }
            "from_millis" => {
                let millis = Self::required_i64(&args, 0, method)?;
                Ok(Value::Date(Self::from_millis(millis)?))
            }
            "from_micros" => {
                let micros = Self::required_i64(&args, 0, method)?;
                Ok(Value::Date(Self::from_micros(micros)?))
            }
            "from_nanos" => {
                let nanos = Self::required_i64(&args, 0, method)?;
                Ok(Value::Date(Self::from_nanos(nanos)?))
            }
            "add" => {
                let amount = Self::required_i64(&args, 0, method)?;
                let unit = args
                    .get(1)
                    .map(Value::to_string)
                    .unwrap_or_else(|| "seconds".to_string());
                Ok(Value::Date(self.add(amount, &unit)?))
            }
            "diff" => {
                let other = Self::required_date(&args, 0, method)?;
                let unit = args
                    .get(1)
                    .map(Value::to_string)
                    .unwrap_or_else(|| "millis".to_string());
                Ok(Value::Int(self.diff(&other, &unit)?))
            }
            "start_of_day" => Ok(Value::Date(self.start_of_day()?)),
            "timestamp" => Ok(Value::Int(self.time.timestamp())),
            "timestamp_millis" => Ok(Value::Int(self.time.timestamp_millis())),
            "timestamp_micros" => Ok(Value::Int(self.time.timestamp_micros())),
            "timestamp_nanos" => Ok(Value::Int(self.time.timestamp_nanos_opt().unwrap_or(0))),
            "year" => Ok(Value::Int(self.time.year() as i64)),
            "month" => Ok(Value::Int(self.time.month() as i64)),
            "day" => Ok(Value::Int(self.time.day() as i64)),
            "hour" => Ok(Value::Int(self.time.hour() as i64)),
            "minute" => Ok(Value::Int(self.time.minute() as i64)),
            "second" => Ok(Value::Int(self.time.second() as i64)),
            "millis" => Ok(Value::Int(self.time.timestamp_subsec_millis() as i64)),
            "micros" => Ok(Value::Int(self.time.timestamp_subsec_micros() as i64)),
            "nanos" => Ok(Value::Int(self.time.timestamp_subsec_nanos() as i64)),
            "weekday" => Ok(Value::Int(self.time.weekday().number_from_monday() as i64)),
            "week" => Ok(Value::Int(self.time.iso_week().week() as i64)),
            "yearday" => Ok(Value::Int(self.time.ordinal() as i64)),
            "quarter" => Ok(Value::Int(((self.time.month() - 1) / 3 + 1) as i64)),
            "to_string" => Ok(Value::Str(self.format("%Y-%m-%d %H:%M:%S"))),
            _ => Err(format!("date has no method `{}`", method)),
        }
    }

    /// Format date to string.
    ///
    /// BT Date uses chrono/strftime style format characters, for example:
    /// - `%Y-%m-%d %H:%M:%S`
    /// - `%F %T`
    /// - `%Y-%m-%d %H:%M:%S`
    pub fn format(&self, pattern: &str) -> String {
        self.time.format(pattern).to_string()
    }

    /// Returns a new date after adding or subtracting the requested duration.
    fn add(&self, amount: i64, unit: &str) -> Result<Self, String> {
        let time = match unit {
            "millisecond" | "milliseconds" | "millis" => self.time + Duration::milliseconds(amount),
            "second" | "seconds" => self.time + Duration::seconds(amount),
            "minute" | "minutes" => self.time + Duration::minutes(amount),
            "hour" | "hours" => self.time + Duration::hours(amount),
            "day" | "days" => self.time + Duration::days(amount),
            "week" | "weeks" => self.time + Duration::weeks(amount),
            "month" | "months" => return self.add_months(amount),
            "year" | "years" => return self.add_months(amount.saturating_mul(12)),
            _ => return Err(format!("date.add() does not support the `{}` unit", unit)),
        };
        Ok(Self { time })
    }

    /// Returns the difference between two dates in the requested unit.
    fn diff(&self, other: &Self, unit: &str) -> Result<i64, String> {
        let duration = self.time.signed_duration_since(other.time);
        let value = match unit {
            "millisecond" | "milliseconds" | "millis" => duration.num_milliseconds(),
            "second" | "seconds" => duration.num_seconds(),
            "minute" | "minutes" => duration.num_minutes(),
            "hour" | "hours" => duration.num_hours(),
            "day" | "days" => duration.num_days(),
            "week" | "weeks" => duration.num_weeks(),
            _ => return Err(format!("date.diff() does not support the `{}` unit", unit)),
        };
        Ok(value)
    }

    /// Returns a new date at the start of the same local day.
    fn start_of_day(&self) -> Result<Self, String> {
        let Some(naive) = self.time.date_naive().and_hms_opt(0, 0, 0) else {
            return Err(
                "date.start_of_day() cannot construct the start of this local day".to_string(),
            );
        };
        Self::from_local_naive(naive, "start_of_day")
    }

    /// Adds or subtracts whole calendar months.
    fn add_months(&self, amount: i64) -> Result<Self, String> {
        let months = u32::try_from(amount.unsigned_abs())
            .map_err(|_| "date.add(): month count is too large".to_string())?;
        let months = Months::new(months);
        let time = if amount >= 0 {
            self.time.checked_add_months(months).ok_or_else(|| {
                "date.add(): result is outside the supported date range".to_string()
            })?
        } else {
            self.time.checked_sub_months(months).ok_or_else(|| {
                "date.add(): result is outside the supported date range".to_string()
            })?
        };
        Ok(Self { time })
    }

    /// Creates a date with a second-level timestamp.
    fn from_timestamp(timestamp: i64) -> Result<Self, String> {
        match Local.timestamp_opt(timestamp, 0) {
            LocalResult::Single(time) => Ok(Self { time }),
            LocalResult::Ambiguous(time, _) => Ok(Self { time }),
            LocalResult::None => Err(format!(
                "Timestamp `{}` cannot be converted to local time",
                timestamp
            )),
        }
    }

    /// Creates a date from a millisecond timestamp.
    fn from_millis(millis: i64) -> Result<Self, String> {
        match Local.timestamp_millis_opt(millis) {
            LocalResult::Single(time) => Ok(Self { time }),
            LocalResult::Ambiguous(time, _) => Ok(Self { time }),
            LocalResult::None => Err(format!(
                "Millisecond timestamp `{}` cannot be converted to local time",
                millis
            )),
        }
    }

    /// Creates a date from a microsecond timestamp.
    fn from_micros(micros: i64) -> Result<Self, String> {
        let seconds = micros.div_euclid(1_000_000);
        let nanos = (micros.rem_euclid(1_000_000) * 1_000) as u32;
        Self::from_timestamp_with_nanos(
            seconds,
            nanos,
            &format!("Microsecond timestamp `{}`", micros),
        )
    }

    /// Creates a date with a nanosecond timestamp.
    fn from_nanos(nanos: i64) -> Result<Self, String> {
        let seconds = nanos.div_euclid(1_000_000_000);
        let sub_nanos = nanos.rem_euclid(1_000_000_000) as u32;
        Self::from_timestamp_with_nanos(
            seconds,
            sub_nanos,
            &format!("Nanosecond Timestamp `{}`", nanos),
        )
    }

    /// Creates a date from the seconds and nanosecond parts.
    fn from_timestamp_with_nanos(seconds: i64, nanos: u32, source: &str) -> Result<Self, String> {
        match Local.timestamp_opt(seconds, nanos) {
            LocalResult::Single(time) => Ok(Self { time }),
            LocalResult::Ambiguous(time, _) => Ok(Self { time }),
            LocalResult::None => Err(format!("{} cannot be converted to local time", source)),
        }
    }

    /// Parses common date strings.
    ///
    /// Fixed format tables avoid the ambiguity and extra allocation of regex-based guessing. Supported forms cover common Web input such as
    /// `YYYY-MM-DD HH:mm:ss`, `YYYY/MM/DD`, and ISO-style timestamps with a `T` separator.
    fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("date() cannot parse an empty date string".to_string());
        }
        if let Ok(timestamp) = text.parse::<i64>() {
            return Self::from_timestamp(timestamp);
        }

        const PATTERNS: [&str; 8] = [
            "%Y-%m-%d %H:%M:%S",
            "%Y/%m/%d %H:%M:%S",
            "%Y-%m-%dT%H:%M:%S",
            "%Y/%m/%dT%H:%M:%S",
            "%Y-%m-%d %H:%M",
            "%Y/%m/%d %H:%M",
            "%Y-%m-%d",
            "%Y/%m/%d",
        ];

        for pattern in PATTERNS {
            if let Ok(naive) = NaiveDateTime::parse_from_str(text, pattern) {
                return Self::from_local_naive(naive, text);
            }
            if pattern.ends_with("%H:%M:%S") || pattern.ends_with("%H:%M") {
                continue;
            }
            let full_text = format!("{} 00:00:00", text);
            let full_pattern = format!("{} %H:%M:%S", pattern);
            if let Ok(naive) = NaiveDateTime::parse_from_str(&full_text, &full_pattern) {
                return Self::from_local_naive(naive, text);
            }
        }

        Err(format!("date() cannot parse a date string `{}`", text))
    }

    /// Interprets a timezone-free value in the local time zone.
    fn from_local_naive(naive: NaiveDateTime, source: &str) -> Result<Self, String> {
        match Local.from_local_datetime(&naive) {
            LocalResult::Single(time) => Ok(Self { time }),
            LocalResult::Ambiguous(time, _) => Ok(Self { time }),
            LocalResult::None => Err(format!(
                "date string `{}` does not have a corresponding local time",
                source
            )),
        }
    }

    /// Reads a required integer argument.
    fn required_i64(args: &[Value], index: usize, method: &str) -> Result<i64, String> {
        args.get(index)
            .map(Value::to_i64_lossy)
            .ok_or_else(|| format!("date.{}() requires argument {}", method, index + 1))
    }

    /// Reads a required string argument.
    fn required_string(args: &[Value], index: usize, method: &str) -> Result<String, String> {
        args.get(index)
            .map(Value::to_string)
            .ok_or_else(|| format!("date.{}() requires argument {}", method, index + 1))
    }

    /// Reads a required date argument.
    fn required_date(args: &[Value], index: usize, method: &str) -> Result<Self, String> {
        match args.get(index) {
            Some(Value::Date(value)) => Ok(value.clone()),
            Some(Value::Str(value)) => Self::parse(value),
            Some(Value::Int(value)) => Self::from_timestamp(*value),
            Some(Value::Float(value)) => Self::from_timestamp(*value as i64),
            Some(value) => Err(format!(
                "date.{}() argument {} must be a Date, timestamp, or date string; received {}",
                method,
                index + 1,
                value.type_name()
            )),
            None => Err(format!("date.{}() requires argument {}", method, index + 1)),
        }
    }
}
