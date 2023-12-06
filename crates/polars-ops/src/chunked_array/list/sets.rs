use std::fmt::{Display, Formatter};
use std::hash::Hash;
use std::ops::Not;

use arrow::array::{
    BinaryArray, BooleanArray, ListArray, MutableArray, MutableBinaryArray, MutableBooleanArray,
    MutablePrimitiveArray, PrimitiveArray, Utf8Array,
};
use arrow::bitmap::Bitmap;
use arrow::compute::utils::combine_validities_and;
use arrow::offset::OffsetsBuffer;
use arrow::types::NativeType;
use either::Either;
use polars_core::prelude::*;
use polars_core::with_match_physical_integer_type;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

trait MaterializeValues<K> {
    // extends the iterator to the values and returns the current offset
    fn extend_buf<I: Iterator<Item = K>>(&mut self, values: I) -> usize;
}

impl<T> MaterializeValues<Option<T>> for MutablePrimitiveArray<T>
where
    T: NativeType,
{
    fn extend_buf<I: Iterator<Item = Option<T>>>(&mut self, values: I) -> usize {
        self.extend(values);
        self.len()
    }
}

impl<'a> MaterializeValues<Option<&'a [u8]>> for MutableBinaryArray<i64> {
    fn extend_buf<I: Iterator<Item = Option<&'a [u8]>>>(&mut self, values: I) -> usize {
        self.extend(values);
        self.len()
    }
}

impl MaterializeValues<bool> for MutableBooleanArray {
    fn extend_buf<I: Iterator<Item = bool>>(&mut self, values: I) -> usize {
        self.extend(values.map(Some));
        self.len()
    }
}

fn set_operation<K, I, J, R, B>(
    set: &mut PlIndexSet<K>,
    set2: &mut PlIndexSet<K>,
    a: I,
    b: J,
    out: &mut R,
    bool_out: &mut B,
    set_op: SetOperation,
    broadcast_rhs: bool,
) -> usize
where
    K: Eq + Hash + Copy,
    I: IntoIterator<Item = K>,
    J: IntoIterator<Item = K>,
    R: MaterializeValues<K>,
    B: MaterializeValues<bool>,
{
    set.clear();
    let a = a.into_iter();
    let b = b.into_iter();

    match set_op {
        SetOperation::Intersection => {
            set.extend(a);
            // If broadcast `set2` should already be filled.
            if !broadcast_rhs {
                set2.clear();
                set2.extend(b);
            }
            out.extend_buf(set.intersection(set2).copied())
        },
        SetOperation::Union => {
            set.extend(a);
            set.extend(b);
            out.extend_buf(set.drain(..))
        },
        SetOperation::Difference => {
            set.extend(a);
            for v in b {
                set.remove(&v);
            }
            out.extend_buf(set.drain(..))
        },
        SetOperation::SymmetricDifference => {
            // If broadcast `set2` should already be filled.
            if !broadcast_rhs {
                set2.clear();
                set2.extend(b);
            }
            // We could speed this up, but implementing ourselves, but we need to have a clonable
            // iterator as we need 2 passes
            set.extend(a);
            out.extend_buf(set.symmetric_difference(set2).copied())
        },
        SetOperation::IsDisjoint => {
            set.extend(a);
            // If broadcast `set2` should already be filled.
            if !broadcast_rhs {
                set2.clear();
                set2.extend(b);
            }

            bool_out.extend_buf([set.is_disjoint(set2)].into_iter())
        },
        SetOperation::IsSubset => {
            set.extend(a);
            // If broadcast `set2` should already be filled.
            if !broadcast_rhs {
                set2.clear();
                set2.extend(b);
            }
            bool_out.extend_buf([set.is_subset(set2)].into_iter())
        },
        SetOperation::IsSuperset => {
            set.extend(a);
            // If broadcast `set2` should already be filled.
            if !broadcast_rhs {
                set2.clear();
                set2.extend(b);
            }
            bool_out.extend_buf([set.is_superset(set2)].into_iter())
        },
    }
}

