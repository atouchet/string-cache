// Copyright 2014 The Servo Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::dynamic_set::{Entry, DYNAMIC_SET};
use crate::static_sets::StaticAtomSet;
use debug_unreachable::debug_unreachable;

use std::borrow::Cow;
use std::cmp::Ordering::{self, Equal};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::mem;
use std::num::NonZeroU64;
use std::ops;
use std::slice;
use std::str;
use std::sync::atomic::Ordering::SeqCst;

const DYNAMIC_TAG: u8 = 0b_00;
const INLINE_TAG: u8 = 0b_01; // len in upper nybble
const STATIC_TAG: u8 = 0b_10;
const TAG_MASK: u64 = 0b_11;
const LEN_OFFSET: u64 = 4;
const LEN_MASK: u64 = 0xF0;

const MAX_INLINE_LEN: usize = 7;
const STATIC_SHIFT_BITS: usize = 32;

/// Represents a string that has been interned.
///
/// While the type definition for `Atom` indicates that it generic on a particular
/// implementation of an atom set, you don't need to worry about this.  Atoms can be static
/// and come from a `StaticAtomSet` generated by the `string_cache_codegen` crate, or they
/// can be dynamic and created by you on an `EmptyStaticAtomSet`.
///
/// `Atom` implements `Clone` but not `Copy`, since internally atoms are reference-counted;
/// this means that you may need to `.clone()` an atom to keep copies to it in different
/// places, or when passing it to a function that takes an `Atom` rather than an `&Atom`.
///
/// ## Creating an atom at runtime
///
/// If you use `string_cache_codegen` to generate a precomputed list of atoms, your code
/// may then do something like read data from somewhere and extract tokens that need to be
/// compared to the atoms.  In this case, you can use `Atom::from(&str)` or
/// `Atom::from(String)`.  These create a reference-counted atom which will be
/// automatically freed when all references to it are dropped.
///
/// This means that your application can safely have a loop which tokenizes data, creates
/// atoms from the tokens, and compares the atoms to a predefined set of keywords, without
/// running the risk of arbitrary memory consumption from creating large numbers of atoms —
/// as long as your application does not store clones of the atoms it creates along the
/// way.
///
/// For example, the following is safe and will not consume arbitrary amounts of memory:
///
/// ```ignore
/// let untrusted_data = "large amounts of text ...";
///
/// for token in untrusted_data.split_whitespace() {
///     let atom = Atom::from(token); // interns the string
///
///     if atom == Atom::from("keyword") {
///         // handle that keyword
///     } else if atom == Atom::from("another_keyword") {
///         // handle that keyword
///     } else {
///         println!("unknown keyword");
///     }
/// } // atom is dropped here, so it is not kept around in memory
/// ```
#[derive(PartialEq, Eq)]
// NOTE: Deriving PartialEq requires that a given string must always be interned the same way.
pub struct Atom<Static> {
    unsafe_data: NonZeroU64,
    phantom: PhantomData<Static>,
}

// FIXME: bound removed from the struct definition before of this error for pack_static:
// "error[E0723]: trait bounds other than `Sized` on const fn parameters are unstable"
// https://github.com/rust-lang/rust/issues/57563
impl<Static> Atom<Static> {
    /// For the atom!() macros
    #[inline(always)]
    #[doc(hidden)]
    pub const fn pack_static(n: u32) -> Self {
        Self {
            unsafe_data: unsafe {
                // STATIC_TAG ensures this is non-zero
                NonZeroU64::new_unchecked((STATIC_TAG as u64) | ((n as u64) << STATIC_SHIFT_BITS))
            },
            phantom: PhantomData,
        }
    }

    fn tag(&self) -> u8 {
        (self.unsafe_data.get() & TAG_MASK) as u8
    }
}

impl<Static: StaticAtomSet> Atom<Static> {
    /// Return the internal representation. For testing.
    #[doc(hidden)]
    pub fn unsafe_data(&self) -> u64 {
        self.unsafe_data.get()
    }

    /// Return true if this is a static Atom. For testing.
    #[doc(hidden)]
    pub fn is_static(&self) -> bool {
        self.tag() == STATIC_TAG
    }

    /// Return true if this is a dynamic Atom. For testing.
    #[doc(hidden)]
    pub fn is_dynamic(&self) -> bool {
        self.tag() == DYNAMIC_TAG
    }

    /// Return true if this is an inline Atom. For testing.
    #[doc(hidden)]
    pub fn is_inline(&self) -> bool {
        self.tag() == INLINE_TAG
    }

