// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! This file has test utils for hash joins

use std::sync::Arc;
use std::usize;

use crate::joins::nested_loop_join::distribution_from_join_type;
use crate::joins::utils::{JoinFilter, JoinOn};
use crate::joins::{
    HashJoinExec, NestedLoopJoinExec, PartitionMode, SlidingNestedLoopJoinExec,
    SlidingWindowWorkingMode, StreamJoinPartitionMode, SymmetricHashJoinExec,
};
use crate::memory::MemoryExec;
use crate::repartition::RepartitionExec;
use crate::Distribution;
use crate::{common, ExecutionPlan, Partitioning};

use arrow::util::pretty::pretty_format_batches;
use arrow_array::{
    ArrayRef, Float64Array, Int32Array, IntervalDayTimeArray, RecordBatch,
    TimestampMillisecondArray,
};
use arrow_schema::{DataType, Schema};
use datafusion_common::{DataFusionError, Result, ScalarValue};
use datafusion_execution::TaskContext;
use datafusion_expr::{JoinType, Operator};
use datafusion_physical_expr::expressions::{
    binary, cast, col, lit, BinaryExpr, Column, Literal,
};
use datafusion_physical_expr::intervals::test_utils::{
    gen_conjunctive_numerical_expr, gen_conjunctive_temporal_expr,
};
use datafusion_physical_expr::{LexOrdering, PhysicalExpr};

use rand::prelude::StdRng;
use rand::{Rng, SeedableRng};

pub fn compare_batches(collected_1: &[RecordBatch], collected_2: &[RecordBatch]) {
    // compare
    let first_formatted = pretty_format_batches(collected_1).unwrap().to_string();
    let second_formatted = pretty_format_batches(collected_2).unwrap().to_string();

    let mut first_formatted_sorted: Vec<&str> = first_formatted.trim().lines().collect();
    first_formatted_sorted.sort_unstable();

    let mut second_formatted_sorted: Vec<&str> =
        second_formatted.trim().lines().collect();
    second_formatted_sorted.sort_unstable();

    for (i, (first_line, second_line)) in first_formatted_sorted
        .iter()
        .zip(&second_formatted_sorted)
        .enumerate()
    {
        assert_eq!((i, first_line), (i, second_line));
    }
}

pub async fn partitioned_sym_join_with_filter(
    left: Arc<dyn ExecutionPlan>,
    right: Arc<dyn ExecutionPlan>,
    on: JoinOn,
    filter: Option<JoinFilter>,
    join_type: &JoinType,
    null_equals_null: bool,
    context: Arc<TaskContext>,
) -> Result<Vec<RecordBatch>> {
    let partition_count = 4;

    let left_expr = on
        .iter()
        .map(|(l, _)| Arc::new(l.clone()) as _)
        .collect::<Vec<_>>();

    let right_expr = on
        .iter()
        .map(|(_, r)| Arc::new(r.clone()) as _)
        .collect::<Vec<_>>();

    let join = SymmetricHashJoinExec::try_new(
        Arc::new(RepartitionExec::try_new(
            left,
            Partitioning::Hash(left_expr, partition_count),
        )?),
        Arc::new(RepartitionExec::try_new(
            right,
            Partitioning::Hash(right_expr, partition_count),
        )?),
        on,
        filter,
        join_type,
        null_equals_null,
        StreamJoinPartitionMode::Partitioned,
    )?;

    let mut batches = vec![];
    for i in 0..partition_count {
        let stream = join.execute(i, context.clone())?;
        let more_batches = common::collect(stream).await?;
        batches.extend(
            more_batches
                .into_iter()
                .filter(|b| b.num_rows() > 0)
                .collect::<Vec<_>>(),
        );
    }

    Ok(batches)
}

