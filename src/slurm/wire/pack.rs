//! Slurm's byte format: big-endian integers and length-prefixed strings.

use anyhow::{Context, Result, bail};

/// Slurm packs a double as the bits of the value times this.
const FLOAT_MULT: f64 = 1_000_000.0;

/// Slurm's "no value" sentinels.
pub const NO_VAL: u32 = 0xffff_fffe;
/// The 64-bit "no value" sentinel.
pub const NO_VAL64: u64 = 0xffff_ffff_ffff_fffe;
/// The 16-bit "no value" sentinel.
pub const NO_VAL16: u16 = 0xfffe;
/// The 32-bit "infinite" sentinel.
pub const INFINITE: u32 = 0xffff_ffff;
/// The 64-bit "infinite" sentinel.
pub const INFINITE64: u64 = 0xffff_ffff_ffff_ffff;
/// The 16-bit "infinite" sentinel.
pub const INFINITE16: u16 = 0xffff;

/// Reads values in the order the sender packed them.
pub struct Unpack<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Unpack<'a> {
    /// Starts at the first byte.
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    /// Bytes not yet read.
    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.at
    }

    /// The next `len` raw bytes.
    ///
    /// # Errors
    ///
    /// Fails when fewer remain.
    pub fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(len)
            .filter(|&end| end <= self.bytes.len())
            .with_context(|| {
                format!(
                    "{len} bytes wanted at offset {} of {}",
                    self.at,
                    self.bytes.len()
                )
            })?;
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().expect("take returns N bytes"))
    }

    /// An 8-bit integer.
    ///
    /// # Errors
    ///
    /// Fails when the buffer runs out.
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    /// A 16-bit integer.
    ///
    /// # Errors
    ///
    /// Fails when the buffer runs out.
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    /// A 32-bit integer.
    ///
    /// # Errors
    ///
    /// Fails when the buffer runs out.
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    /// A 64-bit integer.
    ///
    /// # Errors
    ///
    /// Fails when the buffer runs out.
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    /// A boolean, packed as one byte.
    ///
    /// # Errors
    ///
    /// Fails when the buffer runs out.
    pub fn bool(&mut self) -> Result<bool> {
        Ok(self.u8()? != 0)
    }

    /// A `time_t`, packed as a signed 64-bit integer.
    ///
    /// # Errors
    ///
    /// Fails when the buffer runs out.
    pub fn time(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(self.array()?))
    }

    /// A double.
    ///
    /// # Errors
    ///
    /// Fails when the buffer runs out.
    pub fn double(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.u64()?) / FLOAT_MULT)
    }

    /// A length-prefixed run of bytes. An empty run is `None`.
    ///
    /// # Errors
    ///
    /// Fails when the buffer runs out.
    pub fn mem(&mut self) -> Result<Option<&'a [u8]>> {
        let len = self.u32()? as usize;
        if len == 0 {
            return Ok(None);
        }
        self.take(len).map(Some)
    }

    /// A string, whose length counts its NUL. A null string is `None`.
    ///
    /// # Errors
    ///
    /// Fails when the buffer runs out or the string lacks its NUL.
    pub fn str(&mut self) -> Result<Option<String>> {
        let Some(bytes) = self.mem()? else {
            return Ok(None);
        };
        let Some((0, text)) = bytes.split_last() else {
            bail!("string at offset {} lacks its NUL", self.at - bytes.len());
        };
        Ok(Some(String::from_utf8_lossy(text).into_owned()))
    }

    /// A string, skipped.
    ///
    /// # Errors
    ///
    /// Fails when the buffer runs out.
    pub fn skip_str(&mut self) -> Result<()> {
        self.mem().map(drop)
    }

    /// Skips `count` strings.
    ///
    /// # Errors
    ///
    /// Fails when the buffer runs out.
    pub fn skip_strs(&mut self, count: usize) -> Result<()> {
        (0..count).try_for_each(|_| self.skip_str())
    }

    /// A counted list, read item by item. A count of `NO_VAL` is an empty list.
    ///
    /// # Errors
    ///
    /// Fails when the buffer runs out or an item fails.
    pub fn list<T>(&mut self, mut item: impl FnMut(&mut Self) -> Result<T>) -> Result<Vec<T>> {
        let count = self.u32()?;
        if count == NO_VAL {
            return Ok(Vec::new());
        }
        (0..count).map(|_| item(self)).collect()
    }

    /// A counted array of strings.
    ///
    /// # Errors
    ///
    /// Fails when the buffer runs out.
    pub fn str_array(&mut self) -> Result<Vec<Option<String>>> {
        let count = self.u32()?;
        (0..count).map(|_| self.str()).collect()
    }

    /// A counted array of 16-bit integers.
    ///
    /// # Errors
    ///
    /// Fails when the buffer runs out.
    pub fn u16_array(&mut self) -> Result<Vec<u16>> {
        let count = self.u32()?;
        (0..count).map(|_| self.u16()).collect()
    }

    /// A counted array of 32-bit integers.
    ///
    /// # Errors
    ///
    /// Fails when the buffer runs out.
    pub fn u32_array(&mut self) -> Result<Vec<u32>> {
        let count = self.u32()?;
        (0..count).map(|_| self.u32()).collect()
    }

    /// A counted array of 64-bit integers.
    ///
    /// # Errors
    ///
    /// Fails when the buffer runs out.
    pub fn u64_array(&mut self) -> Result<Vec<u64>> {
        let count = self.u32()?;
        (0..count).map(|_| self.u64()).collect()
    }

    /// A bitmap packed as its size and a hex mask such as `0x0F00`. Bit 0 is the mask's last digit.
    /// A null bitmap is `None`.
    ///
    /// # Errors
    ///
    /// Fails when the buffer runs out or the mask is not hex.
    pub fn bitmap(&mut self) -> Result<Option<Bitmap>> {
        let size = self.u32()?;
        if size == NO_VAL {
            return Ok(None);
        }
        let mask = self.str()?.unwrap_or_default();
        let digits = mask.strip_prefix("0x").unwrap_or(&mask);
        let mut set = Vec::new();
        for (position, digit) in digits.bytes().rev().enumerate() {
            let nibble = char::from(digit)
                .to_digit(16)
                .with_context(|| format!("bad hex mask {mask:?}"))?;
            for bit in 0..4 {
                let index = u32::try_from(position * 4 + bit)?;
                if nibble & (1 << bit) != 0 && index < size {
                    set.push(index);
                }
            }
        }
        Ok(Some(Bitmap { size, set }))
    }
}

