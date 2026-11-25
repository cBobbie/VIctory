use super::{LdsUse, lds_conflict, lds_use};

#[test]
fn kinds() {
    assert_eq!(lds_use("ds_read_b32"), LdsUse::Read);
    assert_eq!(lds_use("ds_read2_b32"), LdsUse::Read);
    assert_eq!(lds_use("ds_write_b32"), LdsUse::Write);
    assert_eq!(lds_use("ds_add_u32"), lds_use("ds_add_rtn_u32"));
    assert_eq!(lds_use("ds_add_u32"), LdsUse::Atomic("add_u32".into()));
    assert_ne!(lds_use("ds_add_u32"), lds_use("ds_max_u32"));
    // not order-free: exchange, compare-store, wrapping increments
    assert_eq!(lds_use("ds_wrxchg_rtn_b32"), LdsUse::Write);
    assert_eq!(lds_use("ds_cmpst_b32"), LdsUse::Write);
    assert_eq!(lds_use("ds_inc_u32"), LdsUse::Write);
}

#[test]
fn conflicts() {
    let at = |d: &[u32]| Some(d.to_vec());
    // 369cbe906ea72764: read slot 0, add to slots 4 and 5 (bytes 16, 20)
    let add = || LdsUse::Atomic("add_u32".into());
    assert!(!lds_conflict(&[(LdsUse::Read, at(&[0])), (add(), at(&[4])), (add(), at(&[5]))]));
    // a read of a dword the region also adds to
    assert!(lds_conflict(&[(LdsUse::Read, at(&[4])), (add(), at(&[4]))]));
    // a read and a write of the same dword
    assert!(lds_conflict(&[(LdsUse::Read, at(&[2, 3])), (LdsUse::Write, at(&[3]))]));
    // an address not known touches every dword
    assert!(lds_conflict(&[(LdsUse::Read, None), (LdsUse::Write, at(&[9]))]));
    assert!(lds_conflict(&[(LdsUse::Read, at(&[0])), (add(), None)]));
    // atomics of one kind need no order; of two kinds on one dword they do
    assert!(!lds_conflict(&[(add(), at(&[1])), (add(), at(&[1]))]));
    assert!(lds_conflict(&[(add(), at(&[1])), (LdsUse::Atomic("max_u32".into()), at(&[1]))]));
    // reads alone never conflict
    assert!(!lds_conflict(&[(LdsUse::Read, None), (LdsUse::Read, None)]));
}