pub async fn aggregative_hash_join_with_filter(
    left: Arc<dyn ExecutionPlan>,
    right: Arc<dyn ExecutionPlan>,
    on: JoinOn,
    filter: Option<JoinFilter>,
    join_type: &JoinType,
    null_equals_null: bool,
    context: Arc<TaskContext>,
) -> Result<Vec<RecordBatch>> {
    let partition_count = 4;
    let (left_expr, right_expr) = on
        .iter()
        .map(|(l, r)| (Arc::new(l.clone()) as _, Arc::new(r.clone()) as _))
        .unzip();

    let join = Arc::new(HashJoinExec::try_new(
        Arc::new(RepartitionExec::try_new(
            left,
            Partitioning::Hash(left_expr, partition_count),
        )?),
        Arc::new(RepartitionExec::try_new(
            right,
            Partitioning::Hash(right_expr, partition_count),
        )?),
        on,
        filter,
        join_type,
        PartitionMode::Partitioned,
        null_equals_null,
    )?);

    let mut batches = vec![];
    for i in 0..partition_count {
        let stream = join.execute(i, context.clone())?;
        let more_batches = common::collect(stream).await?;
        batches.extend(
            more_batches
                .into_iter()
                .filter(|b| b.num_rows() > 0)
                .collect::<Vec<_>>(),
        );
    }

    Ok(batches)
}

pub fn split_record_batches(
    batch: &RecordBatch,
    batch_size: usize,
) -> Result<Vec<RecordBatch>> {
    let row_num = batch.num_rows();
    let number_of_batch = row_num / batch_size;
    let mut sizes = vec![batch_size; number_of_batch];
    sizes.push(row_num - (batch_size * number_of_batch));
    let mut result = vec![];
    for (i, size) in sizes.iter().enumerate() {
        result.push(batch.slice(i * batch_size, *size));
    }
    Ok(result)
}

struct AscendingRandomFloatIterator {
    prev: f64,
    max: f64,
    rng: StdRng,
}

impl AscendingRandomFloatIterator {
    fn new(min: f64, max: f64) -> Self {
        let mut rng = StdRng::seed_from_u64(42);
        let initial = rng.gen_range(min..max);
        AscendingRandomFloatIterator {
            prev: initial,
            max,
            rng,
        }
    }
}

impl Iterator for AscendingRandomFloatIterator {
    type Item = f64;

    fn next(&mut self) -> Option<Self::Item> {
        let value = self.rng.gen_range(self.prev..self.max);
        self.prev = value;
        Some(value)
    }
}

pub fn join_expr_tests_fixture_temporal(
    expr_id: usize,
    left_col: Arc<dyn PhysicalExpr>,
    right_col: Arc<dyn PhysicalExpr>,
    schema: &Schema,
) -> Result<Arc<dyn PhysicalExpr>> {
    match expr_id {
        // constructs ((left_col - INTERVAL '100ms')  > (right_col - INTERVAL '200ms')) AND ((left_col - INTERVAL '450ms') < (right_col - INTERVAL '300ms'))
        0 => gen_conjunctive_temporal_expr(
            left_col,
            right_col,
            Operator::Minus,
            Operator::Minus,
            Operator::Minus,
            Operator::Minus,
            ScalarValue::new_interval_dt(0, 100), // 100 ms
            ScalarValue::new_interval_dt(0, 200), // 200 ms
            ScalarValue::new_interval_dt(0, 450), // 450 ms
            ScalarValue::new_interval_dt(0, 300), // 300 ms
            schema,
        ),
        // constructs ((left_col - TIMESTAMP '2023-01-01:12.00.03')  > (right_col - TIMESTAMP '2023-01-01:12.00.01')) AND ((left_col - TIMESTAMP '2023-01-01:12.00.00') < (right_col - TIMESTAMP '2023-01-01:12.00.02'))
        1 => gen_conjunctive_temporal_expr(
            left_col,
            right_col,
            Operator::Minus,
            Operator::Minus,
            Operator::Minus,
            Operator::Minus,
            ScalarValue::TimestampMillisecond(Some(1672574403000), None), // 2023-01-01:12.00.03
            ScalarValue::TimestampMillisecond(Some(1672574401000), None), // 2023-01-01:12.00.01
            ScalarValue::TimestampMillisecond(Some(1672574400000), None), // 2023-01-01:12.00.00
            ScalarValue::TimestampMillisecond(Some(1672574402000), None), // 2023-01-01:12.00.02
            schema,
        ),
        // constructs ((left_col - DURATION '3 secs')  > (right_col - DURATION '2 secs')) AND ((left_col - DURATION '5 secs') < (right_col - DURATION '4 secs'))
        2 => gen_conjunctive_temporal_expr(
            left_col,
            right_col,
            Operator::Minus,
            Operator::Minus,
            Operator::Minus,
            Operator::Minus,
            ScalarValue::DurationMillisecond(Some(3000)), // 3 secs
            ScalarValue::DurationMillisecond(Some(2000)), // 2 secs
            ScalarValue::DurationMillisecond(Some(5000)), // 5 secs
            ScalarValue::DurationMillisecond(Some(4000)), // 4 secs
            schema,
        ),
        _ => unreachable!(),
    }
}

