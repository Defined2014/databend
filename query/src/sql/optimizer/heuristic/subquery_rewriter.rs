// Copyright 2022 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use common_datavalues::BooleanType;
use common_datavalues::DataValue;
use common_datavalues::NullableType;
use common_exception::ErrorCode;
use common_exception::Result;
use common_functions::aggregates::AggregateFunctionFactory;

use crate::sql::binder::ColumnBinding;
use crate::sql::optimizer::heuristic::decorrelate::try_decorrelate_subquery;
use crate::sql::optimizer::ColumnSet;
use crate::sql::optimizer::RelExpr;
use crate::sql::optimizer::SExpr;
use crate::sql::plans::Aggregate;
use crate::sql::plans::AggregateFunction;
use crate::sql::plans::AndExpr;
use crate::sql::plans::BoundColumnRef;
use crate::sql::plans::CastExpr;
use crate::sql::plans::ComparisonExpr;
use crate::sql::plans::ComparisonOp;
use crate::sql::plans::ConstantExpr;
use crate::sql::plans::CrossApply;
use crate::sql::plans::EvalScalar;
use crate::sql::plans::FunctionCall;
use crate::sql::plans::JoinType;
use crate::sql::plans::Limit;
use crate::sql::plans::LogicalInnerJoin;
use crate::sql::plans::Max1Row;
use crate::sql::plans::OrExpr;
use crate::sql::plans::Project;
use crate::sql::plans::RelOperator;
use crate::sql::plans::Scalar;
use crate::sql::plans::ScalarItem;
use crate::sql::plans::SubqueryExpr;
use crate::sql::plans::SubqueryType;
use crate::sql::IndexType;
use crate::sql::MetadataRef;

enum UnnestResult {
    Uncorrelated,
    Apply,
    SimpleJoin, // SemiJoin or AntiJoin
    MarkJoin { marker_index: IndexType },
}

/// Rewrite subquery into `Apply` operator
pub struct SubqueryRewriter {
    metadata: MetadataRef,
}

impl SubqueryRewriter {
    pub fn new(metadata: MetadataRef) -> Self {
        Self { metadata }
    }

    pub fn rewrite(&mut self, s_expr: &SExpr) -> Result<SExpr> {
        match s_expr.plan().clone() {
            RelOperator::EvalScalar(mut plan) => {
                let mut input = self.rewrite(s_expr.child(0)?)?;

                for item in plan.items.iter_mut() {
                    let res = self.try_rewrite_subquery(&item.scalar, &input, false)?;
                    input = res.1;
                    item.scalar = res.0;
                }

                Ok(SExpr::create_unary(plan.into(), input))
            }
            RelOperator::Filter(mut plan) => {
                let mut input = self.rewrite(s_expr.child(0)?)?;

                for pred in plan.predicates.iter_mut() {
                    let res = self.try_rewrite_subquery(pred, &input, true)?;
                    input = res.1;
                    *pred = res.0;
                }

                Ok(SExpr::create_unary(plan.into(), input))
            }
            RelOperator::Aggregate(mut plan) => {
                let mut input = self.rewrite(s_expr.child(0)?)?;

                for item in plan.group_items.iter_mut() {
                    let res = self.try_rewrite_subquery(&item.scalar, &input, false)?;
                    input = res.1;
                    item.scalar = res.0;
                }

                for item in plan.aggregate_functions.iter_mut() {
                    let res = self.try_rewrite_subquery(&item.scalar, &input, false)?;
                    input = res.1;
                    item.scalar = res.0;
                }

                Ok(SExpr::create_unary(plan.into(), input))
            }

            RelOperator::LogicalInnerJoin(_) => Ok(SExpr::create_binary(
                s_expr.plan().clone(),
                self.rewrite(s_expr.child(0)?)?,
                self.rewrite(s_expr.child(1)?)?,
            )),

            RelOperator::Project(_) | RelOperator::Limit(_) | RelOperator::Sort(_) => Ok(
                SExpr::create_unary(s_expr.plan().clone(), self.rewrite(s_expr.child(0)?)?),
            ),

            RelOperator::LogicalGet(_) => Ok(s_expr.clone()),

            RelOperator::CrossApply(_)
            | RelOperator::Max1Row(_)
            | RelOperator::PhysicalHashJoin(_)
            | RelOperator::Pattern(_)
            | RelOperator::PhysicalScan(_) => Err(ErrorCode::LogicalError("Invalid plan type")),
        }
    }

