//! The order of LDS accesses when a wave64 program runs as two 32-lane waves
//! (moved out of `lower.rs`, which the asset policy caps at 512 KiB).

use super::*;

/// The mode of a group barrier the translation adds (host waves meeting):
/// a wait for the group with a shared-memory fence; the fence alone with
/// `VICTORY_GROUP_SYNC=0` ([`super::super::group_sync`]).
pub(super) fn meeting_mode() -> u32 {
    if super::super::group_sync() == 0 { 8 } else { 1 | 8 }
}

/// An LDS access as [`lds_meetings`] orders it.
#[derive(Clone, PartialEq, Debug)]
pub(super) enum LdsUse {
    Read,
    Write,
    /// An atomic whose result in memory does not depend on the order the lanes run it, by
    /// operation and type. Not the `_rtn` forms: the old value each lane gets back depends on
    /// the order (a running total used as a slot number), so they count as writes.
    Atomic(String),
}

pub(super) fn lds_use(name: &str) -> LdsUse {
    if name.starts_with("ds_read") {
        return LdsUse::Read;
    }
    let rest = name.strip_prefix("ds_").unwrap_or(name);
    let op = rest.split('_').next().unwrap_or("");
    if matches!(op, "add" | "sub" | "min" | "max" | "and" | "or" | "xor")
        && !rest.contains("src2")
        && !rest.contains("_rtn")
    {
        return LdsUse::Atomic(rest.to_owned());
    }
    LdsUse::Write
}

/// Per value, the instruction that writes it (for [`lds_dwords`]).
pub(super) fn lds_value_defs(p: &crate::ir::Program) -> HashMap<Value, &Inst> {
    let mut defs = HashMap::new();
    for block in &p.blocks {
        for inst in &block.insts {
            for &(_, v) in &inst.writes {
                defs.insert(v, inst);
            }
        }
    }
    defs
}

/// The value `v` holds in every lane when a plain move of a constant wrote it.
pub(super) fn lds_constant(defs: &HashMap<Value, &Inst>, v: Value) -> Option<u32> {
    let inst = defs.get(&v)?;
    if !matches!(inst.name(), "v_mov_b32" | "s_mov_b32") {
        return None;
    }
    let src = match &inst.fields {
        Fields::Valu(x) if x.dpp.is_none() && x.sdwa.is_none() => x.src.first().cloned()?,
        Fields::Sop { src, .. } => src.first().cloned()?,
        _ => return None,
    };
    match src {
        Operand::Const(c) => crate::resource::inline(c),
        Operand::Literal(x) => Some(x),
        _ => None,
    }
}

/// The dwords of LDS an access touches, when its address is a constant; `None` if not known.
pub(super) fn lds_dwords(defs: &HashMap<Value, &Inst>, inst: &Inst) -> Option<Vec<u32>> {
    let Fields::Ds { addr, offset0, offset1, gds: false, .. } = &inst.fields else {
        return None;
    };
    let base = match addr {
        Some(Operand::Vector { reg, .. }) => lds_constant(defs, inst.read(Reg::V(*reg))?)?,
        _ => return None,
    };
    let name = inst.name();
    let width = match name.rsplit('_').next() {
        Some("b64" | "u64" | "i64" | "f64") => 2,
        Some("b96") => 3,
        Some("b128") => 4,
        _ => 1,
    };
    let at: Vec<u32> = if name.contains("read2") || name.contains("write2") || name.contains("wrxchg2") {
        let scale = 4 * width * if name.contains("st64") { 64 } else { 1 };
        let at = |o: u8| base.checked_add(u32::from(o).checked_mul(scale)?);
        vec![at(*offset0)?, at(*offset1)?]
    } else {
        vec![base.checked_add(u32::from(*offset0) | u32::from(*offset1) << 8)?]
    };
    // An address past 4 GiB is not one the analysis can place: not known.
    let mut dwords = Vec::new();
    for a in at {
        dwords.extend((a / 4)..a.checked_add(4 * width)?.div_ceil(4));
    }
    Some(dwords)
}

/// Whether two accesses of one region (a read and a write, or writes of different kinds) may
/// touch the same dword.
pub(super) fn lds_conflict(uses: &[(LdsUse, Option<Vec<u32>>)]) -> bool {
    for (i, (a, da)) in uses.iter().enumerate() {
        for (b, db) in &uses[i + 1..] {
            let ordered = match (a, b) {
                (LdsUse::Read, LdsUse::Read) => false,
                (LdsUse::Atomic(x), LdsUse::Atomic(y)) => x != y,
                _ => true,
            };
            if !ordered {
                continue;
            }
            match (da, db) {
                (Some(x), Some(y)) if !x.iter().any(|d| y.contains(d)) => {}
                _ => return true,
            }
        }
    }
    false
}

