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

//! DateTime expressions
use std::sync::Arc;

use super::ColumnarValue;
use crate::arrow_temporal_util::string_to_timestamp_nanos;
use crate::{
    error::{DataFusionError, Result},
    scalar::ScalarValue,
};
use arrow::{
    array::*,
    compute::cast,
    datatypes::{DataType, TimeUnit},
    scalar::PrimitiveScalar,
    types::NativeType,
};
use arrow::{compute::temporal, temporal_conversions::timestamp_ns_to_datetime};
use chrono::prelude::{DateTime, Utc};
use chrono::Datelike;
use chrono::Duration;
use chrono::Timelike;
use std::borrow::Borrow;

/// given a function `op` that maps a `&str` to a Result of an arrow native type,
/// returns a `PrimitiveArray` after the application
/// of the function to `args[0]`.
/// # Errors
/// This function errors iff:
/// * the number of arguments is not 1 or
/// * the first argument is not castable to a `Utf8Array` or
/// * the function `op` errors
pub(crate) fn unary_string_to_primitive_function<'a, T, O, F>(
    args: &[&'a dyn Array],
    op: F,
    name: &str,
) -> Result<PrimitiveArray<O>>
where
    O: NativeType,
    T: Offset,
    F: Fn(&'a str) -> Result<O>,
{
    if args.len() != 1 {
        return Err(DataFusionError::Internal(format!(
            "{:?} args were supplied but {} takes exactly one argument",
            args.len(),
            name,
        )));
    }

    let array = args[0]
        .as_any()
        .downcast_ref::<Utf8Array<T>>()
        .ok_or_else(|| {
            DataFusionError::Internal("failed to downcast to string".to_string())
        })?;

    // first map is the iterator, second is for the `Option<_>`
    array
        .iter()
        .map(|x| x.map(op.borrow()).transpose())
        .collect()
}

// given an function that maps a `&str` to a arrow native type,
// returns a `ColumnarValue` where the function is applied to either a `ArrayRef` or `ScalarValue`
// depending on the `args`'s variant.
fn handle<'a, O, F>(
    args: &'a [ColumnarValue],
    op: F,
    name: &str,
    data_type: DataType,
) -> Result<ColumnarValue>
where
    O: NativeType,
    ScalarValue: From<Option<O>>,
    F: Fn(&'a str) -> Result<O>,
{
    match &args[0] {
        ColumnarValue::Array(a) => match a.data_type() {
            DataType::Utf8 => Ok(ColumnarValue::Array(Arc::new(
                unary_string_to_primitive_function::<i32, O, _>(&[a.as_ref()], op, name)?
                    .to(data_type),
            ))),
            DataType::LargeUtf8 => Ok(ColumnarValue::Array(Arc::new(
                unary_string_to_primitive_function::<i64, O, _>(&[a.as_ref()], op, name)?
                    .to(data_type),
            ))),
            other => Err(DataFusionError::Internal(format!(
                "Unsupported data type {:?} for function {}",
                other, name,
            ))),
        },
        ColumnarValue::Scalar(scalar) => match scalar {
            ScalarValue::Utf8(a) | ScalarValue::LargeUtf8(a) => Ok(match a {
                Some(s) => {
                    let s = PrimitiveScalar::<O>::new(data_type, Some((op)(s)?));
                    ColumnarValue::Scalar(s.try_into()?)
                }
                None => ColumnarValue::Scalar(ScalarValue::new_null(data_type)),
            }),
            other => Err(DataFusionError::Internal(format!(
                "Unsupported data type {:?} for function {}",
                other, name
            ))),
        },
    }
}

/// Calls cast::string_to_timestamp_nanos and converts the error type
fn string_to_timestamp_nanos_shim(s: &str) -> Result<i64> {
    string_to_timestamp_nanos(s).map_err(|e| e.into())
}

/// to_timestamp SQL function
pub fn to_timestamp(args: &[ColumnarValue]) -> Result<ColumnarValue> {
    handle::<i64, _>(
        args,
        string_to_timestamp_nanos_shim,
        "to_timestamp",
        DataType::Timestamp(TimeUnit::Nanosecond, None),
    )
}

/// to_timestamp_millis SQL function
pub fn to_timestamp_millis(args: &[ColumnarValue]) -> Result<ColumnarValue> {
    handle::<i64, _>(
        args,
        |s| string_to_timestamp_nanos_shim(s).map(|n| n / 1_000_000),
        "to_timestamp_millis",
        DataType::Timestamp(TimeUnit::Millisecond, None),
    )
}

/// to_timestamp_micros SQL function
pub fn to_timestamp_micros(args: &[ColumnarValue]) -> Result<ColumnarValue> {
    handle::<i64, _>(
        args,
        |s| string_to_timestamp_nanos_shim(s).map(|n| n / 1_000),
        "to_timestamp_micros",
        DataType::Timestamp(TimeUnit::Microsecond, None),
    )
}

/// to_timestamp_seconds SQL function
pub fn to_timestamp_seconds(args: &[ColumnarValue]) -> Result<ColumnarValue> {
    handle::<i64, _>(
        args,
        |s| string_to_timestamp_nanos_shim(s).map(|n| n / 1_000_000_000),
        "to_timestamp_seconds",
        DataType::Timestamp(TimeUnit::Second, None),
    )
}

/// Create an implementation of `now()` that always returns the
/// specified timestamp.
///
/// The semantics of `now()` require it to return the same value
/// whenever it is called in a query. This this value is chosen during
/// planning time and bound into a closure that
pub fn make_now(
    now_ts: DateTime<Utc>,
) -> impl Fn(&[ColumnarValue]) -> Result<ColumnarValue> {
    let now_ts = Some(now_ts.timestamp_nanos());
    move |_arg| {
        Ok(ColumnarValue::Scalar(ScalarValue::TimestampNanosecond(
            now_ts,
            Some("UTC".to_owned()),
        )))
    }
}

fn date_trunc_single(granularity: &str, value: i64) -> Result<i64> {
    let value = timestamp_ns_to_datetime(value).with_nanosecond(0);
    let value = match granularity {
        "second" => value,
        "minute" => value.and_then(|d| d.with_second(0)),
        "hour" => value
            .and_then(|d| d.with_second(0))
            .and_then(|d| d.with_minute(0)),
        "day" => value
            .and_then(|d| d.with_second(0))
            .and_then(|d| d.with_minute(0))
            .and_then(|d| d.with_hour(0)),
        "week" => value
            .and_then(|d| d.with_second(0))
            .and_then(|d| d.with_minute(0))
            .and_then(|d| d.with_hour(0))
            .map(|d| d - Duration::seconds(60 * 60 * 24 * d.weekday() as i64)),
        "month" => value
            .and_then(|d| d.with_second(0))
            .and_then(|d| d.with_minute(0))
            .and_then(|d| d.with_hour(0))
            .and_then(|d| d.with_day0(0)),
        "year" => value
            .and_then(|d| d.with_second(0))
            .and_then(|d| d.with_minute(0))
            .and_then(|d| d.with_hour(0))
            .and_then(|d| d.with_day0(0))
            .and_then(|d| d.with_month0(0)),
        unsupported => {
            return Err(DataFusionError::Execution(format!(
                "Unsupported date_trunc granularity: {}",
                unsupported
            )));
        }
    };
    // `with_x(0)` are infalible because `0` are always a valid
    Ok(value.unwrap().timestamp_nanos())
}

/// date_trunc SQL function
pub fn date_trunc(args: &[ColumnarValue]) -> Result<ColumnarValue> {
    let (granularity, array) = (&args[0], &args[1]);

    let granularity =
        if let ColumnarValue::Scalar(ScalarValue::Utf8(Some(v))) = granularity {
            v
        } else {
            return Err(DataFusionError::Execution(
                "Granularity of `date_trunc` must be non-null scalar Utf8".to_string(),
            ));
        };

    let f = |x: Option<&i64>| x.map(|x| date_trunc_single(granularity, *x)).transpose();

    Ok(match array {
        ColumnarValue::Scalar(ScalarValue::TimestampNanosecond(v, tz_opt)) => {
            ColumnarValue::Scalar(ScalarValue::TimestampNanosecond(
                (f)(v.as_ref())?,
                tz_opt.clone(),
            ))
        }
        ColumnarValue::Array(array) => {
            let array = array.as_any().downcast_ref::<Int64Array>().unwrap();
            let array = array
                .iter()
                .map(f)
                .collect::<Result<PrimitiveArray<i64>>>()?
                .to(DataType::Timestamp(TimeUnit::Nanosecond, None));

            ColumnarValue::Array(Arc::new(array))
        }
        _ => {
            return Err(DataFusionError::Execution(
                "array of `date_trunc` must be non-null scalar Utf8".to_string(),
            ));
        }
    })
}

/// DATE_PART SQL function
pub fn date_part(args: &[ColumnarValue]) -> Result<ColumnarValue> {
    if args.len() != 2 {
        return Err(DataFusionError::Execution(
            "Expected two arguments in DATE_PART".to_string(),
        ));
    }
    let (date_part, array) = (&args[0], &args[1]);

    let date_part = if let ColumnarValue::Scalar(ScalarValue::Utf8(Some(v))) = date_part {
        v
    } else {
        return Err(DataFusionError::Execution(
            "First argument of `DATE_PART` must be non-null scalar Utf8".to_string(),
        ));
    };

    let is_scalar = matches!(array, ColumnarValue::Scalar(_));

    let array = match array {
        ColumnarValue::Array(array) => array.clone(),
        ColumnarValue::Scalar(scalar) => scalar.to_array(),
    };

    let arr = match date_part.to_lowercase().as_str() {
        "hour" => Ok(temporal::hour(array.as_ref())
            .map(|x| cast::primitive_to_primitive::<u32, i32>(&x, &DataType::Int32))?),
        "year" => Ok(temporal::year(array.as_ref())?),
        _ => Err(DataFusionError::Execution(format!(
            "Date part '{}' not supported",
            date_part
        ))),
    }?;

    Ok(if is_scalar {
        ColumnarValue::Scalar(ScalarValue::try_from_array(
            &(Arc::new(arr) as ArrayRef),
            0,
        )?)
    } else {
        ColumnarValue::Array(Arc::new(arr))
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::*;
    use arrow::datatypes::*;

    use super::*;

    #[test]
    fn to_timestamp_arrays_and_nulls() -> Result<()> {
        // ensure that arrow array implementation is wired up and handles nulls correctly

        let string_array =
            Utf8Array::<i32>::from(&[Some("2020-09-08T13:42:29.190855Z"), None]);

        let ts_array = Int64Array::from(&[Some(1599572549190855000), None])
            .to(DataType::Timestamp(TimeUnit::Nanosecond, None));

        let expected_timestamps = &ts_array as &dyn Array;

        let string_array = ColumnarValue::Array(Arc::new(string_array) as ArrayRef);
        let parsed_timestamps = to_timestamp(&[string_array])
            .expect("that to_timestamp parsed values without error");
        if let ColumnarValue::Array(parsed_array) = parsed_timestamps {
            assert_eq!(parsed_array.len(), 2);
            assert_eq!(expected_timestamps, parsed_array.as_ref());
        } else {
            panic!("Expected a columnar array")
        }
        Ok(())
    }

    #[test]
    fn date_trunc_test() {
        let cases = vec![
            (
                "2020-09-08T13:42:29.190855Z",
                "second",
                "2020-09-08T13:42:29.000000Z",
            ),
            (
                "2020-09-08T13:42:29.190855Z",
                "minute",
                "2020-09-08T13:42:00.000000Z",
            ),
            (
                "2020-09-08T13:42:29.190855Z",
                "hour",
                "2020-09-08T13:00:00.000000Z",
            ),
            (
                "2020-09-08T13:42:29.190855Z",
                "day",
                "2020-09-08T00:00:00.000000Z",
            ),
            (
                "2020-09-08T13:42:29.190855Z",
                "week",
                "2020-09-07T00:00:00.000000Z",
            ),
            (
                "2020-09-08T13:42:29.190855Z",
                "month",
                "2020-09-01T00:00:00.000000Z",
            ),
            (
                "2020-09-08T13:42:29.190855Z",
                "year",
                "2020-01-01T00:00:00.000000Z",
            ),
            (
                "2021-01-01T13:42:29.190855Z",
                "week",
                "2020-12-28T00:00:00.000000Z",
            ),
            (
                "2020-01-01T13:42:29.190855Z",
                "week",
                "2019-12-30T00:00:00.000000Z",
            ),
        ];

        cases.iter().for_each(|(original, granularity, expected)| {
            let original = string_to_timestamp_nanos(original).unwrap();
            let expected = string_to_timestamp_nanos(expected).unwrap();
            let result = date_trunc_single(granularity, original).unwrap();
            assert_eq!(result, expected);
        });
    }

    #[test]
    fn to_timestamp_invalid_input_type() -> Result<()> {
        // pass the wrong type of input array to to_timestamp and test
        // that we get an error.

        let array = Int64Array::from_slice(&[1]);
        let int64array = ColumnarValue::Array(Arc::new(array));

        let expected_err =
            "Internal error: Unsupported data type Int64 for function to_timestamp";
        match to_timestamp(&[int64array]) {
            Ok(_) => panic!("Expected error but got success"),
            Err(e) => {
                assert!(
                    e.to_string().contains(expected_err),
                    "Can not find expected error '{}'. Actual error '{}'",
                    expected_err,
                    e
                );
            }
        }
        Ok(())
    }
}