    fn unnest_subquery(
        &mut self,
        left: &SExpr,
        subquery: &SubqueryExpr,
        is_conjunctive_predicate: bool,
    ) -> Result<(SExpr, UnnestResult)> {
        match subquery.typ {
            SubqueryType::Scalar => {
                let rel_expr = RelExpr::with_s_expr(&subquery.subquery);
                let prop = rel_expr.derive_relational_prop()?;
                let (rel_op, result): (RelOperator, _) = if prop.outer_columns.is_empty() {
                    (
                        LogicalInnerJoin {
                            left_conditions: vec![],
                            right_conditions: vec![],
                            other_conditions: vec![],
                            join_type: JoinType::Cross,
                            marker_index: None,
                        }
                        .into(),
                        UnnestResult::Uncorrelated,
                    )
                } else {
                    (
                        CrossApply {
                            correlated_columns: subquery.outer_columns.clone(),
                        }
                        .into(),
                        UnnestResult::Apply,
                    )
                };
                Ok((
                    SExpr::create_binary(
                        rel_op,
                        left.clone(),
                        SExpr::create_unary(Max1Row.into(), subquery.subquery.clone()),
                    ),
                    result,
                ))
            }
            SubqueryType::Exists | SubqueryType::NotExists => {
                if is_conjunctive_predicate {
                    if let Some(result) = try_decorrelate_subquery(left, subquery)? {
                        return Ok((result, UnnestResult::SimpleJoin));
                    }
                }
                let mut subquery_expr = subquery.subquery.clone();
                // Wrap Limit to current subquery
                let limit = Limit {
                    limit: Some(1),
                    offset: 0,
                };
                subquery_expr = SExpr::create_unary(limit.into(), subquery_expr.clone());

                // We will rewrite EXISTS subquery into the form `COUNT(*) = 1`.
                // In contrast, NOT EXISTS subquery will be rewritten into `COUNT(*) = 0`.
                // For example, `EXISTS(SELECT a FROM t WHERE a > 1)` will be rewritten into
                // `(SELECT COUNT(*) = 1 FROM t WHERE a > 1 LIMIT 1)`.
                let agg_func = AggregateFunctionFactory::instance().get("count", vec![], vec![])?;
                let agg_func_index = self.metadata.write().add_column(
                    "count(*)".to_string(),
                    agg_func.return_type()?,
                    None,
                );

                let agg = Aggregate {
                    group_items: vec![],
                    aggregate_functions: vec![ScalarItem {
                        scalar: AggregateFunction {
                            display_name: "count(*)".to_string(),
                            func_name: "count".to_string(),
                            distinct: false,
                            params: vec![],
                            args: vec![],
                            return_type: agg_func.return_type()?,
                        }
                        .into(),
                        index: agg_func_index,
                    }],
                    from_distinct: false,
                };

                // COUNT(*) = 1 or COUNT(*) = 0
                let compare_index = self.metadata.write().add_column(
                    "subquery".to_string(),
                    BooleanType::new_impl(),
                    None,
                );
                let compare = ComparisonExpr {
                    op: ComparisonOp::Equal,
                    left: Box::new(
                        BoundColumnRef {
                            column: ColumnBinding {
                                database_name: None,
                                table_name: None,
                                column_name: "count(*)".to_string(),
                                index: agg_func_index,
                                data_type: agg_func.return_type()?,
                                visible_in_unqualified_wildcard: false,
                            },
                        }
                        .into(),
                    ),
                    right: Box::new(
                        ConstantExpr {
                            value: if SubqueryType::Exists == subquery.typ {
                                DataValue::Int64(1)
                            } else {
                                DataValue::Int64(0)
                            },
                            data_type: agg_func.return_type()?,
                        }
                        .into(),
                    ),
                    return_type: agg_func.return_type()?,
                };
                let eval_scalar = EvalScalar {
                    items: vec![ScalarItem {
                        scalar: compare.into(),
                        index: compare_index,
                    }],
                };

                let project = Project {
                    columns: ColumnSet::from([compare_index]),
                };

                // Project
                //     EvalScalar: COUNT(*) = 1 or COUNT(*) = 0
                //         Aggregate: COUNT(*)
                let rewritten_subquery = SExpr::create_unary(
                    project.into(),
                    SExpr::create_unary(
                        eval_scalar.into(),
                        SExpr::create_unary(agg.into(), subquery_expr),
                    ),
                );

                let rel_expr = RelExpr::with_s_expr(&rewritten_subquery);
                let prop = rel_expr.derive_relational_prop()?;

                let (rel_op, result): (RelOperator, _) = if prop.outer_columns.is_empty() {
                    (
                        LogicalInnerJoin {
                            left_conditions: vec![],
                            right_conditions: vec![],
                            other_conditions: vec![],
                            join_type: JoinType::Cross,
                            marker_index: None,
                        }
                        .into(),
                        UnnestResult::Uncorrelated,
                    )
                } else {
                    (
                        CrossApply {
                            correlated_columns: subquery.outer_columns.clone(),
                        }
                        .into(),
                        UnnestResult::Apply,
                    )
                };

                Ok((
                    SExpr::create_binary(rel_op, left.clone(), rewritten_subquery),
                    result,
                ))
            }
            SubqueryType::Any => {
                let rel_expr = RelExpr::with_s_expr(&subquery.subquery);
                let prop = rel_expr.derive_relational_prop()?;
                let output_columns = prop.output_columns.clone();
                let index = output_columns
                    .iter()
                    .take(1)
                    .next()
                    .ok_or_else(|| ErrorCode::LogicalError("Invalid subquery"))?;
                let column_name = format!("subquery_{}", index);
                let left_condition = Scalar::BoundColumnRef(BoundColumnRef {
                    column: ColumnBinding {
                        database_name: None,
                        table_name: None,
                        column_name,
                        index: *index,
                        data_type: subquery.data_type.clone(),
                        visible_in_unqualified_wildcard: false,
                    },
                });
                if prop.outer_columns.is_empty() {
                    // Add a marker column to save comparison result.
                    // The column is Nullable(Boolean), the data value is TRUE, FALSE, or NULL.
                    // If subquery contains NULL, the comparison result is TRUE or NULL. Such as t1.a => {1, 3, 4}, select t1.a in (1, 2, NULL) from t1; The sql will return {true, null, null}.
                    // If subquery doesn't contain NULL, the comparison result is FALSE, TRUE, or NULL.
                    let marker_index = self.metadata.write().add_column(
                        "marker".to_string(),
                        NullableType::new_impl(BooleanType::new_impl()),
                        None,
                    );
                    // Consider the sql: select * from t1 where t1.a = any(select t2.a from t2);
                    // Will be transferred to:select t1.a, t2.a, marker_index from t2, t1 where t2.a = t1.a;
                    // Note that subquery is the left table, and it'll be the probe side.
                    let mark_join = LogicalInnerJoin {
                        right_conditions: vec![*subquery.child_expr.as_ref().unwrap().clone()],
                        left_conditions: vec![left_condition],
                        other_conditions: vec![],
                        join_type: JoinType::Mark,
                        marker_index: Some(marker_index),
                    }
                    .into();
                    Ok((
                        SExpr::create_binary(mark_join, subquery.subquery.clone(), left.clone()),
                        UnnestResult::MarkJoin { marker_index },
                    ))
                } else {
                    Err(ErrorCode::LogicalError(
                        "Unsupported subquery type: Correlated AnySubquery",
                    ))
                }
            }
            _ => Err(ErrorCode::LogicalError(format!(
                "Unsupported subquery type: {:?}",
                &subquery.typ
            ))),
        }
    }

