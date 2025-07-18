// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::borrow::Borrow;
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::convert::{TryFrom, TryInto};
use std::fmt::{self, Debug};
use std::mem::{size_of, transmute};
use std::ops::Deref;
use std::str;

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc};
use compact_bytes::CompactBytes;
use mz_ore::cast::{CastFrom, ReinterpretCast};
use mz_ore::soft_assert_no_log;
use mz_ore::vec::Vector;
use mz_persist_types::Codec64;
use num_enum::{IntoPrimitive, TryFromPrimitive};
use ordered_float::OrderedFloat;
use proptest::prelude::*;
use proptest::strategy::{BoxedStrategy, Strategy};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::adt::array::{
    Array, ArrayDimension, ArrayDimensions, InvalidArrayError, MAX_ARRAY_DIMENSIONS,
};
use crate::adt::date::Date;
use crate::adt::interval::Interval;
use crate::adt::mz_acl_item::{AclItem, MzAclItem};
use crate::adt::numeric;
use crate::adt::numeric::Numeric;
use crate::adt::range::{
    self, InvalidRangeError, Range, RangeBound, RangeInner, RangeLowerBound, RangeUpperBound,
};
use crate::adt::timestamp::CheckedTimestamp;
use crate::scalar::{DatumKind, arb_datum};
use crate::{Datum, RelationDesc, Timestamp};

pub(crate) mod encode;
pub mod iter;

include!(concat!(env!("OUT_DIR"), "/mz_repr.row.rs"));

/// A packed representation for `Datum`s.
///
/// `Datum` is easy to work with but very space inefficient. A `Datum::Int32(42)`
/// is laid out in memory like this:
///
///   tag: 3
///   padding: 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0
///   data: 0 0 0 42
///   padding: 0 0 0 0 0 0 0 0 0 0 0 0
///
/// For a total of 32 bytes! The second set of padding is needed in case we were
/// to write a 16-byte datum into this location. The first set of padding is
/// needed to align that hypothetical decimal to a 16 bytes boundary.
///
/// A `Row` stores zero or more `Datum`s without any padding. We avoid the need
/// for the first set of padding by only providing access to the `Datum`s via
/// calls to `ptr::read_unaligned`, which on modern x86 is barely penalized. We
/// avoid the need for the second set of padding by not providing mutable access
/// to the `Datum`. Instead, `Row` is append-only.
///
/// A `Row` can be built from a collection of `Datum`s using `Row::pack`, but it
/// is more efficient to use `Row::pack_slice` so that a right-sized allocation
/// can be created. If that is not possible, consider using the row buffer
/// pattern: allocate one row, pack into it, and then call [`Row::clone`] to
/// receive a copy of that row, leaving behind the original allocation to pack
/// future rows.
///
/// Creating a row via [`Row::pack_slice`]:
///
/// ```
/// # use mz_repr::{Row, Datum};
/// let row = Row::pack_slice(&[Datum::Int32(0), Datum::Int32(1), Datum::Int32(2)]);
/// assert_eq!(row.unpack(), vec![Datum::Int32(0), Datum::Int32(1), Datum::Int32(2)])
/// ```
///
/// `Row`s can be unpacked by iterating over them:
///
/// ```
/// # use mz_repr::{Row, Datum};
/// let row = Row::pack_slice(&[Datum::Int32(0), Datum::Int32(1), Datum::Int32(2)]);
/// assert_eq!(row.iter().nth(1).unwrap(), Datum::Int32(1));
/// ```
///
/// If you want random access to the `Datum`s in a `Row`, use `Row::unpack` to create a `Vec<Datum>`
/// ```
/// # use mz_repr::{Row, Datum};
/// let row = Row::pack_slice(&[Datum::Int32(0), Datum::Int32(1), Datum::Int32(2)]);
/// let datums = row.unpack();
/// assert_eq!(datums[1], Datum::Int32(1));
/// ```
///
/// # Performance
///
/// Rows are dynamically sized, but up to a fixed size their data is stored in-line.
/// It is best to re-use a `Row` across multiple `Row` creation calls, as this
/// avoids the allocations involved in `Row::new()`.
#[derive(Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Row {
    data: CompactBytes,
}

impl Row {
    const SIZE: usize = CompactBytes::MAX_INLINE;

    /// A variant of `Row::from_proto` that allows for reuse of internal allocs
    /// and validates the decoding against a provided [`RelationDesc`].
    pub fn decode_from_proto(
        &mut self,
        proto: &ProtoRow,
        desc: &RelationDesc,
    ) -> Result<(), String> {
        let mut packer = self.packer();
        for (col_idx, _, _) in desc.iter_all() {
            let d = match proto.datums.get(col_idx.to_raw()) {
                Some(x) => x,
                None => {
                    packer.push(Datum::Null);
                    continue;
                }
            };
            packer.try_push_proto(d)?;
        }

        Ok(())
    }

    /// Allocate an empty `Row` with a pre-allocated capacity.
    #[inline]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            data: CompactBytes::with_capacity(cap),
        }
    }

    /// Create an empty `Row`.
    #[inline]
    pub const fn empty() -> Self {
        Self {
            data: CompactBytes::empty(),
        }
    }

    /// Creates a new row from supplied bytes.
    ///
    /// # Safety
    ///
    /// This method relies on `data` being an appropriate row encoding, and can
    /// result in unsafety if this is not the case.
    pub unsafe fn from_bytes_unchecked(data: &[u8]) -> Self {
        Row {
            data: CompactBytes::new(data),
        }
    }

    /// Constructs a [`RowPacker`] that will pack datums into this row's
    /// allocation.
    ///
    /// This method clears the existing contents of the row, but retains the
    /// allocation.
    pub fn packer(&mut self) -> RowPacker<'_> {
        self.clear();
        RowPacker { row: self }
    }

    /// Take some `Datum`s and pack them into a `Row`.
    ///
    /// This method builds a `Row` by repeatedly increasing the backing
    /// allocation. If the contents of the iterator are known ahead of
    /// time, consider [`Row::with_capacity`] to right-size the allocation
    /// first, and then [`RowPacker::extend`] to populate it with `Datum`s.
    /// This avoids the repeated allocation resizing and copying.
    pub fn pack<'a, I, D>(iter: I) -> Row
    where
        I: IntoIterator<Item = D>,
        D: Borrow<Datum<'a>>,
    {
        let mut row = Row::default();
        row.packer().extend(iter);
        row
    }

    /// Use `self` to pack `iter`, and then clone the result.
    ///
    /// This is a convenience method meant to reduce boilerplate around row
    /// formation.
    pub fn pack_using<'a, I, D>(&mut self, iter: I) -> Row
    where
        I: IntoIterator<Item = D>,
        D: Borrow<Datum<'a>>,
    {
        self.packer().extend(iter);
        self.clone()
    }

    /// Like [`Row::pack`], but the provided iterator is allowed to produce an
    /// error, in which case the packing operation is aborted and the error
    /// returned.
    pub fn try_pack<'a, I, D, E>(iter: I) -> Result<Row, E>
    where
        I: IntoIterator<Item = Result<D, E>>,
        D: Borrow<Datum<'a>>,
    {
        let mut row = Row::default();
        row.packer().try_extend(iter)?;
        Ok(row)
    }

    /// Pack a slice of `Datum`s into a `Row`.
    ///
    /// This method has the advantage over `pack` that it can determine the required
    /// allocation before packing the elements, ensuring only one allocation and no
    /// redundant copies required.
    pub fn pack_slice<'a>(slice: &[Datum<'a>]) -> Row {
        // Pre-allocate the needed number of bytes.
        let mut row = Row::with_capacity(datums_size(slice.iter()));
        row.packer().extend(slice.iter());
        row
    }

    /// Returns the total amount of bytes used by this row.
    pub fn byte_len(&self) -> usize {
        let heap_size = if self.data.spilled() {
            self.data.len()
        } else {
            0
        };
        let inline_size = std::mem::size_of::<Self>();
        inline_size.saturating_add(heap_size)
    }

    /// The length of the encoded row in bytes. Does not include the size of the `Row` struct itself.
    pub fn data_len(&self) -> usize {
        self.data.len()
    }

    /// Returns the total capacity in bytes used by this row.
    pub fn byte_capacity(&self) -> usize {
        self.data.capacity()
    }

    /// Extracts a Row slice containing the entire [`Row`].
    #[inline]
    pub fn as_row_ref(&self) -> &RowRef {
        RowRef::from_slice(self.data.as_slice())
    }

    /// Clear the contents of the [`Row`], leaving any allocation in place.
    #[inline]
    fn clear(&mut self) {
        self.data.clear();
    }
}

impl Borrow<RowRef> for Row {
    #[inline]
    fn borrow(&self) -> &RowRef {
        self.as_row_ref()
    }
}

impl AsRef<RowRef> for Row {
    #[inline]
    fn as_ref(&self) -> &RowRef {
        self.as_row_ref()
    }
}

impl Deref for Row {
    type Target = RowRef;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.as_row_ref()
    }
}

// Nothing depends on Row being exactly 24, we just want to add visibility to the size.
static_assertions::const_assert_eq!(std::mem::size_of::<Row>(), 24);

impl Clone for Row {
    fn clone(&self) -> Self {
        Row {
            data: self.data.clone(),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.data.clone_from(&source.data);
    }
}

// Row's `Hash` implementation defers to `RowRef` to ensure they hash equivalently.
impl std::hash::Hash for Row {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_row_ref().hash(state)
    }
}

impl Arbitrary for Row {
    type Parameters = prop::collection::SizeRange;
    type Strategy = BoxedStrategy<Row>;

    fn arbitrary_with(size: Self::Parameters) -> Self::Strategy {
        prop::collection::vec(arb_datum(), size)
            .prop_map(|items| {
                let mut row = Row::default();
                let mut packer = row.packer();
                for item in items.iter() {
                    let datum: Datum<'_> = item.into();
                    packer.push(datum);
                }
                row
            })
            .boxed()
    }
}

impl PartialOrd for Row {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Row {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_ref().cmp(other.as_ref())
    }
}

#[allow(missing_debug_implementations)]
mod columnation {
    use columnation::{Columnation, Region};
    use mz_ore::region::LgAllocRegion;

    use crate::Row;

    /// Region allocation for `Row` data.
    ///
    /// Content bytes are stored in stable contiguous memory locations,
    /// and then a `Row` referencing them is falsified.
    pub struct RowStack {
        region: LgAllocRegion<u8>,
    }

    impl RowStack {
        const LIMIT: usize = 2 << 20;
    }

    // Implement `Default` manually to specify a region allocation limit.
    impl Default for RowStack {
        fn default() -> Self {
            Self {
                // Limit the region size to 2MiB.
                region: LgAllocRegion::with_limit(Self::LIMIT),
            }
        }
    }

    impl Columnation for Row {
        type InnerRegion = RowStack;
    }

    impl Region for RowStack {
        type Item = Row;
        #[inline]
        fn clear(&mut self) {
            self.region.clear();
        }
        #[inline(always)]
        unsafe fn copy(&mut self, item: &Row) -> Row {
            if item.data.spilled() {
                let bytes = self.region.copy_slice(&item.data[..]);
                Row {
                    data: compact_bytes::CompactBytes::from_raw_parts(
                        bytes.as_mut_ptr(),
                        item.data.len(),
                        item.data.capacity(),
                    ),
                }
            } else {
                item.clone()
            }
        }

        fn reserve_items<'a, I>(&mut self, items: I)
        where
            Self: 'a,
            I: Iterator<Item = &'a Self::Item> + Clone,
        {
            let size = items
                .filter(|row| row.data.spilled())
                .map(|row| row.data.len())
                .sum();
            let size = std::cmp::min(size, Self::LIMIT);
            self.region.reserve(size);
        }

        fn reserve_regions<'a, I>(&mut self, regions: I)
        where
            Self: 'a,
            I: Iterator<Item = &'a Self> + Clone,
        {
            let size = regions.map(|r| r.region.len()).sum();
            let size = std::cmp::min(size, Self::LIMIT);
            self.region.reserve(size);
        }

        fn heap_size(&self, callback: impl FnMut(usize, usize)) {
            self.region.heap_size(callback)
        }
    }
}

mod columnar {
    use columnar::{
        AsBytes, Clear, Columnar, Container, FromBytes, HeapSize, Index, IndexAs, Len, Push,
    };
    use mz_ore::cast::CastFrom;

    use crate::{Row, RowRef};

    #[derive(Copy, Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
    pub struct Rows<BC = Vec<u64>, VC = Vec<u8>> {
        /// Bounds container; provides indexed access to offsets.
        pub bounds: BC,
        /// Values container; provides slice access to bytes.
        pub values: VC,
    }