    fn static_index(&self) -> u64 {
        self.unsafe_data.get() >> STATIC_SHIFT_BITS
    }

    /// Get the hash of the string as it is stored in the set.
    pub fn get_hash(&self) -> u32 {
        match self.tag() {
            DYNAMIC_TAG => {
                let entry = self.unsafe_data.get() as *const Entry;
                unsafe { (*entry).hash }
            }
            STATIC_TAG => Static::get().hashes[self.static_index() as usize],
            INLINE_TAG => {
                let data = self.unsafe_data.get();
                // This may or may not be great...
                ((data >> 32) ^ data) as u32
            }
            _ => unsafe { debug_unreachable!() },
        }
    }

    pub fn try_static(string_to_add: &str) -> Option<Self> {
        Self::try_static_internal(string_to_add).ok()
    }

    fn try_static_internal(string_to_add: &str) -> Result<Self, phf_shared::Hashes> {
        let static_set = Static::get();
        let hash = phf_shared::hash(&*string_to_add, &static_set.key);
        let index = phf_shared::get_index(&hash, static_set.disps, static_set.atoms.len());

        if static_set.atoms[index as usize] == string_to_add {
            Ok(Self::pack_static(index))
        } else {
            Err(hash)
        }
    }
}

impl<Static: StaticAtomSet> Default for Atom<Static> {
    #[inline]
    fn default() -> Self {
        Atom::pack_static(Static::empty_string_index())
    }
}

impl<Static: StaticAtomSet> Hash for Atom<Static> {
    #[inline]
    fn hash<H>(&self, state: &mut H)
    where
        H: Hasher,
    {
        state.write_u32(self.get_hash())
    }
}

impl<'a, Static: StaticAtomSet> From<Cow<'a, str>> for Atom<Static> {
    fn from(string_to_add: Cow<'a, str>) -> Self {
        Self::try_static_internal(&*string_to_add).unwrap_or_else(|hash| {
            let len = string_to_add.len();
            if len <= MAX_INLINE_LEN {
                let mut data: u64 = (INLINE_TAG as u64) | ((len as u64) << LEN_OFFSET);
                {
                    let dest = inline_atom_slice_mut(&mut data);
                    dest[..len].copy_from_slice(string_to_add.as_bytes())
                }
                Atom {
                    // INLINE_TAG ensures this is never zero
                    unsafe_data: unsafe { NonZeroU64::new_unchecked(data) },
                    phantom: PhantomData,
                }
            } else {
                let ptr: std::ptr::NonNull<Entry> = DYNAMIC_SET.insert(string_to_add, hash.g);
                let data = ptr.as_ptr() as u64;
                debug_assert!(0 == data & TAG_MASK);
                Atom {
                    // The address of a ptr::NonNull is non-zero
                    unsafe_data: unsafe { NonZeroU64::new_unchecked(data) },
                    phantom: PhantomData,
                }
            }
        })
    }
}

impl<Static: StaticAtomSet> Clone for Atom<Static> {
    #[inline(always)]
    fn clone(&self) -> Self {
        if self.tag() == DYNAMIC_TAG {
            let entry = self.unsafe_data.get() as *const Entry;
            unsafe { &*entry }.ref_count.fetch_add(1, SeqCst);
        }
        Atom { ..*self }
    }
}

impl<Static> Drop for Atom<Static> {
    #[inline]
    fn drop(&mut self) {
        if self.tag() == DYNAMIC_TAG {
            let entry = self.unsafe_data.get() as *const Entry;
            if unsafe { &*entry }.ref_count.fetch_sub(1, SeqCst) == 1 {
                drop_slow(self)
            }
        }

        // Out of line to guide inlining.
        fn drop_slow<Static>(this: &mut Atom<Static>) {
            DYNAMIC_SET.remove(this.unsafe_data.get() as *mut Entry);
        }
    }
}

impl<Static: StaticAtomSet> ops::Deref for Atom<Static> {
    type Target = str;

    #[inline]
    fn deref(&self) -> &str {
        unsafe {
            match self.tag() {
                DYNAMIC_TAG => {
                    let entry = self.unsafe_data.get() as *const Entry;
                    &(*entry).string
                }
                INLINE_TAG => {
                    let len = (self.unsafe_data() & LEN_MASK) >> LEN_OFFSET;
                    debug_assert!(len as usize <= MAX_INLINE_LEN);
                    let src = inline_atom_slice(&self.unsafe_data);
                    str::from_utf8_unchecked(src.get_unchecked(..(len as usize)))
                }
                STATIC_TAG => Static::get().atoms[self.static_index() as usize],
                _ => debug_unreachable!(),
            }
        }
    }
}