fn copied_opt<T: Copy>(v: Option<&T>) -> Option<T> {
    v.copied()
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum SetOperation {
    Intersection,
    Union,
    Difference,
    SymmetricDifference,
    IsDisjoint,
    IsSubset,
    IsSuperset,
}

impl SetOperation {
    pub fn is_boolean(&self) -> bool {
        match self {
            SetOperation::IsDisjoint | SetOperation::IsSubset | SetOperation::IsSuperset => true,
            _ => false,
        }
    }
}

impl Display for SetOperation {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            SetOperation::Intersection => "intersection",
            SetOperation::Union => "union",
            SetOperation::Difference => "difference",
            SetOperation::SymmetricDifference => "symmetric_difference",
            SetOperation::IsDisjoint => "is_disjoint",
            SetOperation::IsSubset => "is_subset",
            SetOperation::IsSuperset => "is_superset",
        };
        write!(f, "{s}")
    }
}

fn primitive<T>(
    a: &PrimitiveArray<T>,
    b: &PrimitiveArray<T>,
    offsets_a: &[i64],
    offsets_b: &[i64],
    set_op: SetOperation,
    validity: Option<Bitmap>,
) -> PolarsResult<Either<ListArray<i64>, BooleanArray>>
where
    T: NativeType + Hash + Copy + Eq + Not,
{
    let broadcast_lhs = offsets_a.len() == 2;
    let broadcast_rhs = offsets_b.len() == 2;

    let mut set = Default::default();
    let mut set2: PlIndexSet<Option<T>> = Default::default();

    let mut values_out = MutablePrimitiveArray::new();
    let mut bool_values_out = MutableBooleanArray::new();

    if set_op.is_boolean() {
        bool_values_out
            .reserve(std::cmp::max(*offsets_a.last().unwrap(), *offsets_b.last().unwrap()) as usize)
    } else {
        values_out
            .reserve(std::cmp::max(*offsets_a.last().unwrap(), *offsets_b.last().unwrap()) as usize)
    }

    let mut offsets = Vec::with_capacity(std::cmp::max(offsets_a.len(), offsets_b.len()));
    offsets.push(0i64);

    if broadcast_rhs {
        set2.extend(b.into_iter().map(copied_opt));
    }
    let offsets_slice = if offsets_a.len() > offsets_b.len() {
        offsets_a
    } else {
        offsets_b
    };
    let first_a = offsets_a[0];
    let second_a = offsets_a[1];
    let first_b = offsets_b[0];
    let second_b = offsets_b[1];
    for i in 1..offsets_slice.len() {
        // If we go OOB we take the first element as we are then broadcasting.
        let start_a = *offsets_a.get(i - 1).unwrap_or(&first_a) as usize;
        let end_a = *offsets_a.get(i).unwrap_or(&second_a) as usize;

        let start_b = *offsets_b.get(i - 1).unwrap_or(&first_b) as usize;
        let end_b = *offsets_b.get(i).unwrap_or(&second_b) as usize;

        // The branches are the same every loop.
        // We rely on branch prediction here.
        let offset = if broadcast_rhs {
            // going via skip iterator instead of slice doesn't heap alloc nor trigger a bitcount
            let a_iter = a
                .into_iter()
                .skip(start_a)
                .take(end_a - start_a)
                .map(copied_opt);
            let b_iter = b.into_iter().map(copied_opt);
            set_operation(
                &mut set,
                &mut set2,
                a_iter,
                b_iter,
                &mut values_out,
                &mut bool_values_out,
                set_op,
                true,
            )
        } else if broadcast_lhs {
            let a_iter = a.into_iter().map(copied_opt);

            let b_iter = b
                .into_iter()
                .skip(start_b)
                .take(end_b - start_b)
                .map(copied_opt);

            set_operation(
                &mut set,
                &mut set2,
                a_iter,
                b_iter,
                &mut values_out,
                &mut bool_values_out,
                set_op,
                false,
            )
        } else {
            // going via skip iterator instead of slice doesn't heap alloc nor trigger a bitcount
            let a_iter = a
                .into_iter()
                .skip(start_a)
                .take(end_a - start_a)
                .map(copied_opt);

            let b_iter = b
                .into_iter()
                .skip(start_b)
                .take(end_b - start_b)
                .map(copied_opt);
            set_operation(
                &mut set,
                &mut set2,
                a_iter,
                b_iter,
                &mut values_out,
                &mut bool_values_out,
                set_op,
                false,
            )
        };

        offsets.push(offset as i64);
    }
    let offsets = unsafe { OffsetsBuffer::new_unchecked(offsets.into()) };
    let dtype = ListArray::<i64>::default_datatype(values_out.data_type().clone());

    let values: PrimitiveArray<T> = values_out.into();
    Ok(Either::Left(ListArray::new(
        dtype,
        offsets,
        values.boxed(),
        validity,
    )))
}