    impl Columnar for Row {
        type Ref<'a> = &'a RowRef;
        #[inline(always)]
        fn copy_from(&mut self, other: Self::Ref<'_>) {
            self.clear();
            self.data.extend_from_slice(other.data());
        }
        #[inline(always)]
        fn into_owned(other: Self::Ref<'_>) -> Self {
            other.to_owned()
        }
        type Container = Rows;
        #[inline(always)]
        fn reborrow<'b, 'a: 'b>(thing: Self::Ref<'a>) -> Self::Ref<'b>
        where
            Self: 'a,
        {
            thing
        }
    }

    impl<'b, BC: Container<u64>> Container<Row> for Rows<BC, &'b [u8]> {
        type Borrowed<'a>
            = Rows<BC::Borrowed<'a>, &'a [u8]>
        where
            Self: 'a;
        #[inline(always)]
        fn borrow<'a>(&'a self) -> Self::Borrowed<'a> {
            Rows {
                bounds: self.bounds.borrow(),
                values: self.values,
            }
        }
        #[inline(always)]
        fn reborrow<'c, 'a: 'c>(item: Self::Borrowed<'a>) -> Self::Borrowed<'c>
        where
            Self: 'a,
        {
            Rows {
                bounds: BC::reborrow(item.bounds),
                values: item.values,
            }
        }
    }
    impl<BC: Container<u64>> Container<Row> for Rows<BC, Vec<u8>> {
        type Borrowed<'a>
            = Rows<BC::Borrowed<'a>, &'a [u8]>
        where
            BC: 'a;
        #[inline(always)]
        fn borrow<'a>(&'a self) -> Self::Borrowed<'a> {
            Rows {
                bounds: self.bounds.borrow(),
                values: self.values.borrow(),
            }
        }
        #[inline(always)]
        fn reborrow<'c, 'a: 'c>(item: Self::Borrowed<'a>) -> Self::Borrowed<'c>
        where
            Self: 'a,
        {
            Rows {
                bounds: BC::reborrow(item.bounds),
                values: item.values,
            }
        }
    }

    impl<'a, BC: AsBytes<'a>, VC: AsBytes<'a>> AsBytes<'a> for Rows<BC, VC> {
        #[inline(always)]
        fn as_bytes(&self) -> impl Iterator<Item = (u64, &'a [u8])> {
            columnar::chain(self.bounds.as_bytes(), self.values.as_bytes())
        }
    }
    impl<'a, BC: FromBytes<'a>, VC: FromBytes<'a>> FromBytes<'a> for Rows<BC, VC> {
        #[inline(always)]
        fn from_bytes(bytes: &mut impl Iterator<Item = &'a [u8]>) -> Self {
            Self {
                bounds: FromBytes::from_bytes(bytes),
                values: FromBytes::from_bytes(bytes),
            }
        }
    }

    impl<BC: Len, VC> Len for Rows<BC, VC> {
        #[inline(always)]
        fn len(&self) -> usize {
            self.bounds.len()
        }
    }

    impl<'a, BC: Len + IndexAs<u64>> Index for Rows<BC, &'a [u8]> {
        type Ref = &'a RowRef;
        #[inline(always)]
        fn get(&self, index: usize) -> Self::Ref {
            let lower = if index == 0 {
                0
            } else {
                self.bounds.index_as(index - 1)
            };
            let upper = self.bounds.index_as(index);
            let lower = usize::cast_from(lower);
            let upper = usize::cast_from(upper);
            RowRef::from_slice(&self.values[lower..upper])
        }
    }
    impl<'a, BC: Len + IndexAs<u64>> Index for &'a Rows<BC, Vec<u8>> {
        type Ref = &'a RowRef;
        #[inline(always)]
        fn get(&self, index: usize) -> Self::Ref {
            let lower = if index == 0 {
                0
            } else {
                self.bounds.index_as(index - 1)
            };
            let upper = self.bounds.index_as(index);
            let lower = usize::cast_from(lower);
            let upper = usize::cast_from(upper);
            RowRef::from_slice(&self.values[lower..upper])
        }
    }

    impl<BC: Push<u64>> Push<&Row> for Rows<BC> {
        #[inline(always)]
        fn push(&mut self, item: &Row) {
            self.values.extend_from_slice(item.data.as_slice());
            self.bounds.push(u64::cast_from(self.values.len()));
        }
    }
    impl<BC: Push<u64>> Push<&RowRef> for Rows<BC> {
        #[inline(always)]
        fn push(&mut self, item: &RowRef) {
            self.values.extend_from_slice(item.data());
            self.bounds.push(u64::cast_from(self.values.len()));
        }
    }
    impl<BC: Clear, VC: Clear> Clear for Rows<BC, VC> {
        #[inline(always)]
        fn clear(&mut self) {
            self.bounds.clear();
            self.values.clear();
        }
    }
    impl<BC: HeapSize, VC: HeapSize> HeapSize for Rows<BC, VC> {
        #[inline(always)]
        fn heap_size(&self) -> (usize, usize) {
            let (l0, c0) = self.bounds.heap_size();
            let (l1, c1) = self.values.heap_size();
            (l0 + l1, c0 + c1)
        }
    }
}

/// A contiguous slice of bytes that are row data.
///
/// A [`RowRef`] is to [`Row`] as [`prim@str`] is to [`String`].
#[derive(PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct RowRef([u8]);

impl RowRef {
    /// Create a [`RowRef`] from a slice of data.
    ///
    /// We do not check that the provided slice is valid [`Row`] data, will panic on read
    /// if the data is invalid.
    pub fn from_slice(row: &[u8]) -> &RowRef {
        #[allow(clippy::as_conversions)]
        let ptr = row as *const [u8] as *const RowRef;
        // SAFETY: We know `ptr` is non-null and aligned because it came from a &[u8].
        unsafe { &*ptr }
    }

    /// Unpack `self` into a `Vec<Datum>` for efficient random access.
    pub fn unpack(&self) -> Vec<Datum> {
        // It's usually cheaper to unpack twice to figure out the right length than it is to grow the vec as we go
        let len = self.iter().count();
        let mut vec = Vec::with_capacity(len);
        vec.extend(self.iter());
        vec
    }

    /// Return the first [`Datum`] in `self`
    ///
    /// Panics if the [`RowRef`] is empty.
    pub fn unpack_first(&self) -> Datum {
        self.iter().next().unwrap()
    }

    /// Iterate the [`Datum`] elements of the [`RowRef`].
    pub fn iter(&self) -> DatumListIter {
        DatumListIter { data: &self.0 }
    }

    /// Return the byte length of this [`RowRef`].
    pub fn byte_len(&self) -> usize {
        self.0.len()
    }

    /// For debugging only.
    pub fn data(&self) -> &[u8] {
        &self.0
    }

    /// True iff there is no data in this [`RowRef`].
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl ToOwned for RowRef {
    type Owned = Row;

    fn to_owned(&self) -> Self::Owned {
        // SAFETY: RowRef has the invariant that the wrapped data must be a valid Row encoding.
        unsafe { Row::from_bytes_unchecked(&self.0) }
    }
}

impl<'a> IntoIterator for &'a RowRef {
    type Item = Datum<'a>;
    type IntoIter = DatumListIter<'a>;

    fn into_iter(self) -> DatumListIter<'a> {
        DatumListIter { data: &self.0 }
    }
}

/// These implementations order first by length, and then by slice contents.
/// This allows many comparisons to complete without dereferencing memory.
/// Warning: These order by the u8 array representation, and NOT by Datum::cmp.
impl PartialOrd for RowRef {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RowRef {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match self.0.len().cmp(&other.0.len()) {
            std::cmp::Ordering::Less => std::cmp::Ordering::Less,
            std::cmp::Ordering::Greater => std::cmp::Ordering::Greater,
            std::cmp::Ordering::Equal => self.0.cmp(&other.0),
        }
    }
}

impl fmt::Debug for RowRef {
    /// Debug representation using the internal datums
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("RowRef{")?;
        f.debug_list().entries(self.into_iter()).finish()?;
        f.write_str("}")
    }
}

/// Packs datums into a [`Row`].
///
/// Creating a `RowPacker` via [`Row::packer`] starts a packing operation on the
/// row. A packing operation always starts from scratch: the existing contents
/// of the underlying row are cleared.
///
/// To complete a packing operation, drop the `RowPacker`.
#[derive(Debug)]
pub struct RowPacker<'a> {
    row: &'a mut Row,
}

#[derive(Debug, Clone)]
pub struct DatumListIter<'a> {
    data: &'a [u8],
}

#[derive(Debug, Clone)]
pub struct DatumDictIter<'a> {
    data: &'a [u8],
    prev_key: Option<&'a str>,
}

/// `RowArena` is used to hold on to temporary `Row`s for functions like `eval` that need to create complex `Datum`s but don't have a `Row` to put them in yet.
#[derive(Debug)]
pub struct RowArena {
    // Semantically, this field would be better represented by a `Vec<Box<[u8]>>`,
    // as once the arena takes ownership of a byte vector the vector is never
    // modified. But `RowArena::push_bytes` takes ownership of a `Vec<u8>`, so
    // storing that `Vec<u8>` directly avoids an allocation. The cost is
    // additional memory use, as the vector may have spare capacity, but row
    // arenas are short lived so this is the better tradeoff.
    inner: RefCell<Vec<Vec<u8>>>,
}

// DatumList and DatumDict defined here rather than near Datum because we need private access to the unsafe data field

/// A sequence of Datums
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct DatumList<'a> {
    /// Points at the serialized datums
    data: &'a [u8],
}

impl<'a> Debug for DatumList<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl Ord for DatumList<'_> {
    fn cmp(&self, other: &DatumList) -> Ordering {
        self.iter().cmp(other.iter())
    }
}

impl PartialOrd for DatumList<'_> {
    fn partial_cmp(&self, other: &DatumList) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A mapping from string keys to Datums
#[derive(Clone, Copy, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct DatumMap<'a> {
    /// Points at the serialized datums, which should be sorted in key order
    data: &'a [u8],
}

/// Represents a single `Datum`, appropriate to be nested inside other
/// `Datum`s.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct DatumNested<'a> {
    val: &'a [u8],
}

impl<'a> std::fmt::Display for DatumNested<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        std::fmt::Display::fmt(&self.datum(), f)
    }
}

impl<'a> std::fmt::Debug for DatumNested<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DatumNested")
            .field("val", &self.datum())
            .finish()
    }
}

impl<'a> DatumNested<'a> {
    // Figure out which bytes `read_datum` returns (e.g. including the tag),
    // and then store a reference to those bytes, so we can "replay" this same
    // call later on without storing the datum itself.
    pub fn extract(data: &mut &'a [u8]) -> DatumNested<'a> {
        let prev = *data;
        let _ = unsafe { read_datum(data) };
        DatumNested {
            val: &prev[..(prev.len() - data.len())],
        }
    }

    /// Returns the datum `self` contains.
    pub fn datum(&self) -> Datum<'a> {
        let mut temp = self.val;
        unsafe { read_datum(&mut temp) }
    }
}

impl<'a> Ord for DatumNested<'a> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.datum().cmp(&other.datum())
    }
}

impl<'a> PartialOrd for DatumNested<'a> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// Prefer adding new tags to the end of the enum. Certain behavior, like row ordering and EXPLAIN
// PHYSICAL PLAN, rely on the ordering of this enum. Neither of these are breaking changes, but
// it's annoying when they change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoPrimitive, TryFromPrimitive)]
#[repr(u8)]
enum Tag {
    Null,
    False,
    True,
    Int16,
    Int32,
    Int64,
    UInt8,
    UInt32,
    Float32,
    Float64,
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Interval,
    BytesTiny,
    BytesShort,
    BytesLong,
    BytesHuge,
    StringTiny,
    StringShort,
    StringLong,
    StringHuge,
    Uuid,
    Array,
    ListTiny,
    ListShort,
    ListLong,
    ListHuge,
    Dict,
    JsonNull,
    Dummy,
    Numeric,
    UInt16,
    UInt64,
    MzTimestamp,
    Range,
    MzAclItem,
    AclItem,
    // Everything except leap seconds and times beyond the range of
    // i64 nanoseconds. (Note that Materialize does not support leap
    // seconds, but this module does).
    CheapTimestamp,
    // Everything except leap seconds and times beyond the range of
    // i64 nanoseconds. (Note that Materialize does not support leap
    // seconds, but this module does).
    CheapTimestampTz,
    // The next several tags are for variable-length signed integer encoding.
    // The basic idea is that `NonNegativeIntN_K` is used to encode a datum of type
    // IntN whose actual value is positive or zero and fits in K bits, and similarly for
    // NegativeIntN_K with negative values.
    //
    // The order of these tags matters, because we want to be able to choose the
    // tag for a given datum quickly, with arithmetic, rather than slowly, with a
    // stack of `if` statements.
    //
    // Separate tags for non-negative and negative numbers are used to avoid having to
    // waste one bit in the actual data space to encode the sign.
    NonNegativeInt16_0, // i.e., 0
    NonNegativeInt16_8,
    NonNegativeInt16_16,

    NonNegativeInt32_0,
    NonNegativeInt32_8,
    NonNegativeInt32_16,
    NonNegativeInt32_24,
    NonNegativeInt32_32,

    NonNegativeInt64_0,
    NonNegativeInt64_8,
    NonNegativeInt64_16,
    NonNegativeInt64_24,
    NonNegativeInt64_32,
    NonNegativeInt64_40,
    NonNegativeInt64_48,
    NonNegativeInt64_56,
    NonNegativeInt64_64,

    NegativeInt16_0, // i.e., -1
    NegativeInt16_8,
    NegativeInt16_16,

    NegativeInt32_0,
    NegativeInt32_8,
    NegativeInt32_16,
    NegativeInt32_24,
    NegativeInt32_32,

    NegativeInt64_0,
    NegativeInt64_8,
    NegativeInt64_16,
    NegativeInt64_24,
    NegativeInt64_32,
    NegativeInt64_40,
    NegativeInt64_48,
    NegativeInt64_56,
    NegativeInt64_64,

    // These are like the ones above, but for unsigned types. The
    // situation is slightly simpler as we don't have negatives.
    UInt8_0, // i.e., 0
    UInt8_8,

    UInt16_0,
    UInt16_8,
    UInt16_16,

    UInt32_0,
    UInt32_8,
    UInt32_16,
    UInt32_24,
    UInt32_32,

    UInt64_0,
    UInt64_8,
    UInt64_16,
    UInt64_24,
    UInt64_32,
    UInt64_40,
    UInt64_48,
    UInt64_56,
    UInt64_64,
}

