/*
 * Copyright 2019 The Starlark in Rust Authors.
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

use std::any;
use std::cmp::Ordering;
use std::fmt;
use std::fmt::Debug;
use std::fmt::Display;
use std::slice;

use allocative::Allocative;
use cmp_any::OrdAny;
use cmp_any::PartialEqAny;
use either::Either;
use serde::Serialize;
use serde::Serializer;

use crate::codemap::CodeMap;
use crate::codemap::Span;
use crate::codemap::Spanned;
use crate::docs::DocFunction;
use crate::docs::DocMember;
use crate::docs::DocParam;
use crate::docs::DocProperty;
use crate::docs::DocType;
use crate::eval::compiler::scope::payload::CstExpr;
use crate::eval::compiler::scope::payload::CstTypeExpr;
use crate::eval::compiler::scope::ResolvedIdent;
use crate::slice_vec_ext::SliceExt;
use crate::slice_vec_ext::VecExt;
use crate::syntax::ast::ArgumentP;
use crate::syntax::ast::AstLiteral;
use crate::syntax::ast::ExprP;
use crate::typing::ctx::TypingContext;
use crate::typing::error::InternalError;
use crate::typing::error::TypingError;
use crate::typing::function::Arg;
use crate::typing::function::Param;
use crate::typing::function::ParamMode;
use crate::typing::function::TyCustomFunction;
use crate::typing::function::TyCustomFunctionImpl;
use crate::typing::function::TyFunction;
use crate::typing::mode::TypecheckMode;
use crate::typing::oracle::ctx::TypingOracleCtx;
use crate::typing::oracle::traits::TypingAttr;
use crate::typing::structs::TyStruct;
use crate::typing::TypingOracle;
use crate::values::typing::TypeCompiled;
use crate::values::Heap;

/// A typing operation wasn't able to produce a precise result,
/// so made some kind of approximation.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct Approximation {
    /// The category of the approximation, e.g. `"Unknown type"`.
    pub category: &'static str,
    /// The precise details of this approximation, e.g. which type was unknown.
    pub message: String,
}

impl Approximation {
    /// Create a new [`Approximation`].
    pub fn new(category: &'static str, message: impl Debug) -> Self {
        Self {
            category,
            message: format!("{:?}", message),
        }
    }
}

impl Display for Approximation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Approximation: {} = {:?}", self.category, self.message)
    }
}

/// A Starlark type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Allocative)]
pub enum Ty {
    /// Type that can't be inhabited.
    /// If an expression has this type, then the code cannot be reached.
    Never,
    /// Type that contain anything
    Any,
    /// A series of alternative types.
    ///
    /// When typechecking, we try all alternatives, and if at least one of them
    /// succeeds, then the whole expression is considered to be a success.
    ///
    /// For example, when typechecking:
    ///
    /// ```python
    /// x = ... # string or int
    /// y = ... # string
    /// x + y   # `int + string` fails, but `string + string` succeeds,
    ///         # so the whole expression is typechecked successfully as `string`
    /// ```
    ///
    /// This is different handling of union types than in TypeScript for example,
    /// TypeScript would consider such expression to be an error.
    Union(TyUnion),
    /// A name, represented by `"name"` in the Starlark type.
    /// Will never be a type that can be represented by another operation,
    /// e.g. never `"list"` because `Ty::List` could be used instead.
    Name(TyName),
    /// Iter is a type that supports iteration, only used as arguments to primitive functions.
    /// The inner type is applicable for each iteration element.
    Iter(Box<Ty>),
    /// A list.
    List(Box<Ty>),
    /// A tuple. May be empty, to indicate the empty tuple.
    Tuple(Vec<Ty>),
    /// A dictionary, with key and value types
    Dict(Box<(Ty, Ty)>),
    /// A `struct`.
    Struct(TyStruct),
    /// Custom type.
    Custom(TyCustom),
}

impl Serialize for Ty {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Arbitrary custom types are not deserializable, so serialization to string is enough.
        serializer.serialize_str(&self.to_string())
    }
}

/// The name of an atomic type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Allocative)]
pub struct TyName(String);

impl Display for TyName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.as_str() {
            "string" => write!(f, "str.type"),
            "int" => write!(f, "int.type"),
            "bool" => write!(f, "bool.type"),
            "NoneType" => write!(f, "None"),
            other => write!(f, "\"{}\"", other),
        }
    }
}

impl PartialEq<str> for TyName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl TyName {
    /// Get the underlying `str` for a `TyName`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A series of types that are unioned together.
/// Must be at least two elements, all distinct elements, with no nested `Union` types directly inside it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Allocative)]
pub struct TyUnion(Vec<Ty>);

impl Display for TyUnion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        display_container::fmt_container(f, "[", "]", &self.0)
    }
}

impl TyUnion {
    /// The alternatives within a union, will always be at least two elements.
    pub fn alternatives(&self) -> &[Ty] {
        &self.0
    }
}

/// Custom type implementation. [`Display`] must implement the representation of the type.
pub trait TyCustomImpl: Debug + Display + Clone + Ord + Allocative + Send + Sync + 'static {
    fn as_name(&self) -> Option<&str>;
    fn has_type_attr(&self) -> bool {
        false
    }
    fn validate_call(
        &self,
        span: Span,
        args: &[Spanned<Arg>],
        oracle: TypingOracleCtx,
    ) -> Result<Ty, TypingError>;
}

pub(crate) trait TyCustomDyn: Debug + Display + Allocative + Send + Sync + 'static {
    fn eq_token(&self) -> PartialEqAny;
    fn cmp_token(&self) -> (OrdAny, &'static str);

    fn clone_box(&self) -> Box<dyn TyCustomDyn>;
    fn as_name(&self) -> Option<&str>;
    fn has_type_attr(&self) -> bool;
    fn validate_call(
        &self,
        span: Span,
        args: &[Spanned<Arg>],
        oracle: TypingOracleCtx,
    ) -> Result<Ty, TypingError>;
}

impl<T: TyCustomImpl> TyCustomDyn for T {
    fn eq_token(&self) -> PartialEqAny {
        PartialEqAny::new(self)
    }

    fn cmp_token(&self) -> (OrdAny, &'static str) {
        (OrdAny::new(self), any::type_name::<Self>())
    }

    fn clone_box(&self) -> Box<dyn TyCustomDyn> {
        Box::new(self.clone())
    }

    fn as_name(&self) -> Option<&str> {
        self.as_name()
    }

    fn has_type_attr(&self) -> bool {
        self.has_type_attr()
    }

    fn validate_call(
        &self,
        span: Span,
        args: &[Spanned<Arg>],
        oracle: TypingOracleCtx,
    ) -> Result<Ty, TypingError> {
        self.validate_call(span, args, oracle)
    }
}

#[derive(Debug, derive_more::Display, Allocative)]
pub struct TyCustom(pub(crate) Box<dyn TyCustomDyn>);

impl TyCustom {
    pub(crate) fn as_name(&self) -> Option<&str> {
        self.0.as_name()
    }
}

impl PartialEq for TyCustom {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq_token() == other.0.eq_token()
    }
}

impl Eq for TyCustom {}

impl PartialOrd for TyCustom {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TyCustom {
    fn cmp(&self, other: &Self) -> Ordering {
        let (a_cmp, a_type_name) = self.0.cmp_token();
        let (b_cmp, b_type_name) = other.0.cmp_token();

        // Type ids are comparable, but we want comparison independent of hashing.
        if OrdAny::type_id(&a_cmp) != OrdAny::type_id(&b_cmp) {
            let type_name_cmp = a_type_name.cmp(b_type_name);
            if type_name_cmp != Ordering::Equal {
                return type_name_cmp;
            }

            // This is unreachable: if the type names are the same,
            // the type ids should be the same.
        }

        a_cmp.cmp(&b_cmp)
    }
}

impl Clone for TyCustom {
    fn clone(&self) -> TyCustom {
        TyCustom(self.0.clone_box())
    }
}

fn merge_adjacent<T>(xs: Vec<T>, f: impl Fn(T, T) -> Either<T, (T, T)>) -> Vec<T> {
    let mut res = Vec::new();
    let mut last = None;
    for x in xs {
        match last {
            None => last = Some(x),
            Some(l) => match f(l, x) {
                Either::Left(x) => last = Some(x),
                Either::Right((l, x)) => {
                    res.push(l);
                    last = Some(x)
                }
            },
        }
    }
    if let Some(l) = last {
        res.push(l)
    }
    res
}

impl Ty {
    /// Create a [`Ty::Any`], but tagged in such a way it can easily be found.
    pub fn todo() -> Self {
        Ty::Any
    }

    /// Create a [`Ty::Name`], or one of the standard functions.
    pub fn name(name: &str) -> Self {
        match name {
            "list" => Self::List(Box::new(Ty::Any)),
            "dict" => Self::Dict(Box::new((Ty::Any, Ty::Any))),
            "function" => {
                Self::function(vec![Param::args(Ty::Any), Param::kwargs(Ty::Any)], Ty::Any)
            }
            "struct" => Self::Struct(TyStruct::any()),
            "never" => Self::Never,
            // Note that "tuple" cannot be converted to Ty::Tuple
            // since we don't know the length of the tuple.
            _ => Self::Name(TyName(name.to_owned())),
        }
    }

    /// Turn a type back into a name, potentially erasing some structure.
    /// E.g. the type `[bool]` would return `list`.
    /// Types like [`Ty::Any`] will return `None`.
    pub fn as_name(&self) -> Option<&str> {
        match self {
            Ty::Name(x) => Some(x.as_str()),
            Ty::List(_) => Some("list"),
            Ty::Tuple(_) => Some("tuple"),
            Ty::Dict(_) => Some("dict"),
            Ty::Struct { .. } => Some("struct"),
            Ty::Never => Some("never"),
            Ty::Custom(c) => c.as_name(),
            Ty::Any | Ty::Union(_) | Ty::Iter(_) => None,
        }
    }

    /// Create a `None` type.
    pub fn none() -> Self {
        Self::name("NoneType")
    }

    /// Create a boolean type.
    pub fn bool() -> Self {
        Self::name("bool")
    }

    /// Create the int type.
    pub fn int() -> Self {
        Self::name("int")
    }

    /// Create a float type.
    pub fn float() -> Self {
        Self::name("float")
    }

    /// Create a string type.
    pub fn string() -> Self {
        Self::name("string")
    }

    /// Create a list type.
    pub fn list(element: Ty) -> Self {
        Ty::List(Box::new(element))
    }

    /// Create a iterable type.
    pub fn iter(item: Ty) -> Self {
        Ty::Iter(Box::new(item))
    }

    /// Create a dictionary type.
    pub fn dict(key: Ty, value: Ty) -> Self {
        Ty::Dict(Box::new((key, value)))
    }

    /// Create a tuple of two elements
    pub fn tuple2(a: Ty, b: Ty) -> Self {
        Ty::Tuple(vec![a, b])
    }

    /// Create a function type.
    pub fn function(params: Vec<Param>, result: Ty) -> Self {
        Self::custom(TyCustomFunction(TyFunction {
            type_attr: String::new(),
            params,
            result: Box::new(result),
        }))
    }

    /// Create a function, where the first argument is the result of `.type`.
    pub fn ctor_function(type_attr: &str, params: Vec<Param>, result: Ty) -> Self {
        Self::custom(TyCustomFunction(TyFunction {
            type_attr: type_attr.to_owned(),
            params,
            result: Box::new(result),
        }))
    }

    pub(crate) fn is_any(&self) -> bool {
        self == &Ty::Any
    }

    pub(crate) fn is_never(&self) -> bool {
        self == &Ty::Never
    }

    pub(crate) fn is_list(&self) -> bool {
        matches!(self, Ty::List(_))
    }

    pub(crate) fn is_name(&self, name: &str) -> bool {
        match self {
            Ty::Name(x) => x == name,
            _ => false,
        }
    }

    /// Create a unions type, which will be normalised before being created.
    pub fn unions(mut xs: Vec<Self>) -> Self {
        xs = xs.into_iter().flat_map(|x| x.into_iter_union()).collect();
        xs.sort();
        xs.dedup();
        xs.retain(|x| x != &Ty::Never);
        if xs.contains(&Ty::Any) {
            return Ty::Any;
        }
        // Try merging adjacent elements
        xs = merge_adjacent(xs, |x, y| match (x, y) {
            (Ty::List(x), Ty::List(y)) => Either::Left(Ty::list(Ty::union2(*x, *y))),
            (Ty::Dict(x), Ty::Dict(y)) => {
                Either::Left(Ty::dict(Ty::union2(x.0, y.0), Ty::union2(x.1, y.1)))
            }
            (Ty::Struct(x), Ty::Struct(y)) => match TyStruct::union2(x, y) {
                Ok(u) => Either::Left(Ty::Struct(u)),
                Err((x, y)) => Either::Right((Ty::Struct(x), Ty::Struct(y))),
            },
            xy => Either::Right(xy),
        });

        if xs.is_empty() {
            Ty::Never
        } else if xs.len() == 1 {
            xs.pop().unwrap()
        } else {
            Self::Union(TyUnion(xs))
        }
    }

    /// Iterate over the types within a union, pretending the type is a singleton union if not a union.
    pub(crate) fn iter_union(&self) -> &[Self] {
        match self {
            Self::Union(xs) => &xs.0,
            Self::Never => &[],
            _ => slice::from_ref(self),
        }
    }

    /// Iterate over the types within a union, pretending the type is a singleton union if not a union.
    pub(crate) fn into_iter_union(self) -> impl Iterator<Item = Self> {
        match self {
            Self::Union(xs) => Either::Left(xs.0.into_iter()),
            Self::Never => Either::Left(Vec::new().into_iter()),
            _ => Either::Right(std::iter::once(self)),
        }
    }

    /// Create a union of two entries.
    pub fn union2(a: Self, b: Self) -> Self {
        Self::unions(vec![a, b])
    }

    /// Create a custom type.
    /// This is called from generated code.
    pub fn custom(t: impl TyCustomImpl) -> Self {
        Ty::Custom(TyCustom(Box::new(t)))
    }

    /// Create a custom function type.
    pub fn custom_function(f: impl TyCustomFunctionImpl) -> Self {
        Ty::custom(TyCustomFunction(f))
    }

    /// If I do `self[i]` what will the resulting type be.
    pub(crate) fn indexed(self, i: usize) -> Ty {
        match self {
            Ty::Any => Ty::Any,
            Ty::Never => Ty::Never,
            Ty::List(x) => *x,
            Ty::Tuple(xs) => xs.get(i).cloned().unwrap_or(Ty::Never),
            Ty::Union(xs) => Ty::unions(xs.0.into_map(|x| x.indexed(i))),
            // Not exactly sure what we should do here
            _ => Ty::Any,
        }
    }

    /// Returns false on Void, since that is definitely not a list
    pub(crate) fn probably_a_list(&self, ctx: TypingOracleCtx) -> bool {
        if self.is_never() {
            return false;
        }
        self.intersects(&Self::list(Ty::Any), ctx)
    }

    /// See what lies behind an attribute on a type
    pub(crate) fn attribute(&self, attr: TypingAttr, ctx: &TypingContext) -> Result<Ty, ()> {
        // There are some structural types which have to be handled in a specific way
        match self {
            Ty::Any => Ok(Ty::Any),
            Ty::Never => Ok(Ty::Never),
            Ty::Union(xs) => {
                let rs = xs
                    .alternatives()
                    .iter()
                    .flat_map(|x| x.attribute(attr, ctx))
                    .collect::<Vec<_>>();
                if rs.is_empty() {
                    // Since xs wasn't empty, we must have had all types give us an invalid attribute.
                    // So therefore this attribute must be invalid.
                    Err(())
                } else {
                    Ok(Ty::unions(rs))
                }
            }
            Ty::Custom(c) if attr == TypingAttr::Regular("type") && c.0.has_type_attr() => {
                Ok(Ty::string())
            }
            _ => match ctx.oracle.attribute(self, attr) {
                Some(r) => r,
                None => Ok(ctx.approximation("oracle.attribute", format!("{}.{}", self, attr))),
            },
        }
    }

    /// If you get to a point where these types are being checked, might they succeed
    pub(crate) fn intersects(&self, other: &Self, oracle: TypingOracleCtx) -> bool {
        if self.is_any() || self.is_never() || other.is_any() || other.is_never() {
            return true;
        }

        let equal_names =
            |x: &TyName, y: &TyName| x == y || oracle.subtype(x, y) || oracle.subtype(y, x);

        let itered = |ty: &Ty| oracle.attribute(ty, TypingAttr::Iter)?.ok();

        for x in self.iter_union() {
            for y in other.iter_union() {
                let b = match (x, y) {
                    (Ty::Name(x), Ty::Name(y)) => equal_names(x, y),
                    (Ty::List(x), Ty::List(y)) => x.intersects(y, oracle),
                    (Ty::Dict(x), Ty::Dict(y)) => {
                        x.0.intersects(&y.0, oracle) && x.1.intersects(&y.1, oracle)
                    }
                    (Ty::Tuple(_), t) | (t, Ty::Tuple(_)) if t.is_name("tuple") => true,
                    (Ty::Tuple(xs), Ty::Tuple(ys)) if xs.len() == ys.len() => {
                        std::iter::zip(xs, ys).all(|(x, y)| x.intersects(y, oracle))
                    }
                    (Ty::Iter(x), Ty::Iter(y)) => x.intersects(y, oracle),
                    (Ty::Iter(x), y) | (y, Ty::Iter(x)) => match itered(y) {
                        Some(yy) => x.intersects(&yy, oracle),
                        None => false,
                    },
                    (Ty::Struct { .. }, Ty::Struct { .. }) => {
                        // FIXME: Can probably be a bit more precise here
                        true
                    }
                    (x, y)
                        if x.as_name() == Some("function") && y.as_name() == Some("function") =>
                    {
                        true
                    }
                    // There are lots of other cases that overlap, but add them as we need them
                    (x, y) => x == y,
                };
                if b {
                    return true;
                }
            }
        }
        return false;
    }

    pub(crate) fn from_type_expr_opt(
        x: &Option<Box<CstTypeExpr>>,
        typecheck_mode: TypecheckMode,
        approximations: &mut Vec<Approximation>,
        codemap: &CodeMap,
    ) -> Result<Self, InternalError> {
        match x {
            None => Ok(Ty::Any),
            Some(x) => Self::from_type_expr(x, typecheck_mode, approximations, codemap),
        }
    }

    pub(crate) fn from_type_expr(
        x: &CstTypeExpr,
        typecheck_mode: TypecheckMode,
        approximations: &mut Vec<Approximation>,
        codemap: &CodeMap,
    ) -> Result<Self, InternalError> {
        match typecheck_mode {
            TypecheckMode::Lint => {
                // TODO(nga): remove this branch: in lint, populate types in CstPayload
                //   before running typechecking, and always fetch the type from the payload.
                Ok(Self::from_expr(&x.expr, approximations))
            }
            TypecheckMode::Compiler => match x.payload {
                Some(ty) => Ok(ty.as_ty()),
                None => Err(InternalError::msg(
                    "type payload is not populated",
                    x.span,
                    codemap,
                )),
            },
        }
    }

    // This should go away when `ExprType` is disconnected from `Expr`.
    fn from_expr(x: &CstExpr, approximations: &mut Vec<Approximation>) -> Self {
        let mut unknown = || {
            approximations.push(Approximation::new("Unknown type", x));
            Ty::Any
        };
        match &x.node {
            ExprP::Tuple(xs) => Ty::Tuple(xs.map(|x| Self::from_expr(x, approximations))),
            ExprP::Dot(x, b) if &**b == "type" => match &***x {
                ExprP::Identifier(x) => match x.node.0.as_str() {
                    "str" => Ty::string(),
                    x => Ty::name(x),
                },
                _ => unknown(),
            },
            ExprP::Literal(AstLiteral::String(x)) => {
                if x.is_empty() || x.starts_with('_') {
                    Ty::Any
                } else {
                    Ty::name(x.as_str())
                }
            }
            ExprP::List(x) => {
                if x.len() == 1 {
                    Ty::list(Self::from_expr(&x[0], approximations))
                } else {
                    Ty::unions(x.map(|x| Self::from_expr(x, approximations)))
                }
            }
            ExprP::Dict(x) if x.len() == 1 => Ty::dict(
                Self::from_expr(&x[0].0, approximations),
                Self::from_expr(&x[0].1, approximations),
            ),
            ExprP::Identifier(x) => {
                if let Some(resolved) = &x.node.1 {
                    match resolved {
                        ResolvedIdent::Slot(_, _) => {
                            // Should not happen: only global identifiers are allowed in type.
                            unknown()
                        }
                        ResolvedIdent::Global(v) => {
                            let heap = Heap::new();
                            match TypeCompiled::new(v.to_value(), &heap) {
                                Ok(ty) => ty.as_ty(),
                                Err(_) => unknown(),
                            }
                        }
                    }
                } else {
                    // Scopes must be resolved, but we can still run typechecking
                    // if scope resolution fails.
                    unknown()
                }
            }
            ExprP::Call(fun, args) if args.len() == 1 => match (&fun.node, &args[0].node) {
                (ExprP::Identifier(name), ArgumentP::Positional(arg)) if name.node.0 == "iter" => {
                    Ty::iter(Ty::from_expr(arg, approximations))
                }
                _ => unknown(),
            },
            _ => unknown(),
        }
    }

    pub(crate) fn from_docs_member(member: &DocMember) -> Self {
        match member {
            DocMember::Property(x) => Self::from_docs_type(&x.typ),
            DocMember::Function(x) => Self::from_docs_function(x),
        }
    }

    pub(crate) fn from_docs_property(property: &DocProperty) -> Self {
        Self::from_docs_type(&property.typ)
    }

    pub(crate) fn from_docs_function(function: &DocFunction) -> Self {
        let mut params = Vec::with_capacity(function.params.len());
        let mut seen_no_args = false;
        for p in &function.params {
            match p {
                DocParam::Arg {
                    name,
                    typ,
                    default_value,
                    ..
                } => {
                    let mut r = if seen_no_args {
                        Param::name_only(name, Ty::from_docs_type(typ))
                    } else {
                        Param::pos_or_name(name, Ty::from_docs_type(typ))
                    };
                    if default_value.is_some() {
                        r = r.optional();
                    }
                    params.push(r);
                }
                DocParam::OnlyPosBefore => {
                    for x in params.iter_mut() {
                        if matches!(x.mode, ParamMode::PosOrName(_)) {
                            x.mode = ParamMode::PosOnly;
                        }
                    }
                }
                DocParam::NoArgs => seen_no_args = true,
                DocParam::Args { typ, .. } => {
                    seen_no_args = true;
                    params.push(Param::args(Ty::from_docs_type(typ)))
                }
                DocParam::Kwargs { typ, .. } => params.push(Param::kwargs(Ty::from_docs_type(typ))),
            }
        }
        let result = Self::from_docs_type(&function.ret.typ);
        match &function.dot_type {
            None => Ty::function(params, result),
            Some(type_attr) => Ty::ctor_function(type_attr, params, result),
        }
    }

    pub(crate) fn from_docs_type(ty: &Option<DocType>) -> Self {
        match ty {
            None => Ty::Any,
            Some(x) => x.raw_type.clone(),
        }
    }
}

impl Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ty::Never => write!(f, "\"never\""),
            Ty::Any => write!(f, "\"\""),
            Ty::Union(xs) => write!(f, "{}", xs),
            Ty::Name(x) => write!(f, "{}", x),
            Ty::Iter(x) => write!(f, "iter({})", x),
            Ty::List(x) => write!(f, "[{}]", x),
            Ty::Tuple(xs) => {
                if xs.len() == 1 {
                    write!(f, "({},)", xs[0])
                } else {
                    display_container::fmt_container(f, "(", ")", xs)
                }
            }
            Ty::Dict(k_v) => write!(f, "{{{}: {}}}", k_v.0, k_v.1),
            Ty::Struct(s) => Display::fmt(s, f),
            Ty::Custom(c) => Display::fmt(c, f),
        }
    }
}