fn binary(
    a: &BinaryArray<i64>,
    b: &BinaryArray<i64>,
    offsets_a: &[i64],
    offsets_b: &[i64],
    set_op: SetOperation,
    validity: Option<Bitmap>,
    as_utf8: bool,
) -> PolarsResult<Either<ListArray<i64>, BooleanArray>> {
    let broadcast_lhs = offsets_a.len() == 2;
    let broadcast_rhs = offsets_b.len() == 2;
    let mut set = Default::default();
    let mut set2: PlIndexSet<Option<&[u8]>> = Default::default();

    let mut values_out = MutableBinaryArray::new();
    let mut bool_values_out = MutableBooleanArray::new();
    if set_op.is_boolean() {
        bool_values_out
            .reserve(
                std::cmp::max(*offsets_a.last().unwrap(), *offsets_b.last().unwrap()) as usize,
            );
    } else {
        values_out.reserve(
            std::cmp::max(*offsets_a.last().unwrap(), *offsets_b.last().unwrap()) as usize,
            0,
        );
    }

    let mut offsets = Vec::with_capacity(std::cmp::max(offsets_a.len(), offsets_b.len()));
    offsets.push(0i64);

    if broadcast_rhs {
        set2.extend(b);
    }
    let offsets_slice = if offsets_a.len() > offsets_b.len() {
        offsets_a
    } else {
        offsets_b
    };
    let first_a = offsets_a[0];
    let second_a = offsets_a[1];
    let first_b = offsets_b[0];
    let second_b = offsets_b[1];
    for i in 1..offsets_slice.len() {
        // If we go OOB we take the first element as we are then broadcasting.
        let start_a = *offsets_a.get(i - 1).unwrap_or(&first_a) as usize;
        let end_a = *offsets_a.get(i).unwrap_or(&second_a) as usize;

        let start_b = *offsets_b.get(i - 1).unwrap_or(&first_b) as usize;
        let end_b = *offsets_b.get(i).unwrap_or(&second_b) as usize;

        // The branches are the same every loop.
        // We rely on branch prediction here.
        let offset = if broadcast_rhs {
            // going via skip iterator instead of slice doesn't heap alloc nor trigger a bitcount
            let a_iter = a.into_iter().skip(start_a).take(end_a - start_a);
            let b_iter = b.into_iter();
            set_operation(
                &mut set,
                &mut set2,
                a_iter,
                b_iter,
                &mut values_out,
                &mut bool_values_out,
                set_op,
                true,
            )
        } else if broadcast_lhs {
            let a_iter = a.into_iter();
            let b_iter = b.into_iter().skip(start_b).take(end_b - start_b);
            set_operation(
                &mut set,
                &mut set2,
                a_iter,
                b_iter,
                &mut values_out,
                &mut bool_values_out,
                set_op,
                false,
            )
        } else {
            // going via skip iterator instead of slice doesn't heap alloc nor trigger a bitcount
            let a_iter = a.into_iter().skip(start_a).take(end_a - start_a);
            let b_iter = b.into_iter().skip(start_b).take(end_b - start_b);
            set_operation(
                &mut set,
                &mut set2,
                a_iter,
                b_iter,
                &mut values_out,
                &mut bool_values_out,
                set_op,
                false,
            )
        };
        offsets.push(offset as i64);
    }
    let offsets = unsafe { OffsetsBuffer::new_unchecked(offsets.into()) };
    let values: BinaryArray<i64> = values_out.into();

    if as_utf8 {
        let values = unsafe {
            Utf8Array::<i64>::new_unchecked(
                ArrowDataType::LargeUtf8,
                values.offsets().clone(),
                values.values().clone(),
                values.validity().cloned(),
            )
        };
        let dtype = ListArray::<i64>::default_datatype(values.data_type().clone());
        Ok(Either::Left(ListArray::new(
            dtype,
            offsets,
            values.boxed(),
            validity,
        )))
    } else {
        let dtype = ListArray::<i64>::default_datatype(values.data_type().clone());
        Ok(Either::Left(ListArray::new(
            dtype,
            offsets,
            values.boxed(),
            validity,
        )))
    }
}