impl Tag {
    fn actual_int_length(self) -> Option<usize> {
        use Tag::*;
        let val = match self {
            NonNegativeInt16_0 | NonNegativeInt32_0 | NonNegativeInt64_0 | UInt8_0 | UInt16_0
            | UInt32_0 | UInt64_0 => 0,
            NonNegativeInt16_8 | NonNegativeInt32_8 | NonNegativeInt64_8 | UInt8_8 | UInt16_8
            | UInt32_8 | UInt64_8 => 1,
            NonNegativeInt16_16 | NonNegativeInt32_16 | NonNegativeInt64_16 | UInt16_16
            | UInt32_16 | UInt64_16 => 2,
            NonNegativeInt32_24 | NonNegativeInt64_24 | UInt32_24 | UInt64_24 => 3,
            NonNegativeInt32_32 | NonNegativeInt64_32 | UInt32_32 | UInt64_32 => 4,
            NonNegativeInt64_40 | UInt64_40 => 5,
            NonNegativeInt64_48 | UInt64_48 => 6,
            NonNegativeInt64_56 | UInt64_56 => 7,
            NonNegativeInt64_64 | UInt64_64 => 8,
            NegativeInt16_0 | NegativeInt32_0 | NegativeInt64_0 => 0,
            NegativeInt16_8 | NegativeInt32_8 | NegativeInt64_8 => 1,
            NegativeInt16_16 | NegativeInt32_16 | NegativeInt64_16 => 2,
            NegativeInt32_24 | NegativeInt64_24 => 3,
            NegativeInt32_32 | NegativeInt64_32 => 4,
            NegativeInt64_40 => 5,
            NegativeInt64_48 => 6,
            NegativeInt64_56 => 7,
            NegativeInt64_64 => 8,

            _ => return None,
        };
        Some(val)
    }
}

// --------------------------------------------------------------------------------
// reading data

/// Read a byte slice starting at byte `offset`.
///
/// Updates `offset` to point to the first byte after the end of the read region.
fn read_untagged_bytes<'a>(data: &mut &'a [u8]) -> &'a [u8] {
    let len = u64::from_le_bytes(read_byte_array(data));
    let len = usize::cast_from(len);
    let (bytes, next) = data.split_at(len);
    *data = next;
    bytes
}

/// Read a data whose length is encoded in the row before its contents.
///
/// Updates `offset` to point to the first byte after the end of the read region.
///
/// # Safety
///
/// This function is safe if the datum's length and contents were previously written by `push_lengthed_bytes`,
/// and it was only written with a `String` tag if it was indeed UTF-8.
unsafe fn read_lengthed_datum<'a>(data: &mut &'a [u8], tag: Tag) -> Datum<'a> {
    let len = match tag {
        Tag::BytesTiny | Tag::StringTiny | Tag::ListTiny => usize::from(read_byte(data)),
        Tag::BytesShort | Tag::StringShort | Tag::ListShort => {
            usize::from(u16::from_le_bytes(read_byte_array(data)))
        }
        Tag::BytesLong | Tag::StringLong | Tag::ListLong => {
            usize::cast_from(u32::from_le_bytes(read_byte_array(data)))
        }
        Tag::BytesHuge | Tag::StringHuge | Tag::ListHuge => {
            usize::cast_from(u64::from_le_bytes(read_byte_array(data)))
        }
        _ => unreachable!(),
    };
    let (bytes, next) = data.split_at(len);
    *data = next;
    match tag {
        Tag::BytesTiny | Tag::BytesShort | Tag::BytesLong | Tag::BytesHuge => Datum::Bytes(bytes),
        Tag::StringTiny | Tag::StringShort | Tag::StringLong | Tag::StringHuge => {
            Datum::String(str::from_utf8_unchecked(bytes))
        }
        Tag::ListTiny | Tag::ListShort | Tag::ListLong | Tag::ListHuge => {
            Datum::List(DatumList { data: bytes })
        }
        _ => unreachable!(),
    }
}

fn read_byte(data: &mut &[u8]) -> u8 {
    let byte = data[0];
    *data = &data[1..];
    byte
}

/// Read `length` bytes from `data` at `offset`, updating the
/// latter. Extend the resulting buffer to an array of `N` bytes by
/// inserting `FILL` in the k most significant bytes, where k = N - length.
///
/// SAFETY:
///   * length <= N
///   * offset + length <= data.len()
fn read_byte_array_sign_extending<const N: usize, const FILL: u8>(
    data: &mut &[u8],
    length: usize,
) -> [u8; N] {
    let mut raw = [FILL; N];
    let (prev, next) = data.split_at(length);
    (raw[..prev.len()]).copy_from_slice(prev);
    *data = next;
    raw
}
/// Read `length` bytes from `data` at `offset`, updating the
/// latter. Extend the resulting buffer to a negative `N`-byte
/// twos complement integer by filling the remaining bits with 1.
///
/// SAFETY:
///   * length <= N
///   * offset + length <= data.len()
fn read_byte_array_extending_negative<const N: usize>(data: &mut &[u8], length: usize) -> [u8; N] {
    read_byte_array_sign_extending::<N, 255>(data, length)
}

/// Read `length` bytes from `data` at `offset`, updating the
/// latter. Extend the resulting buffer to a positive or zero `N`-byte
/// twos complement integer by filling the remaining bits with 0.
///
/// SAFETY:
///   * length <= N
///   * offset + length <= data.len()
fn read_byte_array_extending_nonnegative<const N: usize>(
    data: &mut &[u8],
    length: usize,
) -> [u8; N] {
    read_byte_array_sign_extending::<N, 0>(data, length)
}

pub(super) fn read_byte_array<const N: usize>(data: &mut &[u8]) -> [u8; N] {
    let (prev, next) = data.split_first_chunk().unwrap();
    *data = next;
    *prev
}

pub(super) fn read_date(data: &mut &[u8]) -> Date {
    let days = i32::from_le_bytes(read_byte_array(data));
    Date::from_pg_epoch(days).expect("unexpected date")
}

pub(super) fn read_naive_date(data: &mut &[u8]) -> NaiveDate {
    let year = i32::from_le_bytes(read_byte_array(data));
    let ordinal = u32::from_le_bytes(read_byte_array(data));
    NaiveDate::from_yo_opt(year, ordinal).unwrap()
}

pub(super) fn read_time(data: &mut &[u8]) -> NaiveTime {
    let secs = u32::from_le_bytes(read_byte_array(data));
    let nanos = u32::from_le_bytes(read_byte_array(data));
    NaiveTime::from_num_seconds_from_midnight_opt(secs, nanos).unwrap()
}

/// Read a datum starting at byte `offset`.
///
/// Updates `offset` to point to the first byte after the end of the read region.
///
/// # Safety
///
/// This function is safe if a `Datum` was previously written at this offset by `push_datum`.
/// Otherwise it could return invalid values, which is Undefined Behavior.
pub unsafe fn read_datum<'a>(data: &mut &'a [u8]) -> Datum<'a> {
    let tag = Tag::try_from_primitive(read_byte(data)).expect("unknown row tag");
    match tag {
        Tag::Null => Datum::Null,
        Tag::False => Datum::False,
        Tag::True => Datum::True,
        Tag::UInt8_0 | Tag::UInt8_8 => {
            let i = u8::from_le_bytes(read_byte_array_extending_nonnegative(
                data,
                tag.actual_int_length()
                    .expect("returns a value for variable-length-encoded integer tags"),
            ));
            Datum::UInt8(i)
        }
        Tag::Int16 => {
            let i = i16::from_le_bytes(read_byte_array(data));
            Datum::Int16(i)
        }
        Tag::NonNegativeInt16_0 | Tag::NonNegativeInt16_16 | Tag::NonNegativeInt16_8 => {
            // SAFETY:`tag.actual_int_length()` is <= 16 for these tags,
            // and `data` is big enough because it was encoded validly. These assumptions
            // are checked in debug asserts.
            let i = i16::from_le_bytes(read_byte_array_extending_nonnegative(
                data,
                tag.actual_int_length()
                    .expect("returns a value for variable-length-encoded integer tags"),
            ));
            Datum::Int16(i)
        }
        Tag::UInt16_0 | Tag::UInt16_8 | Tag::UInt16_16 => {
            let i = u16::from_le_bytes(read_byte_array_extending_nonnegative(
                data,
                tag.actual_int_length()
                    .expect("returns a value for variable-length-encoded integer tags"),
            ));
            Datum::UInt16(i)
        }
        Tag::Int32 => {
            let i = i32::from_le_bytes(read_byte_array(data));
            Datum::Int32(i)
        }
        Tag::NonNegativeInt32_0
        | Tag::NonNegativeInt32_32
        | Tag::NonNegativeInt32_8
        | Tag::NonNegativeInt32_16
        | Tag::NonNegativeInt32_24 => {
            // SAFETY:`tag.actual_int_length()` is <= 32 for these tags,
            // and `data` is big enough because it was encoded validly. These assumptions
            // are checked in debug asserts.
            let i = i32::from_le_bytes(read_byte_array_extending_nonnegative(
                data,
                tag.actual_int_length()
                    .expect("returns a value for variable-length-encoded integer tags"),
            ));
            Datum::Int32(i)
        }
        Tag::UInt32_0 | Tag::UInt32_8 | Tag::UInt32_16 | Tag::UInt32_24 | Tag::UInt32_32 => {
            let i = u32::from_le_bytes(read_byte_array_extending_nonnegative(
                data,
                tag.actual_int_length()
                    .expect("returns a value for variable-length-encoded integer tags"),
            ));
            Datum::UInt32(i)
        }
        Tag::Int64 => {
            let i = i64::from_le_bytes(read_byte_array(data));
            Datum::Int64(i)
        }
        Tag::NonNegativeInt64_0
        | Tag::NonNegativeInt64_64
        | Tag::NonNegativeInt64_8
        | Tag::NonNegativeInt64_16
        | Tag::NonNegativeInt64_24
        | Tag::NonNegativeInt64_32
        | Tag::NonNegativeInt64_40
        | Tag::NonNegativeInt64_48
        | Tag::NonNegativeInt64_56 => {
            // SAFETY:`tag.actual_int_length()` is <= 64 for these tags,
            // and `data` is big enough because it was encoded validly. These assumptions
            // are checked in debug asserts.

            let i = i64::from_le_bytes(read_byte_array_extending_nonnegative(
                data,
                tag.actual_int_length()
                    .expect("returns a value for variable-length-encoded integer tags"),
            ));
            Datum::Int64(i)
        }
        Tag::UInt64_0
        | Tag::UInt64_8
        | Tag::UInt64_16
        | Tag::UInt64_24
        | Tag::UInt64_32
        | Tag::UInt64_40
        | Tag::UInt64_48
        | Tag::UInt64_56
        | Tag::UInt64_64 => {
            let i = u64::from_le_bytes(read_byte_array_extending_nonnegative(
                data,
                tag.actual_int_length()
                    .expect("returns a value for variable-length-encoded integer tags"),
            ));
            Datum::UInt64(i)
        }
        Tag::NegativeInt16_0 | Tag::NegativeInt16_16 | Tag::NegativeInt16_8 => {
            // SAFETY:`tag.actual_int_length()` is <= 16 for these tags,
            // and `data` is big enough because it was encoded validly. These assumptions
            // are checked in debug asserts.
            let i = i16::from_le_bytes(read_byte_array_extending_negative(
                data,
                tag.actual_int_length()
                    .expect("returns a value for variable-length-encoded integer tags"),
            ));
            Datum::Int16(i)
        }
        Tag::NegativeInt32_0
        | Tag::NegativeInt32_32
        | Tag::NegativeInt32_8
        | Tag::NegativeInt32_16
        | Tag::NegativeInt32_24 => {
            // SAFETY:`tag.actual_int_length()` is <= 32 for these tags,
            // and `data` is big enough because it was encoded validly. These assumptions
            // are checked in debug asserts.
            let i = i32::from_le_bytes(read_byte_array_extending_negative(
                data,
                tag.actual_int_length()
                    .expect("returns a value for variable-length-encoded integer tags"),
            ));
            Datum::Int32(i)
        }
        Tag::NegativeInt64_0
        | Tag::NegativeInt64_64
        | Tag::NegativeInt64_8
        | Tag::NegativeInt64_16
        | Tag::NegativeInt64_24
        | Tag::NegativeInt64_32
        | Tag::NegativeInt64_40
        | Tag::NegativeInt64_48
        | Tag::NegativeInt64_56 => {
            // SAFETY:`tag.actual_int_length()` is <= 64 for these tags,
            // and `data` is big enough because the row was encoded validly. These assumptions
            // are checked in debug asserts.
            let i = i64::from_le_bytes(read_byte_array_extending_negative(
                data,
                tag.actual_int_length()
                    .expect("returns a value for variable-length-encoded integer tags"),
            ));
            Datum::Int64(i)
        }

        Tag::UInt8 => {
            let i = u8::from_le_bytes(read_byte_array(data));
            Datum::UInt8(i)
        }
        Tag::UInt16 => {
            let i = u16::from_le_bytes(read_byte_array(data));
            Datum::UInt16(i)
        }
        Tag::UInt32 => {
            let i = u32::from_le_bytes(read_byte_array(data));
            Datum::UInt32(i)
        }
        Tag::UInt64 => {
            let i = u64::from_le_bytes(read_byte_array(data));
            Datum::UInt64(i)
        }
        Tag::Float32 => {
            let f = f32::from_bits(u32::from_le_bytes(read_byte_array(data)));
            Datum::Float32(OrderedFloat::from(f))
        }
        Tag::Float64 => {
            let f = f64::from_bits(u64::from_le_bytes(read_byte_array(data)));
            Datum::Float64(OrderedFloat::from(f))
        }
        Tag::Date => Datum::Date(read_date(data)),
        Tag::Time => Datum::Time(read_time(data)),
        Tag::CheapTimestamp => {
            let ts = i64::from_le_bytes(read_byte_array(data));
            let secs = ts.div_euclid(1_000_000_000);
            let nsecs: u32 = ts.rem_euclid(1_000_000_000).try_into().unwrap();
            let ndt = DateTime::from_timestamp(secs, nsecs)
                .expect("We only write round-trippable timestamps")
                .naive_utc();
            Datum::Timestamp(
                CheckedTimestamp::from_timestamplike(ndt).expect("unexpected timestamp"),
            )
        }
        Tag::CheapTimestampTz => {
            let ts = i64::from_le_bytes(read_byte_array(data));
            let secs = ts.div_euclid(1_000_000_000);
            let nsecs: u32 = ts.rem_euclid(1_000_000_000).try_into().unwrap();
            let dt = DateTime::from_timestamp(secs, nsecs)
                .expect("We only write round-trippable timestamps");
            Datum::TimestampTz(
                CheckedTimestamp::from_timestamplike(dt).expect("unexpected timestamp"),
            )
        }
        Tag::Timestamp => {
            let date = read_naive_date(data);
            let time = read_time(data);
            Datum::Timestamp(
                CheckedTimestamp::from_timestamplike(date.and_time(time))
                    .expect("unexpected timestamp"),
            )
        }
        Tag::TimestampTz => {
            let date = read_naive_date(data);
            let time = read_time(data);
            Datum::TimestampTz(
                CheckedTimestamp::from_timestamplike(DateTime::from_naive_utc_and_offset(
                    date.and_time(time),
                    Utc,
                ))
                .expect("unexpected timestamptz"),
            )
        }
        Tag::Interval => {
            let months = i32::from_le_bytes(read_byte_array(data));
            let days = i32::from_le_bytes(read_byte_array(data));
            let micros = i64::from_le_bytes(read_byte_array(data));
            Datum::Interval(Interval {
                months,
                days,
                micros,
            })
        }
        Tag::BytesTiny
        | Tag::BytesShort
        | Tag::BytesLong
        | Tag::BytesHuge
        | Tag::StringTiny
        | Tag::StringShort
        | Tag::StringLong
        | Tag::StringHuge
        | Tag::ListTiny
        | Tag::ListShort
        | Tag::ListLong
        | Tag::ListHuge => read_lengthed_datum(data, tag),
        Tag::Uuid => Datum::Uuid(Uuid::from_bytes(read_byte_array(data))),
        Tag::Array => {
            // See the comment in `Row::push_array` for details on the encoding
            // of arrays.
            let ndims = read_byte(data);
            let dims_size = usize::from(ndims) * size_of::<u64>() * 2;
            let (dims, next) = data.split_at(dims_size);
            *data = next;
            let bytes = read_untagged_bytes(data);
            Datum::Array(Array {
                dims: ArrayDimensions { data: dims },
                elements: DatumList { data: bytes },
            })
        }
        Tag::Dict => {
            let bytes = read_untagged_bytes(data);
            Datum::Map(DatumMap { data: bytes })
        }
        Tag::JsonNull => Datum::JsonNull,
        Tag::Dummy => Datum::Dummy,
        Tag::Numeric => {
            let digits = read_byte(data).into();
            let exponent = i8::reinterpret_cast(read_byte(data));
            let bits = read_byte(data);

            let lsu_u16_len = Numeric::digits_to_lsu_elements_len(digits);
            let lsu_u8_len = lsu_u16_len * 2;
            let (lsu_u8, next) = data.split_at(lsu_u8_len);
            *data = next;

            // TODO: if we refactor the decimal library to accept the owned
            // array as a parameter to `from_raw_parts` below, we could likely
            // avoid a copy because it is exactly the value we want
            let mut lsu = [0; numeric::NUMERIC_DATUM_WIDTH_USIZE];
            for (i, c) in lsu_u8.chunks(2).enumerate() {
                lsu[i] = u16::from_le_bytes(c.try_into().unwrap());
            }

            let d = Numeric::from_raw_parts(digits, exponent.into(), bits, lsu);
            Datum::from(d)
        }
        Tag::MzTimestamp => {
            let t = Timestamp::decode(read_byte_array(data));
            Datum::MzTimestamp(t)
        }
        Tag::Range => {
            // See notes on `push_range_with` for details about encoding.
            let flag_byte = read_byte(data);
            let flags = range::InternalFlags::from_bits(flag_byte)
                .expect("range flags must be encoded validly");

            if flags.contains(range::InternalFlags::EMPTY) {
                assert!(
                    flags == range::InternalFlags::EMPTY,
                    "empty ranges contain only RANGE_EMPTY flag"
                );

                return Datum::Range(Range { inner: None });
            }

            let lower_bound = if flags.contains(range::InternalFlags::LB_INFINITE) {
                None
            } else {
                Some(DatumNested::extract(data))
            };

            let lower = RangeBound {
                inclusive: flags.contains(range::InternalFlags::LB_INCLUSIVE),
                bound: lower_bound,
            };

            let upper_bound = if flags.contains(range::InternalFlags::UB_INFINITE) {
                None
            } else {
                Some(DatumNested::extract(data))
            };

            let upper = RangeBound {
                inclusive: flags.contains(range::InternalFlags::UB_INCLUSIVE),
                bound: upper_bound,
            };

            Datum::Range(Range {
                inner: Some(RangeInner { lower, upper }),
            })
        }
        Tag::MzAclItem => {
            const N: usize = MzAclItem::binary_size();
            let mz_acl_item =
                MzAclItem::decode_binary(&read_byte_array::<N>(data)).expect("invalid mz_aclitem");
            Datum::MzAclItem(mz_acl_item)
        }
        Tag::AclItem => {
            const N: usize = AclItem::binary_size();
            let acl_item =
                AclItem::decode_binary(&read_byte_array::<N>(data)).expect("invalid aclitem");
            Datum::AclItem(acl_item)
        }
    }
}