// It creates join filters for different type of fields for testing.
macro_rules! join_expr_tests {
    ($func_name:ident, $type:ty, $SCALAR:ident) => {
        pub fn $func_name(
            expr_id: usize,
            left_col: Arc<dyn PhysicalExpr>,
            right_col: Arc<dyn PhysicalExpr>,
        ) -> Arc<dyn PhysicalExpr> {
            match expr_id {
                // left_col + 1 > right_col + 5 AND left_col + 3 < right_col + 10
                0 => gen_conjunctive_numerical_expr(
                    left_col,
                    right_col,
                    (
                        Operator::Plus,
                        Operator::Plus,
                        Operator::Plus,
                        Operator::Plus,
                    ),
                    ScalarValue::$SCALAR(Some(1 as $type)),
                    ScalarValue::$SCALAR(Some(5 as $type)),
                    ScalarValue::$SCALAR(Some(3 as $type)),
                    ScalarValue::$SCALAR(Some(10 as $type)),
                    (Operator::Gt, Operator::Lt),
                ),
                // left_col - 1 > right_col + 5 AND left_col + 3 < right_col + 10
                1 => gen_conjunctive_numerical_expr(
                    left_col,
                    right_col,
                    (
                        Operator::Minus,
                        Operator::Plus,
                        Operator::Plus,
                        Operator::Plus,
                    ),
                    ScalarValue::$SCALAR(Some(1 as $type)),
                    ScalarValue::$SCALAR(Some(5 as $type)),
                    ScalarValue::$SCALAR(Some(3 as $type)),
                    ScalarValue::$SCALAR(Some(10 as $type)),
                    (Operator::Gt, Operator::Lt),
                ),
                // left_col - 1 > right_col + 5 AND left_col - 3 < right_col + 10
                2 => gen_conjunctive_numerical_expr(
                    left_col,
                    right_col,
                    (
                        Operator::Minus,
                        Operator::Plus,
                        Operator::Minus,
                        Operator::Plus,
                    ),
                    ScalarValue::$SCALAR(Some(1 as $type)),
                    ScalarValue::$SCALAR(Some(5 as $type)),
                    ScalarValue::$SCALAR(Some(3 as $type)),
                    ScalarValue::$SCALAR(Some(10 as $type)),
                    (Operator::Gt, Operator::Lt),
                ),
                // left_col - 10 > right_col - 5 AND left_col - 3 < right_col + 10
                3 => gen_conjunctive_numerical_expr(
                    left_col,
                    right_col,
                    (
                        Operator::Minus,
                        Operator::Minus,
                        Operator::Minus,
                        Operator::Plus,
                    ),
                    ScalarValue::$SCALAR(Some(10 as $type)),
                    ScalarValue::$SCALAR(Some(5 as $type)),
                    ScalarValue::$SCALAR(Some(3 as $type)),
                    ScalarValue::$SCALAR(Some(10 as $type)),
                    (Operator::Gt, Operator::Lt),
                ),
                // left_col - 10 > right_col - 5 AND left_col - 30 < right_col - 3
                4 => gen_conjunctive_numerical_expr(
                    left_col,
                    right_col,
                    (
                        Operator::Minus,
                        Operator::Minus,
                        Operator::Minus,
                        Operator::Minus,
                    ),
                    ScalarValue::$SCALAR(Some(10 as $type)),
                    ScalarValue::$SCALAR(Some(5 as $type)),
                    ScalarValue::$SCALAR(Some(30 as $type)),
                    ScalarValue::$SCALAR(Some(3 as $type)),
                    (Operator::Gt, Operator::Lt),
                ),
                // left_col - 2 >= right_col - 5 AND left_col - 7 <= right_col - 3
                5 => gen_conjunctive_numerical_expr(
                    left_col,
                    right_col,
                    (
                        Operator::Minus,
                        Operator::Plus,
                        Operator::Plus,
                        Operator::Minus,
                    ),
                    ScalarValue::$SCALAR(Some(2 as $type)),
                    ScalarValue::$SCALAR(Some(5 as $type)),
                    ScalarValue::$SCALAR(Some(7 as $type)),
                    ScalarValue::$SCALAR(Some(3 as $type)),
                    (Operator::GtEq, Operator::LtEq),
                ),
                // left_col - 28 >= right_col - 11 AND left_col - 21 <= right_col - 39
                6 => gen_conjunctive_numerical_expr(
                    left_col,
                    right_col,
                    (
                        Operator::Plus,
                        Operator::Minus,
                        Operator::Plus,
                        Operator::Plus,
                    ),
                    ScalarValue::$SCALAR(Some(28 as $type)),
                    ScalarValue::$SCALAR(Some(11 as $type)),
                    ScalarValue::$SCALAR(Some(21 as $type)),
                    ScalarValue::$SCALAR(Some(39 as $type)),
                    (Operator::Gt, Operator::LtEq),
                ),
                // left_col - 28 >= right_col - 11 AND left_col - 21 <= right_col + 39
                7 => gen_conjunctive_numerical_expr(
                    left_col,
                    right_col,
                    (
                        Operator::Plus,
                        Operator::Minus,
                        Operator::Minus,
                        Operator::Plus,
                    ),
                    ScalarValue::$SCALAR(Some(28 as $type)),
                    ScalarValue::$SCALAR(Some(11 as $type)),
                    ScalarValue::$SCALAR(Some(21 as $type)),
                    ScalarValue::$SCALAR(Some(39 as $type)),
                    (Operator::GtEq, Operator::Lt),
                ),
                _ => panic!("No case"),
            }
        }
    };
}

