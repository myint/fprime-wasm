//! Time tags: `RHH:MM:SS[.ffffff]` and `AYYYY-DDDTHH:MM:SS[.ffffff]`, as `fprime-seqgen` reads
//! them.

/// When a command is dispatched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeTag {
    /// Microseconds after the previous command completed, or after the sequence started.
    Relative(u64),
    /// Microseconds since the F Prime time epoch (1970-001T00:00:00 UTC).
    Absolute(u64),
}

const RELATIVE_FORMAT: &str = "HH:MM:SS or HH:MM:SS.ffffff";
const ABSOLUTE_FORMAT: &str = "YYYY-DDDTHH:MM:SS or YYYY-DDDTHH:MM:SS.ffffff";

/// `Fw::Time` holds whole seconds in a `U32`; the sequencer traps a sleep past it.
const MAX_SECONDS: u64 = u32::MAX as u64;

/// The text after an `R`.
pub fn relative(text: &str) -> Result<u64, String> {
    let (hms, fraction) = split_fraction(text, RELATIVE_FORMAT)?;
    let us = clock(hms, RELATIVE_FORMAT)?;
    Ok(us + fraction)
}

/// The text after an `A`.
pub fn absolute(text: &str) -> Result<u64, String> {
    let malformed = || format!("`A{text}` is not a time; write {ABSOLUTE_FORMAT}");

    let (date, time) = text.split_once('T').ok_or_else(malformed)?;
    let (year, day) = date.split_once('-').ok_or_else(malformed)?;
    let year = digits(year, 4).ok_or_else(malformed)?;
    let day = digits(day, 3).ok_or_else(malformed)?;

    if year < 1970 {
        return Err(format!(
            "`A{text}` is before 1970, where F Prime time starts"
        ));
    }
    let days_in_year = if leap(year) { 366 } else { 365 };
    if day == 0 || day > days_in_year {
        return Err(format!(
            "`A{text}`: day of the year must be 001 to {days_in_year} in {year}"
        ));
    }

    let (hms, fraction) = split_fraction(time, ABSOLUTE_FORMAT)?;
    let since_midnight = clock(hms, ABSOLUTE_FORMAT)?;
    let days = days_before(year) + (day - 1);
    let us = days * 86_400 * 1_000_000 + since_midnight + fraction;

    if us / 1_000_000 > MAX_SECONDS {
        return Err(format!(
            "`A{text}` is after 2106-038T06:28:15, the last second `Fw::Time` can hold"
        ));
    }
    Ok(us)
}

/// `HH:MM:SS` and the microseconds of an optional `.ffffff`, read as a decimal fraction
/// (`.5` is half a second).
fn split_fraction<'a>(text: &'a str, format: &str) -> Result<(&'a str, u64), String> {
    let Some((hms, fraction)) = text.split_once('.') else {
        return Ok((text, 0));
    };
    if fraction.is_empty() || fraction.len() > 6 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "`{text}`: a fraction of a second is 1 to 6 digits; write {format}"
        ));
    }
    let scale = 10u64.pow(6 - fraction.len() as u32);
    let value: u64 = fraction.parse().expect("checked to be digits");
    Ok((hms, value * scale))
}

/// `HH:MM:SS` in microseconds.
fn clock(text: &str, format: &str) -> Result<u64, String> {
    let malformed = || format!("`{text}` is not a time of day; write {format}");

    let mut parts = text.split(':');
    let (Some(hours), Some(minutes), Some(seconds), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(malformed());
    };
    let hours = digits(hours, 2).ok_or_else(malformed)?;
    let minutes = digits(minutes, 2).ok_or_else(malformed)?;
    let seconds = digits(seconds, 2).ok_or_else(malformed)?;

    if hours > 23 || minutes > 59 || seconds > 59 {
        return Err(format!(
            "`{text}` is out of range: hours run 00 to 23, minutes and seconds 00 to 59"
        ));
    }
    Ok(((hours * 60 + minutes) * 60 + seconds) * 1_000_000)
}

/// Exactly `width` ASCII digits.
fn digits(text: &str, width: usize) -> Option<u64> {
    (text.len() == width && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().expect("checked to be digits"))
}

fn leap(year: u64) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

/// Days from 1970-001 to the first day of `year`.
fn days_before(year: u64) -> u64 {
    // Leap years in [1, y]
    let leaps = |y: u64| y / 4 - y / 100 + y / 400;
    365 * (year - 1970) + leaps(year - 1) - leaps(1969)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times() {
        assert_eq!(relative("00:00:00"), Ok(0));
        assert_eq!(relative("00:00:01"), Ok(1_000_000));
        assert_eq!(relative("01:00:01.050"), Ok(3_601_050_000));
        assert_eq!(relative("00:00:00.5"), Ok(500_000));
        assert_eq!(relative("00:00:00.000001"), Ok(1));
        assert_eq!(relative("23:59:59.999999"), Ok(86_399_999_999));
    }

    #[test]
    fn rejects_malformed_relative_times() {
        for bad in [
            "1:00:00",
            "00:00",
            "00:00:00:00",
            "24:00:00",
            "00:60:00",
            "00:00:60",
            "00:00:00.",
            "00:00:00.1234567",
            "00:00:0a",
        ] {
            assert!(relative(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn absolute_times() {
        assert_eq!(absolute("1970-001T00:00:00"), Ok(0));
        assert_eq!(absolute("1970-002T00:00:00.25"), Ok(86_400_250_000));
        // `date -u -d 2015-03-16T22:32:40 +%s` (day 075 of 2015)
        assert_eq!(absolute("2015-075T22:32:40.123"), Ok(1_426_545_160_123_000));
        // A leap day, and the day after it
        assert_eq!(absolute("2024-060T00:00:00"), Ok(1_709_164_800_000_000));
        assert_eq!(absolute("2024-366T00:00:00"), Ok(1_735_603_200_000_000));
        // The last second `Fw::Time` holds
        assert_eq!(
            absolute("2106-038T06:28:15"),
            Ok(u64::from(u32::MAX) * 1_000_000)
        );
    }

    #[test]
    fn rejects_malformed_absolute_times() {
        for bad in [
            "2024-366",
            "2024-1T00:00:00",
            "24-001T00:00:00",
            "1969-365T23:59:59",
            "2023-366T00:00:00",
            "2024-000T00:00:00",
            "2106-038T06:28:16",
            "2024-001T25:00:00",
        ] {
            assert!(absolute(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn days_before_counts_leap_years() {
        assert_eq!(days_before(1970), 0);
        assert_eq!(days_before(1971), 365);
        assert_eq!(days_before(1973), 365 * 3 + 1);
        assert_eq!(days_before(2000), 10_957);
        assert_eq!(days_before(2001), 10_957 + 366);
    }
}