// --------------------------------------------------------------------------------
// writing data

fn push_untagged_bytes<D>(data: &mut D, bytes: &[u8])
where
    D: Vector<u8>,
{
    let len = u64::cast_from(bytes.len());
    data.extend_from_slice(&len.to_le_bytes());
    data.extend_from_slice(bytes);
}

fn push_lengthed_bytes<D>(data: &mut D, bytes: &[u8], tag: Tag)
where
    D: Vector<u8>,
{
    match tag {
        Tag::BytesTiny | Tag::StringTiny | Tag::ListTiny => {
            let len = bytes.len().to_le_bytes();
            data.push(len[0]);
        }
        Tag::BytesShort | Tag::StringShort | Tag::ListShort => {
            let len = bytes.len().to_le_bytes();
            data.extend_from_slice(&len[0..2]);
        }
        Tag::BytesLong | Tag::StringLong | Tag::ListLong => {
            let len = bytes.len().to_le_bytes();
            data.extend_from_slice(&len[0..4]);
        }
        Tag::BytesHuge | Tag::StringHuge | Tag::ListHuge => {
            let len = bytes.len().to_le_bytes();
            data.extend_from_slice(&len);
        }
        _ => unreachable!(),
    }
    data.extend_from_slice(bytes);
}

pub(super) fn date_to_array(date: Date) -> [u8; size_of::<i32>()] {
    i32::to_le_bytes(date.pg_epoch_days())
}

fn push_date<D>(data: &mut D, date: Date)
where
    D: Vector<u8>,
{
    data.extend_from_slice(&date_to_array(date));
}

pub(super) fn naive_date_to_arrays(
    date: NaiveDate,
) -> ([u8; size_of::<i32>()], [u8; size_of::<u32>()]) {
    (
        i32::to_le_bytes(date.year()),
        u32::to_le_bytes(date.ordinal()),
    )
}

fn push_naive_date<D>(data: &mut D, date: NaiveDate)
where
    D: Vector<u8>,
{
    let (ds1, ds2) = naive_date_to_arrays(date);
    data.extend_from_slice(&ds1);
    data.extend_from_slice(&ds2);
}

pub(super) fn time_to_arrays(time: NaiveTime) -> ([u8; size_of::<u32>()], [u8; size_of::<u32>()]) {
    (
        u32::to_le_bytes(time.num_seconds_from_midnight()),
        u32::to_le_bytes(time.nanosecond()),
    )
}

fn push_time<D>(data: &mut D, time: NaiveTime)
where
    D: Vector<u8>,
{
    let (ts1, ts2) = time_to_arrays(time);
    data.extend_from_slice(&ts1);
    data.extend_from_slice(&ts2);
}

/// Returns an i64 representing a `NaiveDateTime`, if
/// said i64 can be round-tripped back to a `NaiveDateTime`.
///
/// The only exotic NDTs for which this can't happen are those that
/// are hundreds of years in the future or past, or those that
/// represent a leap second. (Note that Materialize does not support
/// leap seconds, but this module does).
// This function is inspired by `NaiveDateTime::timestamp_nanos`,
// with extra checking.
fn checked_timestamp_nanos(dt: NaiveDateTime) -> Option<i64> {
    let subsec_nanos = dt.and_utc().timestamp_subsec_nanos();
    if subsec_nanos >= 1_000_000_000 {
        return None;
    }
    let as_ns = dt.and_utc().timestamp().checked_mul(1_000_000_000)?;
    as_ns.checked_add(i64::from(subsec_nanos))
}

// This function is extremely hot, so
// we just use `as` to avoid the overhead of
// `try_into` followed by `unwrap`.
// `leading_ones` and `leading_zeros`
// can never return values greater than 64, so the conversion is safe.
#[inline(always)]
#[allow(clippy::as_conversions)]
fn min_bytes_signed<T>(i: T) -> u8
where
    T: Into<i64>,
{
    let i: i64 = i.into();

    // To fit in n bytes, we require that
    // everything but the leading sign bits fits in n*8
    // bits.
    let n_sign_bits = if i.is_negative() {
        i.leading_ones() as u8
    } else {
        i.leading_zeros() as u8
    };

    (64 - n_sign_bits + 7) / 8
}

// In principle we could just use `min_bytes_signed`, rather than
// having a separate function here, as long as we made that one take
// `T: Into<i128>` instead of 64. But LLVM doesn't seem smart enough
// to realize that that function is the same as the current version,
// and generates worse code.
//
// Justification for `as` is the same as in `min_bytes_signed`.
#[inline(always)]
#[allow(clippy::as_conversions)]
fn min_bytes_unsigned<T>(i: T) -> u8
where
    T: Into<u64>,
{
    let i: u64 = i.into();

    let n_sign_bits = i.leading_zeros() as u8;

    (64 - n_sign_bits + 7) / 8
}

const TINY: usize = 1 << 8;
const SHORT: usize = 1 << 16;
const LONG: usize = 1 << 32;

fn push_datum<D>(data: &mut D, datum: Datum)
where
    D: Vector<u8>,
{
    match datum {
        Datum::Null => data.push(Tag::Null.into()),
        Datum::False => data.push(Tag::False.into()),
        Datum::True => data.push(Tag::True.into()),
        Datum::Int16(i) => {
            let mbs = min_bytes_signed(i);
            let tag = u8::from(if i.is_negative() {
                Tag::NegativeInt16_0
            } else {
                Tag::NonNegativeInt16_0
            }) + mbs;

            data.push(tag);
            data.extend_from_slice(&i.to_le_bytes()[0..usize::from(mbs)]);
        }
        Datum::Int32(i) => {
            let mbs = min_bytes_signed(i);
            let tag = u8::from(if i.is_negative() {
                Tag::NegativeInt32_0
            } else {
                Tag::NonNegativeInt32_0
            }) + mbs;

            data.push(tag);
            data.extend_from_slice(&i.to_le_bytes()[0..usize::from(mbs)]);
        }
        Datum::Int64(i) => {
            let mbs = min_bytes_signed(i);
            let tag = u8::from(if i.is_negative() {
                Tag::NegativeInt64_0
            } else {
                Tag::NonNegativeInt64_0
            }) + mbs;

            data.push(tag);
            data.extend_from_slice(&i.to_le_bytes()[0..usize::from(mbs)]);
        }
        Datum::UInt8(i) => {
            let mbu = min_bytes_unsigned(i);
            let tag = u8::from(Tag::UInt8_0) + mbu;
            data.push(tag);
            data.extend_from_slice(&i.to_le_bytes()[0..usize::from(mbu)]);
        }
        Datum::UInt16(i) => {
            let mbu = min_bytes_unsigned(i);
            let tag = u8::from(Tag::UInt16_0) + mbu;
            data.push(tag);
            data.extend_from_slice(&i.to_le_bytes()[0..usize::from(mbu)]);
        }
        Datum::UInt32(i) => {
            let mbu = min_bytes_unsigned(i);
            let tag = u8::from(Tag::UInt32_0) + mbu;
            data.push(tag);
            data.extend_from_slice(&i.to_le_bytes()[0..usize::from(mbu)]);
        }
        Datum::UInt64(i) => {
            let mbu = min_bytes_unsigned(i);
            let tag = u8::from(Tag::UInt64_0) + mbu;
            data.push(tag);
            data.extend_from_slice(&i.to_le_bytes()[0..usize::from(mbu)]);
        }
        Datum::Float32(f) => {
            data.push(Tag::Float32.into());
            data.extend_from_slice(&f.to_bits().to_le_bytes());
        }
        Datum::Float64(f) => {
            data.push(Tag::Float64.into());
            data.extend_from_slice(&f.to_bits().to_le_bytes());
        }
        Datum::Date(d) => {
            data.push(Tag::Date.into());
            push_date(data, d);
        }
        Datum::Time(t) => {
            data.push(Tag::Time.into());
            push_time(data, t);
        }
        Datum::Timestamp(t) => {
            let datetime = t.to_naive();
            if let Some(nanos) = checked_timestamp_nanos(datetime) {
                data.push(Tag::CheapTimestamp.into());
                data.extend_from_slice(&nanos.to_le_bytes());
            } else {
                data.push(Tag::Timestamp.into());
                push_naive_date(data, datetime.date());
                push_time(data, datetime.time());
            }
        }
        Datum::TimestampTz(t) => {
            let datetime = t.to_naive();
            if let Some(nanos) = checked_timestamp_nanos(datetime) {
                data.push(Tag::CheapTimestampTz.into());
                data.extend_from_slice(&nanos.to_le_bytes());
            } else {
                data.push(Tag::TimestampTz.into());
                push_naive_date(data, datetime.date());
                push_time(data, datetime.time());
            }
        }
        Datum::Interval(i) => {
            data.push(Tag::Interval.into());
            data.extend_from_slice(&i.months.to_le_bytes());
            data.extend_from_slice(&i.days.to_le_bytes());
            data.extend_from_slice(&i.micros.to_le_bytes());
        }
        Datum::Bytes(bytes) => {
            let tag = match bytes.len() {
                0..TINY => Tag::BytesTiny,
                TINY..SHORT => Tag::BytesShort,
                SHORT..LONG => Tag::BytesLong,
                _ => Tag::BytesHuge,
            };
            data.push(tag.into());
            push_lengthed_bytes(data, bytes, tag);
        }
        Datum::String(string) => {
            let tag = match string.len() {
                0..TINY => Tag::StringTiny,
                TINY..SHORT => Tag::StringShort,
                SHORT..LONG => Tag::StringLong,
                _ => Tag::StringHuge,
            };
            data.push(tag.into());
            push_lengthed_bytes(data, string.as_bytes(), tag);
        }
        Datum::List(list) => {
            let tag = match list.data.len() {
                0..TINY => Tag::ListTiny,
                TINY..SHORT => Tag::ListShort,
                SHORT..LONG => Tag::ListLong,
                _ => Tag::ListHuge,
            };
            data.push(tag.into());
            push_lengthed_bytes(data, list.data, tag);
        }
        Datum::Uuid(u) => {
            data.push(Tag::Uuid.into());
            data.extend_from_slice(u.as_bytes());
        }
        Datum::Array(array) => {
            // See the comment in `Row::push_array` for details on the encoding
            // of arrays.
            data.push(Tag::Array.into());
            data.push(array.dims.ndims());
            data.extend_from_slice(array.dims.data);
            push_untagged_bytes(data, array.elements.data);
        }
        Datum::Map(dict) => {
            data.push(Tag::Dict.into());
            push_untagged_bytes(data, dict.data);
        }
        Datum::JsonNull => data.push(Tag::JsonNull.into()),
        Datum::MzTimestamp(t) => {
            data.push(Tag::MzTimestamp.into());
            data.extend_from_slice(&t.encode());
        }
        Datum::Dummy => data.push(Tag::Dummy.into()),
        Datum::Numeric(mut n) => {
            // Pseudo-canonical representation of decimal values with
            // insignificant zeroes trimmed. This compresses the number further
            // than `Numeric::trim` by removing all zeroes, and not only those in
            // the fractional component.
            numeric::cx_datum().reduce(&mut n.0);
            let (digits, exponent, bits, lsu) = n.0.to_raw_parts();
            data.push(Tag::Numeric.into());
            data.push(u8::try_from(digits).expect("digits to fit within u8; should not exceed 39"));
            data.push(
                i8::try_from(exponent)
                    .expect("exponent to fit within i8; should not exceed +/- 39")
                    .to_le_bytes()[0],
            );
            data.push(bits);

            let lsu = &lsu[..Numeric::digits_to_lsu_elements_len(digits)];

            // Little endian machines can take the lsu directly from u16 to u8.
            if cfg!(target_endian = "little") {
                // SAFETY: `lsu` (returned by `coefficient_units()`) is a `&[u16]`, so
                // each element can safely be transmuted into two `u8`s.
                let (prefix, lsu_bytes, suffix) = unsafe { lsu.align_to::<u8>() };
                // The `u8` aligned version of the `lsu` should have twice as many
                // elements as we expect for the `u16` version.
                soft_assert_no_log!(
                    lsu_bytes.len() == Numeric::digits_to_lsu_elements_len(digits) * 2,
                    "u8 version of numeric LSU contained the wrong number of elements; expected {}, but got {}",
                    Numeric::digits_to_lsu_elements_len(digits) * 2,
                    lsu_bytes.len()
                );
                // There should be no unaligned elements in the prefix or suffix.
                soft_assert_no_log!(prefix.is_empty() && suffix.is_empty());
                data.extend_from_slice(lsu_bytes);
            } else {
                for u in lsu {
                    data.extend_from_slice(&u.to_le_bytes());
                }
            }
        }
        Datum::Range(range) => {
            // See notes on `push_range_with` for details about encoding.
            data.push(Tag::Range.into());
            data.push(range.internal_flag_bits());

            if let Some(RangeInner { lower, upper }) = range.inner {
                for bound in [lower.bound, upper.bound] {
                    if let Some(bound) = bound {
                        match bound.datum() {
                            Datum::Null => panic!("cannot push Datum::Null into range"),
                            d => push_datum::<D>(data, d),
                        }
                    }
                }
            }
        }
        Datum::MzAclItem(mz_acl_item) => {
            data.push(Tag::MzAclItem.into());
            data.extend_from_slice(&mz_acl_item.encode_binary());
        }
        Datum::AclItem(acl_item) => {
            data.push(Tag::AclItem.into());
            data.extend_from_slice(&acl_item.encode_binary());
        }
    }
}

