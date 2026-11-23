//! Which processor this is, from `CPUID`.

use std::arch::x86_64::{__cpuid, CpuidResult};

/// Processor vendor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    /// `GenuineIntel`.
    Intel,
    /// `AuthenticAMD`.
    Amd,
    /// Anything else.
    Other,
}

/// What `CPUID` reports about the processor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuIdentity {
    /// Vendor.
    pub vendor: Vendor,
    /// Family, with the extended family added where the architecture says
    /// so (`0x6` Intel Core, `0x17` AMD Zen).
    pub family: u32,
    /// Model, with the extended model in the high nibble where it applies.
    pub model: u32,
    /// Stepping.
    pub stepping: u32,
    /// Marketing name, e.g. `Intel(R) Xeon(R) CPU E5-2680 v4 @ 2.40GHz`.
    pub brand: String,
}

impl CpuIdentity {
    /// Reads the identity of the processor the calling thread runs on.
    pub fn read() -> Self {
        // `CPUID` exists on every x86-64 processor; the extended brand
        // leaves are read only when the reported maximum leaf covers them.
        let leaf = |index: u32| -> CpuidResult { __cpuid(index) };
        let basic = leaf(0);
        let vendor_registers = [basic.ebx, basic.edx, basic.ecx];
        let signature = leaf(1).eax;
        let mut brand_registers = Vec::new();
        if leaf(0x8000_0000).eax >= 0x8000_0004 {
            for index in 0x8000_0002..=0x8000_0004 {
                let result = leaf(index);
                brand_registers.extend([result.eax, result.ebx, result.ecx, result.edx]);
            }
        }
        Self::from_cpuid(vendor_registers, signature, &brand_registers)
    }

    /// Decodes raw `CPUID` output: leaf 0 registers in `EBX, EDX, ECX` order,
    /// the leaf 1 `EAX` signature, and the twelve registers of leaves
    /// `0x80000002..=0x80000004` (empty when the brand string is missing).
    pub fn from_cpuid(vendor: [u32; 3], signature: u32, brand: &[u32]) -> Self {
        let vendor_bytes: Vec<u8> = vendor.iter().flat_map(|word| word.to_le_bytes()).collect();
        let vendor = match vendor_bytes.as_slice() {
            b"GenuineIntel" => Vendor::Intel,
            b"AuthenticAMD" => Vendor::Amd,
            _ => Vendor::Other,
        };
        let base_family = (signature >> 8) & 0xF;
        let extended_family = (signature >> 20) & 0xFF;
        let family = if base_family == 0xF { base_family + extended_family } else { base_family };
        let base_model = (signature >> 4) & 0xF;
        let extended_model = (signature >> 16) & 0xF;
        // The extended model counts for family 6 and family 15 and up,
        // which `base_family == 0xF` covers on AMD.
        let model = if base_family == 0x6 || base_family == 0xF {
            (extended_model << 4) | base_model
        } else {
            base_model
        };
        let brand_bytes: Vec<u8> = brand.iter().flat_map(|word| word.to_le_bytes()).collect();
        let brand = String::from_utf8_lossy(&brand_bytes)
            .trim_matches(|c: char| c == '\0' || c.is_whitespace())
            .to_owned();
        Self { vendor, family, model, stepping: signature & 0xF, brand }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registers(text: &[u8; 12]) -> [u32; 3] {
        let word =
            |at: usize| u32::from_le_bytes([text[at], text[at + 1], text[at + 2], text[at + 3]]);
        // Leaf 0 returns the vendor in EBX, EDX, ECX order.
        [word(0), word(4), word(8)]
    }

    fn brand_words(text: &str) -> Vec<u32> {
        let mut bytes = text.as_bytes().to_vec();
        bytes.resize(48, 0);
        bytes.chunks(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
    }

    #[test]
    fn broadwell_ep_decodes_to_family_6_model_0x4f() {
        let identity = CpuIdentity::from_cpuid(
            registers(b"GenuineIntel"),
            0x0004_06F1,
            &brand_words("Intel(R) Xeon(R) CPU E5-2680 v4 @ 2.40GHz"),
        );
        assert_eq!(identity.vendor, Vendor::Intel);
        assert_eq!((identity.family, identity.model, identity.stepping), (6, 0x4F, 1));
        assert_eq!(identity.brand, "Intel(R) Xeon(R) CPU E5-2680 v4 @ 2.40GHz");
    }

    #[test]
    fn zen_3_decodes_to_family_0x19_model_0x21() {
        let identity = CpuIdentity::from_cpuid(
            registers(b"AuthenticAMD"),
            0x00A2_0F10,
            &brand_words("AMD Ryzen 9 5950X"),
        );
        assert_eq!(identity.vendor, Vendor::Amd);
        assert_eq!((identity.family, identity.model, identity.stepping), (0x19, 0x21, 0));
    }

    #[test]
    fn zen_5_decodes_to_family_0x1a() {
        let identity = CpuIdentity::from_cpuid(registers(b"AuthenticAMD"), 0x00B4_0F40, &[]);
        assert_eq!((identity.family, identity.model), (0x1A, 0x44));
        assert_eq!(identity.brand, "", "a missing brand string is empty, not an error");
    }

    #[test]
    fn extended_model_is_ignored_outside_families_6_and_15() {
        // Family 5, extended model nibble set: the extended model must not apply.
        let identity = CpuIdentity::from_cpuid(registers(b"GenuineIntel"), 0x0005_0541, &[]);
        assert_eq!((identity.family, identity.model), (5, 4));
    }

    #[test]
    fn an_unknown_vendor_is_other() {
        let identity = CpuIdentity::from_cpuid(registers(b"CentaurHauls"), 0x0000_06F1, &[]);
        assert_eq!(identity.vendor, Vendor::Other);
    }

    #[test]
    fn the_brand_string_is_trimmed_of_padding() {
        let identity = CpuIdentity::from_cpuid(
            registers(b"GenuineIntel"),
            0x0004_06F1,
            &brand_words("   Intel(R) Core(TM) i7   "),
        );
        assert_eq!(identity.brand, "Intel(R) Core(TM) i7");
    }

    #[test]
    fn this_machine_reports_a_known_vendor_and_a_brand() {
        let identity = CpuIdentity::read();
        assert_ne!(identity.vendor, Vendor::Other, "{identity:?}");
        assert!(!identity.brand.is_empty(), "{identity:?}");
        assert!(identity.family >= 6, "{identity:?}");
    }
}
