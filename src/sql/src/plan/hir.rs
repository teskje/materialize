// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! This file houses HIR, a representation of a SQL plan that is parallel to MIR, but represents
//! an earlier phase of planning. It's structurally very similar to MIR, with some differences
//! which are noted below. It gets turned into MIR via a call to lower().

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::sync::Arc;
use std::{fmt, mem};

use itertools::Itertools;
use mz_expr::virtual_syntax::{AlgExcept, Except, IR};
use mz_expr::visit::{Visit, VisitChildren};
use mz_expr::{CollectionPlan, Id, LetRecLimit, RowSetFinishing, func};
// these happen to be unchanged at the moment, but there might be additions later
use mz_expr::AggregateFunc::{FusedWindowAggregate, WindowAggregate};
pub use mz_expr::{
    BinaryFunc, ColumnOrder, TableFunc, UnaryFunc, UnmaterializableFunc, VariadicFunc, WindowFrame,
};
use mz_ore::collections::CollectionExt;
use mz_ore::error::ErrorExt;
use mz_ore::stack::RecursionLimitError;
use mz_ore::str::separated;
use mz_ore::treat_as_equal::TreatAsEqual;
use mz_ore::{soft_assert_or_log, stack};
use mz_repr::adt::array::ArrayDimension;
use mz_repr::adt::numeric::NumericMaxScale;
use mz_repr::*;
use serde::{Deserialize, Serialize};

use crate::plan::error::PlanError;
use crate::plan::query::{EXECUTE_CAST_CONTEXT, ExprContext, execute_expr_context};
use crate::plan::typeconv::{self, CastContext, plan_cast};
use crate::plan::{Params, QueryContext, QueryLifetime, StatementContext};

use super::plan_utils::GroupSizeHints;

#[allow(missing_debug_implementations)]
pub struct Hir;

impl IR for Hir {
    type Relation = HirRelationExpr;
    type Scalar = HirScalarExpr;
}

impl AlgExcept for Hir {
    fn except(all: &bool, lhs: Self::Relation, rhs: Self::Relation) -> Self::Relation {
        if *all {
            let rhs = rhs.negate();
            HirRelationExpr::union(lhs, rhs).threshold()
        } else {
            let lhs = lhs.distinct();
            let rhs = rhs.distinct().negate();
            HirRelationExpr::union(lhs, rhs).threshold()
        }
    }

    fn un_except<'a>(expr: &'a Self::Relation) -> Option<Except<'a, Self>> {
        let mut result = None;

        use HirRelationExpr::*;
        if let Threshold { input } = expr {
            if let Union { base: lhs, inputs } = input.as_ref() {
                if let [rhs] = &inputs[..] {
                    if let Negate { input: rhs } = rhs {
                        match (lhs.as_ref(), rhs.as_ref()) {
                            (Distinct { input: lhs }, Distinct { input: rhs }) => {
                                let all = false;
                                let lhs = lhs.as_ref();
                                let rhs = rhs.as_ref();
                                result = Some(Except { all, lhs, rhs })
                            }
                            (lhs, rhs) => {
                                let all = true;
                                result = Some(Except { all, lhs, rhs })
                            }
                        }
                    }
                }
            }
        }

        result
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
/// Just like [`mz_expr::MirRelationExpr`], except where otherwise noted below.
pub enum HirRelationExpr {
    Constant {
        rows: Vec<Row>,
        typ: RelationType,
    },
    Get {
        id: mz_expr::Id,
        typ: RelationType,
    },
    /// Mutually recursive CTE
    LetRec {
        /// Maximum number of iterations to evaluate. If None, then there is no limit.
        limit: Option<LetRecLimit>,
        /// List of bindings all of which are in scope of each other.
        bindings: Vec<(String, mz_expr::LocalId, HirRelationExpr, RelationType)>,
        /// Result of the AST node.
        body: Box<HirRelationExpr>,
    },
    /// CTE
    Let {
        name: String,
        /// The identifier to be used in `Get` variants to retrieve `value`.
        id: mz_expr::LocalId,
        /// The collection to be bound to `name`.
        value: Box<HirRelationExpr>,
        /// The result of the `Let`, evaluated with `name` bound to `value`.
        body: Box<HirRelationExpr>,
    },
    Project {
        input: Box<HirRelationExpr>,
        outputs: Vec<usize>,
    },
    Map {
        input: Box<HirRelationExpr>,
        scalars: Vec<HirScalarExpr>,
    },
    CallTable {
        func: TableFunc,
        exprs: Vec<HirScalarExpr>,
    },
    Filter {
        input: Box<HirRelationExpr>,
        predicates: Vec<HirScalarExpr>,
    },
    /// Unlike MirRelationExpr, we haven't yet compiled LeftOuter/RightOuter/FullOuter
    /// joins away into more primitive exprs
    Join {
        left: Box<HirRelationExpr>,
        right: Box<HirRelationExpr>,
        on: HirScalarExpr,
        kind: JoinKind,
    },
    /// Unlike MirRelationExpr, when `key` is empty AND `input` is empty this returns
    /// a single row with the aggregates evaluated over empty groups, rather than returning zero
    /// rows
    Reduce {
        input: Box<HirRelationExpr>,
        group_key: Vec<usize>,
        aggregates: Vec<AggregateExpr>,
        expected_group_size: Option<u64>,
    },
    Distinct {
        input: Box<HirRelationExpr>,
    },
    /// Groups and orders within each group, limiting output.
    TopK {
        /// The source collection.
        input: Box<HirRelationExpr>,
        /// Column indices used to form groups.
        group_key: Vec<usize>,
        /// Column indices used to order rows within groups.
        order_key: Vec<ColumnOrder>,
        /// Number of records to retain.
        /// It is of ScalarType::Int64.
        /// (UInt64 would make sense in theory: Then we wouldn't need to manually check
        /// non-negativity, but would just get this for free when casting to UInt64. However, Int64
        /// is better for Postgres compat. This is because if there is a $1 here, then when external
        /// tools `describe` the prepared statement, they discover this type. If what they find
        /// were UInt64, then they might have trouble calling the prepared statement, because the
        /// unsigned types are non-standard, and also don't exist even in Postgres.)
        limit: Option<HirScalarExpr>,
        /// Number of records to skip.
        /// It is of ScalarType::Int64.
        /// This can contain parameters at first, but by the time we reach lowering, this should
        /// already be simply a Literal.
        offset: HirScalarExpr,
        /// User-supplied hint: how many rows will have the same group key.
        expected_group_size: Option<u64>,
    },
    Negate {
        input: Box<HirRelationExpr>,
    },
    /// Keep rows from a dataflow where the row counts are positive.
    Threshold {
        input: Box<HirRelationExpr>,
    },
    Union {
        base: Box<HirRelationExpr>,
        inputs: Vec<HirRelationExpr>,
    },
}

/// Stored column metadata.
pub type NameMetadata = TreatAsEqual<Option<Arc<str>>>;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
/// Just like [`mz_expr::MirScalarExpr`], except where otherwise noted below.
pub enum HirScalarExpr {
    /// Unlike mz_expr::MirScalarExpr, we can nest HirRelationExprs via eg Exists. This means that a
    /// variable could refer to a column of the current input, or to a column of an outer relation.
    /// We use ColumnRef to denote the difference.
    Column(ColumnRef, NameMetadata),
    Parameter(usize, NameMetadata),
    Literal(Row, ColumnType, NameMetadata),
    CallUnmaterializable(UnmaterializableFunc, NameMetadata),
    CallUnary {
        func: UnaryFunc,
        expr: Box<HirScalarExpr>,
        name: NameMetadata,
    },
    CallBinary {
        func: BinaryFunc,
        expr1: Box<HirScalarExpr>,
        expr2: Box<HirScalarExpr>,
        name: NameMetadata,
    },
    CallVariadic {
        func: VariadicFunc,
        exprs: Vec<HirScalarExpr>,
        name: NameMetadata,
    },
    If {
        cond: Box<HirScalarExpr>,
        then: Box<HirScalarExpr>,
        els: Box<HirScalarExpr>,
        name: NameMetadata,
    },
    /// Returns true if `expr` returns any rows
    Exists(Box<HirRelationExpr>, NameMetadata),
    /// Given `expr` with arity 1. If expr returns:
    /// * 0 rows, return NULL
    /// * 1 row, return the value of that row
    /// * >1 rows, we return an error
    Select(Box<HirRelationExpr>, NameMetadata),
    Windowing(WindowExpr, NameMetadata),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
/// Represents the invocation of a window function over an optional partitioning with an optional
/// order.
pub struct WindowExpr {
    pub func: WindowExprType,
    pub partition_by: Vec<HirScalarExpr>,
    /// ORDER BY is represented in a complicated way: `plan_function_order_by` gave us two things:
    ///  - the `ColumnOrder`s we have put in the `order_by` fields in the `WindowExprType` in `func`
    ///    above,
    ///  - the `HirScalarExpr`s we have put in the following `order_by` field.
    /// These are separated because they are used in different places: the outer `order_by` is used
    /// in the lowering: based on it, we create a Row constructor that collects the scalar exprs;
    /// the inner `order_by` is used in the rendering to actually execute the ordering on these Rows.
    /// (`WindowExpr` exists only in HIR, but not in MIR.)
    /// Note that the `column` field in the `ColumnOrder`s point into the Row constructed in the
    /// lowering, and not to original input columns.
    pub order_by: Vec<HirScalarExpr>,
}

impl WindowExpr {
    pub fn visit_expressions<'a, F, E>(&'a self, f: &mut F) -> Result<(), E>
    where
        F: FnMut(&'a HirScalarExpr) -> Result<(), E>,
    {
        #[allow(deprecated)]
        self.func.visit_expressions(f)?;
        for expr in self.partition_by.iter() {
            f(expr)?;
        }
        for expr in self.order_by.iter() {
            f(expr)?;
        }
        Ok(())
    }

    pub fn visit_expressions_mut<'a, F, E>(&'a mut self, f: &mut F) -> Result<(), E>
    where
        F: FnMut(&'a mut HirScalarExpr) -> Result<(), E>,
    {
        #[allow(deprecated)]
        self.func.visit_expressions_mut(f)?;
        for expr in self.partition_by.iter_mut() {
            f(expr)?;
        }
        for expr in self.order_by.iter_mut() {
            f(expr)?;
        }
        Ok(())
    }
}

impl VisitChildren<HirScalarExpr> for WindowExpr {
    fn visit_children<F>(&self, mut f: F)
    where
        F: FnMut(&HirScalarExpr),
    {
        self.func.visit_children(&mut f);
        for expr in self.partition_by.iter() {
            f(expr);
        }
        for expr in self.order_by.iter() {
            f(expr);
        }
    }

    fn visit_mut_children<F>(&mut self, mut f: F)
    where
        F: FnMut(&mut HirScalarExpr),
    {
        self.func.visit_mut_children(&mut f);
        for expr in self.partition_by.iter_mut() {
            f(expr);
        }
        for expr in self.order_by.iter_mut() {
            f(expr);
        }
    }

    fn try_visit_children<F, E>(&self, mut f: F) -> Result<(), E>
    where
        F: FnMut(&HirScalarExpr) -> Result<(), E>,
        E: From<RecursionLimitError>,
    {
        self.func.try_visit_children(&mut f)?;
        for expr in self.partition_by.iter() {
            f(expr)?;
        }
        for expr in self.order_by.iter() {
            f(expr)?;
        }
        Ok(())
    }

