//! Target-independent GIR passes (spec §5), run in a fixed order.

pub mod cse;
pub mod dce;
pub mod fold;
pub mod mem2reg;
pub mod uniformity;

use crate::Function;

/// The standard pipeline: mem2reg, then folding, CSE and DCE until nothing
/// changes (bounded).
pub fn optimize(f: &mut Function) {
    mem2reg::mem2reg(f);
    for _ in 0..8 {
        let a = fold::fold(f);
        let b = cse::cse(f);
        let c = dce::dce(f);
        if !(a || b || c) {
            break;
        }
    }
}
