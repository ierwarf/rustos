//! Types whose in-memory bytes may be copied to user space verbatim.
//!
//! A `Copy` value is not enough: `repr(C)` structs carry padding bytes that the
//! compiler never initialises, and copying them out leaks kernel stack
//! contents (the classic `copy_to_user` infoleak). [`UserWire`] is therefore
//! only implemented through [`user_wire!`], which lists every field of the type
//! and proves at compile time that the field sizes add up to the type's size,
//! i.e. that the type has no padding and every byte is a field byte.
//!
//! The field list is checked three ways, so it cannot silently go stale:
//! the destructuring pattern must name every field (a missing field is a
//! compile error), each binding is type-annotated (a wrong type is a compile
//! error), and each field type must itself be [`UserWire`] (nested structs are
//! checked recursively). Adding a field without extending the list, or
//! reordering fields so the compiler inserts padding, breaks the build.

/// # Safety
///
/// Every byte of a value of `Self` is initialised: `Self` has no padding and no
/// uninitialised or pointer-provenance-bearing field. Implement only through
/// [`user_wire!`], or for a primitive integer.
pub unsafe trait UserWire: Copy {}

macro_rules! primitive_user_wire {
    ($($ty:ty),* $(,)?) => { $(
        // SAFETY: primitive integers have no padding.
        unsafe impl UserWire for $ty {}
    )* };
}
primitive_user_wire!(u8, u16, u32, u64, u128, i8, i16, i32, i64, i128);

// SAFETY: an array of padding-free elements has no inter-element padding.
unsafe impl<T: UserWire, const N: usize> UserWire for [T; N] {}

/// Implements [`UserWire`] for a struct after proving it has no padding.
///
/// ```ignore
/// user_wire!(LifecycleEventWire { event: u16, reserved0: u16, exit_status: i32,
///     pid: u64, parent_pid: u64, tid: u64 });
/// ```
#[macro_export]
macro_rules! user_wire {
    ($ty:path { $($field:ident : $field_ty:ty),* $(,)? }) => {
        const _: () = {
            // Exhaustive destructuring: a field missing from the list, or one
            // with a different type, fails here.
            // LAYOUT: compile-time proof only; the function is never called.
            #[allow(dead_code, unused_variables)]
            fn fields_are_listed_exactly(value: &$ty) {
                let $ty { $($field),* } = value;
                $( let _: &$field_ty = $field; )*
            }
            const fn field_is_user_wire<T: $crate::wire::UserWire>() {}
            $( field_is_user_wire::<$field_ty>(); )*
            assert!(
                0 $(+ ::core::mem::size_of::<$field_ty>())*
                    == ::core::mem::size_of::<$ty>(),
                "user_wire!: type has padding bytes; make them explicit fields"
            );
        };
        // SAFETY: the const block above proves the listed fields tile the
        // whole value, each field is itself `UserWire`.
        unsafe impl $crate::wire::UserWire for $ty {}
    };
}
