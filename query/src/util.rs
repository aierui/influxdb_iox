//! This module contains DataFusion utility functions and helpers

use std::{convert::TryInto, sync::Arc};

use arrow::{compute::SortOptions, datatypes::Schema as ArrowSchema, record_batch::RecordBatch};

use datafusion::{
    error::DataFusionError,
    execution::context::ExecutionProps,
    logical_plan::{DFSchema, Expr, LogicalPlan, LogicalPlanBuilder},
    physical_plan::{
        expressions::{col as physical_col, PhysicalSortExpr},
        planner::create_physical_expr,
        ExecutionPlan, PhysicalExpr,
    },
};
use observability_deps::tracing::trace;
use schema::sort::SortKey;

/// Create a logical plan that produces the record batch
pub fn make_scan_plan(batch: RecordBatch) -> std::result::Result<LogicalPlan, DataFusionError> {
    let schema = batch.schema();
    let partitions = vec![vec![batch]];
    let projection = None; // scan all columns
    LogicalPlanBuilder::scan_memory(partitions, schema, projection)?.build()
}

/// Returns the pk in arrow's expression used for data sorting
pub fn arrow_pk_sort_exprs(
    key_columns: Vec<&str>,
    input_schema: &ArrowSchema,
) -> Vec<PhysicalSortExpr> {
    let mut sort_exprs = vec![];
    for key in key_columns {
        let expr = physical_col(key, input_schema).expect("pk in schema");
        sort_exprs.push(PhysicalSortExpr {
            expr,
            options: SortOptions {
                descending: false,
                nulls_first: false,
            },
        });
    }

    sort_exprs
}

pub fn arrow_sort_key_exprs(
    sort_key: &SortKey<'_>,
    input_schema: &ArrowSchema,
) -> Vec<PhysicalSortExpr> {
    let mut sort_exprs = vec![];
    for (key, options) in sort_key.iter() {
        let expr = physical_col(key, input_schema).expect("sort key column in schema");
        sort_exprs.push(PhysicalSortExpr {
            expr,
            options: SortOptions {
                descending: options.descending,
                nulls_first: options.nulls_first,
            },
        });
    }

    sort_exprs
}

/// Build a datafusion physical expression from its logical one
pub fn df_physical_expr(
    input: &dyn ExecutionPlan,
    expr: Expr,
) -> std::result::Result<Arc<dyn PhysicalExpr>, DataFusionError> {
    df_physical_expr_from_schema_and_expr(input.schema(), expr)

    // let execution_props = ExecutionProps::new();

    // let input_physical_schema = input.schema();
    // let input_logical_schema: DFSchema = input_physical_schema.as_ref().clone().try_into()?;

    // trace!(%expr, "logical expression");
    // trace!(%input_logical_schema, "input logical schema");
    // trace!(%input_physical_schema, "input physical schema");

    // create_physical_expr(
    //     &expr,
    //     &input_logical_schema,
    //     &input_physical_schema,
    //     &execution_props,
    // )
}

/// Build a datafusion physical expression from its logical one
pub fn df_physical_expr_from_schema_and_expr(
    schema: Arc<ArrowSchema>,
    expr: Expr,
) -> std::result::Result<Arc<dyn PhysicalExpr>, DataFusionError> {
    let execution_props = ExecutionProps::new();

    let input_physical_schema = schema;
    let input_logical_schema: DFSchema = input_physical_schema.as_ref().clone().try_into()?;

    trace!(%expr, "logical expression");
    trace!(%input_logical_schema, "input logical schema");
    trace!(%input_physical_schema, "input physical schema");

    create_physical_expr(
        &expr,
        &input_logical_schema,
        &input_physical_schema,
        &execution_props,
    )
}