/// A bitmap's size and the indices of its set bits, in ascending order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bitmap {
    /// How many bits it holds.
    pub size: u32,
    /// The set bits.
    pub set: Vec<u32>,
}

/// Writes values in Slurm's byte format.
#[derive(Default)]
pub struct Pack(pub Vec<u8>);

impl Pack {
    /// Appends a 16-bit integer.
    pub fn u16(&mut self, value: u16) -> &mut Self {
        self.0.extend_from_slice(&value.to_be_bytes());
        self
    }

    /// Appends a 32-bit integer.
    pub fn u32(&mut self, value: u32) -> &mut Self {
        self.0.extend_from_slice(&value.to_be_bytes());
        self
    }

    /// Appends a 64-bit integer.
    pub fn u64(&mut self, value: u64) -> &mut Self {
        self.0.extend_from_slice(&value.to_be_bytes());
        self
    }

    /// Appends a `time_t`.
    pub fn time(&mut self, value: i64) -> &mut Self {
        self.0.extend_from_slice(&value.to_be_bytes());
        self
    }

    /// Appends a length-prefixed run of bytes.
    ///
    /// # Panics
    ///
    /// Panics when `bytes` is 4 GiB or longer.
    pub fn mem(&mut self, bytes: &[u8]) -> &mut Self {
        self.u32(u32::try_from(bytes.len()).expect("under 4 GiB"));
        self.0.extend_from_slice(bytes);
        self
    }

    /// Appends a string with its NUL, or a null string for `None`.
    ///
    /// # Panics
    ///
    /// Panics when `text` is 4 GiB or longer.
    pub fn str(&mut self, text: Option<&str>) -> &mut Self {
        match text {
            Some(text) => {
                self.u32(u32::try_from(text.len() + 1).expect("under 4 GiB"));
                self.0.extend_from_slice(text.as_bytes());
                self.0.push(0);
            }
            None => {
                self.u32(0);
            }
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_integers_times_and_strings() {
        let mut pack = Pack::default();
        pack.u16(7)
            .u32(NO_VAL)
            .u64(1 << 40)
            .time(-1)
            .str(Some("z1014"))
            .str(None);
        let mut unpack = Unpack::new(&pack.0);
        assert_eq!(unpack.u16().unwrap(), 7);
        assert_eq!(unpack.u32().unwrap(), NO_VAL);
        assert_eq!(unpack.u64().unwrap(), 1 << 40);
        assert_eq!(unpack.time().unwrap(), -1);
        assert_eq!(unpack.str().unwrap().as_deref(), Some("z1014"));
        assert_eq!(unpack.str().unwrap(), None);
        assert_eq!(unpack.remaining(), 0);
        assert!(unpack.u8().is_err());
    }

    #[test]
    fn doubles_are_scaled_by_a_million() {
        let mut pack = Pack::default();
        pack.u64((24.71 * FLOAT_MULT).to_bits());
        assert!((Unpack::new(&pack.0).double().unwrap() - 24.71).abs() < 1e-9);
    }

    #[test]
    fn hex_bitmaps_count_from_the_last_digit() {
        let mut pack = Pack::default();
        pack.u32(10).str(Some("0x305"));
        assert_eq!(
            Unpack::new(&pack.0).bitmap().unwrap(),
            Some(Bitmap {
                size: 10,
                set: vec![0, 2, 8, 9]
            })
        );
        let mut null = Pack::default();
        null.u32(NO_VAL);
        assert_eq!(Unpack::new(&null.0).bitmap().unwrap(), None);
    }

    #[test]
    fn a_string_without_its_nul_fails() {
        let mut pack = Pack::default();
        pack.mem(b"abc");
        assert!(Unpack::new(&pack.0).str().is_err());
    }
}