join_expr_tests!(join_expr_tests_fixture_i32, i32, Int32);
join_expr_tests!(join_expr_tests_fixture_f64, f64, Float64);

fn generate_ordered_array(size: i32, duplicate_ratio: f32) -> Arc<Int32Array> {
    let mut rng = StdRng::seed_from_u64(42);
    let unique_count = (size as f32 * (1.0 - duplicate_ratio)) as i32;

    // Generate unique random values
    let mut values: Vec<i32> = (0..unique_count)
        .map(|_| rng.gen_range(1..500)) // Modify as per your range
        .collect();

    // Duplicate the values according to the duplicate ratio
    for _ in 0..(size - unique_count) {
        let index = rng.gen_range(0..unique_count);
        values.push(values[index as usize]);
    }

    // Sort the values to ensure they are ordered
    values.sort();

    Arc::new(Int32Array::from_iter(values))
}

pub fn build_sides_record_batches(
    table_size: i32,
    key_cardinality: (i32, i32),
) -> Result<(RecordBatch, RecordBatch)> {
    let null_ratio: f64 = 0.4;
    let duplicate_ratio = 0.4;
    let initial_range = 0..table_size;
    let index = (table_size as f64 * null_ratio).round() as i32;
    let rest_of = index..table_size;
    let ordered: ArrayRef = Arc::new(Int32Array::from_iter(
        initial_range.clone().collect::<Vec<i32>>(),
    ));
    let random_ordered = generate_ordered_array(table_size, duplicate_ratio);
    let ordered_des = Arc::new(Int32Array::from_iter(
        initial_range.clone().rev().collect::<Vec<i32>>(),
    ));
    let cardinality = Arc::new(Int32Array::from_iter(
        initial_range.clone().map(|x| x % 4).collect::<Vec<i32>>(),
    ));
    let cardinality_key_left = Arc::new(Int32Array::from_iter(
        initial_range
            .clone()
            .map(|x| x % key_cardinality.0)
            .collect::<Vec<i32>>(),
    ));
    let cardinality_key_right = Arc::new(Int32Array::from_iter(
        initial_range
            .clone()
            .map(|x| x % key_cardinality.1)
            .collect::<Vec<i32>>(),
    ));
    let ordered_asc_null_first = Arc::new(Int32Array::from_iter({
        std::iter::repeat(None)
            .take(index as usize)
            .chain(rest_of.clone().map(Some))
            .collect::<Vec<Option<i32>>>()
    }));
    let ordered_asc_null_last = Arc::new(Int32Array::from_iter({
        rest_of
            .clone()
            .map(Some)
            .chain(std::iter::repeat(None).take(index as usize))
            .collect::<Vec<Option<i32>>>()
    }));

    let ordered_desc_null_first = Arc::new(Int32Array::from_iter({
        std::iter::repeat(None)
            .take(index as usize)
            .chain(rest_of.rev().map(Some))
            .collect::<Vec<Option<i32>>>()
    }));

    let time = Arc::new(TimestampMillisecondArray::from(
        initial_range
            .clone()
            .map(|x| x as i64 + 1672531200000) // x + 2023-01-01:00.00.00
            .collect::<Vec<i64>>(),
    ));
    let interval_time: ArrayRef = Arc::new(IntervalDayTimeArray::from(
        initial_range
            .map(|x| x as i64 * 100) // x * 100ms
            .collect::<Vec<i64>>(),
    ));

    let float_asc = Arc::new(Float64Array::from_iter_values(
        AscendingRandomFloatIterator::new(0., table_size as f64)
            .take(table_size as usize),
    ));

    let left = RecordBatch::try_from_iter(vec![
        ("la1", ordered.clone()),
        ("lb1", cardinality.clone()),
        ("lc1", cardinality_key_left),
        ("lt1", time.clone()),
        ("la2", ordered.clone()),
        ("la1_des", ordered_des.clone()),
        ("l_asc_null_first", ordered_asc_null_first.clone()),
        ("l_asc_null_last", ordered_asc_null_last.clone()),
        ("l_desc_null_first", ordered_desc_null_first.clone()),
        ("li1", interval_time.clone()),
        ("l_float", float_asc.clone()),
        ("l_random_ordered", random_ordered.clone()),
    ])?;
    let right = RecordBatch::try_from_iter(vec![
        ("ra1", ordered.clone()),
        ("rb1", cardinality),
        ("rc1", cardinality_key_right),
        ("rt1", time),
        ("ra2", ordered),
        ("ra1_des", ordered_des),
        ("r_asc_null_first", ordered_asc_null_first),
        ("r_asc_null_last", ordered_asc_null_last),
        ("r_desc_null_first", ordered_desc_null_first),
        ("ri1", interval_time),
        ("r_float", float_asc),
        ("r_random_ordered", random_ordered),
    ])?;
    Ok((left, right))
}