fn utf8_to_binary(arr: &Utf8Array<i64>) -> BinaryArray<i64> {
    BinaryArray::<i64>::new(
        ArrowDataType::LargeBinary,
        arr.offsets().clone(),
        arr.values().clone(),
        arr.validity().cloned(),
    )
}

fn array_set_operation(
    a: &ListArray<i64>,
    b: &ListArray<i64>,
    set_op: SetOperation,
) -> PolarsResult<Either<ListArray<i64>, BooleanArray>> {
    let offsets_a = a.offsets().as_slice();
    let offsets_b = b.offsets().as_slice();

    let values_a = a.values();
    let values_b = b.values();
    assert_eq!(values_a.data_type(), values_b.data_type());

    let dtype = values_b.data_type();
    let validity = combine_validities_and(a.validity(), b.validity());

    match dtype {
        ArrowDataType::LargeUtf8 => {
            let a = values_a.as_any().downcast_ref::<Utf8Array<i64>>().unwrap();
            let b = values_b.as_any().downcast_ref::<Utf8Array<i64>>().unwrap();

            let a = utf8_to_binary(a);
            let b = utf8_to_binary(b);
            binary(&a, &b, offsets_a, offsets_b, set_op, validity, true)
        },
        ArrowDataType::LargeBinary => {
            let a = values_a
                .as_any()
                .downcast_ref::<BinaryArray<i64>>()
                .unwrap();
            let b = values_b
                .as_any()
                .downcast_ref::<BinaryArray<i64>>()
                .unwrap();
            binary(a, b, offsets_a, offsets_b, set_op, validity, false)
        },
        ArrowDataType::Boolean => {
            polars_bail!(InvalidOperation: "boolean type not yet supported in list 'set' operations")
        },
        _ => {
            with_match_physical_integer_type!(dtype.into(), |$T| {
                let a = values_a.as_any().downcast_ref::<PrimitiveArray<$T>>().unwrap();
                let b = values_b.as_any().downcast_ref::<PrimitiveArray<$T>>().unwrap();

                primitive(&a, &b, offsets_a, offsets_b, set_op, validity)
            })
        },
    }
}

pub fn list_set_operation(
    a: &ListChunked,
    b: &ListChunked,
    set_op: SetOperation,
) -> PolarsResult<ListChunked> {
    polars_ensure!(a.len() == b.len() || b.len() == 1 || a.len() == 1, ShapeMismatch: "column lengths don't match");
    let mut a = a.clone();
    let mut b = b.clone();
    if a.len() != b.len() {
        a = a.rechunk();
        b = b.rechunk();
    }

    // we use the unsafe variant because we want to keep the nested logical types type.
    unsafe {
        arity::try_binary_unchecked_same_type(
            &a,
            &b,
            |a, b| array_set_operation(a, b, set_op).map(|arr| arr.unwrap_left().boxed()),
            false,
            false,
        )
    }
}

pub fn boolean_list_set_operation(
    a: &ListChunked,
    b: &ListChunked,
    set_op: SetOperation,
) -> PolarsResult<BooleanChunked> {
    polars_ensure!(a.len() == b.len() || b.len() == 1 || a.len() == 1, ShapeMismatch: "column lengths don't match");

    let mut a = a.clone();
    let mut b = b.clone();
    if a.len() != b.len() {
        a = a.rechunk();
        b = b.rechunk();
    }

    unimplemented!("WIP")
}