    fn try_visit_mut_children<F, E>(&mut self, mut f: F) -> Result<(), E>
    where
        F: FnMut(&mut HirScalarExpr) -> Result<(), E>,
        E: From<RecursionLimitError>,
    {
        self.func.try_visit_mut_children(&mut f)?;
        for expr in self.partition_by.iter_mut() {
            f(expr)?;
        }
        for expr in self.order_by.iter_mut() {
            f(expr)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
/// A window function with its parameters.
///
/// There are three types of window functions:
/// - scalar window functions, which return a different scalar value for each
///   row within a partition that depends exclusively on the position of the row
///   within the partition;
/// - value window functions, which return a scalar value for each row within a
///   partition that might be computed based on a single row, which is usually not
///   the current row (e.g., previous or following row; first or last row of the
///   partition);
/// - aggregate window functions, which compute a traditional aggregation as a
///   window function (e.g. `sum(x) OVER (...)`).
///   (Aggregate window  functions can in some cases be computed by joining the
///   input relation with a reduction over the same relation that computes the
///   aggregation using the partition key as its grouping key, but we don't
///   automatically do this currently.)
pub enum WindowExprType {
    Scalar(ScalarWindowExpr),
    Value(ValueWindowExpr),
    Aggregate(AggregateWindowExpr),
}

impl WindowExprType {
    #[deprecated = "Use `VisitChildren<HirScalarExpr>::visit_children` instead."]
    pub fn visit_expressions<'a, F, E>(&'a self, f: &mut F) -> Result<(), E>
    where
        F: FnMut(&'a HirScalarExpr) -> Result<(), E>,
    {
        #[allow(deprecated)]
        match self {
            Self::Scalar(expr) => expr.visit_expressions(f),
            Self::Value(expr) => expr.visit_expressions(f),
            Self::Aggregate(expr) => expr.visit_expressions(f),
        }
    }

    #[deprecated = "Use `VisitChildren<HirScalarExpr>::visit_mut_children` instead."]
    pub fn visit_expressions_mut<'a, F, E>(&'a mut self, f: &mut F) -> Result<(), E>
    where
        F: FnMut(&'a mut HirScalarExpr) -> Result<(), E>,
    {
        #[allow(deprecated)]
        match self {
            Self::Scalar(expr) => expr.visit_expressions_mut(f),
            Self::Value(expr) => expr.visit_expressions_mut(f),
            Self::Aggregate(expr) => expr.visit_expressions_mut(f),
        }
    }

    fn typ(
        &self,
        outers: &[RelationType],
        inner: &RelationType,
        params: &BTreeMap<usize, ScalarType>,
    ) -> ColumnType {
        match self {
            Self::Scalar(expr) => expr.typ(outers, inner, params),
            Self::Value(expr) => expr.typ(outers, inner, params),
            Self::Aggregate(expr) => expr.typ(outers, inner, params),
        }
    }
}

impl VisitChildren<HirScalarExpr> for WindowExprType {
    fn visit_children<F>(&self, f: F)
    where
        F: FnMut(&HirScalarExpr),
    {
        match self {
            Self::Scalar(_) => (),
            Self::Value(expr) => expr.visit_children(f),
            Self::Aggregate(expr) => expr.visit_children(f),
        }
    }

    fn visit_mut_children<F>(&mut self, f: F)
    where
        F: FnMut(&mut HirScalarExpr),
    {
        match self {
            Self::Scalar(_) => (),
            Self::Value(expr) => expr.visit_mut_children(f),
            Self::Aggregate(expr) => expr.visit_mut_children(f),
        }
    }

    fn try_visit_children<F, E>(&self, f: F) -> Result<(), E>
    where
        F: FnMut(&HirScalarExpr) -> Result<(), E>,
        E: From<RecursionLimitError>,
    {
        match self {
            Self::Scalar(_) => Ok(()),
            Self::Value(expr) => expr.try_visit_children(f),
            Self::Aggregate(expr) => expr.try_visit_children(f),
        }
    }

    fn try_visit_mut_children<F, E>(&mut self, f: F) -> Result<(), E>
    where
        F: FnMut(&mut HirScalarExpr) -> Result<(), E>,
        E: From<RecursionLimitError>,
    {
        match self {
            Self::Scalar(_) => Ok(()),
            Self::Value(expr) => expr.try_visit_mut_children(f),
            Self::Aggregate(expr) => expr.try_visit_mut_children(f),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ScalarWindowExpr {
    pub func: ScalarWindowFunc,
    pub order_by: Vec<ColumnOrder>,
}

impl ScalarWindowExpr {
    #[deprecated = "Implement `VisitChildren<HirScalarExpr>` if needed."]
    pub fn visit_expressions<'a, F, E>(&'a self, _f: &mut F) -> Result<(), E>
    where
        F: FnMut(&'a HirScalarExpr) -> Result<(), E>,
    {
        match self.func {
            ScalarWindowFunc::RowNumber => {}
            ScalarWindowFunc::Rank => {}
            ScalarWindowFunc::DenseRank => {}
        }
        Ok(())
    }

    #[deprecated = "Implement `VisitChildren<HirScalarExpr>` if needed."]
    pub fn visit_expressions_mut<'a, F, E>(&'a self, _f: &mut F) -> Result<(), E>
    where
        F: FnMut(&'a mut HirScalarExpr) -> Result<(), E>,
    {
        match self.func {
            ScalarWindowFunc::RowNumber => {}
            ScalarWindowFunc::Rank => {}
            ScalarWindowFunc::DenseRank => {}
        }
        Ok(())
    }

    fn typ(
        &self,
        _outers: &[RelationType],
        _inner: &RelationType,
        _params: &BTreeMap<usize, ScalarType>,
    ) -> ColumnType {
        self.func.output_type()
    }

    pub fn into_expr(self) -> mz_expr::AggregateFunc {
        match self.func {
            ScalarWindowFunc::RowNumber => mz_expr::AggregateFunc::RowNumber {
                order_by: self.order_by,
            },
            ScalarWindowFunc::Rank => mz_expr::AggregateFunc::Rank {
                order_by: self.order_by,
            },
            ScalarWindowFunc::DenseRank => mz_expr::AggregateFunc::DenseRank {
                order_by: self.order_by,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
/// Scalar Window functions
pub enum ScalarWindowFunc {
    RowNumber,
    Rank,
    DenseRank,
}

impl Display for ScalarWindowFunc {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            ScalarWindowFunc::RowNumber => write!(f, "row_number"),
            ScalarWindowFunc::Rank => write!(f, "rank"),
            ScalarWindowFunc::DenseRank => write!(f, "dense_rank"),
        }
    }
}

impl ScalarWindowFunc {
    pub fn output_type(&self) -> ColumnType {
        match self {
            ScalarWindowFunc::RowNumber => ScalarType::Int64.nullable(false),
            ScalarWindowFunc::Rank => ScalarType::Int64.nullable(false),
            ScalarWindowFunc::DenseRank => ScalarType::Int64.nullable(false),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ValueWindowExpr {
    pub func: ValueWindowFunc,
    /// If the argument list has a single element (e.g., for `first_value`), then it's that element.
    /// If the argument list has multiple elements (e.g., for `lag`), then it's encoded in a record,
    /// e.g., `row(#1, 3, null)`.
    /// If it's a fused window function, then the arguments of each of the constituent function
    /// calls are wrapped in an outer record.
    pub args: Box<HirScalarExpr>,
    /// See comment on `WindowExpr::order_by`.
    pub order_by: Vec<ColumnOrder>,
    pub window_frame: WindowFrame,
    pub ignore_nulls: bool,
}

impl Display for ValueWindowFunc {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            ValueWindowFunc::Lag => write!(f, "lag"),
            ValueWindowFunc::Lead => write!(f, "lead"),
            ValueWindowFunc::FirstValue => write!(f, "first_value"),
            ValueWindowFunc::LastValue => write!(f, "last_value"),
            ValueWindowFunc::Fused(funcs) => write!(f, "fused[{}]", separated(", ", funcs)),
        }
    }
}

impl ValueWindowExpr {
    #[deprecated = "Use `VisitChildren<HirScalarExpr>::visit_children` instead."]
    pub fn visit_expressions<'a, F, E>(&'a self, f: &mut F) -> Result<(), E>
    where
        F: FnMut(&'a HirScalarExpr) -> Result<(), E>,
    {
        f(&self.args)
    }

    #[deprecated = "Use `VisitChildren<HirScalarExpr>::visit_mut_children` instead."]
    pub fn visit_expressions_mut<'a, F, E>(&'a mut self, f: &mut F) -> Result<(), E>
    where
        F: FnMut(&'a mut HirScalarExpr) -> Result<(), E>,
    {
        f(&mut self.args)
    }

    fn typ(
        &self,
        outers: &[RelationType],
        inner: &RelationType,
        params: &BTreeMap<usize, ScalarType>,
    ) -> ColumnType {
        self.func.output_type(self.args.typ(outers, inner, params))
    }

    /// Converts into `mz_expr::AggregateFunc`.
    pub fn into_expr(self) -> (Box<HirScalarExpr>, mz_expr::AggregateFunc) {
        (
            self.args,
            self.func
                .into_expr(self.order_by, self.window_frame, self.ignore_nulls),
        )
    }
}

impl VisitChildren<HirScalarExpr> for ValueWindowExpr {
    fn visit_children<F>(&self, mut f: F)
    where
        F: FnMut(&HirScalarExpr),
    {
        f(&self.args)
    }

    fn visit_mut_children<F>(&mut self, mut f: F)
    where
        F: FnMut(&mut HirScalarExpr),
    {
        f(&mut self.args)
    }

    fn try_visit_children<F, E>(&self, mut f: F) -> Result<(), E>
    where
        F: FnMut(&HirScalarExpr) -> Result<(), E>,
        E: From<RecursionLimitError>,
    {
        f(&self.args)
    }

    fn try_visit_mut_children<F, E>(&mut self, mut f: F) -> Result<(), E>
    where
        F: FnMut(&mut HirScalarExpr) -> Result<(), E>,
        E: From<RecursionLimitError>,
    {
        f(&mut self.args)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
/// Value Window functions
pub enum ValueWindowFunc {
    Lag,
    Lead,
    FirstValue,
    LastValue,
    Fused(Vec<ValueWindowFunc>),
}

impl ValueWindowFunc {
    pub fn output_type(&self, input_type: ColumnType) -> ColumnType {
        match self {
            ValueWindowFunc::Lag | ValueWindowFunc::Lead => {
                // The input is a (value, offset, default) record, so extract the type of the first arg
                input_type.scalar_type.unwrap_record_element_type()[0]
                    .clone()
                    .nullable(true)
            }
            ValueWindowFunc::FirstValue | ValueWindowFunc::LastValue => {
                input_type.scalar_type.nullable(true)
            }
            ValueWindowFunc::Fused(funcs) => {
                let input_types = input_type.scalar_type.unwrap_record_element_column_type();
                ScalarType::Record {
                    fields: funcs
                        .iter()
                        .zip_eq(input_types)
                        .map(|(f, t)| (ColumnName::from(""), f.output_type(t.clone())))
                        .collect(),
                    custom_id: None,
                }
                .nullable(false)
            }
        }
    }

    pub fn into_expr(
        self,
        order_by: Vec<ColumnOrder>,
        window_frame: WindowFrame,
        ignore_nulls: bool,
    ) -> mz_expr::AggregateFunc {
        match self {
            // Lag and Lead are fundamentally the same function, just with opposite directions
            ValueWindowFunc::Lag => mz_expr::AggregateFunc::LagLead {
                order_by,
                lag_lead: mz_expr::LagLeadType::Lag,
                ignore_nulls,
            },
            ValueWindowFunc::Lead => mz_expr::AggregateFunc::LagLead {
                order_by,
                lag_lead: mz_expr::LagLeadType::Lead,
                ignore_nulls,
            },
            ValueWindowFunc::FirstValue => mz_expr::AggregateFunc::FirstValue {
                order_by,
                window_frame,
            },
            ValueWindowFunc::LastValue => mz_expr::AggregateFunc::LastValue {
                order_by,
                window_frame,
            },
            ValueWindowFunc::Fused(funcs) => mz_expr::AggregateFunc::FusedValueWindowFunc {
                funcs: funcs
                    .into_iter()
                    .map(|func| {
                        func.into_expr(order_by.clone(), window_frame.clone(), ignore_nulls)
                    })
                    .collect(),
                order_by,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AggregateWindowExpr {
    pub aggregate_expr: AggregateExpr,
    pub order_by: Vec<ColumnOrder>,
    pub window_frame: WindowFrame,
}

impl AggregateWindowExpr {
    #[deprecated = "Use `VisitChildren<HirScalarExpr>::visit_children` instead."]
    pub fn visit_expressions<'a, F, E>(&'a self, f: &mut F) -> Result<(), E>
    where
        F: FnMut(&'a HirScalarExpr) -> Result<(), E>,
    {
        f(&self.aggregate_expr.expr)
    }

    #[deprecated = "Use `VisitChildren<HirScalarExpr>::visit_mut_children` instead."]
    pub fn visit_expressions_mut<'a, F, E>(&'a mut self, f: &mut F) -> Result<(), E>
    where
        F: FnMut(&'a mut HirScalarExpr) -> Result<(), E>,
    {
        f(&mut self.aggregate_expr.expr)
    }

    fn typ(
        &self,
        outers: &[RelationType],
        inner: &RelationType,
        params: &BTreeMap<usize, ScalarType>,
    ) -> ColumnType {
        self.aggregate_expr
            .func
            .output_type(self.aggregate_expr.expr.typ(outers, inner, params))
    }

    pub fn into_expr(self) -> (Box<HirScalarExpr>, mz_expr::AggregateFunc) {
        if let AggregateFunc::FusedWindowAgg { funcs } = &self.aggregate_expr.func {
            (
                self.aggregate_expr.expr,
                FusedWindowAggregate {
                    wrapped_aggregates: funcs.iter().map(|f| f.clone().into_expr()).collect(),
                    order_by: self.order_by,
                    window_frame: self.window_frame,
                },
            )
        } else {
            (
                self.aggregate_expr.expr,
                WindowAggregate {
                    wrapped_aggregate: Box::new(self.aggregate_expr.func.into_expr()),
                    order_by: self.order_by,
                    window_frame: self.window_frame,
                },
            )
        }
    }
}

impl VisitChildren<HirScalarExpr> for AggregateWindowExpr {
    fn visit_children<F>(&self, mut f: F)
    where
        F: FnMut(&HirScalarExpr),
    {
        f(&self.aggregate_expr.expr)
    }

    fn visit_mut_children<F>(&mut self, mut f: F)
    where
        F: FnMut(&mut HirScalarExpr),
    {
        f(&mut self.aggregate_expr.expr)
    }

    fn try_visit_children<F, E>(&self, mut f: F) -> Result<(), E>
    where
        F: FnMut(&HirScalarExpr) -> Result<(), E>,
        E: From<RecursionLimitError>,
    {
        f(&self.aggregate_expr.expr)
    }

    fn try_visit_mut_children<F, E>(&mut self, mut f: F) -> Result<(), E>
    where
        F: FnMut(&mut HirScalarExpr) -> Result<(), E>,
        E: From<RecursionLimitError>,
    {
        f(&mut self.aggregate_expr.expr)
    }
}

/// A `CoercibleScalarExpr` is a [`HirScalarExpr`] whose type is not fully
/// determined. Several SQL expressions can be freely coerced based upon where
/// in the expression tree they appear. For example, the string literal '42'
/// will be automatically coerced to the integer 42 if used in a numeric
/// context:
///
/// ```sql
/// SELECT '42' + 42
/// ```
///
/// This separate type gives the code that needs to interact with coercions very
/// fine-grained control over what coercions happen and when.
///
/// The primary driver of coercion is function and operator selection, as
/// choosing the correct function or operator implementation depends on the type
/// of the provided arguments. Coercion also occurs at the very root of the
/// scalar expression tree. For example in
///
/// ```sql
/// SELECT ... WHERE $1
/// ```
///
/// the `WHERE` clause will coerce the contained unconstrained type parameter
/// `$1` to have type bool.
#[derive(Clone, Debug)]
pub enum CoercibleScalarExpr {
    Coerced(HirScalarExpr),
    Parameter(usize),
    LiteralNull,
    LiteralString(String),
    LiteralRecord(Vec<CoercibleScalarExpr>),
}

impl CoercibleScalarExpr {
    pub fn type_as(self, ecx: &ExprContext, ty: &ScalarType) -> Result<HirScalarExpr, PlanError> {
        let expr = typeconv::plan_coerce(ecx, self, ty)?;
        let expr_ty = ecx.scalar_type(&expr);
        if ty != &expr_ty {
            sql_bail!(
                "{} must have type {}, not type {}",
                ecx.name,
                ecx.humanize_scalar_type(ty, false),
                ecx.humanize_scalar_type(&expr_ty, false),
            );
        }
        Ok(expr)
    }

    pub fn type_as_any(self, ecx: &ExprContext) -> Result<HirScalarExpr, PlanError> {
        typeconv::plan_coerce(ecx, self, &ScalarType::String)
    }

    pub fn cast_to(
        self,
        ecx: &ExprContext,
        ccx: CastContext,
        ty: &ScalarType,
    ) -> Result<HirScalarExpr, PlanError> {
        let expr = typeconv::plan_coerce(ecx, self, ty)?;
        typeconv::plan_cast(ecx, ccx, expr, ty)
    }
}

/// The column type for a [`CoercibleScalarExpr`].
#[derive(Clone, Debug)]
pub enum CoercibleColumnType {
    Coerced(ColumnType),
    Record(Vec<CoercibleColumnType>),
    Uncoerced,
}

impl CoercibleColumnType {
    /// Reports the nullability of the type.
    pub fn nullable(&self) -> bool {
        match self {
            // A coerced value's nullability is known.
            CoercibleColumnType::Coerced(ct) => ct.nullable,

            // A literal record can never be null.
            CoercibleColumnType::Record(_) => false,

            // An uncoerced literal may be the literal `NULL`, so we have
            // to conservatively assume it is nullable.
            CoercibleColumnType::Uncoerced => true,
        }
    }
}

/// The scalar type for a [`CoercibleScalarExpr`].
#[derive(Clone, Debug)]
pub enum CoercibleScalarType {
    Coerced(ScalarType),
    Record(Vec<CoercibleColumnType>),
    Uncoerced,
}

impl CoercibleScalarType {
    /// Reports whether the scalar type has been coerced.
    pub fn is_coerced(&self) -> bool {
        matches!(self, CoercibleScalarType::Coerced(_))
    }

    /// Returns the coerced scalar type, if the type is coerced.
    pub fn as_coerced(&self) -> Option<&ScalarType> {
        match self {
            CoercibleScalarType::Coerced(t) => Some(t),
            _ => None,
        }
    }

    /// If the type is coerced, apply the mapping function to the contained
    /// scalar type.
    pub fn map_coerced<F>(self, f: F) -> CoercibleScalarType
    where
        F: FnOnce(ScalarType) -> ScalarType,
    {
        match self {
            CoercibleScalarType::Coerced(t) => CoercibleScalarType::Coerced(f(t)),
            _ => self,
        }
    }

    /// If the type is an coercible record, forcibly converts to a coerced
    /// record type. Any uncoerced field types are assumed to be of type text.
    ///
    /// Generally you should prefer to use [`typeconv::plan_coerce`], which
    /// accepts a type hint that can indicate the types of uncoerced field
    /// types.
    pub fn force_coerced_if_record(&mut self) {
        fn convert(uncoerced_fields: impl Iterator<Item = CoercibleColumnType>) -> ScalarType {
            let mut fields = vec![];
            for (i, uf) in uncoerced_fields.enumerate() {
                let name = ColumnName::from(format!("f{}", i + 1));
                let ty = match uf {
                    CoercibleColumnType::Coerced(ty) => ty,
                    CoercibleColumnType::Record(mut fields) => {
                        convert(fields.drain(..)).nullable(false)
                    }
                    CoercibleColumnType::Uncoerced => ScalarType::String.nullable(true),
                };
                fields.push((name, ty))
            }
            ScalarType::Record {
                fields: fields.into(),
                custom_id: None,
            }
        }

        if let CoercibleScalarType::Record(fields) = self {
            *self = CoercibleScalarType::Coerced(convert(fields.drain(..)));
        }
    }
}

/// An expression whose type can be ascertained.
///
/// Abstracts over `ScalarExpr` and `CoercibleScalarExpr`.
pub trait AbstractExpr {
    type Type: AbstractColumnType;

    /// Computes the type of the expression.
    fn typ(
        &self,
        outers: &[RelationType],
        inner: &RelationType,
        params: &BTreeMap<usize, ScalarType>,
    ) -> Self::Type;
}

impl AbstractExpr for CoercibleScalarExpr {
    type Type = CoercibleColumnType;

    fn typ(
        &self,
        outers: &[RelationType],
        inner: &RelationType,
        params: &BTreeMap<usize, ScalarType>,
    ) -> Self::Type {
        match self {
            CoercibleScalarExpr::Coerced(expr) => {
                CoercibleColumnType::Coerced(expr.typ(outers, inner, params))
            }
            CoercibleScalarExpr::LiteralRecord(scalars) => {
                let fields = scalars
                    .iter()
                    .map(|s| s.typ(outers, inner, params))
                    .collect();
                CoercibleColumnType::Record(fields)
            }
            _ => CoercibleColumnType::Uncoerced,
        }
    }
}

/// A column type-like object whose underlying scalar type-like object can be
/// ascertained.
///
/// Abstracts over `ColumnType` and `CoercibleColumnType`.
pub trait AbstractColumnType {
    type AbstractScalarType;

    /// Converts the column type-like object into its inner scalar type-like
    /// object.
    fn scalar_type(self) -> Self::AbstractScalarType;
}

impl AbstractColumnType for ColumnType {
    type AbstractScalarType = ScalarType;

    fn scalar_type(self) -> Self::AbstractScalarType {
        self.scalar_type
    }
}

impl AbstractColumnType for CoercibleColumnType {
    type AbstractScalarType = CoercibleScalarType;

    fn scalar_type(self) -> Self::AbstractScalarType {
        match self {
            CoercibleColumnType::Coerced(t) => CoercibleScalarType::Coerced(t.scalar_type),
            CoercibleColumnType::Record(t) => CoercibleScalarType::Record(t),
            CoercibleColumnType::Uncoerced => CoercibleScalarType::Uncoerced,
        }
    }
}

impl From<HirScalarExpr> for CoercibleScalarExpr {
    fn from(expr: HirScalarExpr) -> CoercibleScalarExpr {
        CoercibleScalarExpr::Coerced(expr)
    }
}

/// A leveled column reference.
///
/// In the course of decorrelation, multiple levels of nested subqueries are
/// traversed, and references to columns may correspond to different levels
/// of containing outer subqueries.
///
/// A `ColumnRef` allows expressions to refer to columns while being clear
/// about which level the column references without manually performing the
/// bookkeeping tracking their actual column locations.
///
/// Specifically, a `ColumnRef` refers to a column `level` subquery level *out*
/// from the reference, using `column` as a unique identifier in that subquery level.
/// A `level` of zero corresponds to the current scope, and levels increase to
/// indicate subqueries further "outwards".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ColumnRef {
    // scope level, where 0 is the current scope and 1+ are outer scopes.
    pub level: usize,
    // level-local column identifier used.
    pub column: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum JoinKind {
    Inner,
    LeftOuter,
    RightOuter,
    FullOuter,
}

impl fmt::Display for JoinKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                JoinKind::Inner => "Inner",
                JoinKind::LeftOuter => "LeftOuter",
                JoinKind::RightOuter => "RightOuter",
                JoinKind::FullOuter => "FullOuter",
            }
        )
    }
}

impl JoinKind {
    pub fn can_be_correlated(&self) -> bool {
        match self {
            JoinKind::Inner | JoinKind::LeftOuter => true,
            JoinKind::RightOuter | JoinKind::FullOuter => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AggregateExpr {
    pub func: AggregateFunc,
    pub expr: Box<HirScalarExpr>,
    pub distinct: bool,
}

/// Aggregate functions analogous to `mz_expr::AggregateFunc`, but whose
/// types may be different.
///
/// Specifically, the nullability of the aggregate columns is more common
/// here than in `expr`, as these aggregates may be applied over empty
/// result sets and should be null in those cases, whereas `expr` variants
/// only return null values when supplied nulls as input.
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum AggregateFunc {
    MaxNumeric,
    MaxInt16,
    MaxInt32,
    MaxInt64,
    MaxUInt16,
    MaxUInt32,
    MaxUInt64,
    MaxMzTimestamp,
    MaxFloat32,
    MaxFloat64,
    MaxBool,
    MaxString,
    MaxDate,
    MaxTimestamp,
    MaxTimestampTz,
    MaxInterval,
    MaxTime,
    MinNumeric,
    MinInt16,
    MinInt32,
    MinInt64,
    MinUInt16,
    MinUInt32,
    MinUInt64,
    MinMzTimestamp,
    MinFloat32,
    MinFloat64,
    MinBool,
    MinString,
    MinDate,
    MinTimestamp,
    MinTimestampTz,
    MinInterval,
    MinTime,
    SumInt16,
    SumInt32,
    SumInt64,
    SumUInt16,
    SumUInt32,
    SumUInt64,
    SumFloat32,
    SumFloat64,
    SumNumeric,
    Count,
    Any,
    All,
    /// Accumulates `Datum::List`s whose first element is a JSON-typed `Datum`s
    /// into a JSON list. The other elements are columns used by `order_by`.
    ///
    /// WARNING: Unlike the `jsonb_agg` function that is exposed by the SQL
    /// layer, this function filters out `Datum::Null`, for consistency with
    /// the other aggregate functions.
    JsonbAgg {
        order_by: Vec<ColumnOrder>,
    },
    /// Zips `Datum::List`s whose first element is a JSON-typed `Datum`s into a
    /// JSON map. The other elements are columns used by `order_by`.
    JsonbObjectAgg {
        order_by: Vec<ColumnOrder>,
    },
    /// Zips a `Datum::List` whose first element is a `Datum::List` guaranteed
    /// to be non-empty and whose len % 2 == 0 into a `Datum::Map`. The other
    /// elements are columns used by `order_by`.
    MapAgg {
        order_by: Vec<ColumnOrder>,
        value_type: ScalarType,
    },
    /// Accumulates `Datum::List`s whose first element is a `Datum::Array` into a
    /// single `Datum::Array`. The other elements are columns used by `order_by`.
    ArrayConcat {
        order_by: Vec<ColumnOrder>,
    },
    /// Accumulates `Datum::List`s whose first element is a `Datum::List` into a
    /// single `Datum::List`. The other elements are columns used by `order_by`.
    ListConcat {
        order_by: Vec<ColumnOrder>,
    },
    StringAgg {
        order_by: Vec<ColumnOrder>,
    },
    /// A bundle of fused window aggregations: its input is a record, whose each
    /// component will be the input to one of the `AggregateFunc`s.
    ///
    /// Importantly, this aggregation can only be present inside a `WindowExpr`,
    /// more specifically an `AggregateWindowExpr`.
    FusedWindowAgg {
        funcs: Vec<AggregateFunc>,
    },
    /// Accumulates any number of `Datum::Dummy`s into `Datum::Dummy`.
    ///
    /// Useful for removing an expensive aggregation while maintaining the shape
    /// of a reduce operator.
    Dummy,
}

impl AggregateFunc {
    /// Converts the `sql::AggregateFunc` to a corresponding `mz_expr::AggregateFunc`.
    pub fn into_expr(self) -> mz_expr::AggregateFunc {
        match self {
            AggregateFunc::MaxNumeric => mz_expr::AggregateFunc::MaxNumeric,
            AggregateFunc::MaxInt16 => mz_expr::AggregateFunc::MaxInt16,
            AggregateFunc::MaxInt32 => mz_expr::AggregateFunc::MaxInt32,
            AggregateFunc::MaxInt64 => mz_expr::AggregateFunc::MaxInt64,
            AggregateFunc::MaxUInt16 => mz_expr::AggregateFunc::MaxUInt16,
            AggregateFunc::MaxUInt32 => mz_expr::AggregateFunc::MaxUInt32,
            AggregateFunc::MaxUInt64 => mz_expr::AggregateFunc::MaxUInt64,
            AggregateFunc::MaxMzTimestamp => mz_expr::AggregateFunc::MaxMzTimestamp,
            AggregateFunc::MaxFloat32 => mz_expr::AggregateFunc::MaxFloat32,
            AggregateFunc::MaxFloat64 => mz_expr::AggregateFunc::MaxFloat64,
            AggregateFunc::MaxBool => mz_expr::AggregateFunc::MaxBool,
            AggregateFunc::MaxString => mz_expr::AggregateFunc::MaxString,
            AggregateFunc::MaxDate => mz_expr::AggregateFunc::MaxDate,
            AggregateFunc::MaxTimestamp => mz_expr::AggregateFunc::MaxTimestamp,
            AggregateFunc::MaxTimestampTz => mz_expr::AggregateFunc::MaxTimestampTz,
            AggregateFunc::MaxInterval => mz_expr::AggregateFunc::MaxInterval,
            AggregateFunc::MaxTime => mz_expr::AggregateFunc::MaxTime,
            AggregateFunc::MinNumeric => mz_expr::AggregateFunc::MinNumeric,
            AggregateFunc::MinInt16 => mz_expr::AggregateFunc::MinInt16,
            AggregateFunc::MinInt32 => mz_expr::AggregateFunc::MinInt32,
            AggregateFunc::MinInt64 => mz_expr::AggregateFunc::MinInt64,
            AggregateFunc::MinUInt16 => mz_expr::AggregateFunc::MinUInt16,
            AggregateFunc::MinUInt32 => mz_expr::AggregateFunc::MinUInt32,
            AggregateFunc::MinUInt64 => mz_expr::AggregateFunc::MinUInt64,
            AggregateFunc::MinMzTimestamp => mz_expr::AggregateFunc::MinMzTimestamp,
            AggregateFunc::MinFloat32 => mz_expr::AggregateFunc::MinFloat32,
            AggregateFunc::MinFloat64 => mz_expr::AggregateFunc::MinFloat64,
            AggregateFunc::MinBool => mz_expr::AggregateFunc::MinBool,
            AggregateFunc::MinString => mz_expr::AggregateFunc::MinString,
            AggregateFunc::MinDate => mz_expr::AggregateFunc::MinDate,
            AggregateFunc::MinTimestamp => mz_expr::AggregateFunc::MinTimestamp,
            AggregateFunc::MinTimestampTz => mz_expr::AggregateFunc::MinTimestampTz,
            AggregateFunc::MinInterval => mz_expr::AggregateFunc::MinInterval,
            AggregateFunc::MinTime => mz_expr::AggregateFunc::MinTime,
            AggregateFunc::SumInt16 => mz_expr::AggregateFunc::SumInt16,
            AggregateFunc::SumInt32 => mz_expr::AggregateFunc::SumInt32,
            AggregateFunc::SumInt64 => mz_expr::AggregateFunc::SumInt64,
            AggregateFunc::SumUInt16 => mz_expr::AggregateFunc::SumUInt16,
            AggregateFunc::SumUInt32 => mz_expr::AggregateFunc::SumUInt32,
            AggregateFunc::SumUInt64 => mz_expr::AggregateFunc::SumUInt64,
            AggregateFunc::SumFloat32 => mz_expr::AggregateFunc::SumFloat32,
            AggregateFunc::SumFloat64 => mz_expr::AggregateFunc::SumFloat64,
            AggregateFunc::SumNumeric => mz_expr::AggregateFunc::SumNumeric,
            AggregateFunc::Count => mz_expr::AggregateFunc::Count,
            AggregateFunc::Any => mz_expr::AggregateFunc::Any,
            AggregateFunc::All => mz_expr::AggregateFunc::All,
            AggregateFunc::JsonbAgg { order_by } => mz_expr::AggregateFunc::JsonbAgg { order_by },
            AggregateFunc::JsonbObjectAgg { order_by } => {
                mz_expr::AggregateFunc::JsonbObjectAgg { order_by }
            }
            AggregateFunc::MapAgg {
                order_by,
                value_type,
            } => mz_expr::AggregateFunc::MapAgg {
                order_by,
                value_type,
            },
            AggregateFunc::ArrayConcat { order_by } => {
                mz_expr::AggregateFunc::ArrayConcat { order_by }
            }
            AggregateFunc::ListConcat { order_by } => {
                mz_expr::AggregateFunc::ListConcat { order_by }
            }
            AggregateFunc::StringAgg { order_by } => mz_expr::AggregateFunc::StringAgg { order_by },
            // `AggregateFunc::FusedWindowAgg` should be specially handled in
            // `AggregateWindowExpr::into_expr`.
            AggregateFunc::FusedWindowAgg { funcs: _ } => {
                panic!("into_expr called on FusedWindowAgg")
            }
            AggregateFunc::Dummy => mz_expr::AggregateFunc::Dummy,
        }
    }

    /// Returns a datum whose inclusion in the aggregation will not change its
    /// result.
    ///
    /// # Panics
    ///
    /// Panics if called on a `FusedWindowAgg`.
    pub fn identity_datum(&self) -> Datum<'static> {
        match self {
            AggregateFunc::Any => Datum::False,
            AggregateFunc::All => Datum::True,
            AggregateFunc::Dummy => Datum::Dummy,
            AggregateFunc::ArrayConcat { .. } => Datum::empty_array(),
            AggregateFunc::ListConcat { .. } => Datum::empty_list(),
            AggregateFunc::MaxNumeric
            | AggregateFunc::MaxInt16
            | AggregateFunc::MaxInt32
            | AggregateFunc::MaxInt64
            | AggregateFunc::MaxUInt16
            | AggregateFunc::MaxUInt32
            | AggregateFunc::MaxUInt64
            | AggregateFunc::MaxMzTimestamp
            | AggregateFunc::MaxFloat32
            | AggregateFunc::MaxFloat64
            | AggregateFunc::MaxBool
            | AggregateFunc::MaxString
            | AggregateFunc::MaxDate
            | AggregateFunc::MaxTimestamp
            | AggregateFunc::MaxTimestampTz
            | AggregateFunc::MaxInterval
            | AggregateFunc::MaxTime
            | AggregateFunc::MinNumeric
            | AggregateFunc::MinInt16
            | AggregateFunc::MinInt32
            | AggregateFunc::MinInt64
            | AggregateFunc::MinUInt16
            | AggregateFunc::MinUInt32
            | AggregateFunc::MinUInt64
            | AggregateFunc::MinMzTimestamp
            | AggregateFunc::MinFloat32
            | AggregateFunc::MinFloat64
            | AggregateFunc::MinBool
            | AggregateFunc::MinString
            | AggregateFunc::MinDate
            | AggregateFunc::MinTimestamp
            | AggregateFunc::MinTimestampTz
            | AggregateFunc::MinInterval
            | AggregateFunc::MinTime
            | AggregateFunc::SumInt16
            | AggregateFunc::SumInt32
            | AggregateFunc::SumInt64
            | AggregateFunc::SumUInt16
            | AggregateFunc::SumUInt32
            | AggregateFunc::SumUInt64
            | AggregateFunc::SumFloat32
            | AggregateFunc::SumFloat64
            | AggregateFunc::SumNumeric
            | AggregateFunc::Count
            | AggregateFunc::JsonbAgg { .. }
            | AggregateFunc::JsonbObjectAgg { .. }
            | AggregateFunc::MapAgg { .. }
            | AggregateFunc::StringAgg { .. } => Datum::Null,
            AggregateFunc::FusedWindowAgg { funcs: _ } => {
                // `identity_datum` is used only in HIR planning, and `FusedWindowAgg` can't occur
                // in HIR planning, because it is introduced only during HIR transformation.
                //
                // The implementation could be something like the following, except that we need to
                // return a `Datum<'static>`, so we can't actually dynamically compute this.
                // ```
                // let temp_storage = RowArena::new();
                // temp_storage.make_datum(|packer| packer.push_list(funcs.iter().map(|f| f.identity_datum())))
                // ```
                panic!("FusedWindowAgg doesn't have an identity_datum")
            }
        }
    }

    /// The output column type for the result of an aggregation.
    ///
    /// The output column type also contains nullability information, which
    /// is (without further information) true for aggregations that are not
    /// counts.
    pub fn output_type(&self, input_type: ColumnType) -> ColumnType {
        let scalar_type = match self {
            AggregateFunc::Count => ScalarType::Int64,
            AggregateFunc::Any => ScalarType::Bool,
            AggregateFunc::All => ScalarType::Bool,
            AggregateFunc::JsonbAgg { .. } => ScalarType::Jsonb,
            AggregateFunc::JsonbObjectAgg { .. } => ScalarType::Jsonb,
            AggregateFunc::StringAgg { .. } => ScalarType::String,
            AggregateFunc::SumInt16 | AggregateFunc::SumInt32 => ScalarType::Int64,
            AggregateFunc::SumInt64 => ScalarType::Numeric {
                max_scale: Some(NumericMaxScale::ZERO),
            },
            AggregateFunc::SumUInt16 | AggregateFunc::SumUInt32 => ScalarType::UInt64,
            AggregateFunc::SumUInt64 => ScalarType::Numeric {
                max_scale: Some(NumericMaxScale::ZERO),
            },
            AggregateFunc::MapAgg { value_type, .. } => ScalarType::Map {
                value_type: Box::new(value_type.clone()),
                custom_id: None,
            },
            AggregateFunc::ArrayConcat { .. } | AggregateFunc::ListConcat { .. } => {
                match input_type.scalar_type {
                    // The input is wrapped in a Record if there's an ORDER BY, so extract it out.
                    ScalarType::Record { fields, .. } => fields[0].1.scalar_type.clone(),
                    _ => unreachable!(),
                }
            }
            AggregateFunc::MaxNumeric
            | AggregateFunc::MaxInt16
            | AggregateFunc::MaxInt32
            | AggregateFunc::MaxInt64
            | AggregateFunc::MaxUInt16
            | AggregateFunc::MaxUInt32
            | AggregateFunc::MaxUInt64
            | AggregateFunc::MaxMzTimestamp
            | AggregateFunc::MaxFloat32
            | AggregateFunc::MaxFloat64
            | AggregateFunc::MaxBool
            | AggregateFunc::MaxString
            | AggregateFunc::MaxDate
            | AggregateFunc::MaxTimestamp
            | AggregateFunc::MaxTimestampTz
            | AggregateFunc::MaxInterval
            | AggregateFunc::MaxTime
            | AggregateFunc::MinNumeric
            | AggregateFunc::MinInt16
            | AggregateFunc::MinInt32
            | AggregateFunc::MinInt64
            | AggregateFunc::MinUInt16
            | AggregateFunc::MinUInt32
            | AggregateFunc::MinUInt64
            | AggregateFunc::MinMzTimestamp
            | AggregateFunc::MinFloat32
            | AggregateFunc::MinFloat64
            | AggregateFunc::MinBool
            | AggregateFunc::MinString
            | AggregateFunc::MinDate
            | AggregateFunc::MinTimestamp
            | AggregateFunc::MinTimestampTz
            | AggregateFunc::MinInterval
            | AggregateFunc::MinTime
            | AggregateFunc::SumFloat32
            | AggregateFunc::SumFloat64
            | AggregateFunc::SumNumeric
            | AggregateFunc::Dummy => input_type.scalar_type,
            AggregateFunc::FusedWindowAgg { funcs } => {
                let input_types = input_type.scalar_type.unwrap_record_element_column_type();
                ScalarType::Record {
                    fields: funcs
                        .iter()
                        .zip_eq(input_types)
                        .map(|(f, t)| (ColumnName::from(""), f.output_type(t.clone())))
                        .collect(),
                    custom_id: None,
                }
            }
        };
        // max/min/sum return null on empty sets
        let nullable = !matches!(self, AggregateFunc::Count);
        scalar_type.nullable(nullable)
    }

    pub fn is_order_sensitive(&self) -> bool {
        use AggregateFunc::*;
        matches!(
            self,
            JsonbAgg { .. }
                | JsonbObjectAgg { .. }
                | MapAgg { .. }
                | ArrayConcat { .. }
                | ListConcat { .. }
                | StringAgg { .. }
        )
    }
}

impl HirRelationExpr {
    pub fn typ(
        &self,
        outers: &[RelationType],
        params: &BTreeMap<usize, ScalarType>,
    ) -> RelationType {
        stack::maybe_grow(|| match self {
            HirRelationExpr::Constant { typ, .. } => typ.clone(),
            HirRelationExpr::Get { typ, .. } => typ.clone(),
            HirRelationExpr::Let { body, .. } => body.typ(outers, params),
            HirRelationExpr::LetRec { body, .. } => body.typ(outers, params),
            HirRelationExpr::Project { input, outputs } => {
                let input_typ = input.typ(outers, params);
                RelationType::new(
                    outputs
                        .iter()
                        .map(|&i| input_typ.column_types[i].clone())
                        .collect(),
                )
            }
            HirRelationExpr::Map { input, scalars } => {
                let mut typ = input.typ(outers, params);
                for scalar in scalars {
                    typ.column_types.push(scalar.typ(outers, &typ, params));
                }
                typ
            }
            HirRelationExpr::CallTable { func, exprs: _ } => func.output_type(),
            HirRelationExpr::Filter { input, .. } | HirRelationExpr::TopK { input, .. } => {
                input.typ(outers, params)
            }
            HirRelationExpr::Join {
                left, right, kind, ..
            } => {
                let left_nullable = matches!(kind, JoinKind::RightOuter | JoinKind::FullOuter);
                let right_nullable =
                    matches!(kind, JoinKind::LeftOuter { .. } | JoinKind::FullOuter);
                let lt = left.typ(outers, params).column_types.into_iter().map(|t| {
                    let nullable = t.nullable || left_nullable;
                    t.nullable(nullable)
                });
                let mut outers = outers.to_vec();
                outers.insert(0, RelationType::new(lt.clone().collect()));
                let rt = right
                    .typ(&outers, params)
                    .column_types
                    .into_iter()
                    .map(|t| {
                        let nullable = t.nullable || right_nullable;
                        t.nullable(nullable)
                    });
                RelationType::new(lt.chain(rt).collect())
            }
            HirRelationExpr::Reduce {
                input,
                group_key,
                aggregates,
                expected_group_size: _,
            } => {
                let input_typ = input.typ(outers, params);
                let mut column_types = group_key
                    .iter()
                    .map(|&i| input_typ.column_types[i].clone())
                    .collect::<Vec<_>>();
                for agg in aggregates {
                    column_types.push(agg.typ(outers, &input_typ, params));
                }
                // TODO(frank): add primary key information.
                RelationType::new(column_types)
            }
            // TODO(frank): check for removal; add primary key information.
            HirRelationExpr::Distinct { input }
            | HirRelationExpr::Negate { input }
            | HirRelationExpr::Threshold { input } => input.typ(outers, params),
            HirRelationExpr::Union { base, inputs } => {
                let mut base_cols = base.typ(outers, params).column_types;
                for input in inputs {
                    for (base_col, col) in base_cols
                        .iter_mut()
                        .zip_eq(input.typ(outers, params).column_types)
                    {
                        *base_col = base_col.union(&col).unwrap();
                    }
                }
                RelationType::new(base_cols)
            }
        })
    }

    pub fn arity(&self) -> usize {
        match self {
            HirRelationExpr::Constant { typ, .. } => typ.column_types.len(),
            HirRelationExpr::Get { typ, .. } => typ.column_types.len(),
            HirRelationExpr::Let { body, .. } => body.arity(),
            HirRelationExpr::LetRec { body, .. } => body.arity(),
            HirRelationExpr::Project { outputs, .. } => outputs.len(),
            HirRelationExpr::Map { input, scalars } => input.arity() + scalars.len(),
            HirRelationExpr::CallTable { func, .. } => func.output_arity(),
            HirRelationExpr::Filter { input, .. }
            | HirRelationExpr::TopK { input, .. }
            | HirRelationExpr::Distinct { input }
            | HirRelationExpr::Negate { input }
            | HirRelationExpr::Threshold { input } => input.arity(),
            HirRelationExpr::Join { left, right, .. } => left.arity() + right.arity(),
            HirRelationExpr::Union { base, .. } => base.arity(),
            HirRelationExpr::Reduce {
                group_key,
                aggregates,
                ..
            } => group_key.len() + aggregates.len(),
        }
    }

    /// If self is a constant, return the value and the type, otherwise `None`.
    pub fn as_const(&self) -> Option<(&Vec<Row>, &RelationType)> {
        match self {
            Self::Constant { rows, typ } => Some((rows, typ)),
            _ => None,
        }
    }

    /// Reports whether this expression contains a column reference to its
    /// direct parent scope.
    pub fn is_correlated(&self) -> bool {
        let mut correlated = false;
        #[allow(deprecated)]
        self.visit_columns(0, &mut |depth, col| {
            if col.level > depth && col.level - depth == 1 {
                correlated = true;
            }
        });
        correlated
    }

    pub fn is_join_identity(&self) -> bool {
        match self {
            HirRelationExpr::Constant { rows, .. } => rows.len() == 1 && self.arity() == 0,
            _ => false,
        }
    }

    pub fn project(self, outputs: Vec<usize>) -> Self {
        if outputs.iter().copied().eq(0..self.arity()) {
            // The projection is trivial. Suppress it.
            self
        } else {
            HirRelationExpr::Project {
                input: Box::new(self),
                outputs,
            }
        }
    }

    pub fn map(mut self, scalars: Vec<HirScalarExpr>) -> Self {
        if scalars.is_empty() {
            // The map is trivial. Suppress it.
            self
        } else if let HirRelationExpr::Map {
            scalars: old_scalars,
            input: _,
        } = &mut self
        {
            // Map applied to a map. Fuse the maps.
            old_scalars.extend(scalars);
            self
        } else {
            HirRelationExpr::Map {
                input: Box::new(self),
                scalars,
            }
        }
    }

    pub fn filter(mut self, mut preds: Vec<HirScalarExpr>) -> Self {
        if let HirRelationExpr::Filter {
            input: _,
            predicates,
        } = &mut self
        {
            predicates.extend(preds);
            predicates.sort();
            predicates.dedup();
            self
        } else {
            preds.sort();
            preds.dedup();
            HirRelationExpr::Filter {
                input: Box::new(self),
                predicates: preds,
            }
        }
    }

    pub fn reduce(
        self,
        group_key: Vec<usize>,
        aggregates: Vec<AggregateExpr>,
        expected_group_size: Option<u64>,
    ) -> Self {
        HirRelationExpr::Reduce {
            input: Box::new(self),
            group_key,
            aggregates,
            expected_group_size,
        }
    }

    pub fn top_k(
        self,
        group_key: Vec<usize>,
        order_key: Vec<ColumnOrder>,
        limit: Option<HirScalarExpr>,
        offset: HirScalarExpr,
        expected_group_size: Option<u64>,
    ) -> Self {
        HirRelationExpr::TopK {
            input: Box::new(self),
            group_key,
            order_key,
            limit,
            offset,
            expected_group_size,
        }
    }

    pub fn negate(self) -> Self {
        if let HirRelationExpr::Negate { input } = self {
            *input
        } else {
            HirRelationExpr::Negate {
                input: Box::new(self),
            }
        }
    }

    pub fn distinct(self) -> Self {
        if let HirRelationExpr::Distinct { .. } = self {
            self
        } else {
            HirRelationExpr::Distinct {
                input: Box::new(self),
            }
        }
    }

    pub fn threshold(self) -> Self {
        if let HirRelationExpr::Threshold { .. } = self {
            self
        } else {
            HirRelationExpr::Threshold {
                input: Box::new(self),
            }
        }
    }

    pub fn union(self, other: Self) -> Self {
        let mut terms = Vec::new();
        if let HirRelationExpr::Union { base, inputs } = self {
            terms.push(*base);
            terms.extend(inputs);
        } else {
            terms.push(self);
        }
        if let HirRelationExpr::Union { base, inputs } = other {
            terms.push(*base);
            terms.extend(inputs);
        } else {
            terms.push(other);
        }
        HirRelationExpr::Union {
            base: Box::new(terms.remove(0)),
            inputs: terms,
        }
    }

    pub fn exists(self) -> HirScalarExpr {
        HirScalarExpr::Exists(Box::new(self), NameMetadata::default())
    }

    pub fn select(self) -> HirScalarExpr {
        HirScalarExpr::Select(Box::new(self), NameMetadata::default())
    }

    pub fn join(
        self,
        mut right: HirRelationExpr,
        on: HirScalarExpr,
        kind: JoinKind,
    ) -> HirRelationExpr {
        if self.is_join_identity() && !right.is_correlated() && on == HirScalarExpr::literal_true()
        {
            // The join can be elided, but we need to adjust column references
            // on the right-hand side to account for the removal of the scope
            // introduced by the join.
            #[allow(deprecated)]
            right.visit_columns_mut(0, &mut |depth, col| {
                if col.level > depth {
                    col.level -= 1;
                }
            });
            right
        } else if right.is_join_identity() && on == HirScalarExpr::literal_true() {
            self
        } else {
            HirRelationExpr::Join {
                left: Box::new(self),
                right: Box::new(right),
                on,
                kind,
            }
        }
    }

    pub fn take(&mut self) -> HirRelationExpr {
        mem::replace(
            self,
            HirRelationExpr::constant(vec![], RelationType::new(Vec::new())),
        )
    }

    #[deprecated = "Use `Visit::visit_post`."]
    pub fn visit<'a, F>(&'a self, depth: usize, f: &mut F)
    where
        F: FnMut(&'a Self, usize),
    {
        #[allow(deprecated)]
        let _ = self.visit_fallible(depth, &mut |e: &HirRelationExpr,
                                                 depth: usize|
         -> Result<(), ()> {
            f(e, depth);
            Ok(())
        });
    }

    #[deprecated = "Use `Visit::try_visit_post`."]
    pub fn visit_fallible<'a, F, E>(&'a self, depth: usize, f: &mut F) -> Result<(), E>
    where
        F: FnMut(&'a Self, usize) -> Result<(), E>,
    {
        #[allow(deprecated)]
        self.visit1(depth, |e: &HirRelationExpr, depth: usize| {
            e.visit_fallible(depth, f)
        })?;
        f(self, depth)
    }

    #[deprecated = "Use `VisitChildren<HirRelationExpr>::try_visit_children` instead."]
    pub fn visit1<'a, F, E>(&'a self, depth: usize, mut f: F) -> Result<(), E>
    where
        F: FnMut(&'a Self, usize) -> Result<(), E>,
    {
        match self {
            HirRelationExpr::Constant { .. }
            | HirRelationExpr::Get { .. }
            | HirRelationExpr::CallTable { .. } => (),
            HirRelationExpr::Let { body, value, .. } => {
                f(value, depth)?;
                f(body, depth)?;
            }
            HirRelationExpr::LetRec {
                limit: _,
                bindings,
                body,
            } => {
                for (_, _, value, _) in bindings.iter() {
                    f(value, depth)?;
                }
                f(body, depth)?;
            }
            HirRelationExpr::Project { input, .. } => {
                f(input, depth)?;
            }
            HirRelationExpr::Map { input, .. } => {
                f(input, depth)?;
            }
            HirRelationExpr::Filter { input, .. } => {
                f(input, depth)?;
            }
            HirRelationExpr::Join { left, right, .. } => {
                f(left, depth)?;
                f(right, depth + 1)?;
            }
            HirRelationExpr::Reduce { input, .. } => {
                f(input, depth)?;
            }
            HirRelationExpr::Distinct { input } => {
                f(input, depth)?;
            }
            HirRelationExpr::TopK { input, .. } => {
                f(input, depth)?;
            }
            HirRelationExpr::Negate { input } => {
                f(input, depth)?;
            }
            HirRelationExpr::Threshold { input } => {
                f(input, depth)?;
            }
            HirRelationExpr::Union { base, inputs } => {
                f(base, depth)?;
                for input in inputs {
                    f(input, depth)?;
                }
            }
        }
        Ok(())
    }

    #[deprecated = "Use `Visit::visit_mut_post` instead."]
    pub fn visit_mut<F>(&mut self, depth: usize, f: &mut F)
    where
        F: FnMut(&mut Self, usize),
    {
        #[allow(deprecated)]
        let _ = self.visit_mut_fallible(depth, &mut |e: &mut HirRelationExpr,
                                                     depth: usize|
         -> Result<(), ()> {
            f(e, depth);
            Ok(())
        });
    }

    #[deprecated = "Use `Visit::try_visit_mut_post` instead."]
    pub fn visit_mut_fallible<F, E>(&mut self, depth: usize, f: &mut F) -> Result<(), E>
    where
        F: FnMut(&mut Self, usize) -> Result<(), E>,
    {
        #[allow(deprecated)]
        self.visit1_mut(depth, |e: &mut HirRelationExpr, depth: usize| {
            e.visit_mut_fallible(depth, f)
        })?;
        f(self, depth)
    }

    #[deprecated = "Use `VisitChildren<HirRelationExpr>::try_visit_mut_children` instead."]
    pub fn visit1_mut<'a, F, E>(&'a mut self, depth: usize, mut f: F) -> Result<(), E>
    where
        F: FnMut(&'a mut Self, usize) -> Result<(), E>,
    {
        match self {
            HirRelationExpr::Constant { .. }
            | HirRelationExpr::Get { .. }
            | HirRelationExpr::CallTable { .. } => (),
            HirRelationExpr::Let { body, value, .. } => {
                f(value, depth)?;
                f(body, depth)?;
            }
            HirRelationExpr::LetRec {
                limit: _,
                bindings,
                body,
            } => {
                for (_, _, value, _) in bindings.iter_mut() {
                    f(value, depth)?;
                }
                f(body, depth)?;
            }
            HirRelationExpr::Project { input, .. } => {
                f(input, depth)?;
            }
            HirRelationExpr::Map { input, .. } => {
                f(input, depth)?;
            }
            HirRelationExpr::Filter { input, .. } => {
                f(input, depth)?;
            }
            HirRelationExpr::Join { left, right, .. } => {
                f(left, depth)?;
                f(right, depth + 1)?;
            }
            HirRelationExpr::Reduce { input, .. } => {
                f(input, depth)?;
            }
            HirRelationExpr::Distinct { input } => {
                f(input, depth)?;
            }
            HirRelationExpr::TopK { input, .. } => {
                f(input, depth)?;
            }
            HirRelationExpr::Negate { input } => {
                f(input, depth)?;
            }
            HirRelationExpr::Threshold { input } => {
                f(input, depth)?;
            }
            HirRelationExpr::Union { base, inputs } => {
                f(base, depth)?;
                for input in inputs {
                    f(input, depth)?;
                }
            }
        }
        Ok(())
    }

    #[deprecated = "Use a combination of `Visit` and `VisitChildren` methods."]
    /// Visits all scalar expressions within the sub-tree of the given relation.
    ///
    /// The `depth` argument should indicate the subquery nesting depth of the expression,
    /// which will be incremented when entering the RHS of a join or a subquery and
    /// presented to the supplied function `f`.
    pub fn visit_scalar_expressions<F, E>(&self, depth: usize, f: &mut F) -> Result<(), E>
    where
        F: FnMut(&HirScalarExpr, usize) -> Result<(), E>,
    {
        #[allow(deprecated)]
        self.visit_fallible(depth, &mut |e: &HirRelationExpr,
                                         depth: usize|
         -> Result<(), E> {
            match e {
                HirRelationExpr::Join { on, .. } => {
                    f(on, depth)?;
                }
                HirRelationExpr::Map { scalars, .. } => {
                    for scalar in scalars {
                        f(scalar, depth)?;
                    }
                }
                HirRelationExpr::CallTable { exprs, .. } => {
                    for expr in exprs {
                        f(expr, depth)?;
                    }
                }
                HirRelationExpr::Filter { predicates, .. } => {
                    for predicate in predicates {
                        f(predicate, depth)?;
                    }
                }
                HirRelationExpr::Reduce { aggregates, .. } => {
                    for aggregate in aggregates {
                        f(&aggregate.expr, depth)?;
                    }
                }
                HirRelationExpr::TopK { limit, offset, .. } => {
                    if let Some(limit) = limit {
                        f(limit, depth)?;
                    }
                    f(offset, depth)?;
                }
                HirRelationExpr::Union { .. }
                | HirRelationExpr::Let { .. }
                | HirRelationExpr::LetRec { .. }
                | HirRelationExpr::Project { .. }
                | HirRelationExpr::Distinct { .. }
                | HirRelationExpr::Negate { .. }
                | HirRelationExpr::Threshold { .. }
                | HirRelationExpr::Constant { .. }
                | HirRelationExpr::Get { .. } => (),
            }
            Ok(())
        })
    }

    #[deprecated = "Use a combination of `Visit` and `VisitChildren` methods."]
    /// Like `visit_scalar_expressions`, but permits mutating the expressions.
    pub fn visit_scalar_expressions_mut<F, E>(&mut self, depth: usize, f: &mut F) -> Result<(), E>
    where
        F: FnMut(&mut HirScalarExpr, usize) -> Result<(), E>,
    {
        #[allow(deprecated)]
        self.visit_mut_fallible(depth, &mut |e: &mut HirRelationExpr,
                                             depth: usize|
         -> Result<(), E> {
            match e {
                HirRelationExpr::Join { on, .. } => {
                    f(on, depth)?;
                }
                HirRelationExpr::Map { scalars, .. } => {
                    for scalar in scalars.iter_mut() {
                        f(scalar, depth)?;
                    }
                }
                HirRelationExpr::CallTable { exprs, .. } => {
                    for expr in exprs.iter_mut() {
                        f(expr, depth)?;
                    }
                }
                HirRelationExpr::Filter { predicates, .. } => {
                    for predicate in predicates.iter_mut() {
                        f(predicate, depth)?;
                    }
                }
                HirRelationExpr::Reduce { aggregates, .. } => {
                    for aggregate in aggregates.iter_mut() {
                        f(&mut aggregate.expr, depth)?;
                    }
                }
                HirRelationExpr::TopK { limit, offset, .. } => {
                    if let Some(limit) = limit {
                        f(limit, depth)?;
                    }
                    f(offset, depth)?;
                }
                HirRelationExpr::Union { .. }
                | HirRelationExpr::Let { .. }
                | HirRelationExpr::LetRec { .. }
                | HirRelationExpr::Project { .. }
                | HirRelationExpr::Distinct { .. }
                | HirRelationExpr::Negate { .. }
                | HirRelationExpr::Threshold { .. }
                | HirRelationExpr::Constant { .. }
                | HirRelationExpr::Get { .. } => (),
            }
            Ok(())
        })
    }

    #[deprecated = "Redefine this based on the `Visit` and `VisitChildren` methods."]
    /// Visits the column references in this relation expression.
    ///
    /// The `depth` argument should indicate the subquery nesting depth of the expression,
    /// which will be incremented when entering the RHS of a join or a subquery and
    /// presented to the supplied function `f`.
    pub fn visit_columns<F>(&self, depth: usize, f: &mut F)
    where
        F: FnMut(usize, &ColumnRef),
    {
        #[allow(deprecated)]
        let _ = self.visit_scalar_expressions(depth, &mut |e: &HirScalarExpr,
                                                           depth: usize|
         -> Result<(), ()> {
            e.visit_columns(depth, f);
            Ok(())
        });
    }

    #[deprecated = "Redefine this based on the `Visit` and `VisitChildren` methods."]
    /// Like `visit_columns`, but permits mutating the column references.
    pub fn visit_columns_mut<F>(&mut self, depth: usize, f: &mut F)
    where
        F: FnMut(usize, &mut ColumnRef),
    {
        #[allow(deprecated)]
        let _ = self.visit_scalar_expressions_mut(depth, &mut |e: &mut HirScalarExpr,
                                                               depth: usize|
         -> Result<(), ()> {
            e.visit_columns_mut(depth, f);
            Ok(())
        });
    }

    /// Replaces any parameter references in the expression with the
    /// corresponding datum from `params`.
    pub fn bind_parameters(
        &mut self,
        scx: &StatementContext,
        lifetime: QueryLifetime,
        params: &Params,
    ) -> Result<(), PlanError> {
        #[allow(deprecated)]
        self.visit_scalar_expressions_mut(0, &mut |e: &mut HirScalarExpr, _: usize| {
            e.bind_parameters(scx, lifetime, params)
        })
    }

    pub fn contains_parameters(&self) -> Result<bool, PlanError> {
        let mut contains_parameters = false;
        #[allow(deprecated)]
        self.visit_scalar_expressions(0, &mut |e: &HirScalarExpr, _: usize| {
            if e.contains_parameters() {
                contains_parameters = true;
            }
            Ok::<(), PlanError>(())
        })?;
        Ok(contains_parameters)
    }

    /// See the documentation for [`HirScalarExpr::splice_parameters`].
    pub fn splice_parameters(&mut self, params: &[HirScalarExpr], depth: usize) {
        #[allow(deprecated)]
        let _ = self.visit_scalar_expressions_mut(depth, &mut |e: &mut HirScalarExpr,
                                                               depth: usize|
         -> Result<(), ()> {
            e.splice_parameters(params, depth);
            Ok(())
        });
    }

    /// Constructs a constant collection from specific rows and schema.
    pub fn constant(rows: Vec<Vec<Datum>>, typ: RelationType) -> Self {
        let rows = rows
            .into_iter()
            .map(move |datums| Row::pack_slice(&datums))
            .collect();
        HirRelationExpr::Constant { rows, typ }
    }

    /// A `RowSetFinishing` can only be directly applied to the result of a one-shot select.
    /// This function is concerned with maintained queries, e.g., an index or materialized view.
    /// Instead of directly applying the given `RowSetFinishing`, it converts the `RowSetFinishing`
    /// to a `TopK`, which it then places at the top of `self`. Additionally, it turns the given
    /// finishing into a trivial finishing.
    pub fn finish_maintained(
        &mut self,
        finishing: &mut RowSetFinishing<HirScalarExpr, HirScalarExpr>,
        group_size_hints: GroupSizeHints,
    ) {
        if !HirRelationExpr::is_trivial_row_set_finishing_hir(finishing, self.arity()) {
            let old_finishing = mem::replace(
                finishing,
                HirRelationExpr::trivial_row_set_finishing_hir(finishing.project.len()),
            );
            *self = HirRelationExpr::top_k(
                std::mem::replace(
                    self,
                    HirRelationExpr::Constant {
                        rows: vec![],
                        typ: RelationType::new(Vec::new()),
                    },
                ),
                vec![],
                old_finishing.order_by,
                old_finishing.limit,
                old_finishing.offset,
                group_size_hints.limit_input_group_size,
            )
            .project(old_finishing.project);
        }
    }

    /// Returns a trivial finishing, i.e., that does nothing to the result set.
    ///
    /// (There is also `RowSetFinishing::trivial`, but that is specialized for when the O generic
    /// parameter is not an HirScalarExpr anymore.)
    pub fn trivial_row_set_finishing_hir(
        arity: usize,
    ) -> RowSetFinishing<HirScalarExpr, HirScalarExpr> {
        RowSetFinishing {
            order_by: Vec::new(),
            limit: None,
            offset: HirScalarExpr::literal(Datum::Int64(0), ScalarType::Int64),
            project: (0..arity).collect(),
        }
    }

    /// True if the finishing does nothing to any result set.
    ///
    /// (There is also `RowSetFinishing::is_trivial`, but that is specialized for when the O generic
    /// parameter is not an HirScalarExpr anymore.)
    pub fn is_trivial_row_set_finishing_hir(
        rsf: &RowSetFinishing<HirScalarExpr, HirScalarExpr>,
        arity: usize,
    ) -> bool {
        rsf.limit.is_none()
            && rsf.order_by.is_empty()
            && rsf
                .offset
                .clone()
                .try_into_literal_int64()
                .is_ok_and(|o| o == 0)
            && rsf.project.iter().copied().eq(0..arity)
    }

    /// The HirRelationExpr is considered potentially expensive if and only if
    /// at least one of the following conditions is true:
    ///
    ///  - It contains at least one CallTable or a Reduce operator.
    ///  - It contains at least one HirScalarExpr with a function call.
    ///
    /// !!!WARNING!!!: this method has an MirRelationExpr counterpart. The two
    /// should be kept in sync w.r.t. HIR ⇒ MIR lowering!
    pub fn could_run_expensive_function(&self) -> bool {
        let mut result = false;
        if let Err(_) = self.visit_pre(&mut |e: &HirRelationExpr| {
            use HirRelationExpr::*;
            use HirScalarExpr::*;

            self.visit_children(|scalar: &HirScalarExpr| {
                if let Err(_) = scalar.visit_pre(&mut |scalar: &HirScalarExpr| {
                    result |= match scalar {
                        Column(..)
                        | Literal(..)
                        | CallUnmaterializable(..)
                        | If { .. }
                        | Parameter(..)
                        | Select(..)
                        | Exists(..) => false,
                        // Function calls are considered expensive
                        CallUnary { .. }
                        | CallBinary { .. }
                        | CallVariadic { .. }
                        | Windowing(..) => true,
                    };
                }) {
                    // Conservatively set `true` on RecursionLimitError.
                    result = true;
                }
            });

            // CallTable has a table function; Reduce has an aggregate function.
            // Other constructs use MirScalarExpr to run a function
            result |= matches!(e, CallTable { .. } | Reduce { .. });
        }) {
            // Conservatively set `true` on RecursionLimitError.
            result = true;
        }

        result
    }

    /// Whether the expression contains an [`UnmaterializableFunc::MzNow`] call.
    pub fn contains_temporal(&self) -> Result<bool, RecursionLimitError> {
        let mut contains = false;
        self.visit_post(&mut |expr| {
            expr.visit_children(|expr: &HirScalarExpr| {
                contains = contains || expr.contains_temporal()
            })
        })?;
        Ok(contains)
    }
}

impl CollectionPlan for HirRelationExpr {
    // !!!WARNING!!!: this method has an MirRelationExpr counterpart. The two
    // should be kept in sync w.r.t. HIR ⇒ MIR lowering!
    fn depends_on_into(&self, out: &mut BTreeSet<GlobalId>) {
        if let Self::Get {
            id: Id::Global(id), ..
        } = self
        {
            out.insert(*id);
        }
        self.visit_children(|expr: &HirRelationExpr| expr.depends_on_into(out))
    }
}

impl VisitChildren<Self> for HirRelationExpr {
    fn visit_children<F>(&self, mut f: F)
    where
        F: FnMut(&Self),
    {
        // subqueries of type HirRelationExpr might be wrapped in
        // Exists or Select variants within HirScalarExpr trees
        // attached at the current node, and we want to visit them as well
        VisitChildren::visit_children(self, |expr: &HirScalarExpr| {
            #[allow(deprecated)]
            Visit::visit_post_nolimit(expr, &mut |expr| match expr {
                HirScalarExpr::Exists(expr, _name) | HirScalarExpr::Select(expr, _name) => {
                    f(expr.as_ref())
                }
                _ => (),
            });
        });

        use HirRelationExpr::*;
        match self {
            Constant { rows: _, typ: _ } | Get { id: _, typ: _ } => (),
            Let {
                name: _,
                id: _,
                value,
                body,
            } => {
                f(value);
                f(body);
            }
            LetRec {
                limit: _,
                bindings,
                body,
            } => {
                for (_, _, value, _) in bindings.iter() {
                    f(value);
                }
                f(body);
            }
            Project { input, outputs: _ } => f(input),
            Map { input, scalars: _ } => {
                f(input);
            }
            CallTable { func: _, exprs: _ } => (),
            Filter {
                input,
                predicates: _,
            } => {
                f(input);
            }
            Join {
                left,
                right,
                on: _,
                kind: _,
            } => {
                f(left);
                f(right);
            }
            Reduce {
                input,
                group_key: _,
                aggregates: _,
                expected_group_size: _,
            } => {
                f(input);
            }
            Distinct { input }
            | TopK {
                input,
                group_key: _,
                order_key: _,
                limit: _,
                offset: _,
                expected_group_size: _,
            }
            | Negate { input }
            | Threshold { input } => {
                f(input);
            }
            Union { base, inputs } => {
                f(base);
                for input in inputs {
                    f(input);
                }
            }
        }
    }

    fn visit_mut_children<F>(&mut self, mut f: F)
    where
        F: FnMut(&mut Self),
    {
        // subqueries of type HirRelationExpr might be wrapped in
        // Exists or Select variants within HirScalarExpr trees
        // attached at the current node, and we want to visit them as well
        VisitChildren::visit_mut_children(self, |expr: &mut HirScalarExpr| {
            #[allow(deprecated)]
            Visit::visit_mut_post_nolimit(expr, &mut |expr| match expr {
                HirScalarExpr::Exists(expr, _name) | HirScalarExpr::Select(expr, _name) => {
                    f(expr.as_mut())
                }
                _ => (),
            });
        });

        use HirRelationExpr::*;
        match self {
            Constant { rows: _, typ: _ } | Get { id: _, typ: _ } => (),
            Let {
                name: _,
                id: _,
                value,
                body,
            } => {
                f(value);
                f(body);
            }
            LetRec {
                limit: _,
                bindings,
                body,
            } => {
                for (_, _, value, _) in bindings.iter_mut() {
                    f(value);
                }
                f(body);
            }
            Project { input, outputs: _ } => f(input),
            Map { input, scalars: _ } => {
                f(input);
            }
            CallTable { func: _, exprs: _ } => (),
            Filter {
                input,
                predicates: _,
            } => {
                f(input);
            }
            Join {
                left,
                right,
                on: _,
                kind: _,
            } => {
                f(left);
                f(right);
            }
            Reduce {
                input,
                group_key: _,
                aggregates: _,
                expected_group_size: _,
            } => {
                f(input);
            }
            Distinct { input }
            | TopK {
                input,
                group_key: _,
                order_key: _,
                limit: _,
                offset: _,
                expected_group_size: _,
            }
            | Negate { input }
            | Threshold { input } => {
                f(input);
            }
            Union { base, inputs } => {
                f(base);
                for input in inputs {
                    f(input);
                }
            }
        }
    }

    fn try_visit_children<F, E>(&self, mut f: F) -> Result<(), E>
    where
        F: FnMut(&Self) -> Result<(), E>,
        E: From<RecursionLimitError>,
    {
        // subqueries of type HirRelationExpr might be wrapped in
        // Exists or Select variants within HirScalarExpr trees
        // attached at the current node, and we want to visit them as well
        VisitChildren::try_visit_children(self, |expr: &HirScalarExpr| {
            Visit::try_visit_post(expr, &mut |expr| match expr {
                HirScalarExpr::Exists(expr, _name) | HirScalarExpr::Select(expr, _name) => {
                    f(expr.as_ref())
                }
                _ => Ok(()),
            })
        })?;

        use HirRelationExpr::*;
        match self {
            Constant { rows: _, typ: _ } | Get { id: _, typ: _ } => (),
            Let {
                name: _,
                id: _,
                value,
                body,
            } => {
                f(value)?;
                f(body)?;
            }
            LetRec {
                limit: _,
                bindings,
                body,
            } => {
                for (_, _, value, _) in bindings.iter() {
                    f(value)?;
                }
                f(body)?;
            }
            Project { input, outputs: _ } => f(input)?,
            Map { input, scalars: _ } => {
                f(input)?;
            }
            CallTable { func: _, exprs: _ } => (),
            Filter {
                input,
                predicates: _,
            } => {
                f(input)?;
            }
            Join {
                left,
                right,
                on: _,
                kind: _,
            } => {
                f(left)?;
                f(right)?;
            }
            Reduce {
                input,
                group_key: _,
                aggregates: _,
                expected_group_size: _,
            } => {
                f(input)?;
            }
            Distinct { input }
            | TopK {
                input,
                group_key: _,
                order_key: _,
                limit: _,
                offset: _,
                expected_group_size: _,
            }
            | Negate { input }
            | Threshold { input } => {
                f(input)?;
            }
            Union { base, inputs } => {
                f(base)?;
                for input in inputs {
                    f(input)?;
                }
            }
        }
        Ok(())
    }

    fn try_visit_mut_children<F, E>(&mut self, mut f: F) -> Result<(), E>
    where
        F: FnMut(&mut Self) -> Result<(), E>,
        E: From<RecursionLimitError>,
    {
        // subqueries of type HirRelationExpr might be wrapped in
        // Exists or Select variants within HirScalarExpr trees
        // attached at the current node, and we want to visit them as well
        VisitChildren::try_visit_mut_children(self, |expr: &mut HirScalarExpr| {
            Visit::try_visit_mut_post(expr, &mut |expr| match expr {
                HirScalarExpr::Exists(expr, _name) | HirScalarExpr::Select(expr, _name) => {
                    f(expr.as_mut())
                }
                _ => Ok(()),
            })
        })?;

        use HirRelationExpr::*;
        match self {
            Constant { rows: _, typ: _ } | Get { id: _, typ: _ } => (),
            Let {
                name: _,
                id: _,
                value,
                body,
            } => {
                f(value)?;
                f(body)?;
            }
            LetRec {
                limit: _,
                bindings,
                body,
            } => {
                for (_, _, value, _) in bindings.iter_mut() {
                    f(value)?;
                }
                f(body)?;
            }
            Project { input, outputs: _ } => f(input)?,
            Map { input, scalars: _ } => {
                f(input)?;
            }
            CallTable { func: _, exprs: _ } => (),
            Filter {
                input,
                predicates: _,
            } => {
                f(input)?;
            }
            Join {
                left,
                right,
                on: _,
                kind: _,
            } => {
                f(left)?;
                f(right)?;
            }
            Reduce {
                input,
                group_key: _,
                aggregates: _,
                expected_group_size: _,
            } => {
                f(input)?;
            }
            Distinct { input }
            | TopK {
                input,
                group_key: _,
                order_key: _,
                limit: _,
                offset: _,
                expected_group_size: _,
            }
            | Negate { input }
            | Threshold { input } => {
                f(input)?;
            }
            Union { base, inputs } => {
                f(base)?;
                for input in inputs {
                    f(input)?;
                }
            }
        }
        Ok(())
    }
}

impl VisitChildren<HirScalarExpr> for HirRelationExpr {
    fn visit_children<F>(&self, mut f: F)
    where
        F: FnMut(&HirScalarExpr),
    {
        use HirRelationExpr::*;
        match self {
            Constant { rows: _, typ: _ }
            | Get { id: _, typ: _ }
            | Let {
                name: _,
                id: _,
                value: _,
                body: _,
            }
            | LetRec {
                limit: _,
                bindings: _,
                body: _,
            }
            | Project {
                input: _,
                outputs: _,
            } => (),
            Map { input: _, scalars } => {
                for scalar in scalars {
                    f(scalar);
                }
            }
            CallTable { func: _, exprs } => {
                for expr in exprs {
                    f(expr);
                }
            }
            Filter {
                input: _,
                predicates,
            } => {
                for predicate in predicates {
                    f(predicate);
                }
            }
            Join {
                left: _,
                right: _,
                on,
                kind: _,
            } => f(on),
            Reduce {
                input: _,
                group_key: _,
                aggregates,
                expected_group_size: _,
            } => {
                for aggregate in aggregates {
                    f(aggregate.expr.as_ref());
                }
            }
            TopK {
                input: _,
                group_key: _,
                order_key: _,
                limit,
                offset,
                expected_group_size: _,
            } => {
                if let Some(limit) = limit {
                    f(limit)
                }
                f(offset)
            }
            Distinct { input: _ }
            | Negate { input: _ }
            | Threshold { input: _ }
            | Union { base: _, inputs: _ } => (),
        }
    }

    fn visit_mut_children<F>(&mut self, mut f: F)
    where
        F: FnMut(&mut HirScalarExpr),
    {
        use HirRelationExpr::*;
        match self {
            Constant { rows: _, typ: _ }
            | Get { id: _, typ: _ }
            | Let {
                name: _,
                id: _,
                value: _,
                body: _,
            }
            | LetRec {
                limit: _,
                bindings: _,
                body: _,
            }
            | Project {
                input: _,
                outputs: _,
            } => (),
            Map { input: _, scalars } => {
                for scalar in scalars {
                    f(scalar);
                }
            }
            CallTable { func: _, exprs } => {
                for expr in exprs {
                    f(expr);
                }
            }
            Filter {
                input: _,
                predicates,
            } => {
                for predicate in predicates {
                    f(predicate);
                }
            }
            Join {
                left: _,
                right: _,
                on,
                kind: _,
            } => f(on),
            Reduce {
                input: _,
                group_key: _,
                aggregates,
                expected_group_size: _,
            } => {
                for aggregate in aggregates {
                    f(aggregate.expr.as_mut());
                }
            }
            TopK {
                input: _,
                group_key: _,
                order_key: _,
                limit,
                offset,
                expected_group_size: _,
            } => {
                if let Some(limit) = limit {
                    f(limit)
                }
                f(offset)
            }
            Distinct { input: _ }
            | Negate { input: _ }
            | Threshold { input: _ }
            | Union { base: _, inputs: _ } => (),
        }
    }

    fn try_visit_children<F, E>(&self, mut f: F) -> Result<(), E>
    where
        F: FnMut(&HirScalarExpr) -> Result<(), E>,
        E: From<RecursionLimitError>,
    {
        use HirRelationExpr::*;
        match self {
            Constant { rows: _, typ: _ }
            | Get { id: _, typ: _ }
            | Let {
                name: _,
                id: _,
                value: _,
                body: _,
            }
            | LetRec {
                limit: _,
                bindings: _,
                body: _,
            }
            | Project {
                input: _,
                outputs: _,
            } => (),
            Map { input: _, scalars } => {
                for scalar in scalars {
                    f(scalar)?;
                }
            }
            CallTable { func: _, exprs } => {
                for expr in exprs {
                    f(expr)?;
                }
            }
            Filter {
                input: _,
                predicates,
            } => {
                for predicate in predicates {
                    f(predicate)?;
                }
            }
            Join {
                left: _,
                right: _,
                on,
                kind: _,
            } => f(on)?,
            Reduce {
                input: _,
                group_key: _,
                aggregates,
                expected_group_size: _,
            } => {
                for aggregate in aggregates {
                    f(aggregate.expr.as_ref())?;
                }
            }
            TopK {
                input: _,
                group_key: _,
                order_key: _,
                limit,
                offset,
                expected_group_size: _,
            } => {
                if let Some(limit) = limit {
                    f(limit)?
                }
                f(offset)?
            }
            Distinct { input: _ }
            | Negate { input: _ }
            | Threshold { input: _ }
            | Union { base: _, inputs: _ } => (),
        }
        Ok(())
    }

    fn try_visit_mut_children<F, E>(&mut self, mut f: F) -> Result<(), E>
    where
        F: FnMut(&mut HirScalarExpr) -> Result<(), E>,
        E: From<RecursionLimitError>,
    {
        use HirRelationExpr::*;
        match self {
            Constant { rows: _, typ: _ }
            | Get { id: _, typ: _ }
            | Let {
                name: _,
                id: _,
                value: _,
                body: _,
            }
            | LetRec {
                limit: _,
                bindings: _,
                body: _,
            }
            | Project {
                input: _,
                outputs: _,
            } => (),
            Map { input: _, scalars } => {
                for scalar in scalars {
                    f(scalar)?;
                }
            }
            CallTable { func: _, exprs } => {
                for expr in exprs {
                    f(expr)?;
                }
            }
            Filter {
                input: _,
                predicates,
            } => {
                for predicate in predicates {
                    f(predicate)?;
                }
            }
            Join {
                left: _,
                right: _,
                on,
                kind: _,
            } => f(on)?,
            Reduce {
                input: _,
                group_key: _,
                aggregates,
                expected_group_size: _,
            } => {
                for aggregate in aggregates {
                    f(aggregate.expr.as_mut())?;
                }
            }
            TopK {
                input: _,
                group_key: _,
                order_key: _,
                limit,
                offset,
                expected_group_size: _,
            } => {
                if let Some(limit) = limit {
                    f(limit)?
                }
                f(offset)?
            }
            Distinct { input: _ }
            | Negate { input: _ }
            | Threshold { input: _ }
            | Union { base: _, inputs: _ } => (),
        }
        Ok(())
    }
}

impl HirScalarExpr {
    pub fn name(&self) -> Option<Arc<str>> {
        use HirScalarExpr::*;
        match self {
            Column(_, name)
            | Parameter(_, name)
            | Literal(_, _, name)
            | CallUnmaterializable(_, name)
            | CallUnary { name, .. }
            | CallBinary { name, .. }
            | CallVariadic { name, .. }
            | If { name, .. }
            | Exists(_, name)
            | Select(_, name)
            | Windowing(_, name) => name.0.clone(),
        }
    }

    /// Replaces any parameter references in the expression with the
    /// corresponding datum in `params`.
    pub fn bind_parameters(
        &mut self,
        scx: &StatementContext,
        lifetime: QueryLifetime,
        params: &Params,
    ) -> Result<(), PlanError> {
        #[allow(deprecated)]
        self.visit_recursively_mut(0, &mut |_: usize, e: &mut HirScalarExpr| {
            if let HirScalarExpr::Parameter(n, name) = e {
                let datum = match params.datums.iter().nth(*n - 1) {
                    None => return Err(PlanError::UnknownParameter(*n)),
                    Some(datum) => datum,
                };
                let scalar_type = &params.execute_types[*n - 1];
                let row = Row::pack([datum]);
                let column_type = scalar_type.clone().nullable(datum.is_null());

                let name = if let Some(name) = &name.0 {
                    Some(Arc::clone(name))
                } else {
                    Some(Arc::from(format!("${n}")))
                };

                let qcx = QueryContext::root(scx, lifetime);
                let ecx = execute_expr_context(&qcx);

                *e = plan_cast(
                    &ecx,
                    *EXECUTE_CAST_CONTEXT,
                    HirScalarExpr::Literal(row, column_type, TreatAsEqual(name)),
                    &params.expected_types[*n - 1],
                )
                .expect("checked in plan_params");
            }
            Ok(())
        })
    }

    /// Like [`HirScalarExpr::bind_parameters`], except that parameters are
    /// replaced with the corresponding expression fragment from `params` rather
    /// than a datum.
    ///
    /// Specifically, the parameter `$1` will be replaced with `params[0]`, the
    /// parameter `$2` will be replaced with `params[1]`, and so on. Parameters
    /// in `self` that refer to invalid indices of `params` will cause a panic.
    ///
    /// Column references in parameters will be corrected to account for the
    /// depth at which they are spliced.
    pub fn splice_parameters(&mut self, params: &[HirScalarExpr], depth: usize) {
        #[allow(deprecated)]
        let _ = self.visit_recursively_mut(depth, &mut |depth: usize,
                                                        e: &mut HirScalarExpr|
         -> Result<(), ()> {
            if let HirScalarExpr::Parameter(i, _name) = e {
                *e = params[*i - 1].clone();
                // Correct any column references in the parameter expression for
                // its new depth.
                e.visit_columns_mut(0, &mut |d: usize, col: &mut ColumnRef| {
                    if col.level >= d {
                        col.level += depth
                    }
                });
            }
            Ok(())
        });
    }

    /// Whether the expression contains an [`UnmaterializableFunc::MzNow`] call.
    pub fn contains_temporal(&self) -> bool {
        let mut contains = false;
        #[allow(deprecated)]
        self.visit_post_nolimit(&mut |e| {
            if let Self::CallUnmaterializable(UnmaterializableFunc::MzNow, _name) = e {
                contains = true;
            }
        });
        contains
    }

    /// Constructs an unnamed column reference in the current scope.
    /// Use [`HirScalarExpr::named_column`] when a name is known.
    /// Use [`HirScalarExpr::unnamed_column`] for a `ColumnRef`.
    pub fn column(index: usize) -> HirScalarExpr {
        HirScalarExpr::Column(
            ColumnRef {
                level: 0,
                column: index,
            },
            TreatAsEqual(None),
        )
    }

    /// Constructs an unnamed column reference.
    pub fn unnamed_column(cr: ColumnRef) -> HirScalarExpr {
        HirScalarExpr::Column(cr, TreatAsEqual(None))
    }

    /// Constructs a named column reference.
    /// Names are interned by a `NameManager`.
    pub fn named_column(cr: ColumnRef, name: Arc<str>) -> HirScalarExpr {
        HirScalarExpr::Column(cr, TreatAsEqual(Some(name)))
    }

    pub fn parameter(n: usize) -> HirScalarExpr {
        HirScalarExpr::Parameter(n, TreatAsEqual(None))
    }

    pub fn literal(datum: Datum, scalar_type: ScalarType) -> HirScalarExpr {
        let col_type = scalar_type.nullable(datum.is_null());
        soft_assert_or_log!(datum.is_instance_of(&col_type), "type is correct");
        let row = Row::pack([datum]);
        HirScalarExpr::Literal(row, col_type, TreatAsEqual(None))
    }

    pub fn literal_true() -> HirScalarExpr {
        HirScalarExpr::literal(Datum::True, ScalarType::Bool)
    }

    pub fn literal_false() -> HirScalarExpr {
        HirScalarExpr::literal(Datum::False, ScalarType::Bool)
    }

    pub fn literal_null(scalar_type: ScalarType) -> HirScalarExpr {
        HirScalarExpr::literal(Datum::Null, scalar_type)
    }

    pub fn literal_1d_array(
        datums: Vec<Datum>,
        element_scalar_type: ScalarType,
    ) -> Result<HirScalarExpr, PlanError> {
        let scalar_type = match element_scalar_type {
            ScalarType::Array(_) => {
                sql_bail!("cannot build array from array type");
            }
            typ => ScalarType::Array(Box::new(typ)).nullable(false),
        };

        let mut row = Row::default();
        row.packer()
            .try_push_array(
                &[ArrayDimension {
                    lower_bound: 1,
                    length: datums.len(),
                }],
                datums,
            )
            .expect("array constructed to be valid");

        Ok(HirScalarExpr::Literal(row, scalar_type, TreatAsEqual(None)))
    }

    pub fn as_literal(&self) -> Option<Datum> {
        if let HirScalarExpr::Literal(row, _column_type, _name) = self {
            Some(row.unpack_first())
        } else {
            None
        }
    }

    pub fn is_literal_true(&self) -> bool {
        Some(Datum::True) == self.as_literal()
    }

    pub fn is_literal_false(&self) -> bool {
        Some(Datum::False) == self.as_literal()
    }

    pub fn is_literal_null(&self) -> bool {
        Some(Datum::Null) == self.as_literal()
    }

    /// Return true iff `self` consists only of literals, materializable function calls, and
    /// if-else statements.
    pub fn is_constant(&self) -> bool {
        let mut worklist = vec![self];
        while let Some(expr) = worklist.pop() {
            match expr {
                Self::Literal(..) => {
                    // leaf node, do nothing
                }
                Self::CallUnary { expr, .. } => {
                    worklist.push(expr);
                }
                Self::CallBinary {
                    func: _,
                    expr1,
                    expr2,
                    name: _,
                } => {
                    worklist.push(expr1);
                    worklist.push(expr2);
                }
                Self::CallVariadic {
                    func: _,
                    exprs,
                    name: _,
                } => {
                    worklist.extend(exprs.iter());
                }
                // (CallUnmaterializable is not allowed)
                Self::If {
                    cond,
                    then,
                    els,
                    name: _,
                } => {
                    worklist.push(cond);
                    worklist.push(then);
                    worklist.push(els);
                }
                _ => {
                    return false; // Any other node makes `self` non-constant.
                }
            }
        }
        true
    }

    pub fn call_unary(self, func: UnaryFunc) -> Self {
        HirScalarExpr::CallUnary {
            func,
            expr: Box::new(self),
            name: NameMetadata::default(),
        }
    }

    pub fn call_binary(self, other: Self, func: BinaryFunc) -> Self {
        HirScalarExpr::CallBinary {
            func,
            expr1: Box::new(self),
            expr2: Box::new(other),
            name: NameMetadata::default(),
        }
    }

    pub fn call_unmaterializable(func: UnmaterializableFunc) -> Self {
        HirScalarExpr::CallUnmaterializable(func, NameMetadata::default())
    }

    pub fn call_variadic(func: VariadicFunc, exprs: Vec<Self>) -> Self {
        HirScalarExpr::CallVariadic {
            func,
            exprs,
            name: NameMetadata::default(),
        }
    }

    pub fn if_then_else(cond: Self, then: Self, els: Self) -> Self {
        HirScalarExpr::If {
            cond: Box::new(cond),
            then: Box::new(then),
            els: Box::new(els),
            name: NameMetadata::default(),
        }
    }

    pub fn windowing(expr: WindowExpr) -> Self {
        HirScalarExpr::Windowing(expr, TreatAsEqual(None))
    }

    pub fn or(self, other: Self) -> Self {
        HirScalarExpr::call_variadic(VariadicFunc::Or, vec![self, other])
    }

    pub fn and(self, other: Self) -> Self {
        HirScalarExpr::call_variadic(VariadicFunc::And, vec![self, other])
    }

    pub fn not(self) -> Self {
        self.call_unary(UnaryFunc::Not(func::Not))
    }

    pub fn call_is_null(self) -> Self {
        self.call_unary(UnaryFunc::IsNull(func::IsNull))
    }

    /// Calls AND with the given arguments. Simplifies if 0 or 1 args.
    pub fn variadic_and(mut args: Vec<HirScalarExpr>) -> HirScalarExpr {
        match args.len() {
            0 => HirScalarExpr::literal_true(), // Same as unit_of_and_or, but that's MirScalarExpr
            1 => args.swap_remove(0),
            _ => HirScalarExpr::call_variadic(VariadicFunc::And, args),
        }
    }

    /// Calls OR with the given arguments. Simplifies if 0 or 1 args.
    pub fn variadic_or(mut args: Vec<HirScalarExpr>) -> HirScalarExpr {
        match args.len() {
            0 => HirScalarExpr::literal_false(), // Same as unit_of_and_or, but that's MirScalarExpr
            1 => args.swap_remove(0),
            _ => HirScalarExpr::call_variadic(VariadicFunc::Or, args),
        }
    }

    pub fn take(&mut self) -> Self {
        mem::replace(self, HirScalarExpr::literal_null(ScalarType::String))
    }

    #[deprecated = "Redefine this based on the `Visit` and `VisitChildren` methods."]
    /// Visits the column references in this scalar expression.
    ///
    /// The `depth` argument should indicate the subquery nesting depth of the expression,
    /// which will be incremented with each subquery entered and presented to the supplied
    /// function `f`.
    pub fn visit_columns<F>(&self, depth: usize, f: &mut F)
    where
        F: FnMut(usize, &ColumnRef),
    {
        #[allow(deprecated)]
        let _ = self.visit_recursively(depth, &mut |depth: usize,
                                                    e: &HirScalarExpr|
         -> Result<(), ()> {
            if let HirScalarExpr::Column(col, _name) = e {
                f(depth, col)
            }
            Ok(())
        });
    }

    #[deprecated = "Redefine this based on the `Visit` and `VisitChildren` methods."]
    /// Like `visit_columns`, but permits mutating the column references.
    pub fn visit_columns_mut<F>(&mut self, depth: usize, f: &mut F)
    where
        F: FnMut(usize, &mut ColumnRef),
    {
        #[allow(deprecated)]
        let _ = self.visit_recursively_mut(depth, &mut |depth: usize,
                                                        e: &mut HirScalarExpr|
         -> Result<(), ()> {
            if let HirScalarExpr::Column(col, _name) = e {
                f(depth, col)
            }
            Ok(())
        });
    }

    /// Visits those column references in this scalar expression that refer to the root
    /// level. These include column references that are at the root level, as well as column
    /// references that are at a deeper subquery nesting depth, but refer back to the root level.
    /// (Note that even if `self` is embedded inside a larger expression, we consider the
    /// "root level" to be `self`'s level.)
    pub fn visit_columns_referring_to_root_level<F>(&self, f: &mut F)
    where
        F: FnMut(usize),
    {
        #[allow(deprecated)]
        let _ = self.visit_recursively(0, &mut |depth: usize,
                                                e: &HirScalarExpr|
         -> Result<(), ()> {
            if let HirScalarExpr::Column(col, _name) = e {
                if col.level == depth {
                    f(col.column)
                }
            }
            Ok(())
        });
    }

    /// Like `visit_columns_referring_to_root_level`, but permits mutating the column references.
    pub fn visit_columns_referring_to_root_level_mut<F>(&mut self, f: &mut F)
    where
        F: FnMut(&mut usize),
    {
        #[allow(deprecated)]
        let _ = self.visit_recursively_mut(0, &mut |depth: usize,
                                                    e: &mut HirScalarExpr|
         -> Result<(), ()> {
            if let HirScalarExpr::Column(col, _name) = e {
                if col.level == depth {
                    f(&mut col.column)
                }
            }
            Ok(())
        });
    }

    #[deprecated = "Redefine this based on the `Visit` and `VisitChildren` methods."]
    /// Like `visit` but it enters the subqueries visiting the scalar expressions contained
    /// in them. It takes the current depth of the expression and increases it when
    /// entering a subquery.
    pub fn visit_recursively<F, E>(&self, depth: usize, f: &mut F) -> Result<(), E>
    where
        F: FnMut(usize, &HirScalarExpr) -> Result<(), E>,
    {
        match self {
            HirScalarExpr::Literal(..)
            | HirScalarExpr::Parameter(..)
            | HirScalarExpr::CallUnmaterializable(..)
            | HirScalarExpr::Column(..) => (),
            HirScalarExpr::CallUnary { expr, .. } => expr.visit_recursively(depth, f)?,
            HirScalarExpr::CallBinary { expr1, expr2, .. } => {
                expr1.visit_recursively(depth, f)?;
                expr2.visit_recursively(depth, f)?;
            }
            HirScalarExpr::CallVariadic { exprs, .. } => {
                for expr in exprs {
                    expr.visit_recursively(depth, f)?;
                }
            }
            HirScalarExpr::If {
                cond,
                then,
                els,
                name: _,
            } => {
                cond.visit_recursively(depth, f)?;
                then.visit_recursively(depth, f)?;
                els.visit_recursively(depth, f)?;
            }
            HirScalarExpr::Exists(expr, _name) | HirScalarExpr::Select(expr, _name) => {
                #[allow(deprecated)]
                expr.visit_scalar_expressions(depth + 1, &mut |e, depth| {
                    e.visit_recursively(depth, f)
                })?;
            }
            HirScalarExpr::Windowing(expr, _name) => {
                expr.visit_expressions(&mut |e| e.visit_recursively(depth, f))?;
            }
        }
        f(depth, self)
    }

    #[deprecated = "Redefine this based on the `Visit` and `VisitChildren` methods."]
    /// Like `visit_recursively`, but permits mutating the scalar expressions.
    pub fn visit_recursively_mut<F, E>(&mut self, depth: usize, f: &mut F) -> Result<(), E>
    where
        F: FnMut(usize, &mut HirScalarExpr) -> Result<(), E>,
    {
        match self {
            HirScalarExpr::Literal(..)
            | HirScalarExpr::Parameter(..)
            | HirScalarExpr::CallUnmaterializable(..)
            | HirScalarExpr::Column(..) => (),
            HirScalarExpr::CallUnary { expr, .. } => expr.visit_recursively_mut(depth, f)?,
            HirScalarExpr::CallBinary { expr1, expr2, .. } => {
                expr1.visit_recursively_mut(depth, f)?;
                expr2.visit_recursively_mut(depth, f)?;
            }
            HirScalarExpr::CallVariadic { exprs, .. } => {
                for expr in exprs {
                    expr.visit_recursively_mut(depth, f)?;
                }
            }
            HirScalarExpr::If {
                cond,
                then,
                els,
                name: _,
            } => {
                cond.visit_recursively_mut(depth, f)?;
                then.visit_recursively_mut(depth, f)?;
                els.visit_recursively_mut(depth, f)?;
            }
            HirScalarExpr::Exists(expr, _name) | HirScalarExpr::Select(expr, _name) => {
                #[allow(deprecated)]
                expr.visit_scalar_expressions_mut(depth + 1, &mut |e, depth| {
                    e.visit_recursively_mut(depth, f)
                })?;
            }
            HirScalarExpr::Windowing(expr, _name) => {
                expr.visit_expressions_mut(&mut |e| e.visit_recursively_mut(depth, f))?;
            }
        }
        f(depth, self)
    }

    /// Attempts to simplify self into a literal.
    ///
    /// Returns None if self is not constant and therefore can't be simplified to a literal, or if
    /// an evaluation error occurs during simplification, or if self contains
    /// - a subquery
    /// - a column reference to an outer level
    /// - a parameter
    /// - a window function call
    fn simplify_to_literal(self) -> Option<Row> {
        let mut expr = self.lower_uncorrelated().ok()?;
        expr.reduce(&[]);
        match expr {
            mz_expr::MirScalarExpr::Literal(Ok(row), _) => Some(row),
            _ => None,
        }
    }

    /// Simplifies self into a literal. If this is not possible (e.g., because self is not constant
    /// or an evaluation error occurs during simplification), it returns
    /// [`PlanError::ConstantExpressionSimplificationFailed`].
    ///
    /// The returned error is an _internal_ error if the expression contains
    /// - a subquery
    /// - a column reference to an outer level
    /// - a parameter
    /// - a window function call
    ///
    /// TODO: use this everywhere instead of `simplify_to_literal`, so that we don't hide the error
    /// msg.
    fn simplify_to_literal_with_result(self) -> Result<Row, PlanError> {
        let mut expr = self.lower_uncorrelated().map_err(|err| {
            PlanError::ConstantExpressionSimplificationFailed(err.to_string_with_causes())
        })?;
        expr.reduce(&[]);
        match expr {
            mz_expr::MirScalarExpr::Literal(Ok(row), _) => Ok(row),
            mz_expr::MirScalarExpr::Literal(Err(err), _) => Err(
                PlanError::ConstantExpressionSimplificationFailed(err.to_string_with_causes()),
            ),
            _ => Err(PlanError::ConstantExpressionSimplificationFailed(
                "Not a constant".to_string(),
            )),
        }
    }

    /// Attempts to simplify this expression to a literal 64-bit integer.
    ///
    /// Returns `None` if this expression cannot be simplified, e.g. because it
    /// contains non-literal values.
    ///
    /// # Panics
    ///
    /// Panics if this expression does not have type [`ScalarType::Int64`].
    pub fn into_literal_int64(self) -> Option<i64> {
        self.simplify_to_literal().and_then(|row| {
            let datum = row.unpack_first();
            if datum.is_null() {
                None
            } else {
                Some(datum.unwrap_int64())
            }
        })
    }

    /// Attempts to simplify this expression to a literal string.
    ///
    /// Returns `None` if this expression cannot be simplified, e.g. because it
    /// contains non-literal values.
    ///
    /// # Panics
    ///
    /// Panics if this expression does not have type [`ScalarType::String`].
    pub fn into_literal_string(self) -> Option<String> {
        self.simplify_to_literal().and_then(|row| {
            let datum = row.unpack_first();
            if datum.is_null() {
                None
            } else {
                Some(datum.unwrap_str().to_owned())
            }
        })
    }

    /// Attempts to simplify this expression to a literal MzTimestamp.
    ///
    /// Returns `None` if the expression simplifies to `null` or if the expression cannot be
    /// simplified, e.g. because it contains non-literal values or a cast fails.
    ///
    /// TODO: Make this (and the other similar fns above) return Result, so that we can show the
    /// error when it fails. (E.g., there can be non-trivial cast errors.)
    ///
    /// # Panics
    ///
    /// Panics if this expression does not have type [`ScalarType::MzTimestamp`].
    pub fn into_literal_mz_timestamp(self) -> Option<Timestamp> {
        self.simplify_to_literal().and_then(|row| {
            let datum = row.unpack_first();
            if datum.is_null() {
                None
            } else {
                Some(datum.unwrap_mz_timestamp())
            }
        })
    }

    /// Attempts to simplify this expression of [`ScalarType::Int64`] to a literal Int64 and
    /// returns it as an i64.
    ///
    /// Returns `PlanError::ConstantExpressionSimplificationFailed` if
    /// - it's not a constant expression (as determined by `is_constant`)
    /// - evaluates to null
    /// - an EvalError occurs during evaluation (e.g., a cast fails)
    ///
    /// # Panics
    ///
    /// Panics if this expression does not have type [`ScalarType::Int64`].
    pub fn try_into_literal_int64(self) -> Result<i64, PlanError> {
        // TODO: add the `is_constant` check also to all the other into_literal_... (by adding it to
        // `simplify_to_literal`), but those should be just soft_asserts at first that it doesn't
        // actually happen that it's weaker than `reduce`, and then add them for real after 1 week.
        // (Without the is_constant check, lower_uncorrelated's preconditions spill out to be
        // preconditions also of all the other into_literal_... functions.)
        if !self.is_constant() {
            return Err(PlanError::ConstantExpressionSimplificationFailed(format!(
                "Expected a constant expression, got {}",
                self
            )));
        }
        self.clone()
            .simplify_to_literal_with_result()
            .and_then(|row| {
                let datum = row.unpack_first();
                if datum.is_null() {
                    Err(PlanError::ConstantExpressionSimplificationFailed(format!(
                        "Expected an expression that evaluates to a non-null value, got {}",
                        self
                    )))
                } else {
                    Ok(datum.unwrap_int64())
                }
            })
    }

    pub fn contains_parameters(&self) -> bool {
        let mut contains_parameters = false;
        #[allow(deprecated)]
        let _ = self.visit_recursively(0, &mut |_depth: usize,
                                                expr: &HirScalarExpr|
         -> Result<(), ()> {
            if let HirScalarExpr::Parameter(..) = expr {
                contains_parameters = true;
            }
            Ok(())
        });
        contains_parameters
    }
}

impl VisitChildren<Self> for HirScalarExpr {
    fn visit_children<F>(&self, mut f: F)
    where
        F: FnMut(&Self),
    {
        use HirScalarExpr::*;
        match self {
            Column(..) | Parameter(..) | Literal(..) | CallUnmaterializable(..) => (),
            CallUnary { expr, .. } => f(expr),
            CallBinary { expr1, expr2, .. } => {
                f(expr1);
                f(expr2);
            }
            CallVariadic { exprs, .. } => {
                for expr in exprs {
                    f(expr);
                }
            }
            If {
                cond,
                then,
                els,
                name: _,
            } => {
                f(cond);
                f(then);
                f(els);
            }
            Exists(..) | Select(..) => (),
            Windowing(expr, _name) => expr.visit_children(f),
        }
    }

    fn visit_mut_children<F>(&mut self, mut f: F)
    where
        F: FnMut(&mut Self),
    {
        use HirScalarExpr::*;
        match self {
            Column(..) | Parameter(..) | Literal(..) | CallUnmaterializable(..) => (),
            CallUnary { expr, .. } => f(expr),
            CallBinary { expr1, expr2, .. } => {
                f(expr1);
                f(expr2);
            }
            CallVariadic { exprs, .. } => {
                for expr in exprs {
                    f(expr);
                }
            }
            If {
                cond,
                then,
                els,
                name: _,
            } => {
                f(cond);
                f(then);
                f(els);
            }
            Exists(..) | Select(..) => (),
            Windowing(expr, _name) => expr.visit_mut_children(f),
        }
    }

    fn try_visit_children<F, E>(&self, mut f: F) -> Result<(), E>
    where
        F: FnMut(&Self) -> Result<(), E>,
        E: From<RecursionLimitError>,
    {
        use HirScalarExpr::*;
        match self {
            Column(..) | Parameter(..) | Literal(..) | CallUnmaterializable(..) => (),
            CallUnary { expr, .. } => f(expr)?,
            CallBinary { expr1, expr2, .. } => {
                f(expr1)?;
                f(expr2)?;
            }
            CallVariadic { exprs, .. } => {
                for expr in exprs {
                    f(expr)?;
                }
            }
            If {
                cond,
                then,
                els,
                name: _,
            } => {
                f(cond)?;
                f(then)?;
                f(els)?;
            }
            Exists(..) | Select(..) => (),
            Windowing(expr, _name) => expr.try_visit_children(f)?,
        }
        Ok(())
    }

    fn try_visit_mut_children<F, E>(&mut self, mut f: F) -> Result<(), E>
    where
        F: FnMut(&mut Self) -> Result<(), E>,
        E: From<RecursionLimitError>,
    {
        use HirScalarExpr::*;
        match self {
            Column(..) | Parameter(..) | Literal(..) | CallUnmaterializable(..) => (),
            CallUnary { expr, .. } => f(expr)?,
            CallBinary { expr1, expr2, .. } => {
                f(expr1)?;
                f(expr2)?;
            }
            CallVariadic { exprs, .. } => {
                for expr in exprs {
                    f(expr)?;
                }
            }
            If {
                cond,
                then,
                els,
                name: _,
            } => {
                f(cond)?;
                f(then)?;
                f(els)?;
            }
            Exists(..) | Select(..) => (),
            Windowing(expr, _name) => expr.try_visit_mut_children(f)?,
        }
        Ok(())
    }
}

impl AbstractExpr for HirScalarExpr {
    type Type = ColumnType;

    fn typ(
        &self,
        outers: &[RelationType],
        inner: &RelationType,
        params: &BTreeMap<usize, ScalarType>,
    ) -> Self::Type {
        stack::maybe_grow(|| match self {
            HirScalarExpr::Column(ColumnRef { level, column }, _name) => {
                if *level == 0 {
                    inner.column_types[*column].clone()
                } else {
                    outers[*level - 1].column_types[*column].clone()
                }
            }
            HirScalarExpr::Parameter(n, _name) => params[n].clone().nullable(true),
            HirScalarExpr::Literal(_, typ, _name) => typ.clone(),
            HirScalarExpr::CallUnmaterializable(func, _name) => func.output_type(),
            HirScalarExpr::CallUnary {
                expr,
                func,
                name: _,
            } => func.output_type(expr.typ(outers, inner, params)),
            HirScalarExpr::CallBinary {
                expr1,
                expr2,
                func,
                name: _,
            } => func.output_type(
                expr1.typ(outers, inner, params),
                expr2.typ(outers, inner, params),
            ),
            HirScalarExpr::CallVariadic {
                exprs,
                func,
                name: _,
            } => func.output_type(exprs.iter().map(|e| e.typ(outers, inner, params)).collect()),
            HirScalarExpr::If {
                cond: _,
                then,
                els,
                name: _,
            } => {
                let then_type = then.typ(outers, inner, params);
                let else_type = els.typ(outers, inner, params);
                then_type.union(&else_type).unwrap()
            }
            HirScalarExpr::Exists(_, _name) => ScalarType::Bool.nullable(true),
            HirScalarExpr::Select(expr, _name) => {
                let mut outers = outers.to_vec();
                outers.insert(0, inner.clone());
                expr.typ(&outers, params)
                    .column_types
                    .into_element()
                    .nullable(true)
            }
            HirScalarExpr::Windowing(expr, _name) => expr.func.typ(outers, inner, params),
        })
    }
}

impl AggregateExpr {
    pub fn typ(
        &self,
        outers: &[RelationType],
        inner: &RelationType,
        params: &BTreeMap<usize, ScalarType>,
    ) -> ColumnType {
        self.func.output_type(self.expr.typ(outers, inner, params))
    }

    /// Returns whether the expression is COUNT(*) or not.  Note that
    /// when we define the count builtin in sql::func, we convert
    /// COUNT(*) to COUNT(true), making it indistinguishable from
    /// literal COUNT(true), but we prefer to consider this as the
    /// former.
    ///
    /// (MIR has the same `is_count_asterisk`.)
    pub fn is_count_asterisk(&self) -> bool {
        self.func == AggregateFunc::Count && self.expr.is_literal_true() && !self.distinct
    }
}