pub fn create_memory_table(
    left_partition: Vec<RecordBatch>,
    right_partition: Vec<RecordBatch>,
    left_sorted: Vec<LexOrdering>,
    right_sorted: Vec<LexOrdering>,
) -> Result<(Arc<dyn ExecutionPlan>, Arc<dyn ExecutionPlan>)> {
    let left_schema = left_partition[0].schema();
    let left = MemoryExec::try_new(&[left_partition], left_schema, None)?
        .with_sort_information(left_sorted);
    let right_schema = right_partition[0].schema();
    let right = MemoryExec::try_new(&[right_partition], right_schema, None)?
        .with_sort_information(right_sorted);
    Ok((Arc::new(left), Arc::new(right)))
}

/// Filter expr for a + b > c + 10 AND a + b < c + 100
pub(crate) fn complicated_filter(
    filter_schema: &Schema,
) -> Result<Arc<dyn PhysicalExpr>> {
    let left_expr = binary(
        cast(
            binary(
                col("0", filter_schema)?,
                Operator::Plus,
                col("1", filter_schema)?,
                filter_schema,
            )?,
            filter_schema,
            DataType::Int64,
        )?,
        Operator::Gt,
        binary(
            cast(col("2", filter_schema)?, filter_schema, DataType::Int64)?,
            Operator::Plus,
            lit(ScalarValue::Int64(Some(10))),
            filter_schema,
        )?,
        filter_schema,
    )?;

    let right_expr = binary(
        cast(
            binary(
                col("0", filter_schema)?,
                Operator::Plus,
                col("1", filter_schema)?,
                filter_schema,
            )?,
            filter_schema,
            DataType::Int64,
        )?,
        Operator::Lt,
        binary(
            cast(col("2", filter_schema)?, filter_schema, DataType::Int64)?,
            Operator::Plus,
            lit(ScalarValue::Int64(Some(100))),
            filter_schema,
        )?,
        filter_schema,
    )?;
    binary(left_expr, Operator::And, right_expr, filter_schema)
}

