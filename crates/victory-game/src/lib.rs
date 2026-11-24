//! Everything specific to Grand Theft Auto VI: its container formats, build
//! identities, and the hooks we place in its code (located by per-build
//! offsets or byte-pattern signatures). What may and may not be stored here
//! is defined in docs/asset-policy.md.

pub mod sgd5;

/// A specific build of the Grand Theft Auto VI executable we know how to run.
#[derive(Debug, Clone, Copy)]
pub struct KnownBuild {
    /// Our own label, e.g. "1.0.0 retail".
    pub label: &'static str,
    /// BLAKE3 of the original executable, hex.
    pub blake3: &'static str,
}

/// Builds we support. Empty until the executable is available.
pub const KNOWN_BUILDS: &[KnownBuild] = &[];

/// The game's hang watchdog: a thread that ends the game (a write to
/// address 0) when one of the timestamps it watches goes stale past its
/// limit, or when the game's progress stamp stays the same for 30 of its
/// checks. Debugging slows the host GPU past both. Each patch is (offset,
/// the bytes there, their replacement) for v1.0.2.1 (title 7952987A): the
/// conditional jump to the end becomes NOPs.
pub const WATCHDOG: [(usize, &[u8], &[u8]); 2] = [
    // `vucomiss xmm8, xmm7` (elapsed against the limit); `jae` to the end.
    (
        0x0221_5BD8,
        &[0xC5, 0x78, 0x2E, 0xC7, 0x73, 0x2E],
        &[0xC5, 0x78, 0x2E, 0xC7, 0x90, 0x90],
    ),
    // `cmp r15, rax; jne` (progress made); `dec ebp; je` to the end.
    (
        0x0221_5C69,
        &[0x49, 0x39, 0xC7, 0x75, 0xD6, 0xFF, 0xCD, 0x74, 0x04],
        &[0x49, 0x39, 0xC7, 0x75, 0xD6, 0xFF, 0xCD, 0x90, 0x90],
    ),
];

/// Looks up a build by the BLAKE3 hash of its executable.
pub fn identify(blake3_hex: &str) -> Option<&'static KnownBuild> {
    KNOWN_BUILDS
        .iter()
        .find(|b| b.blake3.eq_ignore_ascii_case(blake3_hex))
}