/// Return the number of bytes these Datums would use if packed as a Row.
pub fn row_size<'a, I>(a: I) -> usize
where
    I: IntoIterator<Item = Datum<'a>>,
{
    // Using datums_size instead of a.data().len() here is safer because it will
    // return the size of the datums if they were packed into a Row. Although
    // a.data().len() happens to give the correct answer (and is faster), data()
    // is documented as for debugging only.
    let sz = datums_size::<_, _>(a);
    let size_of_row = std::mem::size_of::<Row>();
    // The Row struct attempts to inline data until it can't fit in the
    // preallocated size. Otherwise it spills to heap, and uses the Row to point
    // to that.
    if sz > Row::SIZE {
        sz + size_of_row
    } else {
        size_of_row
    }
}

/// Number of bytes required by the datum.
/// This is used to optimistically pre-allocate buffers for packing rows.
pub fn datum_size(datum: &Datum) -> usize {
    match datum {
        Datum::Null => 1,
        Datum::False => 1,
        Datum::True => 1,
        Datum::Int16(i) => 1 + usize::from(min_bytes_signed(*i)),
        Datum::Int32(i) => 1 + usize::from(min_bytes_signed(*i)),
        Datum::Int64(i) => 1 + usize::from(min_bytes_signed(*i)),
        Datum::UInt8(i) => 1 + usize::from(min_bytes_unsigned(*i)),
        Datum::UInt16(i) => 1 + usize::from(min_bytes_unsigned(*i)),
        Datum::UInt32(i) => 1 + usize::from(min_bytes_unsigned(*i)),
        Datum::UInt64(i) => 1 + usize::from(min_bytes_unsigned(*i)),
        Datum::Float32(_) => 1 + size_of::<f32>(),
        Datum::Float64(_) => 1 + size_of::<f64>(),
        Datum::Date(_) => 1 + size_of::<i32>(),
        Datum::Time(_) => 1 + 8,
        Datum::Timestamp(t) => {
            1 + if checked_timestamp_nanos(t.to_naive()).is_some() {
                8
            } else {
                16
            }
        }
        Datum::TimestampTz(t) => {
            1 + if checked_timestamp_nanos(t.naive_utc()).is_some() {
                8
            } else {
                16
            }
        }
        Datum::Interval(_) => 1 + size_of::<i32>() + size_of::<i32>() + size_of::<i64>(),
        Datum::Bytes(bytes) => {
            // We use a variable length representation of slice length.
            let bytes_for_length = match bytes.len() {
                0..TINY => 1,
                TINY..SHORT => 2,
                SHORT..LONG => 4,
                _ => 8,
            };
            1 + bytes_for_length + bytes.len()
        }
        Datum::String(string) => {
            // We use a variable length representation of slice length.
            let bytes_for_length = match string.len() {
                0..TINY => 1,
                TINY..SHORT => 2,
                SHORT..LONG => 4,
                _ => 8,
            };
            1 + bytes_for_length + string.len()
        }
        Datum::Uuid(_) => 1 + size_of::<uuid::Bytes>(),
        Datum::Array(array) => {
            1 + size_of::<u8>()
                + array.dims.data.len()
                + size_of::<u64>()
                + array.elements.data.len()
        }
        Datum::List(list) => 1 + size_of::<u64>() + list.data.len(),
        Datum::Map(dict) => 1 + size_of::<u64>() + dict.data.len(),
        Datum::JsonNull => 1,
        Datum::MzTimestamp(_) => 1 + size_of::<Timestamp>(),
        Datum::Dummy => 1,
        Datum::Numeric(d) => {
            let mut d = d.0.clone();
            // Values must be reduced to determine appropriate number of
            // coefficient units.
            numeric::cx_datum().reduce(&mut d);
            // 4 = 1 bit each for tag, digits, exponent, bits
            4 + (d.coefficient_units().len() * 2)
        }
        Datum::Range(Range { inner }) => {
            // Tag + flags
            2 + match inner {
                None => 0,
                Some(RangeInner { lower, upper }) => [lower.bound, upper.bound]
                    .iter()
                    .map(|bound| match bound {
                        None => 0,
                        Some(bound) => bound.val.len(),
                    })
                    .sum(),
            }
        }
        Datum::MzAclItem(_) => 1 + MzAclItem::binary_size(),
        Datum::AclItem(_) => 1 + AclItem::binary_size(),
    }
}

/// Number of bytes required by a sequence of datums.
///
/// This method can be used to right-size the allocation for a `Row`
/// before calling [`RowPacker::extend`].
pub fn datums_size<'a, I, D>(iter: I) -> usize
where
    I: IntoIterator<Item = D>,
    D: Borrow<Datum<'a>>,
{
    iter.into_iter().map(|d| datum_size(d.borrow())).sum()
}

/// Number of bytes required by a list of datums. This computes the size that would be required if
/// the given datums were packed into a list.
///
/// This is used to optimistically pre-allocate buffers for packing rows.
pub fn datum_list_size<'a, I, D>(iter: I) -> usize
where
    I: IntoIterator<Item = D>,
    D: Borrow<Datum<'a>>,
{
    1 + size_of::<u64>() + datums_size(iter)
}