pub async fn partitioned_sliding_nested_join_with_filter(
    left: Arc<dyn ExecutionPlan>,
    right: Arc<dyn ExecutionPlan>,
    join_type: &JoinType,
    filter: JoinFilter,
    context: Arc<TaskContext>,
    working_mode: SlidingWindowWorkingMode,
) -> Result<Vec<RecordBatch>> {
    let partition_count = 2;
    let mut output_partition = 1;
    let distribution = distribution_from_join_type(join_type);
    // left
    let left = if matches!(distribution[0], Distribution::SinglePartition) {
        left
    } else {
        output_partition = partition_count;
        Arc::new(RepartitionExec::try_new(
            left,
            Partitioning::RoundRobinBatch(partition_count),
        )?)
    } as Arc<dyn ExecutionPlan>;

    let right = if matches!(distribution[1], Distribution::SinglePartition) {
        right
    } else {
        output_partition = partition_count;
        Arc::new(RepartitionExec::try_new(
            right,
            Partitioning::RoundRobinBatch(partition_count),
        )?)
    } as Arc<dyn ExecutionPlan>;

    let left_sort_expr = left.output_ordering().map(|order| order.to_vec()).ok_or(
        DataFusionError::Internal(
            "SlidingNestedLoopJoinExec needs left and right side ordered.".to_owned(),
        ),
    )?;
    let right_sort_expr = right.output_ordering().map(|order| order.to_vec()).ok_or(
        DataFusionError::Internal(
            "SlidingNestedLoopJoinExec needs left and right side ordered.".to_owned(),
        ),
    )?;

    let join = Arc::new(SlidingNestedLoopJoinExec::try_new(
        left,
        right,
        filter,
        join_type,
        left_sort_expr,
        right_sort_expr,
        working_mode,
    )?);
    let mut batches = vec![];
    for i in 0..output_partition {
        let stream = join.execute(i, context.clone())?;
        let more_batches = common::collect(stream).await?;
        batches.extend(
            more_batches
                .into_iter()
                .filter(|b| b.num_rows() > 0)
                .collect::<Vec<_>>(),
        );
    }
    Ok(batches)
}

