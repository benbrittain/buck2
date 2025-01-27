/*
 * Copyright 2018 The Starlark in Rust Authors.
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use crate::typing::Ty;
use crate::values::structs::value::FrozenStruct;
use crate::values::structs::value::Struct;
use crate::values::type_repr::StarlarkTypeRepr;
use crate::values::StringValue;
use crate::values::UnpackValue;
use crate::values::Value;

/// Reference to a struct allocated on the heap.
///
/// Struct implementation (for example, memory layout) may change,
/// this type provides implementation agnostics API to it.
#[derive(Debug)]
pub struct StructRef<'v>(&'v Struct<'v>);

impl<'v> StructRef<'v> {
    /// Downcast a value to a struct reference.
    pub fn from_value(value: Value<'v>) -> Option<StructRef<'v>> {
        Struct::from_value(value).map(StructRef)
    }

    /// Iterate over struct fields.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (StringValue<'v>, Value<'v>)> + '_ {
        self.0.iter()
    }
}

impl<'v> StarlarkTypeRepr for StructRef<'v> {
    fn starlark_type_repr() -> Ty {
        FrozenStruct::starlark_type_repr()
    }
}

impl<'v> UnpackValue<'v> for StructRef<'v> {
    fn unpack_value(value: Value<'v>) -> Option<Self> {
        StructRef::from_value(value)
    }
}