#[cfg(test)]
#[path = "lower_lds_exact_tests.rs"]
mod lds_exact_tests;

/// LDS accesses of a console wave run as two host waves, where some sit in blocks that only
/// some lanes run (`all` is [`all_lane_blocks`]): per IR block, whether the host waves meet (a
/// group barrier) at its start and before its exit. `None` if barriers cannot keep the
/// console's order.
///
/// The console runs the wave's lanes in step, so an LDS write is seen by every later read. Two
/// host waves keep that order if they meet between any two accesses of which one writes. A
/// barrier must be run by every thread, so it cannot stand in a lane branch's region; but
/// every way into and out of such a region passes a block all lanes run, and the waves meet
/// there: before the exit of an all-lane block into a region with LDS accesses, and at the
/// start of an all-lane block entered from one. Accesses in all-lane blocks get their own
/// barrier as before. What that cannot order is a write and a read inside one region (blocks
/// joined without an all-lane block between them): refused.
/// shortcut: two writes in one region are taken not to hit the same dword from both halves.
pub(super) fn lds_meetings(p: &crate::ir::Program, st: &Structured, all: &[bool]) -> Option<Vec<(bool, bool)>> {
    let n = st.exits.len();
    let succ = |b: usize| -> Vec<usize> {
        match st.exits[b] {
            LaneExit::Jump(t) => vec![t],
            LaneExit::Branch { taken, not_taken, .. } => vec![taken, not_taken],
            LaneExit::End | LaneExit::Indirect => Vec::new(),
        }
    };
    // regions: blocks not every lane runs, joined by the edges between them
    let mut region: Vec<usize> = (0..n).collect();
    fn find(region: &mut [usize], mut b: usize) -> usize {
        while region[b] != b {
            region[b] = region[region[b]];
            b = region[b];
        }
        b
    }
    for b in 0..n {
        for t in succ(b) {
            if !all[b] && !all[t] {
                let (x, y) = (find(&mut region, b), find(&mut region, t));
                region[x] = y;
            }
        }
    }
    // per region: LDS reads, LDS writes, and each access with the dwords it touches
    let mut reads = vec![false; n];
    let mut writes = vec![false; n];
    let mut uses: Vec<Vec<(LdsUse, Option<Vec<u32>>)>> = vec![Vec::new(); n];
    let defs = lds_value_defs(p);
    for (b, block) in p.blocks.iter().enumerate().take(n) {
        if all[b] {
            continue;
        }
        let r = find(&mut region, b);
        for i in &block.insts {
            let name = i.name();
            if !matches!(i.fields, Fields::Ds { .. }) || name.starts_with("ds_swizzle") || name.contains("permute") {
                continue;
            }
            if name.starts_with("ds_read") {
                reads[r] = true;
            } else {
                writes[r] = true;
            }
            uses[r].push((lds_use(name), lds_dwords(&defs, i)));
        }
    }
    // `VICTORY_LDS_EXACT=1`: a read and a write in one region are refused only where they may
    // touch the same dword (an address not known is taken to touch every dword), and atomics of
    // one order-free kind (add, min...) on the same dword do not need each other's order.
    // On by default on hosts of 32-lane waves (NVIDIA): without it such a program keeps
    // `WaveSize(64)`, which they refuse, and its pass does not run at all (4 of the
    // prologue's compute programs; 369cbe906ea72764 is made with it). `=0` turns it off.
    let exact = match std::env::var_os("VICTORY_LDS_EXACT") {
        Some(v) => v == "1",
        None => host_max_wave() == 32,
    };
    if (0..n).any(|r| reads[r] && writes[r] && (!exact || lds_conflict(&uses[r]))) {
        return None;
    }
    let mut meet = vec![(false, false); n];
    for b in 0..n {
        for t in succ(b) {
            if all[b] && !all[t] {
                let r = find(&mut region, t);
                meet[b].1 |= reads[r] || writes[r];
            } else if !all[b] && all[t] {
                let r = find(&mut region, b);
                meet[t].0 |= reads[r] || writes[r];
            }
        }
    }
    Some(meet)
}