/// Returns the column names on the schema
pub fn columns(schema: &Schema) -> Vec<String> {
    schema.fields().iter().map(|f| f.name().clone()).collect()
}

pub async fn partitioned_nested_join_with_filter(
    left: Arc<dyn ExecutionPlan>,
    right: Arc<dyn ExecutionPlan>,
    join_type: &JoinType,
    filter: Option<JoinFilter>,
    context: Arc<TaskContext>,
) -> Result<(Vec<String>, Vec<RecordBatch>)> {
    let partition_count = 4;
    let mut output_partition = 1;
    let distribution = distribution_from_join_type(join_type);
    // left
    let left = if matches!(distribution[0], Distribution::SinglePartition) {
        left
    } else {
        output_partition = partition_count;
        Arc::new(RepartitionExec::try_new(
            left,
            Partitioning::RoundRobinBatch(partition_count),
        )?)
    } as Arc<dyn ExecutionPlan>;

    let right = if matches!(distribution[1], Distribution::SinglePartition) {
        right
    } else {
        output_partition = partition_count;
        Arc::new(RepartitionExec::try_new(
            right,
            Partitioning::RoundRobinBatch(partition_count),
        )?)
    } as Arc<dyn ExecutionPlan>;
    let join = Arc::new(NestedLoopJoinExec::try_new(left, right, filter, join_type)?);
    let columns = columns(&join.schema());
    let mut batches = vec![];
    for i in 0..output_partition {
        let stream = join.execute(i, context.clone())?;
        let more_batches = common::collect(stream).await?;
        batches.extend(
            more_batches
                .into_iter()
                .filter(|b| b.num_rows() > 0)
                .collect::<Vec<_>>(),
        );
    }
    Ok((columns, batches))
}

pub(crate) fn complicated_4_column_exprs(
    expr_id: usize,
    filter_schema: &Schema,
) -> Result<Arc<dyn PhysicalExpr>> {
    let columns = filter_schema
        .fields()
        .iter()
        .enumerate()
        .map(|(index, field)| Column::new(field.name(), index))
        .map(Arc::new)
        .collect::<Vec<_>>();
    match expr_id {
        // Filter expr for a + b > d + 10 AND a < c + 20
        0 => {
            let left_expr = binary(
                cast(
                    binary(
                        columns[0].clone(),
                        Operator::Plus,
                        columns[1].clone(),
                        filter_schema,
                    )?,
                    filter_schema,
                    DataType::Int64,
                )?,
                Operator::Gt,
                binary(
                    cast(columns[3].clone(), filter_schema, DataType::Int64)?,
                    Operator::Plus,
                    lit(ScalarValue::Int64(Some(10))),
                    filter_schema,
                )?,
                filter_schema,
            )?;

            let right_expr = binary(
                cast(columns[0].clone(), filter_schema, DataType::Int64)?,
                Operator::Lt,
                binary(
                    cast(columns[2].clone(), filter_schema, DataType::Int64)?,
                    Operator::Plus,
                    lit(ScalarValue::Int64(Some(20))),
                    filter_schema,
                )?,
                filter_schema,
            )?;
            binary(left_expr, Operator::And, right_expr, filter_schema)
        }
        // Filter expr for a + b > d + 10 AND a < c + 20 AND c > b
        1 => {
            let left_expr = binary(
                cast(
                    binary(
                        columns[0].clone(),
                        Operator::Plus,
                        columns[1].clone(),
                        filter_schema,
                    )?,
                    filter_schema,
                    DataType::Int64,
                )?,
                Operator::Gt,
                binary(
                    cast(columns[3].clone(), filter_schema, DataType::Int64)?,
                    Operator::Plus,
                    lit(ScalarValue::Int64(Some(10))),
                    filter_schema,
                )?,
                filter_schema,
            )?;

            let right_expr = binary(
                cast(columns[0].clone(), filter_schema, DataType::Int64)?,
                Operator::Lt,
                binary(
                    cast(columns[2].clone(), filter_schema, DataType::Int64)?,
                    Operator::Plus,
                    lit(ScalarValue::Int64(Some(20))),
                    filter_schema,
                )?,
                filter_schema,
            )?;
            let left_and = binary(left_expr, Operator::And, right_expr, filter_schema)?;
            let right_and = binary(
                cast(columns[2].clone(), filter_schema, DataType::Int64)?,
                Operator::GtEq,
                binary(
                    cast(columns[1].clone(), filter_schema, DataType::Int64)?,
                    Operator::Plus,
                    lit(ScalarValue::Int64(Some(20))),
                    filter_schema,
                )?,
                filter_schema,
            )?;
            binary(left_and, Operator::And, right_and, filter_schema)
        }
        // a + b > c + 10 AND a + b < c + 100
        2 => {
            let left_expr = binary(
                cast(
                    binary(
                        columns[0].clone(),
                        Operator::Plus,
                        columns[1].clone(),
                        filter_schema,
                    )?,
                    filter_schema,
                    DataType::Int64,
                )?,
                Operator::Gt,
                binary(
                    cast(columns[2].clone(), filter_schema, DataType::Int64)?,
                    Operator::Plus,
                    lit(ScalarValue::Int64(Some(10))),
                    filter_schema,
                )?,
                filter_schema,
            )?;

            let right_expr = binary(
                cast(
                    binary(
                        columns[0].clone(),
                        Operator::Plus,
                        columns[1].clone(),
                        filter_schema,
                    )?,
                    filter_schema,
                    DataType::Int64,
                )?,
                Operator::Lt,
                binary(
                    cast(columns[2].clone(), filter_schema, DataType::Int64)?,
                    Operator::Plus,
                    lit(ScalarValue::Int64(Some(100))),
                    filter_schema,
                )?,
                filter_schema,
            )?;
            binary(left_expr, Operator::And, right_expr, filter_schema)
        }
        _ => unimplemented!(),
    }
}

