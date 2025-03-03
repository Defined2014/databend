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

use common_datavalues::type_coercion::merge_types;
use common_exception::Result;

use crate::sql::binder::wrap_cast_if_needed;
use crate::sql::binder::JoinCondition;
use crate::sql::optimizer::heuristic::subquery_rewriter::SubqueryRewriter;
use crate::sql::optimizer::ColumnSet;
use crate::sql::optimizer::RelExpr;
use crate::sql::optimizer::SExpr;
use crate::sql::plans::Filter;
use crate::sql::plans::JoinType;
use crate::sql::plans::LogicalInnerJoin;
use crate::sql::plans::PatternPlan;
use crate::sql::plans::Project;
use crate::sql::plans::RelOp;
use crate::sql::plans::SubqueryExpr;
use crate::sql::plans::SubqueryType;
use crate::sql::MetadataRef;
use crate::sql::ScalarExpr;

/// Decorrelate subqueries inside `s_expr`.
///
/// It will first hoist all the subqueries from `Scalar`s, and transform the hoisted operator
/// into `CrossApply`(if the subquery is correlated) or `CrossJoin`(if the subquery is uncorrelated).
///
/// After hoisted all the subqueries, we will try to decorrelate the subqueries by pushing the `CrossApply`
/// down. Get more detail by reading the paper `Orthogonal Optimization of Subqueries and Aggregation`,
/// which published by Microsoft SQL Server team.
pub fn decorrelate_subquery(metadata: MetadataRef, s_expr: SExpr) -> Result<SExpr> {
    let mut rewriter = SubqueryRewriter::new(metadata);
    let hoisted = rewriter.rewrite(&s_expr)?;

    Ok(hoisted)
}

// Try to decorrelate a `CrossApply` into `SemiJoin` or `AntiJoin`.
// We only do simple decorrelation here, the scheme is:
// 1. If the subquery is correlated, we will try to decorrelate it into `SemiJoin`
pub fn try_decorrelate_subquery(input: &SExpr, subquery: &SubqueryExpr) -> Result<Option<SExpr>> {
    if subquery.outer_columns.is_empty() {
        return Ok(None);
    }

    // TODO(leiysky): this is the canonical plan generated by Binder, we should find a proper
    // way to address such a pattern.
    //
    // Project
    //  \
    //   EvalScalar
    //    \
    //     Filter
    //      \
    //       Get
    let pattern = SExpr::create_unary(
        PatternPlan {
            plan_type: RelOp::Project,
        }
        .into(),
        SExpr::create_unary(
            PatternPlan {
                plan_type: RelOp::EvalScalar,
            }
            .into(),
            SExpr::create_unary(
                PatternPlan {
                    plan_type: RelOp::Filter,
                }
                .into(),
                SExpr::create_leaf(
                    PatternPlan {
                        plan_type: RelOp::LogicalGet,
                    }
                    .into(),
                ),
            ),
        ),
    );

    if !subquery.subquery.match_pattern(&pattern) {
        return Ok(None);
    }

    let filter_tree = subquery
        .subquery // Project
        .child(0)? // EvalScalar
        .child(0)?; // Filter
    let filter_expr = RelExpr::with_s_expr(filter_tree);
    let filter: Filter = subquery
        .subquery // Project
        .child(0)? // EvalScalar
        .child(0)? // Filter
        .plan()
        .clone()
        .try_into()?;
    let filter_prop = filter_expr.derive_relational_prop()?;
    let filter_child_prop = filter_expr.derive_relational_prop_child(0)?;

    let input_expr = RelExpr::with_s_expr(input);
    let input_prop = input_expr.derive_relational_prop()?;

    // First, we will check if all the outer columns are in the filter.
    if !filter_child_prop.outer_columns.is_empty() {
        return Ok(None);
    }

    // Second, we will check if the filter only contains equi-predicates.
    // This is not necessary, but it is a good heuristic for most cases.
    let mut left_conditions = vec![];
    let mut right_conditions = vec![];
    let mut other_conditions = vec![];
    let mut left_filters = vec![];
    let mut right_filters = vec![];
    for pred in filter.predicates.iter() {
        let join_condition = JoinCondition::new(pred, &input_prop, &filter_prop);
        match join_condition {
            JoinCondition::Left(filter) => {
                left_filters.push(filter.clone());
            }
            JoinCondition::Right(filter) => {
                right_filters.push(filter.clone());
            }

            JoinCondition::Other(pred) => {
                other_conditions.push(pred.clone());
            }

            JoinCondition::Both { left, right } => {
                let join_type = merge_types(&left.data_type(), &right.data_type())?;
                let left = wrap_cast_if_needed(left.clone(), &join_type);
                let right = wrap_cast_if_needed(right.clone(), &join_type);
                left_conditions.push(left);
                right_conditions.push(right);
            }
        }
    }

    let join = LogicalInnerJoin {
        left_conditions,
        right_conditions,
        other_conditions,
        join_type: match &subquery.typ {
            SubqueryType::Any | SubqueryType::All | SubqueryType::Scalar => {
                return Ok(None);
            }
            SubqueryType::Exists => JoinType::Semi,
            SubqueryType::NotExists => JoinType::Anti,
        },
        marker_index: None,
    };

    // Rewrite plan to semi-join.
    let mut left_child = input.clone();
    if !left_filters.is_empty() {
        left_child = SExpr::create_unary(
            Filter {
                predicates: left_filters,
                is_having: false,
            }
            .into(),
            left_child,
        );
    }

    // Remove `Filter` from subquery.
    let mut right_child = SExpr::create_unary(
        subquery.subquery.plan().clone(),
        SExpr::create_unary(
            subquery.subquery.child(0)?.plan().clone(),
            SExpr::create_leaf(filter_tree.child(0)?.plan().clone()),
        ),
    );
    if !right_filters.is_empty() {
        right_child = SExpr::create_unary(
            Filter {
                predicates: right_filters,
                is_having: false,
            }
            .into(),
            right_child,
        );
    }
    // Add project for join keys
    let mut used_columns = join
        .right_conditions
        .iter()
        .fold(ColumnSet::new(), |v, acc| {
            v.union(&acc.used_columns()).cloned().collect()
        });
    used_columns = used_columns
        .union(
            &join
                .other_conditions
                .iter()
                .fold(ColumnSet::new(), |v, acc| {
                    v.union(&acc.used_columns()).cloned().collect()
                }),
        )
        .cloned()
        .collect();
    used_columns = used_columns
        .difference(
            &used_columns
                .intersection(&input_prop.output_columns)
                .cloned()
                .collect(),
        )
        .cloned()
        .collect();

    right_child = SExpr::create_unary(
        Project {
            columns: used_columns,
        }
        .into(),
        right_child,
    );

    let result = SExpr::create_binary(join.into(), left_child, right_child);

    Ok(Some(result))
}