impl RowPacker<'_> {
    /// Constructs a row packer that will pack additional datums into the
    /// provided row.
    ///
    /// This function is intentionally somewhat inconvenient to call. You
    /// usually want to call [`Row::packer`] instead to start packing from
    /// scratch.
    pub fn for_existing_row(row: &mut Row) -> RowPacker {
        RowPacker { row }
    }

    /// Extend an existing `Row` with a `Datum`.
    #[inline]
    pub fn push<'a, D>(&mut self, datum: D)
    where
        D: Borrow<Datum<'a>>,
    {
        push_datum(&mut self.row.data, *datum.borrow());
    }

    /// Extend an existing `Row` with additional `Datum`s.
    #[inline]
    pub fn extend<'a, I, D>(&mut self, iter: I)
    where
        I: IntoIterator<Item = D>,
        D: Borrow<Datum<'a>>,
    {
        for datum in iter {
            push_datum(&mut self.row.data, *datum.borrow())
        }
    }

    /// Extend an existing `Row` with additional `Datum`s.
    ///
    /// In the case the iterator produces an error, the pushing of
    /// datums in terminated and the error returned. The `Row` will
    /// be incomplete, but it will be safe to read datums from it.
    #[inline]
    pub fn try_extend<'a, I, E, D>(&mut self, iter: I) -> Result<(), E>
    where
        I: IntoIterator<Item = Result<D, E>>,
        D: Borrow<Datum<'a>>,
    {
        for datum in iter {
            push_datum(&mut self.row.data, *datum?.borrow());
        }
        Ok(())
    }

    /// Appends the datums of an entire `Row`.
    pub fn extend_by_row(&mut self, row: &Row) {
        self.row.data.extend_from_slice(row.data.as_slice());
    }

    /// Appends the slice of data representing an entire `Row`. The data is not validated.
    ///
    /// # Safety
    ///
    /// The requirements from [`Row::from_bytes_unchecked`] apply here, too:
    /// This method relies on `data` being an appropriate row encoding, and can
    /// result in unsafety if this is not the case.
    #[inline]
    pub unsafe fn extend_by_slice_unchecked(&mut self, data: &[u8]) {
        self.row.data.extend_from_slice(data)
    }

    /// Pushes a [`DatumList`] that is built from a closure.
    ///
    /// The supplied closure will be invoked once with a `Row` that can be used
    /// to populate the list. It is valid to call any method on the
    /// [`RowPacker`] except for [`RowPacker::clear`], [`RowPacker::truncate`],
    /// or [`RowPacker::truncate_datums`].
    ///
    /// Returns the value returned by the closure, if any.
    ///
    /// ```
    /// # use mz_repr::{Row, Datum};
    /// let mut row = Row::default();
    /// row.packer().push_list_with(|row| {
    ///     row.push(Datum::String("age"));
    ///     row.push(Datum::Int64(42));
    /// });
    /// assert_eq!(
    ///     row.unpack_first().unwrap_list().iter().collect::<Vec<_>>(),
    ///     vec![Datum::String("age"), Datum::Int64(42)],
    /// );
    /// ```
    #[inline]
    pub fn push_list_with<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut RowPacker) -> R,
    {
        // First, assume that the list will fit in 255 bytes, and thus the length will fit in
        // 1 byte. If not, we'll fix it up later.
        let start = self.row.data.len();
        self.row.data.push(Tag::ListTiny.into());
        // Write a dummy len, will fix it up later.
        self.row.data.push(0);

        let out = f(self);

        // The `- 1 - 1` is for the tag and the len.
        let len = self.row.data.len() - start - 1 - 1;
        // We now know the real len.
        if len < TINY {
            // If the len fits in 1 byte, we just need to fix up the len.
            self.row.data[start + 1] = len.to_le_bytes()[0];
        } else {
            // Note: We move this code path into its own function, so that the common case can be
            // inlined.
            long_list(&mut self.row.data, start, len);
        }

        /// 1. Fix up the tag.
        /// 2. Move the actual data a bit (for which we also need to make room at the end).
        /// 3. Fix up the len.
        /// `data`: The row's backing data.
        /// `start`: where `push_list_with` started writing in `data`.
        /// `len`: the length of the data, excluding the tag and the length.
        #[cold]
        fn long_list(data: &mut CompactBytes, start: usize, len: usize) {
            // `len_len`: the length of the length. (Possible values are: 2, 4, 8. 1 is handled
            // elsewhere.) The other parameters are the same as for `long_list`.
            let long_list_inner = |data: &mut CompactBytes, len_len| {
                // We'll need memory for the new, bigger length, so make the `CompactBytes` bigger.
                // The `- 1` is because the old length was 1 byte.
                const ZEROS: [u8; 8] = [0; 8];
                data.extend_from_slice(&ZEROS[0..len_len - 1]);
                // Move the data to the end of the `CompactBytes`, to make space for the new length.
                // Originally, it started after the 1-byte tag and the 1-byte length, now it will
                // start after the 1-byte tag and the len_len-byte length.
                //
                // Note that this is the only operation in `long_list` whose cost is proportional
                // to `len`. Since `len` is at least 256 here, the other operations' cost are
                // negligible. `copy_within` is a memmove, which is probably a fair bit faster per
                // Datum than a Datum encoding in the `f` closure.
                data.copy_within(start + 1 + 1..start + 1 + 1 + len, start + 1 + len_len);
                // Write the new length.
                data[start + 1..start + 1 + len_len]
                    .copy_from_slice(&len.to_le_bytes()[0..len_len]);
            };
            match len {
                0..TINY => {
                    unreachable!()
                }
                TINY..SHORT => {
                    data[start] = Tag::ListShort.into();
                    long_list_inner(data, 2);
                }
                SHORT..LONG => {
                    data[start] = Tag::ListLong.into();
                    long_list_inner(data, 4);
                }
                _ => {
                    data[start] = Tag::ListHuge.into();
                    long_list_inner(data, 8);
                }
            };
        }

        out
    }

    /// Pushes a [`DatumMap`] that is built from a closure.
    ///
    /// The supplied closure will be invoked once with a `Row` that can be used
    /// to populate the dict.
    ///
    /// The closure **must** alternate pushing string keys and arbitrary values,
    /// otherwise reading the dict will cause a panic.
    ///
    /// The closure **must** push keys in ascending order, otherwise equality
    /// checks on the resulting `Row` may be wrong and reading the dict IN DEBUG
    /// MODE will cause a panic.
    ///
    /// The closure **must not** call [`RowPacker::clear`],
    /// [`RowPacker::truncate`], or [`RowPacker::truncate_datums`].
    ///
    /// # Example
    ///
    /// ```
    /// # use mz_repr::{Row, Datum};
    /// let mut row = Row::default();
    /// row.packer().push_dict_with(|row| {
    ///
    ///     // key
    ///     row.push(Datum::String("age"));
    ///     // value
    ///     row.push(Datum::Int64(42));
    ///
    ///     // key
    ///     row.push(Datum::String("name"));
    ///     // value
    ///     row.push(Datum::String("bob"));
    /// });
    /// assert_eq!(
    ///     row.unpack_first().unwrap_map().iter().collect::<Vec<_>>(),
    ///     vec![("age", Datum::Int64(42)), ("name", Datum::String("bob"))]
    /// );
    /// ```
    pub fn push_dict_with<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut RowPacker) -> R,
    {
        self.row.data.push(Tag::Dict.into());
        let start = self.row.data.len();
        // write a dummy len, will fix it up later
        self.row.data.extend_from_slice(&[0; size_of::<u64>()]);

        let res = f(self);

        let len = u64::cast_from(self.row.data.len() - start - size_of::<u64>());
        // fix up the len
        self.row.data[start..start + size_of::<u64>()].copy_from_slice(&len.to_le_bytes());

        res
    }

    /// Convenience function to construct an array from an iter of `Datum`s.
    ///
    /// Returns an error if the number of elements in `iter` does not match
    /// the cardinality of the array as described by `dims`, or if the
    /// number of dimensions exceeds [`MAX_ARRAY_DIMENSIONS`]. If an error
    /// occurs, the packer's state will be unchanged.
    pub fn try_push_array<'a, I, D>(
        &mut self,
        dims: &[ArrayDimension],
        iter: I,
    ) -> Result<(), InvalidArrayError>
    where
        I: IntoIterator<Item = D>,
        D: Borrow<Datum<'a>>,
    {
        // SAFETY: The function returns the exact number of elements pushed into the array.
        unsafe {
            self.push_array_with_unchecked(dims, |packer| {
                let mut nelements = 0;
                for datum in iter {
                    packer.push(datum);
                    nelements += 1;
                }
                Ok::<_, InvalidArrayError>(nelements)
            })
        }
    }

    /// Convenience function to construct an array from a function. The function must return the
    /// number of elements it pushed into the array. It is undefined behavior if the function returns
    /// a number different to the number of elements it pushed.
    ///
    /// Returns an error if the number of elements pushed by `f` does not match
    /// the cardinality of the array as described by `dims`, or if the
    /// number of dimensions exceeds [`MAX_ARRAY_DIMENSIONS`], or if `f` errors. If an error
    /// occurs, the packer's state will be unchanged.
    pub unsafe fn push_array_with_unchecked<F, E>(
        &mut self,
        dims: &[ArrayDimension],
        f: F,
    ) -> Result<(), E>
    where
        F: FnOnce(&mut RowPacker) -> Result<usize, E>,
        E: From<InvalidArrayError>,
    {
        // Arrays are encoded as follows.
        //
        // u8    ndims
        // u64   dim_0 lower bound
        // u64   dim_0 length
        // ...
        // u64   dim_n lower bound
        // u64   dim_n length
        // u64   element data size in bytes
        // u8    element data, where elements are encoded in row-major order

        if dims.len() > usize::from(MAX_ARRAY_DIMENSIONS) {
            return Err(InvalidArrayError::TooManyDimensions(dims.len()).into());
        }

        let start = self.row.data.len();
        self.row.data.push(Tag::Array.into());

        // Write dimension information.
        self.row
            .data
            .push(dims.len().try_into().expect("ndims verified to fit in u8"));
        for dim in dims {
            self.row
                .data
                .extend_from_slice(&i64::cast_from(dim.lower_bound).to_le_bytes());
            self.row
                .data
                .extend_from_slice(&u64::cast_from(dim.length).to_le_bytes());
        }

        // Write elements.
        let off = self.row.data.len();
        self.row.data.extend_from_slice(&[0; size_of::<u64>()]);
        let nelements = match f(self) {
            Ok(nelements) => nelements,
            Err(e) => {
                self.row.data.truncate(start);
                return Err(e);
            }
        };
        let len = u64::cast_from(self.row.data.len() - off - size_of::<u64>());
        self.row.data[off..off + size_of::<u64>()].copy_from_slice(&len.to_le_bytes());

        // Check that the number of elements written matches the dimension
        // information.
        let cardinality = match dims {
            [] => 0,
            dims => dims.iter().map(|d| d.length).product(),
        };
        if nelements != cardinality {
            self.row.data.truncate(start);
            return Err(InvalidArrayError::WrongCardinality {
                actual: nelements,
                expected: cardinality,
            }
            .into());
        }

        Ok(())
    }

    /// Pushes an [`Array`] that is built from a closure.
    ///
    /// __WARNING__: This is fairly "sharp" tool that is easy to get wrong. You
    /// should prefer [`RowPacker::try_push_array`] when possible.
    ///
    /// Returns an error if the number of elements pushed does not match
    /// the cardinality of the array as described by `dims`, or if the
    /// number of dimensions exceeds [`MAX_ARRAY_DIMENSIONS`]. If an error
    /// occurs, the packer's state will be unchanged.
    pub fn push_array_with_row_major<F, I>(
        &mut self,
        dims: I,
        f: F,
    ) -> Result<(), InvalidArrayError>
    where
        I: IntoIterator<Item = ArrayDimension>,
        F: FnOnce(&mut RowPacker) -> usize,
    {
        let start = self.row.data.len();
        self.row.data.push(Tag::Array.into());

        // Write dummy dimension length for now, we'll fix it up.
        let dims_start = self.row.data.len();
        self.row.data.push(42);

        let mut num_dims: u8 = 0;
        let mut cardinality: usize = 1;
        for dim in dims {
            num_dims += 1;
            cardinality *= dim.length;

            self.row
                .data
                .extend_from_slice(&i64::cast_from(dim.lower_bound).to_le_bytes());
            self.row
                .data
                .extend_from_slice(&u64::cast_from(dim.length).to_le_bytes());
        }

        if num_dims > MAX_ARRAY_DIMENSIONS {
            // Reset the packer state so we don't have invalid data.
            self.row.data.truncate(start);
            return Err(InvalidArrayError::TooManyDimensions(usize::from(num_dims)));
        }
        // Fix up our dimension length.
        self.row.data[dims_start..dims_start + size_of::<u8>()]
            .copy_from_slice(&num_dims.to_le_bytes());

        // Write elements.
        let off = self.row.data.len();
        self.row.data.extend_from_slice(&[0; size_of::<u64>()]);

        let nelements = f(self);

        let len = u64::cast_from(self.row.data.len() - off - size_of::<u64>());
        self.row.data[off..off + size_of::<u64>()].copy_from_slice(&len.to_le_bytes());

        // Check that the number of elements written matches the dimension
        // information.
        let cardinality = match num_dims {
            0 => 0,
            _ => cardinality,
        };
        if nelements != cardinality {
            self.row.data.truncate(start);
            return Err(InvalidArrayError::WrongCardinality {
                actual: nelements,
                expected: cardinality,
            });
        }

        Ok(())
    }

    /// Convenience function to push a `DatumList` from an iter of `Datum`s
    ///
    /// See [`RowPacker::push_dict_with`] if you need to be able to handle errors
    pub fn push_list<'a, I, D>(&mut self, iter: I)
    where
        I: IntoIterator<Item = D>,
        D: Borrow<Datum<'a>>,
    {
        self.push_list_with(|packer| {
            for elem in iter {
                packer.push(*elem.borrow())
            }
        });
    }

    /// Convenience function to push a `DatumMap` from an iter of `(&str, Datum)` pairs
    pub fn push_dict<'a, I, D>(&mut self, iter: I)
    where
        I: IntoIterator<Item = (&'a str, D)>,
        D: Borrow<Datum<'a>>,
    {
        self.push_dict_with(|packer| {
            for (k, v) in iter {
                packer.push(Datum::String(k));
                packer.push(*v.borrow())
            }
        })
    }

    /// Pushes a `Datum::Range` derived from the `Range<Datum<'a>`.
    ///
    /// # Panics
    /// - If lower and upper express finite values and they are datums of
    ///   different types.
    /// - If lower or upper express finite values and are equal to
    ///   `Datum::Null`. To handle `Datum::Null` properly, use
    ///   [`RangeBound::new`].
    ///
    /// # Notes
    /// - This function canonicalizes the range before pushing it to the row.
    /// - Prefer this function over `push_range_with` because of its
    ///   canonicaliztion.
    /// - Prefer creating [`RangeBound`]s using [`RangeBound::new`], which
    ///   handles `Datum::Null` in a SQL-friendly way.
    pub fn push_range<'a>(&mut self, mut range: Range<Datum<'a>>) -> Result<(), InvalidRangeError> {
        range.canonicalize()?;
        match range.inner {
            None => {
                self.row.data.push(Tag::Range.into());
                // Untagged bytes only contains the `RANGE_EMPTY` flag value.
                self.row.data.push(range::InternalFlags::EMPTY.bits());
                Ok(())
            }
            Some(inner) => self.push_range_with(
                RangeLowerBound {
                    inclusive: inner.lower.inclusive,
                    bound: inner
                        .lower
                        .bound
                        .map(|value| move |row: &mut RowPacker| Ok(row.push(value))),
                },
                RangeUpperBound {
                    inclusive: inner.upper.inclusive,
                    bound: inner
                        .upper
                        .bound
                        .map(|value| move |row: &mut RowPacker| Ok(row.push(value))),
                },
            ),
        }
    }

    /// Pushes a `DatumRange` built from the specified arguments.
    ///
    /// # Warning
    /// Unlike `push_range`, `push_range_with` _does not_ canonicalize its
    /// inputs. Consequentially, this means it's possible to generate ranges
    /// that will not reflect the proper ordering and equality.
    ///
    /// # Panics
    /// - If lower or upper expresses a finite value and does not push exactly
    ///   one value into the `RowPacker`.
    /// - If lower and upper express finite values and they are datums of
    ///   different types.
    /// - If lower or upper express finite values and push `Datum::Null`.
    ///
    /// # Notes
    /// - Prefer `push_range_with` over this function. This function should be
    ///   used only when you are not pushing `Datum`s to the inner row.
    /// - Range encoding is `[<flag bytes>,<lower>?,<upper>?]`, where `lower`
    ///   and `upper` are optional, contingent on the flag value expressing an
    ///   empty range (where neither will be present) or infinite bounds (where
    ///   each infinite bound will be absent).
    /// - To push an emtpy range, use `push_range` using `Range { inner: None }`.
    pub fn push_range_with<L, U, E>(
        &mut self,
        lower: RangeLowerBound<L>,
        upper: RangeUpperBound<U>,
    ) -> Result<(), E>
    where
        L: FnOnce(&mut RowPacker) -> Result<(), E>,
        U: FnOnce(&mut RowPacker) -> Result<(), E>,
        E: From<InvalidRangeError>,
    {
        let start = self.row.data.len();
        self.row.data.push(Tag::Range.into());

        let mut flags = range::InternalFlags::empty();

        flags.set(range::InternalFlags::LB_INFINITE, lower.bound.is_none());
        flags.set(range::InternalFlags::UB_INFINITE, upper.bound.is_none());
        flags.set(range::InternalFlags::LB_INCLUSIVE, lower.inclusive);
        flags.set(range::InternalFlags::UB_INCLUSIVE, upper.inclusive);

        let mut expected_datums = 0;

        self.row.data.push(flags.bits());

        let datum_check = self.row.data.len();

        if let Some(value) = lower.bound {
            let start = self.row.data.len();
            value(self)?;
            assert!(
                start < self.row.data.len(),
                "finite values must each push exactly one value; expected 1 but got 0"
            );
            expected_datums += 1;
        }

        if let Some(value) = upper.bound {
            let start = self.row.data.len();
            value(self)?;
            assert!(
                start < self.row.data.len(),
                "finite values must each push exactly one value; expected 1 but got 0"
            );
            expected_datums += 1;
        }

        // Validate the invariants that 0, 1, or 2 elements were pushed, none are Null,
        // and if two are pushed then the second is not less than the first. Panic in
        // some cases and error in others.
        let mut actual_datums = 0;
        let mut seen = None;
        let mut dataz = &self.row.data[datum_check..];
        while !dataz.is_empty() {
            let d = unsafe { read_datum(&mut dataz) };
            assert!(d != Datum::Null, "cannot push Datum::Null into range");

            match seen {
                None => seen = Some(d),
                Some(seen) => {
                    let seen_kind = DatumKind::from(seen);
                    let d_kind = DatumKind::from(d);
                    assert!(
                        seen_kind == d_kind,
                        "range contains inconsistent data; expected {seen_kind:?} but got {d_kind:?}"
                    );

                    if seen > d {
                        self.row.data.truncate(start);
                        return Err(InvalidRangeError::MisorderedRangeBounds.into());
                    }
                }
            }
            actual_datums += 1;
        }

        assert!(
            actual_datums == expected_datums,
            "finite values must each push exactly one value; expected {expected_datums} but got {actual_datums}"
        );

        Ok(())
    }

    /// Clears the contents of the packer without de-allocating its backing memory.
    pub fn clear(&mut self) {
        self.row.data.clear();
    }

    /// Truncates the underlying storage to the specified byte position.
    ///
    /// # Safety
    ///
    /// `pos` MUST specify a byte offset that lies on a datum boundary.
    /// If `pos` specifies a byte offset that is *within* a datum, the row
    /// packer will produce an invalid row, the unpacking of which may
    /// trigger undefined behavior!
    ///
    /// To find the byte offset of a datum boundary, inspect the packer's
    /// byte length by calling `packer.data().len()` after pushing the desired
    /// number of datums onto the packer.
    pub unsafe fn truncate(&mut self, pos: usize) {
        self.row.data.truncate(pos)
    }

    /// Truncates the underlying row to contain at most the first `n` datums.
    pub fn truncate_datums(&mut self, n: usize) {
        let prev_len = self.row.data.len();
        let mut iter = self.row.iter();
        for _ in iter.by_ref().take(n) {}
        let next_len = iter.data.len();
        // SAFETY: iterator offsets always lie on a datum boundary.
        unsafe { self.truncate(prev_len - next_len) }
    }

    /// Returns the total amount of bytes used by the underlying row.
    pub fn byte_len(&self) -> usize {
        self.row.byte_len()
    }
}