    /// Try to extract subquery from a scalar expression. Returns replaced scalar expression
    /// and the subqueries.
    fn try_rewrite_subquery(
        &mut self,
        scalar: &Scalar,
        s_expr: &SExpr,
        is_conjunctive_predicate: bool,
    ) -> Result<(Scalar, SExpr)> {
        match scalar {
            Scalar::BoundColumnRef(_) => Ok((scalar.clone(), s_expr.clone())),

            Scalar::ConstantExpr(_) => Ok((scalar.clone(), s_expr.clone())),

            Scalar::AndExpr(expr) => {
                // Notice that the conjunctions has been flattened in binder, if we encounter
                // a AND here, we can't treat it as a conjunction.
                let (left, s_expr) = self.try_rewrite_subquery(&expr.left, s_expr, false)?;
                let (right, s_expr) = self.try_rewrite_subquery(&expr.right, &s_expr, false)?;
                Ok((
                    AndExpr {
                        left: Box::new(left),
                        right: Box::new(right),
                        return_type: expr.return_type.clone(),
                    }
                    .into(),
                    s_expr,
                ))
            }

            Scalar::OrExpr(expr) => {
                let (left, s_expr) = self.try_rewrite_subquery(&expr.left, s_expr, false)?;
                let (right, s_expr) = self.try_rewrite_subquery(&expr.right, &s_expr, false)?;
                Ok((
                    OrExpr {
                        left: Box::new(left),
                        right: Box::new(right),
                        return_type: expr.return_type.clone(),
                    }
                    .into(),
                    s_expr,
                ))
            }

            Scalar::ComparisonExpr(expr) => {
                let (left, s_expr) = self.try_rewrite_subquery(&expr.left, s_expr, false)?;
                let (right, s_expr) = self.try_rewrite_subquery(&expr.right, &s_expr, false)?;
                Ok((
                    ComparisonExpr {
                        op: expr.op.clone(),
                        left: Box::new(left),
                        right: Box::new(right),
                        return_type: expr.return_type.clone(),
                    }
                    .into(),
                    s_expr,
                ))
            }

            Scalar::AggregateFunction(_) => Ok((scalar.clone(), s_expr.clone())),

            Scalar::FunctionCall(func) => {
                let mut args = vec![];
                let mut s_expr = s_expr.clone();
                for arg in func.arguments.iter() {
                    let res = self.try_rewrite_subquery(arg, &s_expr, false)?;
                    s_expr = res.1;
                    args.push(res.0);
                }

                let expr: Scalar = FunctionCall {
                    arguments: args,
                    func_name: func.func_name.clone(),
                    arg_types: func.arg_types.clone(),
                    return_type: func.return_type.clone(),
                }
                .into();

                Ok((expr, s_expr))
            }

            Scalar::CastExpr(cast) => {
                let (scalar, s_expr) = self.try_rewrite_subquery(&cast.argument, s_expr, false)?;
                Ok((
                    CastExpr {
                        argument: Box::new(scalar),
                        from_type: cast.from_type.clone(),
                        target_type: cast.target_type.clone(),
                    }
                    .into(),
                    s_expr,
                ))
            }

            Scalar::SubqueryExpr(subquery) => {
                // Rewrite subquery recursively
                let mut subquery = subquery.clone();
                subquery.subquery = self.rewrite(&subquery.subquery)?;

                let (s_expr, result) =
                    self.unnest_subquery(s_expr, &subquery, is_conjunctive_predicate)?;

                // If we unnest the subquery into a simple join, then we can replace the
                // original predicate with a `TRUE` literal to eliminate the conjunction.
                if matches!(result, UnnestResult::SimpleJoin) {
                    return Ok((
                        Scalar::ConstantExpr(ConstantExpr {
                            value: DataValue::Boolean(true),
                            data_type: BooleanType::new_impl(),
                        }),
                        s_expr,
                    ));
                }
                let rel_expr = if subquery.typ == SubqueryType::Any {
                    RelExpr::with_s_expr(s_expr.child(0)?)
                } else {
                    RelExpr::with_s_expr(s_expr.child(1)?)
                };
                let prop = rel_expr.derive_relational_prop()?;

                // Extract the subquery and replace it with the ColumnBinding from it.
                let (index, name) = if let UnnestResult::MarkJoin { marker_index } = result {
                    (marker_index, "marker".to_string())
                } else {
                    let index = *prop
                        .output_columns
                        .iter()
                        .take(1)
                        .next()
                        .ok_or_else(|| ErrorCode::LogicalError("Invalid subquery"))?;
                    (index, format!("subquery_{}", index))
                };
                let column_ref = ColumnBinding {
                    database_name: None,
                    table_name: None,
                    column_name: name,
                    index,
                    data_type: subquery.data_type.clone(),
                    visible_in_unqualified_wildcard: false,
                };

                Ok((BoundColumnRef { column: column_ref }.into(), s_expr))
            }
        }
    }
}