/// This test function generates a conjunctive statement with
/// two scalar values with the following form:
/// left_col (op_1) a  > right_col (op_2)
#[allow(clippy::too_many_arguments)]
pub fn gen_conjunctive_temporal_expr_single_side(
    left_col: Arc<dyn PhysicalExpr>,
    right_col: Arc<dyn PhysicalExpr>,
    op_1: Operator,
    op_2: Operator,
    a: ScalarValue,
    b: ScalarValue,
    schema: &Schema,
    comparison_op: Operator,
) -> Result<Arc<dyn PhysicalExpr>, DataFusionError> {
    let left_and_1 = binary(left_col.clone(), op_1, Arc::new(Literal::new(a)), schema)?;
    let left_and_2 = binary(right_col.clone(), op_2, Arc::new(Literal::new(b)), schema)?;
    Ok(Arc::new(BinaryExpr::new(
        left_and_1,
        comparison_op,
        left_and_2,
    )))
}

/// This test function generates a conjunctive statement with two numeric
/// terms with the following form:
/// left_col (op_1) a  >/>= right_col (op_2)
pub fn gen_conjunctive_numerical_expr_single_side_prunable(
    left_col: Arc<dyn PhysicalExpr>,
    right_col: Arc<dyn PhysicalExpr>,
    op: (Operator, Operator),
    a: ScalarValue,
    b: ScalarValue,
    comparison_op: Operator,
) -> Arc<dyn PhysicalExpr> {
    let (op_1, op_2) = op;
    let left_and_1 = Arc::new(BinaryExpr::new(
        left_col.clone(),
        op_1,
        Arc::new(Literal::new(a)),
    ));
    let left_and_2 = Arc::new(BinaryExpr::new(
        right_col.clone(),
        op_2,
        Arc::new(Literal::new(b)),
    ));
    Arc::new(BinaryExpr::new(left_and_1, comparison_op, left_and_2))
}
