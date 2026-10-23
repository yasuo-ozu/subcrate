//! Regression test for the workspace-member feature-unification bug (issue 1 of
//! `ISSUES-from-announce.md`): a subcrate links `::pm_user`, a member of this
//! workspace whose graph carries a proc-macro and so `proc-macro2` on the host
//! side, where this package's build dependency `subcrate` turns on
//! `proc-macro2/span-locations`.
//!
//! This package deliberately does **not** use the owner crate (`::member_test`), so
//! the owner lands in the nested build's package selection only through the
//! path-dependency gate in `subcrate::build`. Without that gate, Cargo would resolve
//! `pm-user`'s features for `-p pm_user` alone — `span-locations` off — and build it
//! with a different crate identity than the copy this package links, so the
//! `TypeId`s below would differ. The test is in `src/lib.rs` rather than `tests/`
//! precisely so that no `::member_test` appears anywhere in the package.

use subcrate::subcrate;

#[subcrate(::pm_user)]
pub mod uses_pm_user {
    use std::any::TypeId;

    pub fn value() -> u32 {
        pm_user::value() + 1
    }

    pub fn marker_type() -> TypeId {
        TypeId::of::<pm_user::Marker>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::any::TypeId;

    #[test]
    fn a_subcrate_can_use_a_feature_perturbed_workspace_member() {
        assert_eq!(uses_pm_user::value(), 11 + 1);
    }

    // Checked with `TypeId`s at run time (not by passing a `pm_user::Marker` across):
    // under `cargo check` and clippy the subcrate links a different copy of the
    // member than `cargo build` does, so only there would passing a value not
    // type-check, while this comparison merely would not run.
    #[test]
    fn a_feature_perturbed_workspace_member_has_one_identity_across_the_boundary() {
        assert_eq!(uses_pm_user::marker_type(), TypeId::of::<pm_user::Marker>());
    }
}
