//! GUID in on-disk (GPT) format: the first three fields are little-endian.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Guid(pub [u8; 16]);

impl Guid {
    pub const ZERO: Guid = Guid([0; 16]);

    pub fn from_bytes(b: &[u8]) -> Self {
        let mut a = [0u8; 16];
        a.copy_from_slice(&b[..16]);
        Guid(a)
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0; 16]
    }

    /// Parses the canonical form `XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX`.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().trim_start_matches('{').trim_end_matches('}');
        let hex: String = s.chars().filter(|c| *c != '-').collect();
        if hex.len() != 32 || s.len() != 36 {
            return None;
        }
        let mut raw = [0u8; 16];
        for i in 0..16 {
            raw[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
        }
        // Text → disk: fields 1 (4 B), 2 (2 B) and 3 (2 B) are byte-swapped.
        let mut d = raw;
        d[0..4].reverse();
        d[4..6].reverse();
        d[6..8].reverse();
        Some(Guid(d))
    }

    /// Constant from the canonical form; panics if the string is invalid
    /// (reserved for the program's constants).
    pub fn must(s: &str) -> Self {
        Self::parse(s).expect("GUID constant invalide")
    }
}

impl fmt::Display for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = &self.0;
        write!(
            f,
            "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
            d[3], d[2], d[1], d[0], d[5], d[4], d[7], d[6], d[8], d[9], d[10], d[11], d[12], d[13], d[14], d[15]
        )
    }
}

impl fmt::Debug for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl Serialize for Guid {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Guid {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Guid::parse(&s).ok_or_else(|| serde::de::Error::custom("GUID invalide"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_disk_order() {
        let g = Guid::must("C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
        // On-disk bytes of the EFI system partition (reference: UEFI specification).
        assert_eq!(g.0, [0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B]);
        assert_eq!(g.to_string(), "C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
        assert_eq!(Guid::parse("{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}"), Some(g));
        assert_eq!(Guid::parse("nope"), None);
    }
}