impl<'a> IntoIterator for &'a Row {
    type Item = Datum<'a>;
    type IntoIter = DatumListIter<'a>;
    fn into_iter(self) -> DatumListIter<'a> {
        self.iter()
    }
}

impl fmt::Debug for Row {
    /// Debug representation using the internal datums
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("Row{")?;
        f.debug_list().entries(self.iter()).finish()?;
        f.write_str("}")
    }
}

impl fmt::Display for Row {
    /// Display representation using the internal datums
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("(")?;
        for (i, datum) in self.iter().enumerate() {
            if i != 0 {
                f.write_str(", ")?;
            }
            write!(f, "{}", datum)?;
        }
        f.write_str(")")
    }
}

impl<'a> DatumList<'a> {
    pub fn empty() -> DatumList<'static> {
        DatumList { data: &[] }
    }

    pub fn iter(&self) -> DatumListIter<'a> {
        DatumListIter { data: self.data }
    }

    /// For debugging only
    pub fn data(&self) -> &'a [u8] {
        self.data
    }
}

impl<'a> IntoIterator for &'a DatumList<'a> {
    type Item = Datum<'a>;
    type IntoIter = DatumListIter<'a>;
    fn into_iter(self) -> DatumListIter<'a> {
        self.iter()
    }
}

impl<'a> Iterator for DatumListIter<'a> {
    type Item = Datum<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.data.is_empty() {
            None
        } else {
            Some(unsafe { read_datum(&mut self.data) })
        }
    }
}

impl<'a> DatumMap<'a> {
    pub fn empty() -> DatumMap<'static> {
        DatumMap { data: &[] }
    }

    pub fn iter(&self) -> DatumDictIter<'a> {
        DatumDictIter {
            data: self.data,
            prev_key: None,
        }
    }

    /// For debugging only
    pub fn data(&self) -> &'a [u8] {
        self.data
    }
}

impl<'a> Debug for DatumMap<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<'a> IntoIterator for &'a DatumMap<'a> {
    type Item = (&'a str, Datum<'a>);
    type IntoIter = DatumDictIter<'a>;
    fn into_iter(self) -> DatumDictIter<'a> {
        self.iter()
    }
}

impl<'a> Iterator for DatumDictIter<'a> {
    type Item = (&'a str, Datum<'a>);
    fn next(&mut self) -> Option<Self::Item> {
        if self.data.is_empty() {
            None
        } else {
            let key_tag =
                Tag::try_from_primitive(read_byte(&mut self.data)).expect("unknown row tag");
            assert!(
                key_tag == Tag::StringTiny
                    || key_tag == Tag::StringShort
                    || key_tag == Tag::StringLong
                    || key_tag == Tag::StringHuge,
                "Dict keys must be strings, got {:?}",
                key_tag
            );
            let key = unsafe { read_lengthed_datum(&mut self.data, key_tag).unwrap_str() };
            let val = unsafe { read_datum(&mut self.data) };

            // if in debug mode, sanity check keys
            if cfg!(debug_assertions) {
                if let Some(prev_key) = self.prev_key {
                    debug_assert!(
                        prev_key < key,
                        "Dict keys must be unique and given in ascending order: {} came before {}",
                        prev_key,
                        key
                    );
                }
                self.prev_key = Some(key);
            }

            Some((key, val))
        }
    }
}

impl RowArena {
    pub fn new() -> Self {
        RowArena {
            inner: RefCell::new(vec![]),
        }
    }

    /// Creates a `RowArena` with a hint of how many rows will be created in the arena, to avoid
    /// reallocations of its internal vector.
    pub fn with_capacity(capacity: usize) -> Self {
        RowArena {
            inner: RefCell::new(Vec::with_capacity(capacity)),
        }
    }

    /// Does a `reserve` on the underlying `Vec`. Call this when you expect `additional` more datums
    /// to be created in this arena.
    pub fn reserve(&self, additional: usize) {
        self.inner.borrow_mut().reserve(additional);
    }

    /// Take ownership of `bytes` for the lifetime of the arena.
    #[allow(clippy::transmute_ptr_to_ptr)]
    pub fn push_bytes<'a>(&'a self, bytes: Vec<u8>) -> &'a [u8] {
        let mut inner = self.inner.borrow_mut();
        inner.push(bytes);
        let owned_bytes = &inner[inner.len() - 1];
        unsafe {
            // This is safe because:
            //   * We only ever append to self.inner, so the byte vector
            //     will live as long as the arena.
            //   * We return a reference to the byte vector's contents, so it's
            //     okay if self.inner reallocates and moves the byte
            //     vector.
            //   * We don't allow access to the byte vector itself, so it will
            //     never reallocate.
            transmute::<&[u8], &'a [u8]>(owned_bytes)
        }
    }

    /// Take ownership of `string` for the lifetime of the arena.
    pub fn push_string<'a>(&'a self, string: String) -> &'a str {
        let owned_bytes = self.push_bytes(string.into_bytes());
        unsafe {
            // This is safe because we know it was a `String` just before.
            std::str::from_utf8_unchecked(owned_bytes)
        }
    }

    /// Take ownership of `row` for the lifetime of the arena, returning a
    /// reference to the first datum in the row.
    ///
    /// If we had an owned datum type, this method would be much clearer, and
    /// would be called `push_owned_datum`.
    pub fn push_unary_row<'a>(&'a self, row: Row) -> Datum<'a> {
        let mut inner = self.inner.borrow_mut();
        inner.push(row.data.into_vec());
        unsafe {
            // This is safe because:
            //   * We only ever append to self.inner, so the row data will live
            //     as long as the arena.
            //   * We force the row data into its own heap allocation--
            //     importantly, we do NOT store the SmallVec, which might be
            //     storing data inline--so it's okay if self.inner reallocates
            //     and moves the row.
            //   * We don't allow access to the byte vector itself, so it will
            //     never reallocate.
            let datum = read_datum(&mut &inner[inner.len() - 1][..]);
            transmute::<Datum<'_>, Datum<'a>>(datum)
        }
    }

    /// Equivalent to `push_unary_row` but returns a `DatumNested` rather than a
    /// `Datum`.
    fn push_unary_row_datum_nested<'a>(&'a self, row: Row) -> DatumNested<'a> {
        let mut inner = self.inner.borrow_mut();
        inner.push(row.data.into_vec());
        unsafe {
            // This is safe because:
            //   * We only ever append to self.inner, so the row data will live
            //     as long as the arena.
            //   * We force the row data into its own heap allocation--
            //     importantly, we do NOT store the SmallVec, which might be
            //     storing data inline--so it's okay if self.inner reallocates
            //     and moves the row.
            //   * We don't allow access to the byte vector itself, so it will
            //     never reallocate.
            let nested = DatumNested::extract(&mut &inner[inner.len() - 1][..]);
            transmute::<DatumNested<'_>, DatumNested<'a>>(nested)
        }
    }

    /// Convenience function to make a new `Row` containing a single datum, and
    /// take ownership of it for the lifetime of the arena
    ///
    /// ```
    /// # use mz_repr::{RowArena, Datum};
    /// let arena = RowArena::new();
    /// let datum = arena.make_datum(|packer| {
    ///   packer.push_list(&[Datum::String("hello"), Datum::String("world")]);
    /// });
    /// assert_eq!(datum.unwrap_list().iter().collect::<Vec<_>>(), vec![Datum::String("hello"), Datum::String("world")]);
    /// ```
    pub fn make_datum<'a, F>(&'a self, f: F) -> Datum<'a>
    where
        F: FnOnce(&mut RowPacker),
    {
        let mut row = Row::default();
        f(&mut row.packer());
        self.push_unary_row(row)
    }

    /// Convenience function identical to `make_datum` but instead returns a
    /// `DatumNested`.
    pub fn make_datum_nested<'a, F>(&'a self, f: F) -> DatumNested<'a>
    where
        F: FnOnce(&mut RowPacker),
    {
        let mut row = Row::default();
        f(&mut row.packer());
        self.push_unary_row_datum_nested(row)
    }

    /// Like [`RowArena::make_datum`], but the provided closure can return an error.
    pub fn try_make_datum<'a, F, E>(&'a self, f: F) -> Result<Datum<'a>, E>
    where
        F: FnOnce(&mut RowPacker) -> Result<(), E>,
    {
        let mut row = Row::default();
        f(&mut row.packer())?;
        Ok(self.push_unary_row(row))
    }

    /// Clear the contents of the arena.
    pub fn clear(&mut self) {
        self.inner.borrow_mut().clear();
    }
}

impl Default for RowArena {
    fn default() -> RowArena {
        RowArena::new()
    }
}

/// A thread-local row, which can be borrowed and returned.
/// # Example
///
/// Use this type instead of creating a new row:
/// ```
/// use mz_repr::SharedRow;
///
/// let mut row_builder = SharedRow::get();
/// ```
///
/// This allows us to reuse an existing row allocation instead of creating a new one or retaining
/// an allocation locally. Additionally, we can observe the size of the local row in a central
/// place and potentially reallocate to reduce memory needs.
///
/// # Panic
///
/// [`SharedRow::get`] panics when trying to obtain multiple references to the shared row.
#[derive(Debug)]
pub struct SharedRow(Row);

impl SharedRow {
    thread_local! {
        /// A thread-local slot containing a shared Row that can be temporarily used by a function.
        /// There can be at most one active user of this Row, which is tracked by the state of the
        /// `Option<_>` wrapper. When it is `Some(..)`, the row is available for using. When it
        /// is `None`, it is not, and the constructor will panic if a thread attempts to use it.
        static SHARED_ROW: Cell<Option<Row>> = const { Cell::new(Some(Row::empty())) }
    }

    /// Get the shared row.
    ///
    /// The row's contents are cleared before returning it.
    ///
    /// # Panic
    ///
    /// Panics when the row is already borrowed elsewhere.
    pub fn get() -> Self {
        let mut row = Self::SHARED_ROW
            .take()
            .expect("attempted to borrow already borrowed SharedRow");
        // Clear row
        row.packer();
        Self(row)
    }

    /// Gets the shared row and uses it to pack `iter`.
    pub fn pack<'a, I, D>(iter: I) -> Row
    where
        I: IntoIterator<Item = D>,
        D: Borrow<Datum<'a>>,
    {
        let mut row_builder = Self::get();
        let mut row_packer = row_builder.packer();
        row_packer.extend(iter);
        row_builder.clone()
    }
}