impl<Static: StaticAtomSet> fmt::Debug for Atom<Static> {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let ty_str = unsafe {
            match self.tag() {
                DYNAMIC_TAG => "dynamic",
                INLINE_TAG => "inline",
                STATIC_TAG => "static",
                _ => debug_unreachable!(),
            }
        };

        write!(f, "Atom('{}' type={})", &*self, ty_str)
    }
}

impl<Static: StaticAtomSet> PartialOrd for Atom<Static> {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        if self.unsafe_data == other.unsafe_data {
            return Some(Equal);
        }
        self.as_ref().partial_cmp(other.as_ref())
    }
}

impl<Static: StaticAtomSet> Ord for Atom<Static> {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        if self.unsafe_data == other.unsafe_data {
            return Equal;
        }
        self.as_ref().cmp(other.as_ref())
    }
}

// AsciiExt requires mutating methods, so we just implement the non-mutating ones.
// We don't need to implement is_ascii because there's no performance improvement
// over the one from &str.
impl<Static: StaticAtomSet> Atom<Static> {
    fn from_mutated_str<F: FnOnce(&mut str)>(s: &str, f: F) -> Self {
        let mut buffer = mem::MaybeUninit::<[u8; 64]>::uninit();
        let buffer = unsafe { &mut *buffer.as_mut_ptr() };

        if let Some(buffer_prefix) = buffer.get_mut(..s.len()) {
            buffer_prefix.copy_from_slice(s.as_bytes());
            let as_str = unsafe { ::std::str::from_utf8_unchecked_mut(buffer_prefix) };
            f(as_str);
            Atom::from(&*as_str)
        } else {
            let mut string = s.to_owned();
            f(&mut string);
            Atom::from(string)
        }
    }

    /// Like [`to_ascii_uppercase`].
    ///
    /// [`to_ascii_uppercase`]: https://doc.rust-lang.org/std/ascii/trait.AsciiExt.html#tymethod.to_ascii_uppercase
    pub fn to_ascii_uppercase(&self) -> Self {
        for (i, b) in self.bytes().enumerate() {
            if let b'a'..=b'z' = b {
                return Atom::from_mutated_str(self, |s| s[i..].make_ascii_uppercase());
            }
        }
        self.clone()
    }

    /// Like [`to_ascii_lowercase`].
    ///
    /// [`to_ascii_lowercase`]: https://doc.rust-lang.org/std/ascii/trait.AsciiExt.html#tymethod.to_ascii_lowercase
    pub fn to_ascii_lowercase(&self) -> Self {
        for (i, b) in self.bytes().enumerate() {
            if let b'A'..=b'Z' = b {
                return Atom::from_mutated_str(self, |s| s[i..].make_ascii_lowercase());
            }
        }
        self.clone()
    }

    /// Like [`eq_ignore_ascii_case`].
    ///
    /// [`eq_ignore_ascii_case`]: https://doc.rust-lang.org/std/ascii/trait.AsciiExt.html#tymethod.eq_ignore_ascii_case
    pub fn eq_ignore_ascii_case(&self, other: &Self) -> bool {
        (self == other) || self.eq_str_ignore_ascii_case(&**other)
    }

    /// Like [`eq_ignore_ascii_case`], but takes an unhashed string as `other`.
    ///
    /// [`eq_ignore_ascii_case`]: https://doc.rust-lang.org/std/ascii/trait.AsciiExt.html#tymethod.eq_ignore_ascii_case
    pub fn eq_str_ignore_ascii_case(&self, other: &str) -> bool {
        (&**self).eq_ignore_ascii_case(other)
    }
}

#[inline(always)]
fn inline_atom_slice(x: &NonZeroU64) -> &[u8] {
    unsafe {
        let x: *const NonZeroU64 = x;
        let mut data = x as *const u8;
        // All except the lowest byte, which is first in little-endian, last in big-endian.
        if cfg!(target_endian = "little") {
            data = data.offset(1);
        }
        let len = 7;
        slice::from_raw_parts(data, len)
    }
}

#[inline(always)]
fn inline_atom_slice_mut(x: &mut u64) -> &mut [u8] {
    unsafe {
        let x: *mut u64 = x;
        let mut data = x as *mut u8;
        // All except the lowest byte, which is first in little-endian, last in big-endian.
        if cfg!(target_endian = "little") {
            data = data.offset(1);
        }
        let len = 7;
        slice::from_raw_parts_mut(data, len)
    }
}
