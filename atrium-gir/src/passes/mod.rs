//! Target-independent GIR passes (spec §5), run in a fixed order.

pub mod combine;
pub mod cse;
pub mod dce;
pub mod fold;
pub mod mem2reg;
pub mod uniformity;

use crate::Function;

/// The standard pipeline: mem2reg, then folding, CSE, load combining and
/// DCE until nothing
/// changes (bounded).
pub fn optimize(f: &mut Function) {
    mem2reg::mem2reg(f);
    for _ in 0..8 {
        let a = fold::fold(f);
        let b = cse::cse(f);
        let l = combine::combine_loads(f);
        let c = dce::dce(f);
        if !(a || b || l || c) {
            break;
        }
    }
}