impl std::ops::Deref for SharedRow {
    type Target = Row;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for SharedRow {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for SharedRow {
    fn drop(&mut self) {
        // Take the Row allocation from this instance and put it back in the thread local slot for
        // the next user. The Row in `self` is replaced with an empty Row which does not allocate.
        Self::SHARED_ROW.set(Some(std::mem::take(&mut self.0)))
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, NaiveDate};
    use mz_ore::{assert_err, assert_none};

    use crate::ScalarType;

    use super::*;

    #[mz_ore::test]
    fn test_assumptions() {
        assert_eq!(size_of::<Tag>(), 1);
        #[cfg(target_endian = "big")]
        {
            // if you want to run this on a big-endian cpu, we'll need big-endian versions of the serialization code
            assert!(false);
        }
    }

    #[mz_ore::test]
    fn miri_test_arena() {
        let arena = RowArena::new();

        assert_eq!(arena.push_string("".to_owned()), "");
        assert_eq!(arena.push_string("العَرَبِيَّة".to_owned()), "العَرَبِيَّة");

        let empty: &[u8] = &[];
        assert_eq!(arena.push_bytes(vec![]), empty);
        assert_eq!(arena.push_bytes(vec![0, 2, 1, 255]), &[0, 2, 1, 255]);

        let mut row = Row::default();
        let mut packer = row.packer();
        packer.push_dict_with(|row| {
            row.push(Datum::String("a"));
            row.push_list_with(|row| {
                row.push(Datum::String("one"));
                row.push(Datum::String("two"));
                row.push(Datum::String("three"));
            });
            row.push(Datum::String("b"));
            row.push(Datum::String("c"));
        });
        assert_eq!(arena.push_unary_row(row.clone()), row.unpack_first());
    }

    #[mz_ore::test]
    fn miri_test_round_trip() {
        fn round_trip(datums: Vec<Datum>) {
            let row = Row::pack(datums.clone());

            // When run under miri this catches undefined bytes written to data
            // eg by calling push_copy! on a type which contains undefined padding values
            println!("{:?}", row.data());

            let datums2 = row.iter().collect::<Vec<_>>();
            let datums3 = row.unpack();
            assert_eq!(datums, datums2);
            assert_eq!(datums, datums3);
        }

        round_trip(vec![]);
        round_trip(
            ScalarType::enumerate()
                .iter()
                .flat_map(|r#type| r#type.interesting_datums())
                .collect(),
        );
        round_trip(vec![
            Datum::Null,
            Datum::Null,
            Datum::False,
            Datum::True,
            Datum::Int16(-21),
            Datum::Int32(-42),
            Datum::Int64(-2_147_483_648 - 42),
            Datum::UInt8(0),
            Datum::UInt8(1),
            Datum::UInt16(0),
            Datum::UInt16(1),
            Datum::UInt16(1 << 8),
            Datum::UInt32(0),
            Datum::UInt32(1),
            Datum::UInt32(1 << 8),
            Datum::UInt32(1 << 16),
            Datum::UInt32(1 << 24),
            Datum::UInt64(0),
            Datum::UInt64(1),
            Datum::UInt64(1 << 8),
            Datum::UInt64(1 << 16),
            Datum::UInt64(1 << 24),
            Datum::UInt64(1 << 32),
            Datum::UInt64(1 << 40),
            Datum::UInt64(1 << 48),
            Datum::UInt64(1 << 56),
            Datum::Float32(OrderedFloat::from(-42.12)),
            Datum::Float64(OrderedFloat::from(-2_147_483_648.0 - 42.12)),
            Datum::Date(Date::from_pg_epoch(365 * 45 + 21).unwrap()),
            Datum::Timestamp(
                CheckedTimestamp::from_timestamplike(
                    NaiveDate::from_isoywd_opt(2019, 30, chrono::Weekday::Wed)
                        .unwrap()
                        .and_hms_opt(14, 32, 11)
                        .unwrap(),
                )
                .unwrap(),
            ),
            Datum::TimestampTz(
                CheckedTimestamp::from_timestamplike(DateTime::from_timestamp(61, 0).unwrap())
                    .unwrap(),
            ),
            Datum::Interval(Interval {
                months: 312,
                ..Default::default()
            }),
            Datum::Interval(Interval::new(0, 0, 1_012_312)),
            Datum::Bytes(&[]),
            Datum::Bytes(&[0, 2, 1, 255]),
            Datum::String(""),
            Datum::String("العَرَبِيَّة"),
        ]);
    }

    #[mz_ore::test]
    fn test_array() {
        // Construct an array using `Row::push_array` and verify that it unpacks
        // correctly.
        const DIM: ArrayDimension = ArrayDimension {
            lower_bound: 2,
            length: 2,
        };
        let mut row = Row::default();
        let mut packer = row.packer();
        packer
            .try_push_array(&[DIM], vec![Datum::Int32(1), Datum::Int32(2)])
            .unwrap();
        let arr1 = row.unpack_first().unwrap_array();
        assert_eq!(arr1.dims().into_iter().collect::<Vec<_>>(), vec![DIM]);
        assert_eq!(
            arr1.elements().into_iter().collect::<Vec<_>>(),
            vec![Datum::Int32(1), Datum::Int32(2)]
        );

        // Pack a previously-constructed `Datum::Array` and verify that it
        // unpacks correctly.
        let row = Row::pack_slice(&[Datum::Array(arr1)]);
        let arr2 = row.unpack_first().unwrap_array();
        assert_eq!(arr1, arr2);
    }

    #[mz_ore::test]
    fn test_multidimensional_array() {
        let datums = vec![
            Datum::Int32(1),
            Datum::Int32(2),
            Datum::Int32(3),
            Datum::Int32(4),
            Datum::Int32(5),
            Datum::Int32(6),
            Datum::Int32(7),
            Datum::Int32(8),
        ];

        let mut row = Row::default();
        let mut packer = row.packer();
        packer
            .try_push_array(
                &[
                    ArrayDimension {
                        lower_bound: 1,
                        length: 1,
                    },
                    ArrayDimension {
                        lower_bound: 1,
                        length: 4,
                    },
                    ArrayDimension {
                        lower_bound: 1,
                        length: 2,
                    },
                ],
                &datums,
            )
            .unwrap();
        let array = row.unpack_first().unwrap_array();
        assert_eq!(array.elements().into_iter().collect::<Vec<_>>(), datums);
    }

    #[mz_ore::test]
    fn test_array_max_dimensions() {
        let mut row = Row::default();
        let max_dims = usize::from(MAX_ARRAY_DIMENSIONS);

        // An array with one too many dimensions should be rejected.
        let res = row.packer().try_push_array(
            &vec![
                ArrayDimension {
                    lower_bound: 1,
                    length: 1
                };
                max_dims + 1
            ],
            vec![Datum::Int32(4)],
        );
        assert_eq!(res, Err(InvalidArrayError::TooManyDimensions(max_dims + 1)));
        assert!(row.data.is_empty());

        // An array with exactly the maximum allowable dimensions should be
        // accepted.
        row.packer()
            .try_push_array(
                &vec![
                    ArrayDimension {
                        lower_bound: 1,
                        length: 1
                    };
                    max_dims
                ],
                vec![Datum::Int32(4)],
            )
            .unwrap();
    }

    #[mz_ore::test]
    fn test_array_wrong_cardinality() {
        let mut row = Row::default();
        let res = row.packer().try_push_array(
            &[
                ArrayDimension {
                    lower_bound: 1,
                    length: 2,
                },
                ArrayDimension {
                    lower_bound: 1,
                    length: 3,
                },
            ],
            vec![Datum::Int32(1), Datum::Int32(2)],
        );
        assert_eq!(
            res,
            Err(InvalidArrayError::WrongCardinality {
                actual: 2,
                expected: 6,
            })
        );
        assert!(row.data.is_empty());
    }

    #[mz_ore::test]
    fn test_nesting() {
        let mut row = Row::default();
        row.packer().push_dict_with(|row| {
            row.push(Datum::String("favourites"));
            row.push_list_with(|row| {
                row.push(Datum::String("ice cream"));
                row.push(Datum::String("oreos"));
                row.push(Datum::String("cheesecake"));
            });
            row.push(Datum::String("name"));
            row.push(Datum::String("bob"));
        });

        let mut iter = row.unpack_first().unwrap_map().iter();

        let (k, v) = iter.next().unwrap();
        assert_eq!(k, "favourites");
        assert_eq!(
            v.unwrap_list().iter().collect::<Vec<_>>(),
            vec![
                Datum::String("ice cream"),
                Datum::String("oreos"),
                Datum::String("cheesecake"),
            ]
        );

        let (k, v) = iter.next().unwrap();
        assert_eq!(k, "name");
        assert_eq!(v, Datum::String("bob"));
    }

    #[mz_ore::test]
    fn test_dict_errors() -> Result<(), Box<dyn std::error::Error>> {
        let pack = |ok| {
            let mut row = Row::default();
            row.packer().push_dict_with(|row| {
                if ok {
                    row.push(Datum::String("key"));
                    row.push(Datum::Int32(42));
                    Ok(7)
                } else {
                    Err("fail")
                }
            })?;
            Ok(row)
        };

        assert_eq!(pack(false), Err("fail"));

        let row = pack(true)?;
        let mut dict = row.unpack_first().unwrap_map().iter();
        assert_eq!(dict.next(), Some(("key", Datum::Int32(42))));
        assert_eq!(dict.next(), None);

        Ok(())
    }

    #[mz_ore::test]
    #[cfg_attr(miri, ignore)] // unsupported operation: can't call foreign function `decNumberFromInt32` on OS `linux`
    fn test_datum_sizes() {
        let arena = RowArena::new();

        // Test the claims about various datum sizes.
        let values_of_interest = vec![
            Datum::Null,
            Datum::False,
            Datum::Int16(0),
            Datum::Int32(0),
            Datum::Int64(0),
            Datum::UInt8(0),
            Datum::UInt8(1),
            Datum::UInt16(0),
            Datum::UInt16(1),
            Datum::UInt16(1 << 8),
            Datum::UInt32(0),
            Datum::UInt32(1),
            Datum::UInt32(1 << 8),
            Datum::UInt32(1 << 16),
            Datum::UInt32(1 << 24),
            Datum::UInt64(0),
            Datum::UInt64(1),
            Datum::UInt64(1 << 8),
            Datum::UInt64(1 << 16),
            Datum::UInt64(1 << 24),
            Datum::UInt64(1 << 32),
            Datum::UInt64(1 << 40),
            Datum::UInt64(1 << 48),
            Datum::UInt64(1 << 56),
            Datum::Float32(OrderedFloat(0.0)),
            Datum::Float64(OrderedFloat(0.0)),
            Datum::from(numeric::Numeric::from(0)),
            Datum::from(numeric::Numeric::from(1000)),
            Datum::from(numeric::Numeric::from(9999)),
            Datum::Date(
                NaiveDate::from_ymd_opt(1, 1, 1)
                    .unwrap()
                    .try_into()
                    .unwrap(),
            ),
            Datum::Timestamp(
                CheckedTimestamp::from_timestamplike(
                    DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
                )
                .unwrap(),
            ),
            Datum::TimestampTz(
                CheckedTimestamp::from_timestamplike(DateTime::from_timestamp(0, 0).unwrap())
                    .unwrap(),
            ),
            Datum::Interval(Interval::default()),
            Datum::Bytes(&[]),
            Datum::String(""),
            Datum::JsonNull,
            Datum::Range(Range { inner: None }),
            arena.make_datum(|packer| {
                packer
                    .push_range(Range::new(Some((
                        RangeLowerBound::new(Datum::Int32(-1), true),
                        RangeUpperBound::new(Datum::Int32(1), true),
                    ))))
                    .unwrap();
            }),
        ];
        for value in values_of_interest {
            if datum_size(&value) != Row::pack_slice(&[value]).data.len() {
                panic!("Disparity in claimed size for {:?}", value);
            }
        }
    }

    #[mz_ore::test]
    fn test_range_errors() {
        fn test_range_errors_inner<'a>(
            datums: Vec<Vec<Datum<'a>>>,
        ) -> Result<(), InvalidRangeError> {
            let mut row = Row::default();
            let row_len = row.byte_len();
            let mut packer = row.packer();
            let r = packer.push_range_with(
                RangeLowerBound {
                    inclusive: true,
                    bound: Some(|row: &mut RowPacker| {
                        for d in &datums[0] {
                            row.push(d);
                        }
                        Ok(())
                    }),
                },
                RangeUpperBound {
                    inclusive: true,
                    bound: Some(|row: &mut RowPacker| {
                        for d in &datums[1] {
                            row.push(d);
                        }
                        Ok(())
                    }),
                },
            );

            assert_eq!(row_len, row.byte_len());

            r
        }

        for panicking_case in [
            vec![vec![Datum::Int32(1)], vec![]],
            vec![
                vec![Datum::Int32(1), Datum::Int32(2)],
                vec![Datum::Int32(3)],
            ],
            vec![
                vec![Datum::Int32(1)],
                vec![Datum::Int32(2), Datum::Int32(3)],
            ],
            vec![vec![Datum::Int32(1), Datum::Int32(2)], vec![]],
            vec![vec![Datum::Int32(1)], vec![Datum::UInt16(2)]],
            vec![vec![Datum::Null], vec![Datum::Int32(2)]],
            vec![vec![Datum::Int32(1)], vec![Datum::Null]],
        ] {
            #[allow(clippy::disallowed_methods)] // not using enhanced panic handler in tests
            let result = std::panic::catch_unwind(|| test_range_errors_inner(panicking_case));
            assert_err!(result);
        }

        let e = test_range_errors_inner(vec![vec![Datum::Int32(2)], vec![Datum::Int32(1)]]);
        assert_eq!(e, Err(InvalidRangeError::MisorderedRangeBounds));
    }

    /// Lists have a variable-length encoding for their lengths. We test each case here.
    #[mz_ore::test]
    #[cfg_attr(miri, ignore)] // slow
    fn test_list_encoding() {
        fn test_list_encoding_inner(len: usize) {
            let list_elem = |i: usize| {
                if i % 2 == 0 {
                    Datum::False
                } else {
                    Datum::True
                }
            };
            let mut row = Row::default();
            {
                // Push some stuff.
                let mut packer = row.packer();
                packer.push(Datum::String("start"));
                packer.push_list_with(|packer| {
                    for i in 0..len {
                        packer.push(list_elem(i));
                    }
                });
                packer.push(Datum::String("end"));
            }
            // Check that we read back exactly what we pushed.
            let mut row_it = row.iter();
            assert_eq!(row_it.next().unwrap(), Datum::String("start"));
            match row_it.next().unwrap() {
                Datum::List(list) => {
                    let mut list_it = list.iter();
                    for i in 0..len {
                        assert_eq!(list_it.next().unwrap(), list_elem(i));
                    }
                    assert_none!(list_it.next());
                }
                _ => panic!("expected Datum::List"),
            }
            assert_eq!(row_it.next().unwrap(), Datum::String("end"));
            assert_none!(row_it.next());
        }

        test_list_encoding_inner(0);
        test_list_encoding_inner(1);
        test_list_encoding_inner(10);
        test_list_encoding_inner(TINY - 1); // tiny
        test_list_encoding_inner(TINY + 1); // short
        test_list_encoding_inner(SHORT + 1); // long

        // The biggest one takes 40 s on my laptop, probably not worth it.
        //test_list_encoding_inner(LONG + 1); // huge
    }
}
