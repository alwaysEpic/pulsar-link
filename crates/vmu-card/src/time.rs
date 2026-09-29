use crate::{Error, Result};

/// A save's date and time, as the card and the `.VMI` carry it.
///
/// The card stores it as eight BCD bytes (century, year, month, day, hour, minute,
/// second, weekday with 0 = Monday); a `.VMI` stores the same fields in binary, with a
/// 16-bit year and 0 = Sunday. The weekday is kept in the card's convention here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    /// 0 = Monday … 6 = Sunday, as on the card.
    pub weekday: u8,
}

const fn from_bcd(b: u8) -> Option<u8> {
    let (hi, lo) = (b >> 4, b & 0x0F);
    if hi > 9 || lo > 9 {
        None
    } else {
        Some(hi * 10 + lo)
    }
}

const fn to_bcd(v: u8) -> u8 {
    ((v / 10) << 4) | (v % 10)
}

impl Timestamp {
    /// Build one, checking the fields are a plausible date and time.
    ///
    /// # Errors
    /// [`Error::BadTime`] for a field out of range.
    pub fn new(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> Result<Self> {
        let weekday = weekday_monday0(year, month, day);
        let t = Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
            weekday,
        };
        t.check()?;
        Ok(t)
    }

    const fn check(self) -> Result<()> {
        if self.year > 9999
            || self.month < 1
            || self.month > 12
            || self.day < 1
            || self.day > 31
            || self.hour > 23
            || self.minute > 59
            || self.second > 59
            || self.weekday > 6
        {
            Err(Error::BadTime)
        } else {
            Ok(())
        }
    }

    /// Read the card's eight BCD bytes.
    ///
    /// # Errors
    /// [`Error::BadTime`] for a byte that is not BCD or a field out of range.
    pub fn from_card(b: [u8; 8]) -> Result<Self> {
        let d = |i: usize| from_bcd(b[i]).ok_or(Error::BadTime);
        let t = Self {
            year: u16::from(d(0)?) * 100 + u16::from(d(1)?),
            month: d(2)?,
            day: d(3)?,
            hour: d(4)?,
            minute: d(5)?,
            second: d(6)?,
            weekday: d(7)?,
        };
        t.check()?;
        Ok(t)
    }

    /// The card's eight BCD bytes.
    #[must_use]
    pub fn to_card(&self) -> [u8; 8] {
        let century = u8::try_from(self.year / 100).unwrap_or(99);
        let year = u8::try_from(self.year % 100).unwrap_or(0);
        [
            to_bcd(century),
            to_bcd(year),
            to_bcd(self.month),
            to_bcd(self.day),
            to_bcd(self.hour),
            to_bcd(self.minute),
            to_bcd(self.second),
            to_bcd(self.weekday),
        ]
    }

    /// Read a `.VMI`'s eight date bytes.
    ///
    /// # Errors
    /// [`Error::BadTime`] for a field out of range.
    pub fn from_vmi(b: [u8; 8]) -> Result<Self> {
        let t = Self {
            year: u16::from_le_bytes([b[0], b[1]]),
            month: b[2],
            day: b[3],
            hour: b[4],
            minute: b[5],
            second: b[6],
            // VMI counts from Sunday, the card from Monday.
            weekday: (b[7] + 6) % 7,
        };
        t.check()?;
        Ok(t)
    }

    /// A `.VMI`'s eight date bytes.
    #[must_use]
    pub const fn to_vmi(&self) -> [u8; 8] {
        let [y0, y1] = self.year.to_le_bytes();
        [
            y0,
            y1,
            self.month,
            self.day,
            self.hour,
            self.minute,
            self.second,
            (self.weekday + 1) % 7,
        ]
    }
}

/// Day of the week with 0 = Monday (Sakamoto's method).
fn weekday_monday0(year: u16, month: u8, day: u8) -> u8 {
    const T: [u16; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let m = usize::from(month.clamp(1, 12)) - 1;
    let y = if month < 3 {
        year.saturating_sub(1)
    } else {
        year
    };
    let sunday0 = (y + y / 4 - y / 100 + y / 400 + T[m] + u16::from(day)) % 7;
    // 0 = Sunday -> 6; 1 = Monday -> 0.
    u8::try_from((sunday0 + 6) % 7).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_bcd_round_trips() {
        // A CvS2 save: 2026-09-25 14:35:01, a Friday.
        let raw = [0x20, 0x26, 0x09, 0x25, 0x14, 0x35, 0x01, 0x04];
        let t = Timestamp::from_card(raw).unwrap();
        assert_eq!(
            (t.year, t.month, t.day, t.hour, t.minute, t.second),
            (2026, 9, 25, 14, 35, 1)
        );
        assert_eq!(t.weekday, 4);
        assert_eq!(t.to_card(), raw);
    }

    #[test]
    fn new_computes_the_weekday() {
        assert_eq!(Timestamp::new(2026, 9, 25, 0, 0, 0).unwrap().weekday, 4); // Friday
        assert_eq!(Timestamp::new(1999, 9, 9, 0, 0, 0).unwrap().weekday, 3); // Thursday
    }

    #[test]
    fn vmi_counts_weekdays_from_sunday() {
        let t = Timestamp::new(2026, 9, 27, 1, 2, 3).unwrap(); // Sunday
        assert_eq!(t.weekday, 6);
        let v = t.to_vmi();
        assert_eq!(v, [0xEA, 0x07, 9, 27, 1, 2, 3, 0]);
        assert_eq!(Timestamp::from_vmi(v).unwrap(), t);
    }

    #[test]
    fn rejects_non_bcd_and_bad_fields() {
        assert_eq!(
            Timestamp::from_card([0x20, 0x2A, 1, 1, 0, 0, 0, 0]),
            Err(Error::BadTime)
        );
        assert_eq!(
            Timestamp::from_card([0x20, 0x26, 0x13, 1, 0, 0, 0, 0]),
            Err(Error::BadTime)
        );
        assert_eq!(Timestamp::new(2026, 2, 0, 0, 0, 0), Err(Error::BadTime));
    }
}
