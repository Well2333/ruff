use std::hash::Hash;
use std::iter;

use context::InferContext;
use diagnostic::{report_not_iterable, report_not_iterable_possibly_unbound};
use indexmap::IndexSet;
use itertools::Itertools;
use ruff_db::diagnostic::Severity;
use ruff_db::files::File;
use ruff_python_ast as ast;

pub(crate) use self::builder::{IntersectionBuilder, UnionBuilder};
pub(crate) use self::diagnostic::register_lints;
pub use self::diagnostic::{TypeCheckDiagnostic, TypeCheckDiagnostics};
pub(crate) use self::display::TypeArrayDisplay;
pub(crate) use self::infer::{
    infer_deferred_types, infer_definition_types, infer_expression_types, infer_scope_types,
};
pub use self::narrow::KnownConstraintFunction;
pub(crate) use self::signatures::Signature;
pub use self::subclass_of::SubclassOfType;
use crate::module_name::ModuleName;
use crate::module_resolver::{file_to_module, resolve_module, KnownModule};
use crate::semantic_index::ast_ids::HasScopedExpressionId;
use crate::semantic_index::definition::Definition;
use crate::semantic_index::symbol::{self as symbol, ScopeId, ScopedSymbolId};
use crate::semantic_index::{
    global_scope, imported_modules, semantic_index, symbol_table, use_def_map,
    BindingWithConstraints, BindingWithConstraintsIterator, DeclarationWithConstraint,
    DeclarationsIterator,
};
use crate::stdlib::{builtins_symbol, known_module_symbol, typing_extensions_symbol};
use crate::suppression::check_suppressions;
use crate::symbol::{Boundness, Symbol};
use crate::types::call::{
    bind_call, CallArguments, CallBinding, CallDunderResult, CallOutcome, StaticAssertionErrorKind,
};
use crate::types::class_base::ClassBase;
use crate::types::diagnostic::INVALID_TYPE_FORM;
use crate::types::mro::{Mro, MroError, MroIterator};
use crate::types::narrow::narrowing_constraint;
use crate::{Db, FxOrderSet, Module, Program, PythonVersion};

mod builder;
mod call;
mod class_base;
mod context;
mod diagnostic;
mod display;
mod infer;
mod mro;
mod narrow;
mod signatures;
mod slots;
mod string_annotation;
mod subclass_of;
mod unpacker;

#[cfg(test)]
mod property_tests;

#[salsa::tracked(return_ref)]
pub fn check_types(db: &dyn Db, file: File) -> TypeCheckDiagnostics {
    let _span = tracing::trace_span!("check_types", file=?file.path(db)).entered();

    tracing::debug!("Checking file '{path}'", path = file.path(db));

    let index = semantic_index(db, file);
    let mut diagnostics = TypeCheckDiagnostics::default();

    for scope_id in index.scope_ids() {
        let result = infer_scope_types(db, scope_id);
        diagnostics.extend(result.diagnostics());
    }

    check_suppressions(db, file, &mut diagnostics);

    diagnostics
}

/// Infer the public type of a symbol (its type as seen from outside its scope).
fn symbol<'db>(db: &'db dyn Db, scope: ScopeId<'db>, name: &str) -> Symbol<'db> {
    #[salsa::tracked]
    fn symbol_by_id<'db>(
        db: &'db dyn Db,
        scope: ScopeId<'db>,
        symbol: ScopedSymbolId,
    ) -> Symbol<'db> {
        let use_def = use_def_map(db, scope);

        // If the symbol is declared, the public type is based on declarations; otherwise, it's based
        // on inference from bindings.

        let declarations = use_def.public_declarations(symbol);
        let declared = declarations_ty(db, declarations);

        match declared {
            // Symbol is declared, trust the declared type
            Ok(symbol @ Symbol::Type(_, Boundness::Bound)) => symbol,
            // Symbol is possibly declared
            Ok(Symbol::Type(declared_ty, Boundness::PossiblyUnbound)) => {
                let bindings = use_def.public_bindings(symbol);
                let inferred = bindings_ty(db, bindings);

                match inferred {
                    // Symbol is possibly undeclared and definitely unbound
                    Symbol::Unbound => {
                        // TODO: We probably don't want to report `Bound` here. This requires a bit of
                        // design work though as we might want a different behavior for stubs and for
                        // normal modules.
                        Symbol::Type(declared_ty, Boundness::Bound)
                    }
                    // Symbol is possibly undeclared and (possibly) bound
                    Symbol::Type(inferred_ty, boundness) => Symbol::Type(
                        UnionType::from_elements(db, [inferred_ty, declared_ty].iter().copied()),
                        boundness,
                    ),
                }
            }
            // Symbol is undeclared, return the inferred type
            Ok(Symbol::Unbound) => {
                let bindings = use_def.public_bindings(symbol);
                bindings_ty(db, bindings)
            }
            // Symbol is possibly undeclared
            Err((declared_ty, _)) => {
                // Intentionally ignore conflicting declared types; that's not our problem,
                // it's the problem of the module we are importing from.
                declared_ty.into()
            }
        }

        // TODO (ticket: https://github.com/astral-sh/ruff/issues/14297) Our handling of boundness
        // currently only depends on bindings, and ignores declarations. This is inconsistent, since
        // we only look at bindings if the symbol may be undeclared. Consider the following example:
        // ```py
        // x: int
        //
        // if flag:
        //     y: int
        // else
        //     y = 3
        // ```
        // If we import from this module, we will currently report `x` as a definitely-bound symbol
        // (even though it has no bindings at all!) but report `y` as possibly-unbound (even though
        // every path has either a binding or a declaration for it.)
    }

    let _span = tracing::trace_span!("symbol", ?name).entered();

    // We don't need to check for `typing_extensions` here, because `typing_extensions.TYPE_CHECKING`
    // is just a re-export of `typing.TYPE_CHECKING`.
    if name == "TYPE_CHECKING"
        && file_to_module(db, scope.file(db))
            .is_some_and(|module| module.is_known(KnownModule::Typing))
    {
        return Symbol::Type(Type::BooleanLiteral(true), Boundness::Bound);
    }
    if name == "platform"
        && file_to_module(db, scope.file(db))
            .is_some_and(|module| module.is_known(KnownModule::Sys))
    {
        match Program::get(db).python_platform(db) {
            crate::PythonPlatform::Identifier(platform) => {
                return Symbol::Type(
                    Type::StringLiteral(StringLiteralType::new(db, platform.as_str())),
                    Boundness::Bound,
                );
            }
            crate::PythonPlatform::All => {
                // Fall through to the looked up type
            }
        }
    }

    let table = symbol_table(db, scope);
    table
        .symbol_id_by_name(name)
        .map(|symbol| symbol_by_id(db, scope, symbol))
        .unwrap_or(Symbol::Unbound)
}

/// Return a list of the symbols that typeshed declares in the body scope of
/// the stub for the class `types.ModuleType`.
///
/// Conceptually this could be a `Set` rather than a list,
/// but the number of symbols declared in this scope is likely to be very small,
/// so the cost of hashing the names is likely to be more expensive than it's worth.
#[salsa::tracked(return_ref)]
fn module_type_symbols<'db>(db: &'db dyn Db) -> smallvec::SmallVec<[ast::name::Name; 8]> {
    let Some(module_type) = KnownClass::ModuleType
        .to_class_literal(db)
        .into_class_literal()
    else {
        // The most likely way we get here is if a user specified a `--custom-typeshed-dir`
        // without a `types.pyi` stub in the `stdlib/` directory
        return smallvec::SmallVec::default();
    };

    let module_type_scope = module_type.class.body_scope(db);
    let module_type_symbol_table = symbol_table(db, module_type_scope);

    // `__dict__` and `__init__` are very special members that can be accessed as attributes
    // on the module when imported, but cannot be accessed as globals *inside* the module.
    //
    // `__getattr__` is even more special: it doesn't exist at runtime, but typeshed includes it
    // to reduce false positives associated with functions that dynamically import modules
    // and return `Instance(types.ModuleType)`. We should ignore it for any known module-literal type.
    module_type_symbol_table
        .symbols()
        .filter(|symbol| symbol.is_declared())
        .map(symbol::Symbol::name)
        .filter(|symbol_name| !matches!(&***symbol_name, "__dict__" | "__getattr__" | "__init__"))
        .cloned()
        .collect()
}

/// Looks up a module-global symbol by name in a file.
pub(crate) fn global_symbol<'db>(db: &'db dyn Db, file: File, name: &str) -> Symbol<'db> {
    let explicit_symbol = symbol(db, global_scope(db, file), name);

    if !explicit_symbol.possibly_unbound() {
        return explicit_symbol;
    }

    // Not defined explicitly in the global scope?
    // All modules are instances of `types.ModuleType`;
    // look it up there (with a few very special exceptions)
    if module_type_symbols(db)
        .iter()
        .any(|module_type_member| &**module_type_member == name)
    {
        // TODO: this should use `.to_instance(db)`. but we don't understand attribute access
        // on instance types yet.
        let module_type_member = KnownClass::ModuleType.to_class_literal(db).member(db, name);
        return explicit_symbol.or_fall_back_to(db, &module_type_member);
    }

    explicit_symbol
}

/// Infer the type of a binding.
pub(crate) fn binding_ty<'db>(db: &'db dyn Db, definition: Definition<'db>) -> Type<'db> {
    let inference = infer_definition_types(db, definition);
    inference.binding_ty(definition)
}

/// Infer the type of a declaration.
fn declaration_ty<'db>(db: &'db dyn Db, definition: Definition<'db>) -> Type<'db> {
    let inference = infer_definition_types(db, definition);
    inference.declaration_ty(definition)
}

/// Infer the type of a (possibly deferred) sub-expression of a [`Definition`].
///
/// Supports expressions that are evaluated within a type-params sub-scope.
///
/// ## Panics
/// If the given expression is not a sub-expression of the given [`Definition`].
fn definition_expression_ty<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
    expression: &ast::Expr,
) -> Type<'db> {
    let file = definition.file(db);
    let index = semantic_index(db, file);
    let file_scope = index.expression_scope_id(expression);
    let scope = file_scope.to_scope_id(db, file);
    let expr_id = expression.scoped_expression_id(db, scope);
    if scope == definition.scope(db) {
        // expression is in the definition scope
        let inference = infer_definition_types(db, definition);
        if let Some(ty) = inference.try_expression_ty(expr_id) {
            ty
        } else {
            infer_deferred_types(db, definition).expression_ty(expr_id)
        }
    } else {
        // expression is in a type-params sub-scope
        infer_scope_types(db, scope).expression_ty(expr_id)
    }
}

/// Infer the combined type of an iterator of bindings.
///
/// Will return a union if there is more than one binding.
fn bindings_ty<'db>(
    db: &'db dyn Db,
    bindings_with_constraints: BindingWithConstraintsIterator<'_, 'db>,
) -> Symbol<'db> {
    let visibility_constraints = bindings_with_constraints.visibility_constraints;
    let mut bindings_with_constraints = bindings_with_constraints.peekable();

    let unbound_visibility = if let Some(BindingWithConstraints {
        binding: None,
        constraints: _,
        visibility_constraint,
    }) = bindings_with_constraints.peek()
    {
        visibility_constraints.evaluate(db, *visibility_constraint)
    } else {
        Truthiness::AlwaysFalse
    };

    let mut types = bindings_with_constraints.filter_map(
        |BindingWithConstraints {
             binding,
             constraints,
             visibility_constraint,
         }| {
            let binding = binding?;
            let static_visibility = visibility_constraints.evaluate(db, visibility_constraint);

            if static_visibility.is_always_false() {
                return None;
            }

            let mut constraint_tys = constraints
                .filter_map(|constraint| narrowing_constraint(db, constraint, binding))
                .peekable();

            let binding_ty = binding_ty(db, binding);
            if constraint_tys.peek().is_some() {
                let intersection_ty = constraint_tys
                    .fold(
                        IntersectionBuilder::new(db).add_positive(binding_ty),
                        IntersectionBuilder::add_positive,
                    )
                    .build();
                Some(intersection_ty)
            } else {
                Some(binding_ty)
            }
        },
    );

    if let Some(first) = types.next() {
        let boundness = match unbound_visibility {
            Truthiness::AlwaysTrue => {
                unreachable!("If we have at least one binding, the scope-start should not be definitely visible")
            }
            Truthiness::AlwaysFalse => Boundness::Bound,
            Truthiness::Ambiguous => Boundness::PossiblyUnbound,
        };

        if let Some(second) = types.next() {
            Symbol::Type(
                UnionType::from_elements(db, [first, second].into_iter().chain(types)),
                boundness,
            )
        } else {
            Symbol::Type(first, boundness)
        }
    } else {
        Symbol::Unbound
    }
}

/// The result of looking up a declared type from declarations; see [`declarations_ty`].
type DeclaredTypeResult<'db> = Result<Symbol<'db>, (Type<'db>, Box<[Type<'db>]>)>;

/// Build a declared type from a [`DeclarationsIterator`].
///
/// If there is only one declaration, or all declarations declare the same type, returns
/// `Ok(declared_type)`. If there are conflicting declarations, returns
/// `Err((union_of_declared_types, conflicting_declared_types))`.
///
/// If undeclared is a possibility, `undeclared_ty` type will be part of the return type but it
/// will not be considered to be conflicting with any other types.
///
/// # Panics
/// Will panic if there are no declarations and no `undeclared_ty` is provided. This is a logic
/// error, as any symbol with zero live declarations clearly must be undeclared, and the caller
/// should provide an `undeclared_ty`.
fn declarations_ty<'db>(
    db: &'db dyn Db,
    declarations: DeclarationsIterator<'_, 'db>,
) -> DeclaredTypeResult<'db> {
    let visibility_constraints = declarations.visibility_constraints;
    let mut declarations = declarations.peekable();

    let undeclared_visibility = if let Some(DeclarationWithConstraint {
        declaration: None,
        visibility_constraint,
    }) = declarations.peek()
    {
        visibility_constraints.evaluate(db, *visibility_constraint)
    } else {
        Truthiness::AlwaysFalse
    };

    let mut types = declarations.filter_map(
        |DeclarationWithConstraint {
             declaration,
             visibility_constraint,
         }| {
            let declaration = declaration?;
            let static_visibility = visibility_constraints.evaluate(db, visibility_constraint);

            if static_visibility.is_always_false() {
                None
            } else {
                Some(declaration_ty(db, declaration))
            }
        },
    );

    if let Some(first) = types.next() {
        let mut conflicting: Vec<Type<'db>> = vec![];
        let declared_ty = if let Some(second) = types.next() {
            let mut builder = UnionBuilder::new(db).add(first);
            for other in std::iter::once(second).chain(types) {
                if !first.is_equivalent_to(db, other) {
                    conflicting.push(other);
                }
                builder = builder.add(other);
            }
            builder.build()
        } else {
            first
        };
        if conflicting.is_empty() {
            let boundness = match undeclared_visibility {
                Truthiness::AlwaysTrue => {
                    unreachable!("If we have at least one declaration, the scope-start should not be definitely visible")
                }
                Truthiness::AlwaysFalse => Boundness::Bound,
                Truthiness::Ambiguous => Boundness::PossiblyUnbound,
            };

            Ok(Symbol::Type(declared_ty, boundness))
        } else {
            Err((
                declared_ty,
                std::iter::once(first).chain(conflicting).collect(),
            ))
        }
    } else {
        Ok(Symbol::Unbound)
    }
}

/// Meta data for `Type::Todo`, which represents a known limitation in red-knot.
#[cfg(debug_assertions)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum TodoType {
    FileAndLine(&'static str, u32),
    Message(&'static str),
}

#[cfg(debug_assertions)]
impl std::fmt::Display for TodoType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TodoType::FileAndLine(file, line) => write!(f, "[{file}:{line}]"),
            TodoType::Message(msg) => write!(f, "({msg})"),
        }
    }
}

#[cfg(not(debug_assertions))]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TodoType;

#[cfg(not(debug_assertions))]
impl std::fmt::Display for TodoType {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Ok(())
    }
}

/// Create a `Type::Todo` variant to represent a known limitation in the type system.
///
/// It can be used with a custom message (preferred): `todo_type!("PEP 604 not supported")`,
/// or simply using `todo_type!()`, which will include information about the file and line.
#[cfg(debug_assertions)]
macro_rules! todo_type {
    () => {
        $crate::types::Type::Dynamic($crate::types::DynamicType::Todo(
            $crate::types::TodoType::FileAndLine(file!(), line!()),
        ))
    };
    ($message:literal) => {
        $crate::types::Type::Dynamic($crate::types::DynamicType::Todo(
            $crate::types::TodoType::Message($message),
        ))
    };
}

#[cfg(not(debug_assertions))]
macro_rules! todo_type {
    () => {
        $crate::types::Type::Dynamic($crate::types::DynamicType::Todo(crate::types::TodoType))
    };
    ($message:literal) => {
        $crate::types::Type::Dynamic($crate::types::DynamicType::Todo(crate::types::TodoType))
    };
}

pub(crate) use todo_type;

/// Representation of a type: a set of possible values at runtime.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, salsa::Update)]
pub enum Type<'db> {
    /// The dynamic type: a statically unknown set of values
    Dynamic(DynamicType),
    /// The empty set of values
    Never,
    /// A specific function object
    FunctionLiteral(FunctionType<'db>),
    /// A specific module object
    ModuleLiteral(ModuleLiteralType<'db>),
    /// A specific class object
    ClassLiteral(ClassLiteralType<'db>),
    // The set of all class objects that are subclasses of the given class (C), spelled `type[C]`.
    SubclassOf(SubclassOfType<'db>),
    /// The set of Python objects with the given class in their __class__'s method resolution order
    Instance(InstanceType<'db>),
    /// A single Python object that requires special treatment in the type system
    KnownInstance(KnownInstanceType<'db>),
    /// The set of objects in any of the types in the union
    Union(UnionType<'db>),
    /// The set of objects in all of the types in the intersection
    Intersection(IntersectionType<'db>),
    /// Represents objects whose `__bool__` method is deterministic:
    /// - `AlwaysTruthy`: `__bool__` always returns `True`
    /// - `AlwaysFalsy`: `__bool__` always returns `False`
    AlwaysTruthy,
    AlwaysFalsy,
    /// An integer literal
    IntLiteral(i64),
    /// A boolean literal, either `True` or `False`.
    BooleanLiteral(bool),
    /// A string literal whose value is known
    StringLiteral(StringLiteralType<'db>),
    /// A string known to originate only from literal values, but whose value is not known (unlike
    /// `StringLiteral` above).
    LiteralString,
    /// A bytes literal
    BytesLiteral(BytesLiteralType<'db>),
    /// A slice literal, e.g. `1:5`, `10:0:-1` or `:`
    SliceLiteral(SliceLiteralType<'db>),
    /// A heterogeneous tuple type, with elements of the given types in source order.
    // TODO: Support variable length homogeneous tuple type like `tuple[int, ...]`.
    Tuple(TupleType<'db>),
    // TODO protocols, callable types, overloads, generics, type vars
}

impl<'db> Type<'db> {
    pub const fn any() -> Self {
        Self::Dynamic(DynamicType::Any)
    }

    pub const fn unknown() -> Self {
        Self::Dynamic(DynamicType::Unknown)
    }

    pub const fn is_unknown(&self) -> bool {
        matches!(self, Type::Dynamic(DynamicType::Unknown))
    }

    pub const fn is_never(&self) -> bool {
        matches!(self, Type::Never)
    }

    pub const fn is_todo(&self) -> bool {
        matches!(self, Type::Dynamic(DynamicType::Todo(_)))
    }

    pub const fn class_literal(class: Class<'db>) -> Self {
        Self::ClassLiteral(ClassLiteralType { class })
    }

    pub const fn into_class_literal(self) -> Option<ClassLiteralType<'db>> {
        match self {
            Type::ClassLiteral(class_type) => Some(class_type),
            _ => None,
        }
    }

    #[track_caller]
    pub fn expect_class_literal(self) -> ClassLiteralType<'db> {
        self.into_class_literal()
            .expect("Expected a Type::ClassLiteral variant")
    }

    pub const fn is_class_literal(&self) -> bool {
        matches!(self, Type::ClassLiteral(..))
    }

    pub fn module_literal(db: &'db dyn Db, importing_file: File, submodule: Module) -> Self {
        Self::ModuleLiteral(ModuleLiteralType::new(db, importing_file, submodule))
    }

    pub const fn into_module_literal(self) -> Option<ModuleLiteralType<'db>> {
        match self {
            Type::ModuleLiteral(module) => Some(module),
            _ => None,
        }
    }

    #[track_caller]
    pub fn expect_module_literal(self) -> ModuleLiteralType<'db> {
        self.into_module_literal()
            .expect("Expected a Type::ModuleLiteral variant")
    }

    pub const fn into_union(self) -> Option<UnionType<'db>> {
        match self {
            Type::Union(union_type) => Some(union_type),
            _ => None,
        }
    }

    #[track_caller]
    pub fn expect_union(self) -> UnionType<'db> {
        self.into_union().expect("Expected a Type::Union variant")
    }

    pub const fn is_union(&self) -> bool {
        matches!(self, Type::Union(..))
    }

    pub const fn into_intersection(self) -> Option<IntersectionType<'db>> {
        match self {
            Type::Intersection(intersection_type) => Some(intersection_type),
            _ => None,
        }
    }

    #[track_caller]
    pub fn expect_intersection(self) -> IntersectionType<'db> {
        self.into_intersection()
            .expect("Expected a Type::Intersection variant")
    }

    pub const fn into_function_literal(self) -> Option<FunctionType<'db>> {
        match self {
            Type::FunctionLiteral(function_type) => Some(function_type),
            _ => None,
        }
    }

    #[track_caller]
    pub fn expect_function_literal(self) -> FunctionType<'db> {
        self.into_function_literal()
            .expect("Expected a Type::FunctionLiteral variant")
    }

    pub const fn is_function_literal(&self) -> bool {
        matches!(self, Type::FunctionLiteral(..))
    }

    pub const fn into_int_literal(self) -> Option<i64> {
        match self {
            Type::IntLiteral(value) => Some(value),
            _ => None,
        }
    }

    pub fn into_string_literal(self) -> Option<StringLiteralType<'db>> {
        match self {
            Type::StringLiteral(string_literal) => Some(string_literal),
            _ => None,
        }
    }

    #[track_caller]
    pub fn expect_int_literal(self) -> i64 {
        self.into_int_literal()
            .expect("Expected a Type::IntLiteral variant")
    }

    pub const fn into_instance(self) -> Option<InstanceType<'db>> {
        match self {
            Type::Instance(instance_type) => Some(instance_type),
            _ => None,
        }
    }

    pub const fn into_known_instance(self) -> Option<KnownInstanceType<'db>> {
        match self {
            Type::KnownInstance(known_instance) => Some(known_instance),
            _ => None,
        }
    }

    #[track_caller]
    pub fn expect_known_instance(self) -> KnownInstanceType<'db> {
        self.into_known_instance()
            .expect("Expected a Type::KnownInstance variant")
    }

    pub const fn into_tuple(self) -> Option<TupleType<'db>> {
        match self {
            Type::Tuple(tuple_type) => Some(tuple_type),
            _ => None,
        }
    }

    pub const fn is_boolean_literal(&self) -> bool {
        matches!(self, Type::BooleanLiteral(..))
    }

    pub const fn is_literal_string(&self) -> bool {
        matches!(self, Type::LiteralString)
    }

    pub const fn instance(class: Class<'db>) -> Self {
        Self::Instance(InstanceType { class })
    }

    pub fn string_literal(db: &'db dyn Db, string: &str) -> Self {
        Self::StringLiteral(StringLiteralType::new(db, string))
    }

    pub fn bytes_literal(db: &'db dyn Db, bytes: &[u8]) -> Self {
        Self::BytesLiteral(BytesLiteralType::new(db, bytes))
    }

    #[must_use]
    pub fn negate(&self, db: &'db dyn Db) -> Type<'db> {
        IntersectionBuilder::new(db).add_negative(*self).build()
    }

    #[must_use]
    pub fn negate_if(&self, db: &'db dyn Db, yes: bool) -> Type<'db> {
        if yes {
            self.negate(db)
        } else {
            *self
        }
    }

    /// Return true if this type is a [subtype of] type `target`.
    ///
    /// This method returns `false` if either `self` or `other` is not fully static.
    ///
    /// [subtype of]: https://typing.readthedocs.io/en/latest/spec/concepts.html#subtype-supertype-and-type-equivalence
    pub(crate) fn is_subtype_of(self, db: &'db dyn Db, target: Type<'db>) -> bool {
        // Two equivalent types are always subtypes of each other.
        //
        // "Equivalent to" here means that the two types are both fully static
        // and describe exactly the same set of possible runtime objects.
        // For example, `int` is a subtype of `int` because `int` and `int` are equivalent to each other.
        // Equally, `type[object]` is a subtype of `type`,
        // because the former type expresses "all subclasses of `object`"
        // while the latter expresses "all instances of `type`",
        // and these are exactly the same set of objects at runtime.
        if self.is_equivalent_to(db, target) {
            return true;
        }

        // Non-fully-static types do not participate in subtyping.
        //
        // Type `A` can only be a subtype of type `B` if the set of possible runtime objects
        // that `A` represents is a subset of the set of possible runtime objects that `B` represents.
        // But the set of objects described by a non-fully-static type is (either partially or wholly) unknown,
        // so the question is simply unanswerable for non-fully-static types.
        if !self.is_fully_static(db) || !target.is_fully_static(db) {
            return false;
        }

        match (self, target) {
            // We should have handled these immediately above.
            (Type::Dynamic(_), _) | (_, Type::Dynamic(_)) => {
                unreachable!("Non-fully-static types do not participate in subtyping!")
            }

            // `Never` is the bottom type, the empty set.
            // It is a subtype of all other fully static types.
            // No other fully static type is a subtype of `Never`.
            (Type::Never, _) => true,
            (_, Type::Never) => false,

            (Type::Union(union), _) => union
                .elements(db)
                .iter()
                .all(|&elem_ty| elem_ty.is_subtype_of(db, target)),

            (_, Type::Union(union)) => union
                .elements(db)
                .iter()
                .any(|&elem_ty| self.is_subtype_of(db, elem_ty)),

            // `object` is the only type that can be known to be a supertype of any intersection,
            // even an intersection with no positive elements
            (Type::Intersection(_), Type::Instance(InstanceType { class }))
                if class.is_known(db, KnownClass::Object) =>
            {
                true
            }

            (Type::Intersection(self_intersection), Type::Intersection(target_intersection)) => {
                // Check that all target positive values are covered in self positive values
                target_intersection
                    .positive(db)
                    .iter()
                    .all(|&target_pos_elem| {
                        self_intersection
                            .positive(db)
                            .iter()
                            .any(|&self_pos_elem| self_pos_elem.is_subtype_of(db, target_pos_elem))
                    })
                    // Check that all target negative values are excluded in self, either by being
                    // subtypes of a self negative value or being disjoint from a self positive value.
                    && target_intersection
                        .negative(db)
                        .iter()
                        .all(|&target_neg_elem| {
                            // Is target negative value is subtype of a self negative value
                            self_intersection.negative(db).iter().any(|&self_neg_elem| {
                                target_neg_elem.is_subtype_of(db, self_neg_elem)
                            // Is target negative value is disjoint from a self positive value?
                            }) || self_intersection.positive(db).iter().any(|&self_pos_elem| {
                                self_pos_elem.is_disjoint_from(db, target_neg_elem)
                            })
                        })
            }

            (Type::Intersection(intersection), _) => intersection
                .positive(db)
                .iter()
                .any(|&elem_ty| elem_ty.is_subtype_of(db, target)),

            (_, Type::Intersection(intersection)) => {
                intersection
                    .positive(db)
                    .iter()
                    .all(|&pos_ty| self.is_subtype_of(db, pos_ty))
                    && intersection
                        .negative(db)
                        .iter()
                        .all(|&neg_ty| self.is_disjoint_from(db, neg_ty))
            }

            // Note that the definition of `Type::AlwaysFalsy` depends on the return value of `__bool__`.
            // If `__bool__` always returns True or False, it can be treated as a subtype of `AlwaysTruthy` or `AlwaysFalsy`, respectively.
            (left, Type::AlwaysFalsy) => left.bool(db).is_always_false(),
            (left, Type::AlwaysTruthy) => left.bool(db).is_always_true(),
            // Currently, the only supertype of `AlwaysFalsy` and `AlwaysTruthy` is the universal set (object instance).
            (Type::AlwaysFalsy | Type::AlwaysTruthy, _) => {
                target.is_equivalent_to(db, KnownClass::Object.to_instance(db))
            }

            // All `StringLiteral` types are a subtype of `LiteralString`.
            (Type::StringLiteral(_), Type::LiteralString) => true,

            // Except for the special `LiteralString` case above,
            // most `Literal` types delegate to their instance fallbacks
            // unless `self` is exactly equivalent to `target` (handled above)
            (Type::StringLiteral(_) | Type::LiteralString, _) => {
                KnownClass::Str.to_instance(db).is_subtype_of(db, target)
            }
            (Type::BooleanLiteral(_), _) => {
                KnownClass::Bool.to_instance(db).is_subtype_of(db, target)
            }
            (Type::IntLiteral(_), _) => KnownClass::Int.to_instance(db).is_subtype_of(db, target),
            (Type::BytesLiteral(_), _) => {
                KnownClass::Bytes.to_instance(db).is_subtype_of(db, target)
            }
            (Type::ModuleLiteral(_), _) => KnownClass::ModuleType
                .to_instance(db)
                .is_subtype_of(db, target),
            (Type::SliceLiteral(_), _) => {
                KnownClass::Slice.to_instance(db).is_subtype_of(db, target)
            }

            // A `FunctionLiteral` type is a single-valued type like the other literals handled above,
            // so it also, for now, just delegates to its instance fallback.
            // This will change in a way similar to the `LiteralString`/`StringLiteral()` case above
            // when we add support for `typing.Callable`.
            (Type::FunctionLiteral(_), _) => KnownClass::FunctionType
                .to_instance(db)
                .is_subtype_of(db, target),

            // A fully static heterogenous tuple type `A` is a subtype of a fully static heterogeneous tuple type `B`
            // iff the two tuple types have the same number of elements and each element-type in `A` is a subtype
            // of the element-type at the same index in `B`. (Now say that 5 times fast.)
            //
            // For example: `tuple[bool, bool]` is a subtype of `tuple[int, int]`,
            // but `tuple[bool, bool, bool]` is not a subtype of `tuple[int, int]`
            (Type::Tuple(self_tuple), Type::Tuple(target_tuple)) => {
                let self_elements = self_tuple.elements(db);
                let target_elements = target_tuple.elements(db);
                self_elements.len() == target_elements.len()
                    && self_elements.iter().zip(target_elements).all(
                        |(self_element, target_element)| {
                            self_element.is_subtype_of(db, *target_element)
                        },
                    )
            }

            // Other than the special tuple-to-tuple case handled, above,
            // tuple subtyping delegates to `Instance(tuple)` in the same way as the literal types.
            //
            // All heterogenous tuple types are subtypes of `Instance(<tuple>)`:
            // `Instance(<some class T>)` expresses "the set of all possible instances of the class `T`";
            // consequently, `Instance(<tuple>)` expresses "the set of all possible instances of the class `tuple`".
            // This type can be spelled in type annotations as `tuple[object, ...]` (since `tuple` is covariant).
            //
            // Note that this is not the same type as the type spelled in type annotations as `tuple`;
            // as that type is equivalent to `type[Any, ...]` (and therefore not a fully static type).
            (Type::Tuple(_), _) => KnownClass::Tuple.to_instance(db).is_subtype_of(db, target),

            // `Literal[<class 'C'>]` is a subtype of `type[B]` if `C` is a subclass of `B`,
            // since `type[B]` describes all possible runtime subclasses of the class object `B`.
            (
                Type::ClassLiteral(ClassLiteralType { class }),
                Type::SubclassOf(target_subclass_ty),
            ) => target_subclass_ty
                .subclass_of()
                .into_class()
                .is_some_and(|target_class| class.is_subclass_of(db, target_class)),

            // This branch asks: given two types `type[T]` and `type[S]`, is `type[T]` a subtype of `type[S]`?
            (Type::SubclassOf(self_subclass_ty), Type::SubclassOf(target_subclass_ty)) => {
                self_subclass_ty.is_subtype_of(db, target_subclass_ty)
            }

            // `Literal[str]` is a subtype of `type` because the `str` class object is an instance of its metaclass `type`.
            // `Literal[abc.ABC]` is a subtype of `abc.ABCMeta` because the `abc.ABC` class object
            // is an instance of its metaclass `abc.ABCMeta`.
            (
                Type::ClassLiteral(ClassLiteralType { class: self_class }),
                Type::Instance(InstanceType {
                    class: target_class,
                }),
            ) => self_class.is_instance_of(db, target_class),

            // `type[str]` (== `SubclassOf("str")` in red-knot) describes all possible runtime subclasses
            // of the class object `str`. It is a subtype of `type` (== `Instance("type")`) because `str`
            // is an instance of `type`, and so all possible subclasses of `str` will also be instances of `type`.
            //
            // Similarly `type[enum.Enum]`  is a subtype of `enum.EnumMeta` because `enum.Enum`
            // is an instance of `enum.EnumMeta`.
            (
                Type::SubclassOf(subclass_of_ty),
                Type::Instance(InstanceType {
                    class: target_class,
                }),
            ) => subclass_of_ty
                .subclass_of()
                .into_class()
                .is_some_and(|subclass_class| subclass_class.is_instance_of(db, target_class)),

            // Other than the cases enumerated above, `type[]` and class-literal types just delegate to `Instance("type")`
            (Type::SubclassOf(_) | Type::ClassLiteral(_), _) => {
                KnownClass::Type.to_instance(db).is_subtype_of(db, target)
            }

            // For example: `Type::KnownInstance(KnownInstanceType::Type)` is a subtype of `Type::Instance(_SpecialForm)`,
            // because `Type::KnownInstance(KnownInstanceType::Type)` is a set with exactly one runtime value in it
            // (the symbol `typing.Type`), and that symbol is known to be an instance of `typing._SpecialForm` at runtime.
            (Type::KnownInstance(left), right) => {
                left.instance_fallback(db).is_subtype_of(db, right)
            }

            // `bool` is a subtype of `int`, because `bool` subclasses `int`,
            // which means that all instances of `bool` are also instances of `int`
            (Type::Instance(self_instance), Type::Instance(target_instance)) => {
                self_instance.is_subtype_of(db, target_instance)
            }

            // Other than the special cases enumerated above,
            // `Instance` types are never subtypes of any other variants
            (Type::Instance(_), _) => false,
        }
    }

    /// Return true if this type is [assignable to] type `target`.
    ///
    /// [assignable to]: https://typing.readthedocs.io/en/latest/spec/concepts.html#the-assignable-to-or-consistent-subtyping-relation
    pub(crate) fn is_assignable_to(self, db: &'db dyn Db, target: Type<'db>) -> bool {
        if self.is_equivalent_to(db, target) {
            return true;
        }
        match (self, target) {
            // Never can be assigned to any type.
            (Type::Never, _) => true,

            // The dynamic type is assignable-to and assignable-from any type.
            (Type::Dynamic(_), _) => true,
            (_, Type::Dynamic(_)) => true,

            // All types are assignable to `object`.
            // TODO this special case might be removable once the below cases are comprehensive
            (_, Type::Instance(InstanceType { class }))
                if class.is_known(db, KnownClass::Object) =>
            {
                true
            }

            // A union is assignable to a type T iff every element of the union is assignable to T.
            (Type::Union(union), ty) => union
                .elements(db)
                .iter()
                .all(|&elem_ty| elem_ty.is_assignable_to(db, ty)),

            // A type T is assignable to a union iff T is assignable to any element of the union.
            (ty, Type::Union(union)) => union
                .elements(db)
                .iter()
                .any(|&elem_ty| ty.is_assignable_to(db, elem_ty)),

            // A tuple type S is assignable to a tuple type T if their lengths are the same, and
            // each element of S is assignable to the corresponding element of T.
            (Type::Tuple(self_tuple), Type::Tuple(target_tuple)) => {
                let self_elements = self_tuple.elements(db);
                let target_elements = target_tuple.elements(db);
                self_elements.len() == target_elements.len()
                    && self_elements.iter().zip(target_elements).all(
                        |(self_element, target_element)| {
                            self_element.is_assignable_to(db, *target_element)
                        },
                    )
            }

            // `type[Any]` is assignable to any `type[...]` type, because `type[Any]` can
            // materialize to any `type[...]` type.
            (Type::SubclassOf(subclass_of_ty), Type::SubclassOf(_))
                if subclass_of_ty.is_dynamic() =>
            {
                true
            }

            // All `type[...]` types are assignable to `type[Any]`, because `type[Any]` can
            // materialize to any `type[...]` type.
            //
            // Every class literal type is also assignable to `type[Any]`, because the class
            // literal type for a class `C` is a subtype of `type[C]`, and `type[C]` is assignable
            // to `type[Any]`.
            (Type::ClassLiteral(_) | Type::SubclassOf(_), Type::SubclassOf(target_subclass_of))
                if target_subclass_of.is_dynamic() =>
            {
                true
            }

            // `type[Any]` is assignable to any type that `type[object]` is assignable to, because
            // `type[Any]` can materialize to `type[object]`.
            //
            // `type[Any]` is also assignable to any subtype of `type[object]`, because all
            // subtypes of `type[object]` are `type[...]` types (or `Never`), and `type[Any]` can
            // materialize to any `type[...]` type (or to `type[Never]`, which is equivalent to
            // `Never`.)
            (Type::SubclassOf(subclass_of_ty), Type::Instance(_))
                if subclass_of_ty.is_dynamic()
                    && (KnownClass::Type
                        .to_instance(db)
                        .is_assignable_to(db, target)
                        || target.is_subtype_of(db, KnownClass::Type.to_instance(db))) =>
            {
                true
            }

            // Any type that is assignable to `type[object]` is also assignable to `type[Any]`,
            // because `type[Any]` can materialize to `type[object]`.
            (Type::Instance(_), Type::SubclassOf(subclass_of_ty))
                if subclass_of_ty.is_dynamic()
                    && self.is_assignable_to(db, KnownClass::Type.to_instance(db)) =>
            {
                true
            }

            // TODO other types containing gradual forms (e.g. generics containing Any/Unknown)
            _ => self.is_subtype_of(db, target),
        }
    }

    /// Return true if this type is [equivalent to] type `other`.
    ///
    /// This method returns `false` if either `self` or `other` is not fully static.
    ///
    /// [equivalent to]: https://typing.readthedocs.io/en/latest/spec/glossary.html#term-equivalent
    pub(crate) fn is_equivalent_to(self, db: &'db dyn Db, other: Type<'db>) -> bool {
        if !(self.is_fully_static(db) && other.is_fully_static(db)) {
            return false;
        }

        // TODO equivalent but not identical structural types, differently-ordered unions and
        // intersections, other cases?

        // For all other cases, types are equivalent iff they have the same internal
        // representation.
        self == other
    }

    /// Returns true if both `self` and `other` are the same gradual form
    /// (limited to `Any`, `Unknown`, or `Todo`).
    pub(crate) fn is_same_gradual_form(self, other: Type<'db>) -> bool {
        matches!(
            (self, other),
            (
                Type::Dynamic(DynamicType::Any),
                Type::Dynamic(DynamicType::Any)
            ) | (
                Type::Dynamic(DynamicType::Unknown),
                Type::Dynamic(DynamicType::Unknown)
            ) | (
                Type::Dynamic(DynamicType::Todo(_)),
                Type::Dynamic(DynamicType::Todo(_))
            )
        )
    }

    /// Returns true if this type and `other` are gradual equivalent.
    ///
    /// > Two gradual types `A` and `B` are equivalent
    /// > (that is, the same gradual type, not merely consistent with one another)
    /// > if and only if all materializations of `A` are also materializations of `B`,
    /// > and all materializations of `B` are also materializations of `A`.
    /// >
    /// > &mdash; [Summary of type relations]
    ///
    /// This powers the `assert_type()` directive.
    ///
    /// [Summary of type relations]: https://typing.readthedocs.io/en/latest/spec/concepts.html#summary-of-type-relations
    pub(crate) fn is_gradual_equivalent_to(self, db: &'db dyn Db, other: Type<'db>) -> bool {
        let equivalent =
            |(first, second): (&Type<'db>, &Type<'db>)| first.is_gradual_equivalent_to(db, *second);

        match (self, other) {
            (_, _) if self == other => true,

            (Type::Dynamic(_), Type::Dynamic(_)) => true,

            (Type::Instance(instance), Type::SubclassOf(subclass))
            | (Type::SubclassOf(subclass), Type::Instance(instance)) => {
                let Some(base_class) = subclass.subclass_of().into_class() else {
                    return false;
                };

                instance.class.is_known(db, KnownClass::Type)
                    && base_class.is_known(db, KnownClass::Object)
            }

            (Type::SubclassOf(first), Type::SubclassOf(second)) => {
                match (first.subclass_of(), second.subclass_of()) {
                    (first, second) if first == second => true,
                    (ClassBase::Dynamic(_), ClassBase::Dynamic(_)) => true,
                    _ => false,
                }
            }

            (Type::Tuple(first), Type::Tuple(second)) => {
                let first_elements = first.elements(db);
                let second_elements = second.elements(db);

                first_elements.len() == second_elements.len()
                    && iter::zip(first_elements, second_elements).all(equivalent)
            }

            // TODO: Handle equivalent unions with items in different order
            (Type::Union(first), Type::Union(second)) => {
                let first_elements = first.elements(db);
                let second_elements = second.elements(db);

                if first_elements.len() != second_elements.len() {
                    return false;
                }

                iter::zip(first_elements, second_elements).all(equivalent)
            }

            // TODO: Handle equivalent intersections with items in different order
            (Type::Intersection(first), Type::Intersection(second)) => {
                let first_positive = first.positive(db);
                let first_negative = first.negative(db);

                let second_positive = second.positive(db);
                let second_negative = second.negative(db);

                if first_positive.len() != second_positive.len()
                    || first_negative.len() != second_negative.len()
                {
                    return false;
                }

                iter::zip(first_positive, second_positive).all(equivalent)
                    && iter::zip(first_negative, second_negative).all(equivalent)
            }

            _ => false,
        }
    }

    /// Return true if this type and `other` have no common elements.
    ///
    /// Note: This function aims to have no false positives, but might return
    /// wrong `false` answers in some cases.
    pub(crate) fn is_disjoint_from(self, db: &'db dyn Db, other: Type<'db>) -> bool {
        match (self, other) {
            (Type::Never, _) | (_, Type::Never) => true,

            (Type::Dynamic(_), _) | (_, Type::Dynamic(_)) => false,

            (Type::Union(union), other) | (other, Type::Union(union)) => union
                .elements(db)
                .iter()
                .all(|e| e.is_disjoint_from(db, other)),

            (Type::Intersection(intersection), other)
            | (other, Type::Intersection(intersection)) => {
                if intersection
                    .positive(db)
                    .iter()
                    .any(|p| p.is_disjoint_from(db, other))
                {
                    true
                } else {
                    // TODO we can do better here. For example:
                    // X & ~Literal[1] is disjoint from Literal[1]
                    false
                }
            }

            // any single-valued type is disjoint from another single-valued type
            // iff the two types are nonequal
            (
                left @ (Type::BooleanLiteral(..)
                | Type::IntLiteral(..)
                | Type::StringLiteral(..)
                | Type::BytesLiteral(..)
                | Type::SliceLiteral(..)
                | Type::FunctionLiteral(..)
                | Type::ModuleLiteral(..)
                | Type::ClassLiteral(..)
                | Type::KnownInstance(..)),
                right @ (Type::BooleanLiteral(..)
                | Type::IntLiteral(..)
                | Type::StringLiteral(..)
                | Type::BytesLiteral(..)
                | Type::SliceLiteral(..)
                | Type::FunctionLiteral(..)
                | Type::ModuleLiteral(..)
                | Type::ClassLiteral(..)
                | Type::KnownInstance(..)),
            ) => left != right,

            // One tuple type can be a subtype of another tuple type,
            // but we know for sure that any given tuple type is disjoint from all single-valued types
            (
                Type::Tuple(..),
                Type::ClassLiteral(..)
                | Type::ModuleLiteral(..)
                | Type::BooleanLiteral(..)
                | Type::BytesLiteral(..)
                | Type::FunctionLiteral(..)
                | Type::IntLiteral(..)
                | Type::SliceLiteral(..)
                | Type::StringLiteral(..)
                | Type::LiteralString,
            )
            | (
                Type::ClassLiteral(..)
                | Type::ModuleLiteral(..)
                | Type::BooleanLiteral(..)
                | Type::BytesLiteral(..)
                | Type::FunctionLiteral(..)
                | Type::IntLiteral(..)
                | Type::SliceLiteral(..)
                | Type::StringLiteral(..)
                | Type::LiteralString,
                Type::Tuple(..),
            ) => true,

            (
                Type::SubclassOf(subclass_of_ty),
                Type::ClassLiteral(ClassLiteralType { class: class_b }),
            )
            | (
                Type::ClassLiteral(ClassLiteralType { class: class_b }),
                Type::SubclassOf(subclass_of_ty),
            ) => match subclass_of_ty.subclass_of() {
                ClassBase::Dynamic(_) => false,
                ClassBase::Class(class_a) => !class_b.is_subclass_of(db, class_a),
            },

            (Type::SubclassOf(_), Type::SubclassOf(_)) => false,

            (Type::SubclassOf(_), Type::Instance(instance))
            | (Type::Instance(instance), Type::SubclassOf(_)) => {
                // TODO this should be `true` if the instance is of a final type which is not a
                // subclass of type. (With non-final types, we never know whether a subclass might
                // multiply-inherit `type` or a subclass of it, and thus not be disjoint with
                // `type[...]`.) Until we support finality, hardcode None, which is known to be
                // final.
                instance.class.is_known(db, KnownClass::NoneType)
            }

            (
                Type::SubclassOf(_),
                Type::BooleanLiteral(..)
                | Type::IntLiteral(..)
                | Type::StringLiteral(..)
                | Type::LiteralString
                | Type::BytesLiteral(..)
                | Type::SliceLiteral(..)
                | Type::FunctionLiteral(..)
                | Type::ModuleLiteral(..),
            )
            | (
                Type::BooleanLiteral(..)
                | Type::IntLiteral(..)
                | Type::StringLiteral(..)
                | Type::LiteralString
                | Type::BytesLiteral(..)
                | Type::SliceLiteral(..)
                | Type::FunctionLiteral(..)
                | Type::ModuleLiteral(..),
                Type::SubclassOf(_),
            ) => true,

            (Type::AlwaysTruthy, ty) | (ty, Type::AlwaysTruthy) => {
                // `Truthiness::Ambiguous` may include `AlwaysTrue` as a subset, so it's not guaranteed to be disjoint.
                // Thus, they are only disjoint if `ty.bool() == AlwaysFalse`.
                ty.bool(db).is_always_false()
            }
            (Type::AlwaysFalsy, ty) | (ty, Type::AlwaysFalsy) => {
                // Similarly, they are only disjoint if `ty.bool() == AlwaysTrue`.
                ty.bool(db).is_always_true()
            }

            (Type::SubclassOf(_), other) | (other, Type::SubclassOf(_)) => {
                // TODO we could do better here: if both variants are `SubclassOf` and they have different "solid bases",
                // multiple inheritance between the two is impossible, so they are disjoint.
                //
                // Note that `type[<@final class>]` is eagerly simplified to `Literal[<@final class>]` by [`SubclassOfType::from`].
                other.is_disjoint_from(db, KnownClass::Type.to_instance(db))
            }

            (Type::KnownInstance(known_instance), Type::Instance(InstanceType { class }))
            | (Type::Instance(InstanceType { class }), Type::KnownInstance(known_instance)) => {
                !known_instance.is_instance_of(db, class)
            }

            (known_instance_ty @ Type::KnownInstance(_), Type::Tuple(_))
            | (Type::Tuple(_), known_instance_ty @ Type::KnownInstance(_)) => {
                known_instance_ty.is_disjoint_from(db, KnownClass::Tuple.to_instance(db))
            }

            (
                Type::Instance(InstanceType { class: class_none }),
                Type::Instance(InstanceType { class: class_other }),
            )
            | (
                Type::Instance(InstanceType { class: class_other }),
                Type::Instance(InstanceType { class: class_none }),
            ) if class_none.is_known(db, KnownClass::NoneType) => {
                !class_none.is_subclass_of(db, class_other)
            }

            (Type::Instance(InstanceType { class: class_none }), _)
            | (_, Type::Instance(InstanceType { class: class_none }))
                if class_none.is_known(db, KnownClass::NoneType) =>
            {
                true
            }

            (Type::BooleanLiteral(..), Type::Instance(InstanceType { class }))
            | (Type::Instance(InstanceType { class }), Type::BooleanLiteral(..)) => {
                // A `Type::BooleanLiteral()` must be an instance of exactly `bool`
                // (it cannot be an instance of a `bool` subclass)
                !KnownClass::Bool.is_subclass_of(db, class)
            }

            (Type::BooleanLiteral(..), _) | (_, Type::BooleanLiteral(..)) => true,

            (Type::IntLiteral(..), Type::Instance(InstanceType { class }))
            | (Type::Instance(InstanceType { class }), Type::IntLiteral(..)) => {
                // A `Type::IntLiteral()` must be an instance of exactly `int`
                // (it cannot be an instance of an `int` subclass)
                !KnownClass::Int.is_subclass_of(db, class)
            }

            (Type::IntLiteral(..), _) | (_, Type::IntLiteral(..)) => true,

            (Type::StringLiteral(..), Type::LiteralString)
            | (Type::LiteralString, Type::StringLiteral(..)) => false,

            (
                Type::StringLiteral(..) | Type::LiteralString,
                Type::Instance(InstanceType { class }),
            )
            | (
                Type::Instance(InstanceType { class }),
                Type::StringLiteral(..) | Type::LiteralString,
            ) => {
                // A `Type::StringLiteral()` or a `Type::LiteralString` must be an instance of exactly `str`
                // (it cannot be an instance of a `str` subclass)
                !KnownClass::Str.is_subclass_of(db, class)
            }

            (Type::LiteralString, Type::LiteralString) => false,
            (Type::LiteralString, _) | (_, Type::LiteralString) => true,

            (Type::BytesLiteral(..), Type::Instance(InstanceType { class }))
            | (Type::Instance(InstanceType { class }), Type::BytesLiteral(..)) => {
                // A `Type::BytesLiteral()` must be an instance of exactly `bytes`
                // (it cannot be an instance of a `bytes` subclass)
                !KnownClass::Bytes.is_subclass_of(db, class)
            }

            (Type::SliceLiteral(..), Type::Instance(InstanceType { class }))
            | (Type::Instance(InstanceType { class }), Type::SliceLiteral(..)) => {
                // A `Type::SliceLiteral` must be an instance of exactly `slice`
                // (it cannot be an instance of a `slice` subclass)
                !KnownClass::Slice.is_subclass_of(db, class)
            }

            (
                Type::ClassLiteral(ClassLiteralType { class: class_a }),
                Type::Instance(InstanceType { class: class_b }),
            )
            | (
                Type::Instance(InstanceType { class: class_b }),
                Type::ClassLiteral(ClassLiteralType { class: class_a }),
            ) => !class_a.is_instance_of(db, class_b),

            (Type::FunctionLiteral(..), Type::Instance(InstanceType { class }))
            | (Type::Instance(InstanceType { class }), Type::FunctionLiteral(..)) => {
                // A `Type::FunctionLiteral()` must be an instance of exactly `types.FunctionType`
                // (it cannot be an instance of a `types.FunctionType` subclass)
                !KnownClass::FunctionType.is_subclass_of(db, class)
            }

            (Type::ModuleLiteral(..), other @ Type::Instance(..))
            | (other @ Type::Instance(..), Type::ModuleLiteral(..)) => {
                // Modules *can* actually be instances of `ModuleType` subclasses
                other.is_disjoint_from(db, KnownClass::ModuleType.to_instance(db))
            }

            (Type::Instance(..), Type::Instance(..)) => {
                // TODO: once we have support for `final`, there might be some cases where
                // we can determine that two types are disjoint. Once we do this, some cases
                // above (e.g. NoneType) can be removed. For non-final classes, we return
                // false (multiple inheritance).

                // TODO: is there anything specific to do for instances of KnownClass::Type?

                false
            }

            (Type::Tuple(tuple), Type::Tuple(other_tuple)) => {
                let self_elements = tuple.elements(db);
                let other_elements = other_tuple.elements(db);
                self_elements.len() != other_elements.len()
                    || self_elements
                        .iter()
                        .zip(other_elements)
                        .any(|(e1, e2)| e1.is_disjoint_from(db, *e2))
            }

            (Type::Tuple(..), Type::Instance(..)) | (Type::Instance(..), Type::Tuple(..)) => {
                // We cannot be sure if the tuple is disjoint from the instance because:
                //   - 'other' might be the homogeneous arbitrary-length tuple type
                //     tuple[T, ...] (which we don't have support for yet); if all of
                //     our element types are not disjoint with T, this is not disjoint
                //   - 'other' might be a user subtype of tuple, which, if generic
                //     over the same or compatible *Ts, would overlap with tuple.
                //
                // TODO: add checks for the above cases once we support them
                false
            }
        }
    }

    /// Returns true if the type does not contain any gradual forms (as a sub-part).
    pub(crate) fn is_fully_static(self, db: &'db dyn Db) -> bool {
        match self {
            Type::Dynamic(_) => false,
            Type::Never
            | Type::FunctionLiteral(..)
            | Type::ModuleLiteral(..)
            | Type::IntLiteral(_)
            | Type::BooleanLiteral(_)
            | Type::StringLiteral(_)
            | Type::LiteralString
            | Type::BytesLiteral(_)
            | Type::SliceLiteral(_)
            | Type::KnownInstance(_)
            | Type::AlwaysFalsy
            | Type::AlwaysTruthy => true,
            Type::SubclassOf(subclass_of_ty) => subclass_of_ty.is_fully_static(),
            Type::ClassLiteral(_) | Type::Instance(_) => {
                // TODO: Ideally, we would iterate over the MRO of the class, check if all
                // bases are fully static, and only return `true` if that is the case.
                //
                // This does not work yet, because we currently infer `Unknown` for some
                // generic base classes that we don't understand yet. For example, `str`
                // is defined as `class str(Sequence[str])` in typeshed and we currently
                // compute its MRO as `(str, Unknown, object)`. This would make us think
                // that `str` is a gradual type, which causes all sorts of downstream
                // issues because it does not participate in equivalence/subtyping etc.
                //
                // Another problem is that we run into problems if we eagerly query the
                // MRO of class literals here. I have not fully investigated this, but
                // iterating over the MRO alone, without even acting on it, causes us to
                // infer `Unknown` for many classes.

                true
            }
            Type::Union(union) => union
                .elements(db)
                .iter()
                .all(|elem| elem.is_fully_static(db)),
            Type::Intersection(intersection) => {
                intersection
                    .positive(db)
                    .iter()
                    .all(|elem| elem.is_fully_static(db))
                    && intersection
                        .negative(db)
                        .iter()
                        .all(|elem| elem.is_fully_static(db))
            }
            Type::Tuple(tuple) => tuple
                .elements(db)
                .iter()
                .all(|elem| elem.is_fully_static(db)),
            // TODO: Once we support them, make sure that we return `false` for other types
            // containing gradual forms such as `tuple[Any, ...]` or `Callable[..., str]`.
            // Conversely, make sure to return `true` for homogeneous tuples such as
            // `tuple[int, ...]`, once we add support for them.
        }
    }

    /// Return true if there is just a single inhabitant for this type.
    ///
    /// Note: This function aims to have no false positives, but might return `false`
    /// for more complicated types that are actually singletons.
    pub(crate) fn is_singleton(self, db: &'db dyn Db) -> bool {
        match self {
            Type::Dynamic(_)
            | Type::Never
            | Type::IntLiteral(..)
            | Type::StringLiteral(..)
            | Type::BytesLiteral(..)
            | Type::SliceLiteral(..)
            | Type::LiteralString => {
                // Note: The literal types included in this pattern are not true singletons.
                // There can be multiple Python objects (at different memory locations) that
                // are both of type Literal[345], for example.
                false
            }
            // We eagerly transform `SubclassOf` to `ClassLiteral` for final types, so `SubclassOf` is never a singleton.
            Type::SubclassOf(..) => false,
            Type::BooleanLiteral(_)
            | Type::FunctionLiteral(..)
            | Type::ClassLiteral(..)
            | Type::ModuleLiteral(..)
            | Type::KnownInstance(..) => true,
            Type::Instance(InstanceType { class }) => {
                class.known(db).is_some_and(KnownClass::is_singleton)
            }
            Type::Tuple(..) => {
                // The empty tuple is a singleton on CPython and PyPy, but not on other Python
                // implementations such as GraalPy. Its *use* as a singleton is discouraged and
                // should not be relied on for type narrowing, so we do not treat it as one.
                // See:
                // https://docs.python.org/3/reference/expressions.html#parenthesized-forms
                false
            }
            Type::Union(..) => {
                // A single-element union, where the sole element was a singleton, would itself
                // be a singleton type. However, unions with length < 2 should never appear in
                // our model due to [`UnionBuilder::build`].
                false
            }
            Type::Intersection(..) => {
                // Here, we assume that all intersection types that are singletons would have
                // been reduced to a different form via [`IntersectionBuilder::build`] by now.
                // For example:
                //
                //   bool & ~Literal[False]   = Literal[True]
                //   None & (None | int)      = None | None & int = None
                //
                false
            }
            Type::AlwaysTruthy | Type::AlwaysFalsy => false,
        }
    }

    /// Return true if this type is non-empty and all inhabitants of this type compare equal.
    pub(crate) fn is_single_valued(self, db: &'db dyn Db) -> bool {
        match self {
            Type::FunctionLiteral(..)
            | Type::ModuleLiteral(..)
            | Type::ClassLiteral(..)
            | Type::IntLiteral(..)
            | Type::BooleanLiteral(..)
            | Type::StringLiteral(..)
            | Type::BytesLiteral(..)
            | Type::SliceLiteral(..)
            | Type::KnownInstance(..) => true,

            Type::SubclassOf(..) => {
                // TODO: Same comment as above for `is_singleton`
                false
            }

            Type::Tuple(tuple) => tuple
                .elements(db)
                .iter()
                .all(|elem| elem.is_single_valued(db)),

            Type::Instance(InstanceType { class }) => match class.known(db) {
                Some(
                    KnownClass::NoneType
                    | KnownClass::NoDefaultType
                    | KnownClass::VersionInfo
                    | KnownClass::TypeAliasType,
                ) => true,
                Some(
                    KnownClass::Bool
                    | KnownClass::Object
                    | KnownClass::Bytes
                    | KnownClass::Type
                    | KnownClass::Int
                    | KnownClass::Float
                    | KnownClass::Str
                    | KnownClass::List
                    | KnownClass::Tuple
                    | KnownClass::Set
                    | KnownClass::FrozenSet
                    | KnownClass::Dict
                    | KnownClass::Slice
                    | KnownClass::BaseException
                    | KnownClass::BaseExceptionGroup
                    | KnownClass::GenericAlias
                    | KnownClass::ModuleType
                    | KnownClass::FunctionType
                    | KnownClass::SpecialForm
                    | KnownClass::ChainMap
                    | KnownClass::Counter
                    | KnownClass::DefaultDict
                    | KnownClass::Deque
                    | KnownClass::OrderedDict
                    | KnownClass::StdlibAlias
                    | KnownClass::TypeVar,
                ) => false,
                None => false,
            },

            Type::Dynamic(_)
            | Type::Never
            | Type::Union(..)
            | Type::Intersection(..)
            | Type::LiteralString
            | Type::AlwaysTruthy
            | Type::AlwaysFalsy => false,
        }
    }

    /// Resolve a member access of a type.
    ///
    /// For example, if `foo` is `Type::Instance(<Bar>)`,
    /// `foo.member(&db, "baz")` returns the type of `baz` attributes
    /// as accessed from instances of the `Bar` class.
    #[must_use]
    pub(crate) fn member(&self, db: &'db dyn Db, name: &str) -> Symbol<'db> {
        if name == "__class__" {
            return self.to_meta_type(db).into();
        }

        match self {
            Type::Dynamic(_) => self.into(),

            Type::Never => todo_type!("attribute lookup on Never").into(),

            Type::FunctionLiteral(_) => match name {
                "__get__" => todo_type!("`__get__` method on functions").into(),
                "__call__" => todo_type!("`__call__` method on functions").into(),
                _ => KnownClass::FunctionType.to_instance(db).member(db, name),
            },

            Type::ModuleLiteral(module) => module.member(db, name),

            Type::ClassLiteral(class_ty) => class_ty.member(db, name),

            Type::SubclassOf(subclass_of_ty) => subclass_of_ty.member(db, name),

            Type::KnownInstance(known_instance) => known_instance.member(db, name),

            Type::Instance(InstanceType { class }) => {
                let ty = match (class.known(db), name) {
                    (Some(KnownClass::VersionInfo), "major") => {
                        Type::IntLiteral(Program::get(db).python_version(db).major.into())
                    }
                    (Some(KnownClass::VersionInfo), "minor") => {
                        Type::IntLiteral(Program::get(db).python_version(db).minor.into())
                    }
                    // TODO MRO? get_own_instance_member, get_instance_member
                    _ => todo_type!("instance attributes"),
                };
                ty.into()
            }

            Type::Union(union) => {
                let mut builder = UnionBuilder::new(db);

                let mut all_unbound = true;
                let mut possibly_unbound = false;
                for ty in union.elements(db) {
                    let ty_member = ty.member(db, name);
                    match ty_member {
                        Symbol::Unbound => {
                            possibly_unbound = true;
                        }
                        Symbol::Type(ty_member, member_boundness) => {
                            if member_boundness == Boundness::PossiblyUnbound {
                                possibly_unbound = true;
                            }

                            all_unbound = false;
                            builder = builder.add(ty_member);
                        }
                    }
                }

                if all_unbound {
                    Symbol::Unbound
                } else {
                    Symbol::Type(
                        builder.build(),
                        if possibly_unbound {
                            Boundness::PossiblyUnbound
                        } else {
                            Boundness::Bound
                        },
                    )
                }
            }

            Type::Intersection(_) => {
                // TODO perform the get_member on each type in the intersection
                // TODO return the intersection of those results
                todo_type!("Attribute access on `Intersection` types").into()
            }

            Type::IntLiteral(_) => match name {
                "real" | "numerator" => self.into(),
                // TODO more attributes could probably be usefully special-cased
                _ => KnownClass::Int.to_instance(db).member(db, name),
            },

            Type::BooleanLiteral(bool_value) => match name {
                "real" | "numerator" => Type::IntLiteral(i64::from(*bool_value)).into(),
                _ => KnownClass::Bool.to_instance(db).member(db, name),
            },

            Type::StringLiteral(_) => {
                // TODO defer to `typing.LiteralString`/`builtins.str` methods
                // from typeshed's stubs
                todo_type!("Attribute access on `StringLiteral` types").into()
            }

            Type::LiteralString => {
                // TODO defer to `typing.LiteralString`/`builtins.str` methods
                // from typeshed's stubs
                todo_type!("Attribute access on `LiteralString` types").into()
            }

            Type::BytesLiteral(_) => KnownClass::Bytes.to_instance(db).member(db, name),

            // We could plausibly special-case `start`, `step`, and `stop` here,
            // but it doesn't seem worth the complexity given the very narrow range of places
            // where we infer `SliceLiteral` types.
            Type::SliceLiteral(_) => KnownClass::Slice.to_instance(db).member(db, name),

            Type::Tuple(_) => {
                // TODO: implement tuple methods
                todo_type!("Attribute access on heterogeneous tuple types").into()
            }

            Type::AlwaysTruthy | Type::AlwaysFalsy => match name {
                "__bool__" => {
                    // TODO should be `Callable[[], Literal[True/False]]`
                    todo_type!("`__bool__` for `AlwaysTruthy`/`AlwaysFalsy` Type variants").into()
                }
                _ => KnownClass::Object.to_instance(db).member(db, name),
            },
        }
    }

    /// Resolves the boolean value of a type.
    ///
    /// This is used to determine the value that would be returned
    /// when `bool(x)` is called on an object `x`.
    pub(crate) fn bool(&self, db: &'db dyn Db) -> Truthiness {
        match self {
            Type::Dynamic(_) | Type::Never => Truthiness::Ambiguous,
            Type::FunctionLiteral(_) => Truthiness::AlwaysTrue,
            Type::ModuleLiteral(_) => Truthiness::AlwaysTrue,
            Type::ClassLiteral(ClassLiteralType { class }) => {
                class.metaclass(db).to_instance(db).bool(db)
            }
            Type::SubclassOf(subclass_of_ty) => subclass_of_ty
                .subclass_of()
                .into_class()
                .map(|class| Type::class_literal(class).bool(db))
                .unwrap_or(Truthiness::Ambiguous),
            Type::AlwaysTruthy => Truthiness::AlwaysTrue,
            Type::AlwaysFalsy => Truthiness::AlwaysFalse,
            instance_ty @ Type::Instance(InstanceType { class }) => {
                if class.is_known(db, KnownClass::NoneType) {
                    Truthiness::AlwaysFalse
                } else {
                    // We only check the `__bool__` method for truth testing, even though at
                    // runtime there is a fallback to `__len__`, since `__bool__` takes precedence
                    // and a subclass could add a `__bool__` method. We don't use
                    // `Type::call_dunder` here because of the need to check for `__bool__ = bool`.

                    // Don't trust a maybe-unbound `__bool__` method.
                    let Symbol::Type(bool_method, Boundness::Bound) =
                        instance_ty.to_meta_type(db).member(db, "__bool__")
                    else {
                        return Truthiness::Ambiguous;
                    };

                    // Check if the class has `__bool__ = bool` and avoid infinite recursion, since
                    // `Type::call` on `bool` will call `Type::bool` on the argument.
                    if bool_method
                        .into_class_literal()
                        .is_some_and(|ClassLiteralType { class }| {
                            class.is_known(db, KnownClass::Bool)
                        })
                    {
                        return Truthiness::Ambiguous;
                    }

                    if let Some(Type::BooleanLiteral(bool_val)) = bool_method
                        .call(db, &CallArguments::positional([*instance_ty]))
                        .return_ty(db)
                    {
                        bool_val.into()
                    } else {
                        // TODO diagnostic if not assignable to bool
                        Truthiness::Ambiguous
                    }
                }
            }
            Type::KnownInstance(known_instance) => known_instance.bool(),
            Type::Union(union) => {
                let union_elements = union.elements(db);
                let first_element_truthiness = union_elements[0].bool(db);
                if first_element_truthiness.is_ambiguous() {
                    return Truthiness::Ambiguous;
                }
                if !union_elements
                    .iter()
                    .skip(1)
                    .all(|element| element.bool(db) == first_element_truthiness)
                {
                    return Truthiness::Ambiguous;
                }
                first_element_truthiness
            }
            Type::Intersection(_) => {
                // TODO
                Truthiness::Ambiguous
            }
            Type::IntLiteral(num) => Truthiness::from(*num != 0),
            Type::BooleanLiteral(bool) => Truthiness::from(*bool),
            Type::StringLiteral(str) => Truthiness::from(!str.value(db).is_empty()),
            Type::LiteralString => Truthiness::Ambiguous,
            Type::BytesLiteral(bytes) => Truthiness::from(!bytes.value(db).is_empty()),
            Type::SliceLiteral(_) => Truthiness::AlwaysTrue,
            Type::Tuple(items) => Truthiness::from(!items.elements(db).is_empty()),
        }
    }

    /// Return the type of `len()` on a type if it is known more precisely than `int`,
    /// or `None` otherwise.
    ///
    /// In the second case, the return type of `len()` in `typeshed` (`int`)
    /// is used as a fallback.
    fn len(&self, db: &'db dyn Db) -> Option<Type<'db>> {
        fn non_negative_int_literal<'db>(db: &'db dyn Db, ty: Type<'db>) -> Option<Type<'db>> {
            match ty {
                // TODO: Emit diagnostic for non-integers and negative integers
                Type::IntLiteral(value) => (value >= 0).then_some(ty),
                Type::BooleanLiteral(value) => Some(Type::IntLiteral(value.into())),
                Type::Union(union) => {
                    let mut builder = UnionBuilder::new(db);
                    for element in union.elements(db) {
                        builder = builder.add(non_negative_int_literal(db, *element)?);
                    }
                    Some(builder.build())
                }
                _ => None,
            }
        }

        let usize_len = match self {
            Type::BytesLiteral(bytes) => Some(bytes.python_len(db)),
            Type::StringLiteral(string) => Some(string.python_len(db)),
            Type::Tuple(tuple) => Some(tuple.len(db)),
            _ => None,
        };

        if let Some(usize_len) = usize_len {
            return usize_len.try_into().ok().map(Type::IntLiteral);
        }

        let return_ty = match self.call_dunder(db, "__len__", &CallArguments::positional([*self])) {
            // TODO: emit a diagnostic
            CallDunderResult::MethodNotAvailable => return None,

            CallDunderResult::CallOutcome(outcome) | CallDunderResult::PossiblyUnbound(outcome) => {
                outcome.return_ty(db)?
            }
        };

        non_negative_int_literal(db, return_ty)
    }

    /// Return the outcome of calling an object of this type.
    #[must_use]
    fn call(self, db: &'db dyn Db, arguments: &CallArguments<'_, 'db>) -> CallOutcome<'db> {
        match self {
            Type::FunctionLiteral(function_type) => {
                let mut binding = bind_call(db, arguments, function_type.signature(db), Some(self));
                match function_type.known(db) {
                    Some(KnownFunction::RevealType) => {
                        let revealed_ty = binding.one_parameter_ty().unwrap_or(Type::unknown());
                        CallOutcome::revealed(binding, revealed_ty)
                    }
                    Some(KnownFunction::StaticAssert) => {
                        if let Some((parameter_ty, message)) = binding.two_parameter_tys() {
                            let truthiness = parameter_ty.bool(db);

                            if truthiness.is_always_true() {
                                CallOutcome::callable(binding)
                            } else {
                                let error_kind = if let Some(message) =
                                    message.into_string_literal().map(|s| &**s.value(db))
                                {
                                    StaticAssertionErrorKind::CustomError(message)
                                } else if parameter_ty == Type::BooleanLiteral(false) {
                                    StaticAssertionErrorKind::ArgumentIsFalse
                                } else if truthiness.is_always_false() {
                                    StaticAssertionErrorKind::ArgumentIsFalsy(parameter_ty)
                                } else {
                                    StaticAssertionErrorKind::ArgumentTruthinessIsAmbiguous(
                                        parameter_ty,
                                    )
                                };

                                CallOutcome::StaticAssertionError {
                                    binding,
                                    error_kind,
                                }
                            }
                        } else {
                            CallOutcome::callable(binding)
                        }
                    }
                    Some(KnownFunction::IsEquivalentTo) => {
                        let (ty_a, ty_b) = binding
                            .two_parameter_tys()
                            .unwrap_or((Type::unknown(), Type::unknown()));
                        binding
                            .set_return_ty(Type::BooleanLiteral(ty_a.is_equivalent_to(db, ty_b)));
                        CallOutcome::callable(binding)
                    }
                    Some(KnownFunction::IsSubtypeOf) => {
                        let (ty_a, ty_b) = binding
                            .two_parameter_tys()
                            .unwrap_or((Type::unknown(), Type::unknown()));
                        binding.set_return_ty(Type::BooleanLiteral(ty_a.is_subtype_of(db, ty_b)));
                        CallOutcome::callable(binding)
                    }
                    Some(KnownFunction::IsAssignableTo) => {
                        let (ty_a, ty_b) = binding
                            .two_parameter_tys()
                            .unwrap_or((Type::unknown(), Type::unknown()));
                        binding
                            .set_return_ty(Type::BooleanLiteral(ty_a.is_assignable_to(db, ty_b)));
                        CallOutcome::callable(binding)
                    }
                    Some(KnownFunction::IsDisjointFrom) => {
                        let (ty_a, ty_b) = binding
                            .two_parameter_tys()
                            .unwrap_or((Type::unknown(), Type::unknown()));
                        binding
                            .set_return_ty(Type::BooleanLiteral(ty_a.is_disjoint_from(db, ty_b)));
                        CallOutcome::callable(binding)
                    }
                    Some(KnownFunction::IsFullyStatic) => {
                        let ty = binding.one_parameter_ty().unwrap_or(Type::unknown());
                        binding.set_return_ty(Type::BooleanLiteral(ty.is_fully_static(db)));
                        CallOutcome::callable(binding)
                    }
                    Some(KnownFunction::IsSingleton) => {
                        let ty = binding.one_parameter_ty().unwrap_or(Type::unknown());
                        binding.set_return_ty(Type::BooleanLiteral(ty.is_singleton(db)));
                        CallOutcome::callable(binding)
                    }
                    Some(KnownFunction::IsSingleValued) => {
                        let ty = binding.one_parameter_ty().unwrap_or(Type::unknown());
                        binding.set_return_ty(Type::BooleanLiteral(ty.is_single_valued(db)));
                        CallOutcome::callable(binding)
                    }

                    Some(KnownFunction::Len) => {
                        if let Some(first_arg) = binding.one_parameter_ty() {
                            if let Some(len_ty) = first_arg.len(db) {
                                binding.set_return_ty(len_ty);
                            }
                        };

                        CallOutcome::callable(binding)
                    }

                    Some(KnownFunction::AssertType) => {
                        let Some((_, asserted_ty)) = binding.two_parameter_tys() else {
                            return CallOutcome::callable(binding);
                        };

                        CallOutcome::asserted(binding, asserted_ty)
                    }

                    Some(KnownFunction::Cast) => {
                        // TODO: Use `.two_parameter_tys()` exclusively
                        // when overloads are supported.
                        if binding.two_parameter_tys().is_none() {
                            return CallOutcome::callable(binding);
                        };

                        if let Some(casted_ty) = arguments.first_argument() {
                            binding.set_return_ty(casted_ty);
                        };

                        CallOutcome::callable(binding)
                    }

                    _ => CallOutcome::callable(binding),
                }
            }

            // TODO annotated return type on `__new__` or metaclass `__call__`
            // TODO check call vs signatures of `__new__` and/or `__init__`
            Type::ClassLiteral(ClassLiteralType { class }) => {
                CallOutcome::callable(CallBinding::from_return_ty(match class.known(db) {
                    // If the class is the builtin-bool class (for example `bool(1)`), we try to
                    // return the specific truthiness value of the input arg, `Literal[True]` for
                    // the example above.
                    Some(KnownClass::Bool) => arguments
                        .first_argument()
                        .map(|arg| arg.bool(db).into_type(db))
                        .unwrap_or(Type::BooleanLiteral(false)),
                    _ => Type::Instance(InstanceType { class }),
                }))
            }

            instance_ty @ Type::Instance(_) => {
                match instance_ty.call_dunder(db, "__call__", &arguments.with_self(instance_ty)) {
                    CallDunderResult::CallOutcome(CallOutcome::NotCallable { .. }) => {
                        // Turn "`<type of illegal '__call__'>` not callable" into
                        // "`X` not callable"
                        CallOutcome::NotCallable {
                            not_callable_ty: self,
                        }
                    }
                    CallDunderResult::CallOutcome(outcome) => outcome,
                    CallDunderResult::PossiblyUnbound(call_outcome) => {
                        // Turn "possibly unbound object of type `Literal['__call__']`"
                        // into "`X` not callable (possibly unbound `__call__` method)"
                        CallOutcome::PossiblyUnboundDunderCall {
                            called_ty: self,
                            call_outcome: Box::new(call_outcome),
                        }
                    }
                    CallDunderResult::MethodNotAvailable => {
                        // Turn "`X.__call__` unbound" into "`X` not callable"
                        CallOutcome::NotCallable {
                            not_callable_ty: self,
                        }
                    }
                }
            }

            // Dynamic types are callable, and the return type is the same dynamic type
            Type::Dynamic(_) => CallOutcome::callable(CallBinding::from_return_ty(self)),

            Type::Union(union) => CallOutcome::union(
                self,
                union
                    .elements(db)
                    .iter()
                    .map(|elem| elem.call(db, arguments)),
            ),

            Type::Intersection(_) => CallOutcome::callable(CallBinding::from_return_ty(
                todo_type!("Type::Intersection.call()"),
            )),

            _ => CallOutcome::not_callable(self),
        }
    }

    /// Look up a dunder method on the meta type of `self` and call it.
    fn call_dunder(
        self,
        db: &'db dyn Db,
        name: &str,
        arguments: &CallArguments<'_, 'db>,
    ) -> CallDunderResult<'db> {
        match self.to_meta_type(db).member(db, name) {
            Symbol::Type(callable_ty, Boundness::Bound) => {
                CallDunderResult::CallOutcome(callable_ty.call(db, arguments))
            }
            Symbol::Type(callable_ty, Boundness::PossiblyUnbound) => {
                CallDunderResult::PossiblyUnbound(callable_ty.call(db, arguments))
            }
            Symbol::Unbound => CallDunderResult::MethodNotAvailable,
        }
    }

    /// Given the type of an object that is iterated over in some way,
    /// return the type of objects that are yielded by that iteration.
    ///
    /// E.g., for the following loop, given the type of `x`, infer the type of `y`:
    /// ```python
    /// for y in x:
    ///     pass
    /// ```
    fn iterate(self, db: &'db dyn Db) -> IterationOutcome<'db> {
        if let Type::Tuple(tuple_type) = self {
            return IterationOutcome::Iterable {
                element_ty: UnionType::from_elements(db, tuple_type.elements(db)),
            };
        }

        let dunder_iter_result =
            self.call_dunder(db, "__iter__", &CallArguments::positional([self]));
        match dunder_iter_result {
            CallDunderResult::CallOutcome(ref call_outcome)
            | CallDunderResult::PossiblyUnbound(ref call_outcome) => {
                let Some(iterator_ty) = call_outcome.return_ty(db) else {
                    return IterationOutcome::NotIterable {
                        not_iterable_ty: self,
                    };
                };

                return if let Some(element_ty) = iterator_ty
                    .call_dunder(db, "__next__", &CallArguments::positional([iterator_ty]))
                    .return_ty(db)
                {
                    if matches!(dunder_iter_result, CallDunderResult::PossiblyUnbound(..)) {
                        IterationOutcome::PossiblyUnboundDunderIter {
                            iterable_ty: self,
                            element_ty,
                        }
                    } else {
                        IterationOutcome::Iterable { element_ty }
                    }
                } else {
                    IterationOutcome::NotIterable {
                        not_iterable_ty: self,
                    }
                };
            }
            CallDunderResult::MethodNotAvailable => {}
        }

        // Although it's not considered great practice,
        // classes that define `__getitem__` are also iterable,
        // even if they do not define `__iter__`.
        //
        // TODO(Alex) this is only valid if the `__getitem__` method is annotated as
        // accepting `int` or `SupportsIndex`
        if let Some(element_ty) = self
            .call_dunder(
                db,
                "__getitem__",
                &CallArguments::positional([self, KnownClass::Int.to_instance(db)]),
            )
            .return_ty(db)
        {
            IterationOutcome::Iterable { element_ty }
        } else {
            IterationOutcome::NotIterable {
                not_iterable_ty: self,
            }
        }
    }

    #[must_use]
    pub fn to_instance(&self, db: &'db dyn Db) -> Type<'db> {
        match self {
            Type::Dynamic(_) => *self,
            Type::Never => Type::Never,
            Type::ClassLiteral(ClassLiteralType { class }) => Type::instance(*class),
            Type::SubclassOf(subclass_of_ty) => match subclass_of_ty.subclass_of() {
                ClassBase::Class(class) => Type::instance(class),
                ClassBase::Dynamic(dynamic) => Type::Dynamic(dynamic),
            },
            Type::Union(union) => union.map(db, |element| element.to_instance(db)),
            Type::Intersection(_) => todo_type!("Type::Intersection.to_instance()"),
            // TODO: calling `.to_instance()` on any of these should result in a diagnostic,
            // since they already indicate that the object is an instance of some kind:
            Type::BooleanLiteral(_)
            | Type::BytesLiteral(_)
            | Type::FunctionLiteral(_)
            | Type::Instance(_)
            | Type::KnownInstance(_)
            | Type::ModuleLiteral(_)
            | Type::IntLiteral(_)
            | Type::StringLiteral(_)
            | Type::SliceLiteral(_)
            | Type::Tuple(_)
            | Type::LiteralString
            | Type::AlwaysTruthy
            | Type::AlwaysFalsy => Type::unknown(),
        }
    }

    /// If we see a value of this type used as a type expression, what type does it name?
    ///
    /// For example, the builtin `int` as a value expression is of type
    /// `Type::ClassLiteral(builtins.int)`, that is, it is the `int` class itself. As a type
    /// expression, it names the type `Type::Instance(builtins.int)`, that is, all objects whose
    /// `__class__` is `int`.
    pub fn in_type_expression(
        &self,
        db: &'db dyn Db,
    ) -> Result<Type<'db>, InvalidTypeExpressionError<'db>> {
        match self {
            // In a type expression, a bare `type` is interpreted as "instance of `type`", which is
            // equivalent to `type[object]`.
            Type::ClassLiteral(_) | Type::SubclassOf(_) => Ok(self.to_instance(db)),
            // We treat `typing.Type` exactly the same as `builtins.type`:
            Type::KnownInstance(KnownInstanceType::Type) => Ok(KnownClass::Type.to_instance(db)),
            Type::KnownInstance(KnownInstanceType::Tuple) => Ok(KnownClass::Tuple.to_instance(db)),

            // Legacy `typing` aliases
            Type::KnownInstance(KnownInstanceType::List) => Ok(KnownClass::List.to_instance(db)),
            Type::KnownInstance(KnownInstanceType::Dict) => Ok(KnownClass::Dict.to_instance(db)),
            Type::KnownInstance(KnownInstanceType::Set) => Ok(KnownClass::Set.to_instance(db)),
            Type::KnownInstance(KnownInstanceType::FrozenSet) => {
                Ok(KnownClass::FrozenSet.to_instance(db))
            }
            Type::KnownInstance(KnownInstanceType::ChainMap) => {
                Ok(KnownClass::ChainMap.to_instance(db))
            }
            Type::KnownInstance(KnownInstanceType::Counter) => {
                Ok(KnownClass::Counter.to_instance(db))
            }
            Type::KnownInstance(KnownInstanceType::DefaultDict) => {
                Ok(KnownClass::DefaultDict.to_instance(db))
            }
            Type::KnownInstance(KnownInstanceType::Deque) => Ok(KnownClass::Deque.to_instance(db)),
            Type::KnownInstance(KnownInstanceType::OrderedDict) => {
                Ok(KnownClass::OrderedDict.to_instance(db))
            }

            Type::Union(union) => {
                let mut builder = UnionBuilder::new(db);
                let mut invalid_expressions = smallvec::SmallVec::default();
                for element in union.elements(db) {
                    match element.in_type_expression(db) {
                        Ok(type_expr) => builder = builder.add(type_expr),
                        Err(InvalidTypeExpressionError {
                            fallback_type,
                            invalid_expressions: new_invalid_expressions,
                        }) => {
                            invalid_expressions.extend(new_invalid_expressions);
                            builder = builder.add(fallback_type);
                        }
                    }
                }
                if invalid_expressions.is_empty() {
                    Ok(builder.build())
                } else {
                    Err(InvalidTypeExpressionError {
                        fallback_type: builder.build(),
                        invalid_expressions,
                    })
                }
            }
            Type::Dynamic(_) => Ok(*self),
            // TODO map this to a new `Type::TypeVar` variant
            Type::KnownInstance(KnownInstanceType::TypeVar(_)) => Ok(*self),
            Type::KnownInstance(KnownInstanceType::TypeAliasType(alias)) => Ok(alias.value_ty(db)),
            Type::KnownInstance(KnownInstanceType::Never | KnownInstanceType::NoReturn) => {
                Ok(Type::Never)
            }
            Type::KnownInstance(KnownInstanceType::LiteralString) => Ok(Type::LiteralString),
            Type::KnownInstance(KnownInstanceType::Any) => Ok(Type::any()),
            // TODO: Should emit a diagnostic
            Type::KnownInstance(KnownInstanceType::Annotated) => Err(InvalidTypeExpressionError {
                invalid_expressions: smallvec::smallvec![InvalidTypeExpression::BareAnnotated],
                fallback_type: Type::unknown(),
            }),
            Type::KnownInstance(KnownInstanceType::Literal) => Err(InvalidTypeExpressionError {
                invalid_expressions: smallvec::smallvec![InvalidTypeExpression::BareLiteral],
                fallback_type: Type::unknown(),
            }),
            Type::KnownInstance(KnownInstanceType::Unknown) => Ok(Type::unknown()),
            Type::KnownInstance(KnownInstanceType::AlwaysTruthy) => Ok(Type::AlwaysTruthy),
            Type::KnownInstance(KnownInstanceType::AlwaysFalsy) => Ok(Type::AlwaysFalsy),
            _ => Ok(todo_type!(
                "Unsupported or invalid type in a type expression"
            )),
        }
    }

    /// The type `NoneType` / `None`
    pub fn none(db: &'db dyn Db) -> Type<'db> {
        KnownClass::NoneType.to_instance(db)
    }

    /// Return the type of `tuple(sys.version_info)`.
    ///
    /// This is not exactly the type that `sys.version_info` has at runtime,
    /// but it's a useful fallback for us in order to infer `Literal` types from `sys.version_info` comparisons.
    fn version_info_tuple(db: &'db dyn Db) -> Self {
        let python_version = Program::get(db).python_version(db);
        let int_instance_ty = KnownClass::Int.to_instance(db);

        // TODO: just grab this type from typeshed (it's a `sys._ReleaseLevel` type alias there)
        let release_level_ty = {
            let elements: Box<[Type<'db>]> = ["alpha", "beta", "candidate", "final"]
                .iter()
                .map(|level| Type::string_literal(db, level))
                .collect();

            // For most unions, it's better to go via `UnionType::from_elements` or use `UnionBuilder`;
            // those techniques ensure that union elements are deduplicated and unions are eagerly simplified
            // into other types where necessary. Here, however, we know that there are no duplicates
            // in this union, so it's probably more efficient to use `UnionType::new()` directly.
            Type::Union(UnionType::new(db, elements))
        };

        TupleType::from_elements(
            db,
            [
                Type::IntLiteral(python_version.major.into()),
                Type::IntLiteral(python_version.minor.into()),
                int_instance_ty,
                release_level_ty,
                int_instance_ty,
            ],
        )
    }

    /// Given a type that is assumed to represent an instance of a class,
    /// return a type that represents that class itself.
    #[must_use]
    pub fn to_meta_type(&self, db: &'db dyn Db) -> Type<'db> {
        match self {
            Type::Never => Type::Never,
            Type::Instance(InstanceType { class }) => SubclassOfType::from(db, *class),
            Type::KnownInstance(known_instance) => known_instance.class().to_class_literal(db),
            Type::Union(union) => union.map(db, |ty| ty.to_meta_type(db)),
            Type::BooleanLiteral(_) => KnownClass::Bool.to_class_literal(db),
            Type::BytesLiteral(_) => KnownClass::Bytes.to_class_literal(db),
            Type::SliceLiteral(_) => KnownClass::Slice.to_class_literal(db),
            Type::IntLiteral(_) => KnownClass::Int.to_class_literal(db),
            Type::FunctionLiteral(_) => KnownClass::FunctionType.to_class_literal(db),
            Type::ModuleLiteral(_) => KnownClass::ModuleType.to_class_literal(db),
            Type::Tuple(_) => KnownClass::Tuple.to_class_literal(db),
            Type::ClassLiteral(ClassLiteralType { class }) => class.metaclass(db),
            Type::SubclassOf(subclass_of_ty) => match subclass_of_ty.subclass_of() {
                ClassBase::Dynamic(_) => *self,
                ClassBase::Class(class) => SubclassOfType::from(
                    db,
                    ClassBase::try_from_ty(db, class.metaclass(db)).unwrap_or(ClassBase::unknown()),
                ),
            },

            Type::StringLiteral(_) | Type::LiteralString => KnownClass::Str.to_class_literal(db),
            Type::Dynamic(dynamic) => SubclassOfType::from(db, ClassBase::Dynamic(*dynamic)),
            // TODO intersections
            Type::Intersection(_) => SubclassOfType::from(
                db,
                ClassBase::try_from_ty(db, todo_type!("Intersection meta-type"))
                    .expect("Type::Todo should be a valid ClassBase"),
            ),
            Type::AlwaysTruthy | Type::AlwaysFalsy => KnownClass::Type.to_instance(db),
        }
    }

    /// Return the string representation of this type when converted to string as it would be
    /// provided by the `__str__` method.
    ///
    /// When not available, this should fall back to the value of `[Type::repr]`.
    /// Note: this method is used in the builtins `format`, `print`, `str.format` and `f-strings`.
    #[must_use]
    pub fn str(&self, db: &'db dyn Db) -> Type<'db> {
        match self {
            Type::IntLiteral(_) | Type::BooleanLiteral(_) => self.repr(db),
            Type::StringLiteral(_) | Type::LiteralString => *self,
            Type::KnownInstance(known_instance) => {
                Type::string_literal(db, known_instance.repr(db))
            }
            // TODO: handle more complex types
            _ => KnownClass::Str.to_instance(db),
        }
    }

    /// Return the string representation of this type as it would be provided by the  `__repr__`
    /// method at runtime.
    #[must_use]
    pub fn repr(&self, db: &'db dyn Db) -> Type<'db> {
        match self {
            Type::IntLiteral(number) => Type::string_literal(db, &number.to_string()),
            Type::BooleanLiteral(true) => Type::string_literal(db, "True"),
            Type::BooleanLiteral(false) => Type::string_literal(db, "False"),
            Type::StringLiteral(literal) => {
                Type::string_literal(db, &format!("'{}'", literal.value(db).escape_default()))
            }
            Type::LiteralString => Type::LiteralString,
            Type::KnownInstance(known_instance) => {
                Type::string_literal(db, known_instance.repr(db))
            }
            // TODO: handle more complex types
            _ => KnownClass::Str.to_instance(db),
        }
    }
}

impl<'db> From<&Type<'db>> for Type<'db> {
    fn from(value: &Type<'db>) -> Self {
        *value
    }
}

impl<'db> From<Type<'db>> for Symbol<'db> {
    fn from(value: Type<'db>) -> Self {
        Symbol::Type(value, Boundness::Bound)
    }
}

impl<'db> From<&Type<'db>> for Symbol<'db> {
    fn from(value: &Type<'db>) -> Self {
        Self::from(*value)
    }
}

#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum DynamicType {
    // An explicitly annotated `typing.Any`
    Any,
    // An unannotated value, or a dynamic type resulting from an error
    Unknown,
    /// Temporary type for symbols that can't be inferred yet because of missing implementations.
    ///
    /// This variant should eventually be removed once red-knot is spec-compliant.
    ///
    /// General rule: `Todo` should only propagate when the presence of the input `Todo` caused the
    /// output to be unknown. An output should only be `Todo` if fixing all `Todo` inputs to be not
    /// `Todo` would change the output type.
    ///
    /// This variant should be created with the `todo_type!` macro.
    Todo(TodoType),
}

impl std::fmt::Display for DynamicType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DynamicType::Any => f.write_str("Any"),
            DynamicType::Unknown => f.write_str("Unknown"),
            // `DynamicType::Todo`'s display should be explicit that is not a valid display of
            // any other type
            DynamicType::Todo(todo) => write!(f, "@Todo{todo}"),
        }
    }
}

/// Error struct providing information on type(s) that were deemed to be invalid
/// in a type expression context, and the type we should therefore fallback to
/// for the problematic type expression.
#[derive(Debug, PartialEq, Eq)]
pub struct InvalidTypeExpressionError<'db> {
    fallback_type: Type<'db>,
    invalid_expressions: smallvec::SmallVec<[InvalidTypeExpression; 1]>,
}

impl<'db> InvalidTypeExpressionError<'db> {
    fn into_fallback_type(self, context: &InferContext, node: &ast::Expr) -> Type<'db> {
        let InvalidTypeExpressionError {
            fallback_type,
            invalid_expressions,
        } = self;
        for error in invalid_expressions {
            context.report_lint(
                &INVALID_TYPE_FORM,
                node.into(),
                format_args!("{}", error.reason()),
            );
        }
        fallback_type
    }
}

/// Enumeration of various types that are invalid in type-expression contexts
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum InvalidTypeExpression {
    /// `x: Annotated` is invalid as an annotation
    BareAnnotated,
    /// `x: Literal` is invalid as an annotation
    BareLiteral,
}

impl InvalidTypeExpression {
    const fn reason(self) -> &'static str {
        match self {
            Self::BareAnnotated => "`Annotated` requires at least two arguments when used in an annotation or type expression",
            Self::BareLiteral => "`Literal` requires at least one argument when used in a type expression",
        }
    }
}

/// Non-exhaustive enumeration of known classes (e.g. `builtins.int`, `typing.Any`, ...) to allow
/// for easier syntax when interacting with very common classes.
///
/// Feel free to expand this enum if you ever find yourself using the same class in multiple
/// places.
/// Note: good candidates are any classes in `[crate::module_resolver::module::KnownModule]`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KnownClass {
    // To figure out where an stdlib symbol is defined, you can go into `crates/red_knot_vendored`
    // and grep for the symbol name in any `.pyi` file.

    // Builtins
    Bool,
    Object,
    Bytes,
    Type,
    Int,
    Float,
    Str,
    List,
    Tuple,
    Set,
    FrozenSet,
    Dict,
    Slice,
    BaseException,
    BaseExceptionGroup,
    // Types
    GenericAlias,
    ModuleType,
    FunctionType,
    // Typeshed
    NoneType, // Part of `types` for Python >= 3.10
    // Typing
    StdlibAlias,
    SpecialForm,
    TypeVar,
    TypeAliasType,
    NoDefaultType,
    // Collections
    ChainMap,
    Counter,
    DefaultDict,
    Deque,
    OrderedDict,
    // sys
    VersionInfo,
}

impl<'db> KnownClass {
    pub const fn is_bool(self) -> bool {
        matches!(self, Self::Bool)
    }

    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Object => "object",
            Self::Bytes => "bytes",
            Self::Tuple => "tuple",
            Self::Int => "int",
            Self::Float => "float",
            Self::FrozenSet => "frozenset",
            Self::Str => "str",
            Self::Set => "set",
            Self::Dict => "dict",
            Self::List => "list",
            Self::Type => "type",
            Self::Slice => "slice",
            Self::BaseException => "BaseException",
            Self::BaseExceptionGroup => "BaseExceptionGroup",
            Self::GenericAlias => "GenericAlias",
            Self::ModuleType => "ModuleType",
            Self::FunctionType => "FunctionType",
            Self::NoneType => "NoneType",
            Self::SpecialForm => "_SpecialForm",
            Self::TypeVar => "TypeVar",
            Self::TypeAliasType => "TypeAliasType",
            Self::NoDefaultType => "_NoDefaultType",
            Self::ChainMap => "ChainMap",
            Self::Counter => "Counter",
            Self::DefaultDict => "defaultdict",
            Self::Deque => "deque",
            Self::OrderedDict => "OrderedDict",
            // For example, `typing.List` is defined as `List = _Alias()` in typeshed
            Self::StdlibAlias => "_Alias",
            // This is the name the type of `sys.version_info` has in typeshed,
            // which is different to what `type(sys.version_info).__name__` is at runtime.
            // (At runtime, `type(sys.version_info).__name__ == "version_info"`,
            // which is impossible to replicate in the stubs since the sole instance of the class
            // also has that name in the `sys` module.)
            Self::VersionInfo => "_version_info",
        }
    }

    pub fn to_instance(&self, db: &'db dyn Db) -> Type<'db> {
        self.to_class_literal(db).to_instance(db)
    }

    pub fn to_class_literal(self, db: &'db dyn Db) -> Type<'db> {
        known_module_symbol(db, self.canonical_module(db), self.as_str())
            .ignore_possibly_unbound()
            .unwrap_or(Type::unknown())
    }

    pub fn to_subclass_of(self, db: &'db dyn Db) -> Type<'db> {
        self.to_class_literal(db)
            .into_class_literal()
            .map(|ClassLiteralType { class }| SubclassOfType::from(db, class))
            .unwrap_or_else(SubclassOfType::subclass_of_unknown)
    }

    /// Return `true` if this symbol can be resolved to a class definition `class` in typeshed,
    /// *and* `class` is a subclass of `other`.
    pub fn is_subclass_of(self, db: &'db dyn Db, other: Class<'db>) -> bool {
        known_module_symbol(db, self.canonical_module(db), self.as_str())
            .ignore_possibly_unbound()
            .and_then(Type::into_class_literal)
            .is_some_and(|ClassLiteralType { class }| class.is_subclass_of(db, other))
    }

    /// Return the module in which we should look up the definition for this class
    pub(crate) fn canonical_module(self, db: &'db dyn Db) -> KnownModule {
        match self {
            Self::Bool
            | Self::Object
            | Self::Bytes
            | Self::Type
            | Self::Int
            | Self::Float
            | Self::Str
            | Self::List
            | Self::Tuple
            | Self::Set
            | Self::FrozenSet
            | Self::Dict
            | Self::BaseException
            | Self::BaseExceptionGroup
            | Self::Slice => KnownModule::Builtins,
            Self::VersionInfo => KnownModule::Sys,
            Self::GenericAlias | Self::ModuleType | Self::FunctionType => KnownModule::Types,
            Self::NoneType => KnownModule::Typeshed,
            Self::SpecialForm | Self::TypeVar | Self::TypeAliasType | Self::StdlibAlias => {
                KnownModule::Typing
            }
            Self::NoDefaultType => {
                let python_version = Program::get(db).python_version(db);

                // typing_extensions has a 3.13+ re-export for the `typing.NoDefault`
                // singleton, but not for `typing._NoDefaultType`. So we need to switch
                // to `typing._NoDefaultType` for newer versions:
                if python_version >= PythonVersion::PY313 {
                    KnownModule::Typing
                } else {
                    KnownModule::TypingExtensions
                }
            }
            Self::ChainMap
            | Self::Counter
            | Self::DefaultDict
            | Self::Deque
            | Self::OrderedDict => KnownModule::Collections,
        }
    }

    /// Is this class a singleton class?
    ///
    /// A singleton class is a class where it is known that only one instance can ever exist at runtime.
    const fn is_singleton(self) -> bool {
        // TODO there are other singleton types (EllipsisType, NotImplementedType)
        match self {
            Self::NoneType | Self::NoDefaultType | Self::VersionInfo | Self::TypeAliasType => true,
            Self::Bool
            | Self::Object
            | Self::Bytes
            | Self::Tuple
            | Self::Int
            | Self::Float
            | Self::Str
            | Self::Set
            | Self::FrozenSet
            | Self::Dict
            | Self::List
            | Self::Type
            | Self::Slice
            | Self::GenericAlias
            | Self::ModuleType
            | Self::FunctionType
            | Self::SpecialForm
            | Self::ChainMap
            | Self::Counter
            | Self::DefaultDict
            | Self::Deque
            | Self::OrderedDict
            | Self::StdlibAlias
            | Self::BaseException
            | Self::BaseExceptionGroup
            | Self::TypeVar => false,
        }
    }

    pub fn try_from_file_and_name(db: &dyn Db, file: File, class_name: &str) -> Option<Self> {
        // Note: if this becomes hard to maintain (as rust can't ensure at compile time that all
        // variants of `Self` are covered), we might use a macro (in-house or dependency)
        // See: https://stackoverflow.com/q/39070244
        let candidate = match class_name {
            "bool" => Self::Bool,
            "object" => Self::Object,
            "bytes" => Self::Bytes,
            "tuple" => Self::Tuple,
            "type" => Self::Type,
            "int" => Self::Int,
            "float" => Self::Float,
            "str" => Self::Str,
            "set" => Self::Set,
            "frozenset" => Self::FrozenSet,
            "dict" => Self::Dict,
            "list" => Self::List,
            "slice" => Self::Slice,
            "BaseException" => Self::BaseException,
            "BaseExceptionGroup" => Self::BaseExceptionGroup,
            "GenericAlias" => Self::GenericAlias,
            "NoneType" => Self::NoneType,
            "ModuleType" => Self::ModuleType,
            "FunctionType" => Self::FunctionType,
            "TypeAliasType" => Self::TypeAliasType,
            "ChainMap" => Self::ChainMap,
            "Counter" => Self::Counter,
            "defaultdict" => Self::DefaultDict,
            "deque" => Self::Deque,
            "OrderedDict" => Self::OrderedDict,
            "_Alias" => Self::StdlibAlias,
            "_SpecialForm" => Self::SpecialForm,
            "_NoDefaultType" => Self::NoDefaultType,
            "_version_info" => Self::VersionInfo,
            _ => return None,
        };

        candidate
            .check_module(db, file_to_module(db, file)?.known()?)
            .then_some(candidate)
    }

    /// Return `true` if the module of `self` matches `module`
    fn check_module(self, db: &'db dyn Db, module: KnownModule) -> bool {
        match self {
            Self::Bool
            | Self::Object
            | Self::Bytes
            | Self::Type
            | Self::Int
            | Self::Float
            | Self::Str
            | Self::List
            | Self::Tuple
            | Self::Set
            | Self::FrozenSet
            | Self::Dict
            | Self::Slice
            | Self::GenericAlias
            | Self::ChainMap
            | Self::Counter
            | Self::DefaultDict
            | Self::Deque
            | Self::OrderedDict
            | Self::StdlibAlias  // no equivalent class exists in typing_extensions, nor ever will
            | Self::ModuleType
            | Self::VersionInfo
            | Self::BaseException
            | Self::BaseExceptionGroup
            | Self::FunctionType => module == self.canonical_module(db),
            Self::NoneType => matches!(module, KnownModule::Typeshed | KnownModule::Types),
            Self::SpecialForm | Self::TypeVar | Self::TypeAliasType | Self::NoDefaultType => {
                matches!(module, KnownModule::Typing | KnownModule::TypingExtensions)
            }
        }
    }
}

/// Enumeration of specific runtime that are special enough to be considered their own type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, salsa::Update)]
pub enum KnownInstanceType<'db> {
    /// The symbol `typing.Annotated` (which can also be found as `typing_extensions.Annotated`)
    Annotated,
    /// The symbol `typing.Literal` (which can also be found as `typing_extensions.Literal`)
    Literal,
    /// The symbol `typing.LiteralString` (which can also be found as `typing_extensions.LiteralString`)
    LiteralString,
    /// The symbol `typing.Optional` (which can also be found as `typing_extensions.Optional`)
    Optional,
    /// The symbol `typing.Union` (which can also be found as `typing_extensions.Union`)
    Union,
    /// The symbol `typing.NoReturn` (which can also be found as `typing_extensions.NoReturn`)
    NoReturn,
    /// The symbol `typing.Never` available since 3.11 (which can also be found as `typing_extensions.Never`)
    Never,
    /// The symbol `typing.Any` (which can also be found as `typing_extensions.Any`)
    Any,
    /// The symbol `typing.Tuple` (which can also be found as `typing_extensions.Tuple`)
    Tuple,
    /// The symbol `typing.List` (which can also be found as `typing_extensions.List`)
    List,
    /// The symbol `typing.Dict` (which can also be found as `typing_extensions.Dict`)
    Dict,
    /// The symbol `typing.Set` (which can also be found as `typing_extensions.Set`)
    Set,
    /// The symbol `typing.FrozenSet` (which can also be found as `typing_extensions.FrozenSet`)
    FrozenSet,
    /// The symbol `typing.ChainMap` (which can also be found as `typing_extensions.ChainMap`)
    ChainMap,
    /// The symbol `typing.Counter` (which can also be found as `typing_extensions.Counter`)
    Counter,
    /// The symbol `typing.DefaultDict` (which can also be found as `typing_extensions.DefaultDict`)
    DefaultDict,
    /// The symbol `typing.Deque` (which can also be found as `typing_extensions.Deque`)
    Deque,
    /// The symbol `typing.OrderedDict` (which can also be found as `typing_extensions.OrderedDict`)
    OrderedDict,
    /// The symbol `typing.Type` (which can also be found as `typing_extensions.Type`)
    Type,
    /// A single instance of `typing.TypeVar`
    TypeVar(TypeVarInstance<'db>),
    /// A single instance of `typing.TypeAliasType` (PEP 695 type alias)
    TypeAliasType(TypeAliasType<'db>),
    /// The symbol `knot_extensions.Unknown`
    Unknown,
    /// The symbol `knot_extensions.AlwaysTruthy`
    AlwaysTruthy,
    /// The symbol `knot_extensions.AlwaysFalsy`
    AlwaysFalsy,
    /// The symbol `knot_extensions.Not`
    Not,
    /// The symbol `knot_extensions.Intersection`
    Intersection,
    /// The symbol `knot_extensions.TypeOf`
    TypeOf,

    // Various special forms, special aliases and type qualifiers that we don't yet understand
    // (all currently inferred as TODO in most contexts):
    TypingSelf,
    Final,
    ClassVar,
    Callable,
    Concatenate,
    Unpack,
    Required,
    NotRequired,
    TypeAlias,
    TypeGuard,
    TypeIs,
    ReadOnly,
    // TODO: fill this enum out with more special forms, etc.
}

impl<'db> KnownInstanceType<'db> {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Annotated => "Annotated",
            Self::Literal => "Literal",
            Self::LiteralString => "LiteralString",
            Self::Optional => "Optional",
            Self::Union => "Union",
            Self::TypeVar(_) => "TypeVar",
            Self::NoReturn => "NoReturn",
            Self::Never => "Never",
            Self::Any => "Any",
            Self::Tuple => "Tuple",
            Self::Type => "Type",
            Self::TypeAliasType(_) => "TypeAliasType",
            Self::TypingSelf => "Self",
            Self::Final => "Final",
            Self::ClassVar => "ClassVar",
            Self::Callable => "Callable",
            Self::Concatenate => "Concatenate",
            Self::Unpack => "Unpack",
            Self::Required => "Required",
            Self::NotRequired => "NotRequired",
            Self::TypeAlias => "TypeAlias",
            Self::TypeGuard => "TypeGuard",
            Self::TypeIs => "TypeIs",
            Self::List => "List",
            Self::Dict => "Dict",
            Self::DefaultDict => "DefaultDict",
            Self::Set => "Set",
            Self::FrozenSet => "FrozenSet",
            Self::Counter => "Counter",
            Self::Deque => "Deque",
            Self::ChainMap => "ChainMap",
            Self::OrderedDict => "OrderedDict",
            Self::ReadOnly => "ReadOnly",
            Self::Unknown => "Unknown",
            Self::AlwaysTruthy => "AlwaysTruthy",
            Self::AlwaysFalsy => "AlwaysFalsy",
            Self::Not => "Not",
            Self::Intersection => "Intersection",
            Self::TypeOf => "TypeOf",
        }
    }

    /// Evaluate the known instance in boolean context
    pub const fn bool(self) -> Truthiness {
        match self {
            Self::Annotated
            | Self::Literal
            | Self::LiteralString
            | Self::Optional
            | Self::TypeVar(_)
            | Self::Union
            | Self::NoReturn
            | Self::Never
            | Self::Any
            | Self::Tuple
            | Self::Type
            | Self::TypingSelf
            | Self::Final
            | Self::ClassVar
            | Self::Callable
            | Self::Concatenate
            | Self::Unpack
            | Self::Required
            | Self::NotRequired
            | Self::TypeAlias
            | Self::TypeGuard
            | Self::TypeIs
            | Self::List
            | Self::Dict
            | Self::DefaultDict
            | Self::Set
            | Self::FrozenSet
            | Self::Counter
            | Self::Deque
            | Self::ChainMap
            | Self::OrderedDict
            | Self::ReadOnly
            | Self::TypeAliasType(_)
            | Self::Unknown
            | Self::AlwaysTruthy
            | Self::AlwaysFalsy
            | Self::Not
            | Self::Intersection
            | Self::TypeOf => Truthiness::AlwaysTrue,
        }
    }

    /// Return the repr of the symbol at runtime
    pub fn repr(self, db: &'db dyn Db) -> &'db str {
        match self {
            Self::Annotated => "typing.Annotated",
            Self::Literal => "typing.Literal",
            Self::LiteralString => "typing.LiteralString",
            Self::Optional => "typing.Optional",
            Self::Union => "typing.Union",
            Self::NoReturn => "typing.NoReturn",
            Self::Never => "typing.Never",
            Self::Any => "typing.Any",
            Self::Tuple => "typing.Tuple",
            Self::Type => "typing.Type",
            Self::TypingSelf => "typing.Self",
            Self::Final => "typing.Final",
            Self::ClassVar => "typing.ClassVar",
            Self::Callable => "typing.Callable",
            Self::Concatenate => "typing.Concatenate",
            Self::Unpack => "typing.Unpack",
            Self::Required => "typing.Required",
            Self::NotRequired => "typing.NotRequired",
            Self::TypeAlias => "typing.TypeAlias",
            Self::TypeGuard => "typing.TypeGuard",
            Self::TypeIs => "typing.TypeIs",
            Self::List => "typing.List",
            Self::Dict => "typing.Dict",
            Self::DefaultDict => "typing.DefaultDict",
            Self::Set => "typing.Set",
            Self::FrozenSet => "typing.FrozenSet",
            Self::Counter => "typing.Counter",
            Self::Deque => "typing.Deque",
            Self::ChainMap => "typing.ChainMap",
            Self::OrderedDict => "typing.OrderedDict",
            Self::ReadOnly => "typing.ReadOnly",
            Self::TypeVar(typevar) => typevar.name(db),
            Self::TypeAliasType(_) => "typing.TypeAliasType",
            Self::Unknown => "knot_extensions.Unknown",
            Self::AlwaysTruthy => "knot_extensions.AlwaysTruthy",
            Self::AlwaysFalsy => "knot_extensions.AlwaysFalsy",
            Self::Not => "knot_extensions.Not",
            Self::Intersection => "knot_extensions.Intersection",
            Self::TypeOf => "knot_extensions.TypeOf",
        }
    }

    /// Return the [`KnownClass`] which this symbol is an instance of
    pub const fn class(self) -> KnownClass {
        match self {
            Self::Annotated => KnownClass::SpecialForm,
            Self::Literal => KnownClass::SpecialForm,
            Self::LiteralString => KnownClass::SpecialForm,
            Self::Optional => KnownClass::SpecialForm,
            Self::Union => KnownClass::SpecialForm,
            Self::NoReturn => KnownClass::SpecialForm,
            Self::Never => KnownClass::SpecialForm,
            Self::Any => KnownClass::Object,
            Self::Tuple => KnownClass::SpecialForm,
            Self::Type => KnownClass::SpecialForm,
            Self::TypingSelf => KnownClass::SpecialForm,
            Self::Final => KnownClass::SpecialForm,
            Self::ClassVar => KnownClass::SpecialForm,
            Self::Callable => KnownClass::SpecialForm,
            Self::Concatenate => KnownClass::SpecialForm,
            Self::Unpack => KnownClass::SpecialForm,
            Self::Required => KnownClass::SpecialForm,
            Self::NotRequired => KnownClass::SpecialForm,
            Self::TypeAlias => KnownClass::SpecialForm,
            Self::TypeGuard => KnownClass::SpecialForm,
            Self::TypeIs => KnownClass::SpecialForm,
            Self::ReadOnly => KnownClass::SpecialForm,
            Self::List => KnownClass::StdlibAlias,
            Self::Dict => KnownClass::StdlibAlias,
            Self::DefaultDict => KnownClass::StdlibAlias,
            Self::Set => KnownClass::StdlibAlias,
            Self::FrozenSet => KnownClass::StdlibAlias,
            Self::Counter => KnownClass::StdlibAlias,
            Self::Deque => KnownClass::StdlibAlias,
            Self::ChainMap => KnownClass::StdlibAlias,
            Self::OrderedDict => KnownClass::StdlibAlias,
            Self::TypeVar(_) => KnownClass::TypeVar,
            Self::TypeAliasType(_) => KnownClass::TypeAliasType,
            Self::TypeOf => KnownClass::SpecialForm,
            Self::Not => KnownClass::SpecialForm,
            Self::Intersection => KnownClass::SpecialForm,
            Self::Unknown => KnownClass::Object,
            Self::AlwaysTruthy => KnownClass::Object,
            Self::AlwaysFalsy => KnownClass::Object,
        }
    }

    /// Return the instance type which this type is a subtype of.
    ///
    /// For example, the symbol `typing.Literal` is an instance of `typing._SpecialForm`,
    /// so `KnownInstanceType::Literal.instance_fallback(db)`
    /// returns `Type::Instance(InstanceType { class: <typing._SpecialForm> })`.
    pub fn instance_fallback(self, db: &dyn Db) -> Type {
        self.class().to_instance(db)
    }

    /// Return `true` if this symbol is an instance of `class`.
    pub fn is_instance_of(self, db: &'db dyn Db, class: Class<'db>) -> bool {
        self.class().is_subclass_of(db, class)
    }

    pub fn try_from_file_and_name(db: &'db dyn Db, file: File, symbol_name: &str) -> Option<Self> {
        let candidate = match symbol_name {
            "Any" => Self::Any,
            "ClassVar" => Self::ClassVar,
            "Deque" => Self::Deque,
            "List" => Self::List,
            "Dict" => Self::Dict,
            "DefaultDict" => Self::DefaultDict,
            "Set" => Self::Set,
            "FrozenSet" => Self::FrozenSet,
            "Counter" => Self::Counter,
            "ChainMap" => Self::ChainMap,
            "OrderedDict" => Self::OrderedDict,
            "Optional" => Self::Optional,
            "Union" => Self::Union,
            "NoReturn" => Self::NoReturn,
            "Tuple" => Self::Tuple,
            "Type" => Self::Type,
            "Callable" => Self::Callable,
            "Annotated" => Self::Annotated,
            "Literal" => Self::Literal,
            "Never" => Self::Never,
            "Self" => Self::TypingSelf,
            "Final" => Self::Final,
            "Unpack" => Self::Unpack,
            "Required" => Self::Required,
            "TypeAlias" => Self::TypeAlias,
            "TypeGuard" => Self::TypeGuard,
            "TypeIs" => Self::TypeIs,
            "ReadOnly" => Self::ReadOnly,
            "Concatenate" => Self::Concatenate,
            "NotRequired" => Self::NotRequired,
            "LiteralString" => Self::LiteralString,
            "Unknown" => Self::Unknown,
            "AlwaysTruthy" => Self::AlwaysTruthy,
            "AlwaysFalsy" => Self::AlwaysFalsy,
            "Not" => Self::Not,
            "Intersection" => Self::Intersection,
            "TypeOf" => Self::TypeOf,
            _ => return None,
        };

        candidate
            .check_module(file_to_module(db, file)?.known()?)
            .then_some(candidate)
    }

    /// Return `true` if `module` is a module from which this `KnownInstance` variant can validly originate.
    ///
    /// Most variants can only exist in one module, which is the same as `self.class().canonical_module()`.
    /// Some variants could validly be defined in either `typing` or `typing_extensions`, however.
    pub fn check_module(self, module: KnownModule) -> bool {
        match self {
            Self::Any
            | Self::ClassVar
            | Self::Deque
            | Self::List
            | Self::Dict
            | Self::DefaultDict
            | Self::Set
            | Self::FrozenSet
            | Self::Counter
            | Self::ChainMap
            | Self::OrderedDict
            | Self::Optional
            | Self::Union
            | Self::NoReturn
            | Self::Tuple
            | Self::Type
            | Self::Callable => module.is_typing(),
            Self::Annotated
            | Self::Literal
            | Self::LiteralString
            | Self::Never
            | Self::TypingSelf
            | Self::Final
            | Self::Concatenate
            | Self::Unpack
            | Self::Required
            | Self::NotRequired
            | Self::TypeAlias
            | Self::TypeGuard
            | Self::TypeIs
            | Self::ReadOnly
            | Self::TypeAliasType(_)
            | Self::TypeVar(_) => {
                matches!(module, KnownModule::Typing | KnownModule::TypingExtensions)
            }
            Self::Unknown
            | Self::AlwaysTruthy
            | Self::AlwaysFalsy
            | Self::Not
            | Self::Intersection
            | Self::TypeOf => module.is_knot_extensions(),
        }
    }

    fn member(self, db: &'db dyn Db, name: &str) -> Symbol<'db> {
        let ty = match (self, name) {
            (Self::TypeVar(typevar), "__name__") => Type::string_literal(db, typevar.name(db)),
            (Self::TypeAliasType(alias), "__name__") => Type::string_literal(db, alias.name(db)),
            _ => return self.instance_fallback(db).member(db, name),
        };
        ty.into()
    }
}

/// Data regarding a single type variable.
///
/// This is referenced by `KnownInstanceType::TypeVar` (to represent the singleton type of the
/// runtime `typing.TypeVar` object itself). In the future, it will also be referenced also by a
/// new `Type` variant to represent the type that this typevar represents as an annotation: that
/// is, an unknown set of objects, constrained by the upper-bound/constraints on this type var,
/// defaulting to the default type of this type var when not otherwise bound to a type.
///
/// This must be a tracked struct, not an interned one, because typevar equivalence is by identity,
/// not by value. Two typevars that have the same name, bound/constraints, and default, are still
/// different typevars: if used in the same scope, they may be bound to different types.
#[salsa::tracked]
pub struct TypeVarInstance<'db> {
    /// The name of this TypeVar (e.g. `T`)
    #[return_ref]
    name: ast::name::Name,

    /// The upper bound or constraint on the type of this TypeVar
    bound_or_constraints: Option<TypeVarBoundOrConstraints<'db>>,

    /// The default type for this TypeVar
    default_ty: Option<Type<'db>>,
}

impl<'db> TypeVarInstance<'db> {
    #[allow(unused)]
    pub(crate) fn upper_bound(self, db: &'db dyn Db) -> Option<Type<'db>> {
        if let Some(TypeVarBoundOrConstraints::UpperBound(ty)) = self.bound_or_constraints(db) {
            Some(ty)
        } else {
            None
        }
    }

    #[allow(unused)]
    pub(crate) fn constraints(self, db: &'db dyn Db) -> Option<&'db [Type<'db>]> {
        if let Some(TypeVarBoundOrConstraints::Constraints(tuple)) = self.bound_or_constraints(db) {
            Some(tuple.elements(db))
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, salsa::Update)]
pub enum TypeVarBoundOrConstraints<'db> {
    UpperBound(Type<'db>),
    Constraints(TupleType<'db>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IterationOutcome<'db> {
    Iterable {
        element_ty: Type<'db>,
    },
    NotIterable {
        not_iterable_ty: Type<'db>,
    },
    PossiblyUnboundDunderIter {
        iterable_ty: Type<'db>,
        element_ty: Type<'db>,
    },
}

impl<'db> IterationOutcome<'db> {
    fn unwrap_with_diagnostic(
        self,
        context: &InferContext<'db>,
        iterable_node: ast::AnyNodeRef,
    ) -> Type<'db> {
        match self {
            Self::Iterable { element_ty } => element_ty,
            Self::NotIterable { not_iterable_ty } => {
                report_not_iterable(context, iterable_node, not_iterable_ty);
                Type::unknown()
            }
            Self::PossiblyUnboundDunderIter {
                iterable_ty,
                element_ty,
            } => {
                report_not_iterable_possibly_unbound(context, iterable_node, iterable_ty);
                element_ty
            }
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Truthiness {
    /// For an object `x`, `bool(x)` will always return `True`
    AlwaysTrue,
    /// For an object `x`, `bool(x)` will always return `False`
    AlwaysFalse,
    /// For an object `x`, `bool(x)` could return either `True` or `False`
    Ambiguous,
}

impl Truthiness {
    pub(crate) const fn is_ambiguous(self) -> bool {
        matches!(self, Truthiness::Ambiguous)
    }

    pub(crate) const fn is_always_false(self) -> bool {
        matches!(self, Truthiness::AlwaysFalse)
    }

    pub(crate) const fn is_always_true(self) -> bool {
        matches!(self, Truthiness::AlwaysTrue)
    }

    pub(crate) const fn negate(self) -> Self {
        match self {
            Self::AlwaysTrue => Self::AlwaysFalse,
            Self::AlwaysFalse => Self::AlwaysTrue,
            Self::Ambiguous => Self::Ambiguous,
        }
    }

    pub(crate) const fn negate_if(self, condition: bool) -> Self {
        if condition {
            self.negate()
        } else {
            self
        }
    }

    fn into_type(self, db: &dyn Db) -> Type {
        match self {
            Self::AlwaysTrue => Type::BooleanLiteral(true),
            Self::AlwaysFalse => Type::BooleanLiteral(false),
            Self::Ambiguous => KnownClass::Bool.to_instance(db),
        }
    }
}

impl From<bool> for Truthiness {
    fn from(value: bool) -> Self {
        if value {
            Truthiness::AlwaysTrue
        } else {
            Truthiness::AlwaysFalse
        }
    }
}

#[salsa::interned]
pub struct FunctionType<'db> {
    /// name of the function at definition
    #[return_ref]
    pub name: ast::name::Name,

    /// Is this a function that we special-case somehow? If so, which one?
    known: Option<KnownFunction>,

    body_scope: ScopeId<'db>,

    /// types of all decorators on this function
    decorators: Box<[Type<'db>]>,
}

#[salsa::tracked]
impl<'db> FunctionType<'db> {
    pub fn has_decorator(self, db: &dyn Db, decorator: Type<'_>) -> bool {
        self.decorators(db).contains(&decorator)
    }

    /// Typed externally-visible signature for this function.
    ///
    /// This is the signature as seen by external callers, possibly modified by decorators and/or
    /// overloaded.
    ///
    /// ## Why is this a salsa query?
    ///
    /// This is a salsa query to short-circuit the invalidation
    /// when the function's AST node changes.
    ///
    /// Were this not a salsa query, then the calling query
    /// would depend on the function's AST and rerun for every change in that file.
    #[salsa::tracked(return_ref)]
    pub fn signature(self, db: &'db dyn Db) -> Signature<'db> {
        let function_stmt_node = self.body_scope(db).node(db).expect_function();
        let internal_signature = self.internal_signature(db);
        if function_stmt_node.decorator_list.is_empty() {
            return internal_signature;
        }
        // TODO process the effect of decorators on the signature
        Signature::todo()
    }

    /// Typed internally-visible signature for this function.
    ///
    /// This represents the annotations on the function itself, unmodified by decorators and
    /// overloads.
    ///
    /// These are the parameter and return types that should be used for type checking the body of
    /// the function.
    ///
    /// Don't call this when checking any other file; only when type-checking the function body
    /// scope.
    fn internal_signature(self, db: &'db dyn Db) -> Signature<'db> {
        let scope = self.body_scope(db);
        let function_stmt_node = scope.node(db).expect_function();
        let definition = semantic_index(db, scope.file(db)).definition(function_stmt_node);
        Signature::from_function(db, definition, function_stmt_node)
    }

    pub fn is_known(self, db: &'db dyn Db, known_function: KnownFunction) -> bool {
        self.known(db) == Some(known_function)
    }
}

/// Non-exhaustive enumeration of known functions (e.g. `builtins.reveal_type`, ...) that might
/// have special behavior.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum KnownFunction {
    ConstraintFunction(KnownConstraintFunction),
    /// `builtins.reveal_type`, `typing.reveal_type` or `typing_extensions.reveal_type`
    RevealType,
    /// `builtins.len`
    Len,
    /// `typing(_extensions).final`
    Final,

    /// [`typing(_extensions).no_type_check`](https://typing.readthedocs.io/en/latest/spec/directives.html#no-type-check)
    NoTypeCheck,

    /// `typing(_extensions).assert_type`
    AssertType,
    /// `typing(_extensions).cast`
    Cast,

    /// `knot_extensions.static_assert`
    StaticAssert,
    /// `knot_extensions.is_equivalent_to`
    IsEquivalentTo,
    /// `knot_extensions.is_subtype_of`
    IsSubtypeOf,
    /// `knot_extensions.is_assignable_to`
    IsAssignableTo,
    /// `knot_extensions.is_disjoint_from`
    IsDisjointFrom,
    /// `knot_extensions.is_fully_static`
    IsFullyStatic,
    /// `knot_extensions.is_singleton`
    IsSingleton,
    /// `knot_extensions.is_single_valued`
    IsSingleValued,
}

impl KnownFunction {
    pub fn constraint_function(self) -> Option<KnownConstraintFunction> {
        match self {
            Self::ConstraintFunction(f) => Some(f),
            _ => None,
        }
    }

    fn try_from_definition_and_name<'db>(
        db: &'db dyn Db,
        definition: Definition<'db>,
        name: &str,
    ) -> Option<Self> {
        let candidate = match name {
            "isinstance" => Self::ConstraintFunction(KnownConstraintFunction::IsInstance),
            "issubclass" => Self::ConstraintFunction(KnownConstraintFunction::IsSubclass),
            "reveal_type" => Self::RevealType,
            "len" => Self::Len,
            "final" => Self::Final,
            "no_type_check" => Self::NoTypeCheck,
            "assert_type" => Self::AssertType,
            "cast" => Self::Cast,
            "static_assert" => Self::StaticAssert,
            "is_subtype_of" => Self::IsSubtypeOf,
            "is_disjoint_from" => Self::IsDisjointFrom,
            "is_equivalent_to" => Self::IsEquivalentTo,
            "is_assignable_to" => Self::IsAssignableTo,
            "is_fully_static" => Self::IsFullyStatic,
            "is_singleton" => Self::IsSingleton,
            "is_single_valued" => Self::IsSingleValued,
            _ => return None,
        };

        candidate
            .check_module(file_to_module(db, definition.file(db))?.known()?)
            .then_some(candidate)
    }

    /// Return `true` if `self` is defined in `module` at runtime.
    const fn check_module(self, module: KnownModule) -> bool {
        match self {
            Self::ConstraintFunction(constraint_function) => match constraint_function {
                KnownConstraintFunction::IsInstance | KnownConstraintFunction::IsSubclass => {
                    module.is_builtins()
                }
            },
            Self::Len => module.is_builtins(),
            Self::AssertType | Self::Cast | Self::RevealType | Self::Final | Self::NoTypeCheck => {
                matches!(module, KnownModule::Typing | KnownModule::TypingExtensions)
            }
            Self::IsAssignableTo
            | Self::IsDisjointFrom
            | Self::IsEquivalentTo
            | Self::IsFullyStatic
            | Self::IsSingleValued
            | Self::IsSingleton
            | Self::IsSubtypeOf
            | Self::StaticAssert => module.is_knot_extensions(),
        }
    }

    /// Return the [`ParameterExpectations`] for this function.
    const fn parameter_expectations(self) -> ParameterExpectations {
        match self {
            Self::IsFullyStatic | Self::IsSingleton | Self::IsSingleValued => {
                ParameterExpectations::SingleTypeExpression
            }

            Self::IsEquivalentTo
            | Self::IsSubtypeOf
            | Self::IsAssignableTo
            | Self::IsDisjointFrom => ParameterExpectations::TwoTypeExpressions,

            Self::AssertType => ParameterExpectations::ValueExpressionAndTypeExpression,
            Self::Cast => ParameterExpectations::TypeExpressionAndValueExpression,

            Self::ConstraintFunction(_)
            | Self::Len
            | Self::Final
            | Self::NoTypeCheck
            | Self::RevealType
            | Self::StaticAssert => ParameterExpectations::AllValueExpressions,
        }
    }
}

/// Describes whether the parameters in a function expect value expressions or type expressions.
///
/// Whether a specific parameter in the function expects a type expression can be queried
/// using [`ParameterExpectations::expectation_at_index`].
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
enum ParameterExpectations {
    /// All parameters in the function expect value expressions
    #[default]
    AllValueExpressions,
    /// The first parameter in the function expects a type expression
    SingleTypeExpression,
    /// The first two parameters in the function expect type expressions
    TwoTypeExpressions,
    /// The first parameter in the function expects a value expression,
    /// and the second expects a type expression
    ValueExpressionAndTypeExpression,
    /// The first parameter in the function expects a type expression,
    /// and the second expects a value expression
    TypeExpressionAndValueExpression,
}

impl ParameterExpectations {
    /// Query whether the parameter at `parameter_index` expects a value expression or a type expression
    fn expectation_at_index(self, parameter_index: usize) -> ParameterExpectation {
        match self {
            Self::AllValueExpressions => ParameterExpectation::ValueExpression,
            Self::SingleTypeExpression | Self::TypeExpressionAndValueExpression => {
                if parameter_index == 0 {
                    ParameterExpectation::TypeExpression
                } else {
                    ParameterExpectation::ValueExpression
                }
            }
            Self::TwoTypeExpressions => {
                if parameter_index < 2 {
                    ParameterExpectation::TypeExpression
                } else {
                    ParameterExpectation::ValueExpression
                }
            }
            Self::ValueExpressionAndTypeExpression => {
                if parameter_index == 1 {
                    ParameterExpectation::TypeExpression
                } else {
                    ParameterExpectation::ValueExpression
                }
            }
        }
    }
}

/// Whether a single parameter in a given function expects a value expression or a [type expression]
///
/// [type expression]: https://typing.readthedocs.io/en/latest/spec/annotations.html#type-and-annotation-expressions
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
enum ParameterExpectation {
    /// The parameter expects a value expression
    #[default]
    ValueExpression,
    /// The parameter expects a type expression
    TypeExpression,
}

#[salsa::interned]
pub struct ModuleLiteralType<'db> {
    /// The file in which this module was imported.
    ///
    /// We need this in order to know which submodules should be attached to it as attributes
    /// (because the submodules were also imported in this file).
    pub importing_file: File,

    /// The imported module.
    pub module: Module,
}

impl<'db> ModuleLiteralType<'db> {
    fn member(self, db: &'db dyn Db, name: &str) -> Symbol<'db> {
        // `__dict__` is a very special member that is never overridden by module globals;
        // we should always look it up directly as an attribute on `types.ModuleType`,
        // never in the global scope of the module.
        if name == "__dict__" {
            return KnownClass::ModuleType
                .to_instance(db)
                .member(db, "__dict__");
        }

        // If the file that originally imported the module has also imported a submodule
        // named `name`, then the result is (usually) that submodule, even if the module
        // also defines a (non-module) symbol with that name.
        //
        // Note that technically, either the submodule or the non-module symbol could take
        // priority, depending on the ordering of when the submodule is loaded relative to
        // the parent module's `__init__.py` file being evaluated. That said, we have
        // chosen to always have the submodule take priority. (This matches pyright's
        // current behavior, but is the opposite of mypy's current behavior.)
        if let Some(submodule_name) = ModuleName::new(name) {
            let importing_file = self.importing_file(db);
            let imported_submodules = imported_modules(db, importing_file);
            let mut full_submodule_name = self.module(db).name().clone();
            full_submodule_name.extend(&submodule_name);
            if imported_submodules.contains(&full_submodule_name) {
                if let Some(submodule) = resolve_module(db, &full_submodule_name) {
                    let submodule_ty = Type::module_literal(db, importing_file, submodule);
                    return Symbol::Type(submodule_ty, Boundness::Bound);
                }
            }
        }

        let global_lookup = symbol(db, global_scope(db, self.module(db).file()), name);

        // If it's unbound, check if it's present as an instance on `types.ModuleType`
        // or `builtins.object`.
        //
        // We do a more limited version of this in `global_symbol_ty`,
        // but there are two crucial differences here:
        // - If a member is looked up as an attribute, `__init__` is also available
        //   on the module, but it isn't available as a global from inside the module
        // - If a member is looked up as an attribute, members on `builtins.object`
        //   are also available (because `types.ModuleType` inherits from `object`);
        //   these attributes are also not available as globals from inside the module.
        //
        // The same way as in `global_symbol_ty`, however, we need to be careful to
        // ignore `__getattr__`. Typeshed has a fake `__getattr__` on `types.ModuleType`
        // to help out with dynamic imports; we shouldn't use it for `ModuleLiteral` types
        // where we know exactly which module we're dealing with.
        if name != "__getattr__" && global_lookup.possibly_unbound() {
            // TODO: this should use `.to_instance()`, but we don't understand instance attribute yet
            let module_type_instance_member =
                KnownClass::ModuleType.to_class_literal(db).member(db, name);
            global_lookup.or_fall_back_to(db, &module_type_instance_member)
        } else {
            global_lookup
        }
    }
}

/// Representation of a runtime class object.
///
/// Does not in itself represent a type,
/// but is used as the inner data for several structs that *do* represent types.
#[salsa::interned]
pub struct Class<'db> {
    /// Name of the class at definition
    #[return_ref]
    pub name: ast::name::Name,

    body_scope: ScopeId<'db>,

    known: Option<KnownClass>,
}

#[salsa::tracked]
impl<'db> Class<'db> {
    /// Return `true` if this class represents `known_class`
    pub fn is_known(self, db: &'db dyn Db, known_class: KnownClass) -> bool {
        self.known(db) == Some(known_class)
    }

    /// Return an iterator over the inferred types of this class's *explicit* bases.
    ///
    /// Note that any class (except for `object`) that has no explicit
    /// bases will implicitly inherit from `object` at runtime. Nonetheless,
    /// this method does *not* include `object` in the bases it iterates over.
    ///
    /// ## Why is this a salsa query?
    ///
    /// This is a salsa query to short-circuit the invalidation
    /// when the class's AST node changes.
    ///
    /// Were this not a salsa query, then the calling query
    /// would depend on the class's AST and rerun for every change in that file.
    fn explicit_bases(self, db: &'db dyn Db) -> &'db [Type<'db>] {
        self.explicit_bases_query(db)
    }

    /// Iterate over this class's explicit bases, filtering out any bases that are not class objects.
    fn fully_static_explicit_bases(self, db: &'db dyn Db) -> impl Iterator<Item = Class<'db>> {
        self.explicit_bases(db)
            .iter()
            .copied()
            .filter_map(Type::into_class_literal)
            .map(|ClassLiteralType { class }| class)
    }

    #[salsa::tracked(return_ref)]
    fn explicit_bases_query(self, db: &'db dyn Db) -> Box<[Type<'db>]> {
        let class_stmt = self.node(db);

        let class_definition = semantic_index(db, self.file(db)).definition(class_stmt);

        class_stmt
            .bases()
            .iter()
            .map(|base_node| definition_expression_ty(db, class_definition, base_node))
            .collect()
    }

    fn file(self, db: &dyn Db) -> File {
        self.body_scope(db).file(db)
    }

    /// Return the original [`ast::StmtClassDef`] node associated with this class
    ///
    /// ## Note
    /// Only call this function from queries in the same file or your
    /// query depends on the AST of another file (bad!).
    fn node(self, db: &'db dyn Db) -> &'db ast::StmtClassDef {
        self.body_scope(db).node(db).expect_class()
    }

    /// Return the types of the decorators on this class
    #[salsa::tracked(return_ref)]
    fn decorators(self, db: &'db dyn Db) -> Box<[Type<'db>]> {
        let class_stmt = self.node(db);
        if class_stmt.decorator_list.is_empty() {
            return Box::new([]);
        }
        let class_definition = semantic_index(db, self.file(db)).definition(class_stmt);
        class_stmt
            .decorator_list
            .iter()
            .map(|decorator_node| {
                definition_expression_ty(db, class_definition, &decorator_node.expression)
            })
            .collect()
    }

    /// Is this class final?
    fn is_final(self, db: &'db dyn Db) -> bool {
        self.decorators(db)
            .iter()
            .filter_map(|deco| deco.into_function_literal())
            .any(|decorator| decorator.is_known(db, KnownFunction::Final))
    }

    /// Attempt to resolve the [method resolution order] ("MRO") for this class.
    /// If the MRO is unresolvable, return an error indicating why the class's MRO
    /// cannot be accurately determined. The error returned contains a fallback MRO
    /// that will be used instead for the purposes of type inference.
    ///
    /// The MRO is the tuple of classes that can be retrieved as the `__mro__`
    /// attribute on a class at runtime.
    ///
    /// [method resolution order]: https://docs.python.org/3/glossary.html#term-method-resolution-order
    #[salsa::tracked(return_ref)]
    fn try_mro(self, db: &'db dyn Db) -> Result<Mro<'db>, MroError<'db>> {
        Mro::of_class(db, self)
    }

    /// Iterate over the [method resolution order] ("MRO") of the class.
    ///
    /// If the MRO could not be accurately resolved, this method falls back to iterating
    /// over an MRO that has the class directly inheriting from `Unknown`. Use
    /// [`Class::try_mro`] if you need to distinguish between the success and failure
    /// cases rather than simply iterating over the inferred resolution order for the class.
    ///
    /// [method resolution order]: https://docs.python.org/3/glossary.html#term-method-resolution-order
    fn iter_mro(self, db: &'db dyn Db) -> impl Iterator<Item = ClassBase<'db>> {
        MroIterator::new(db, self)
    }

    /// Return `true` if `other` is present in this class's MRO.
    pub fn is_subclass_of(self, db: &'db dyn Db, other: Class) -> bool {
        // `is_subclass_of` is checking the subtype relation, in which gradual types do not
        // participate, so we should not return `True` if we find `Any/Unknown` in the MRO.
        self.iter_mro(db).contains(&ClassBase::Class(other))
    }

    /// Return `true` if this class object is an instance of the class `other`.
    ///
    /// A class is an instance of its metaclass; consequently,
    /// a class will only ever be an instance of another class
    /// if its metaclass is a subclass of that other class.
    fn is_instance_of(self, db: &'db dyn Db, other: Class<'db>) -> bool {
        self.metaclass(db).into_class_literal().is_some_and(
            |ClassLiteralType { class: metaclass }| metaclass.is_subclass_of(db, other),
        )
    }

    /// Return the explicit `metaclass` of this class, if one is defined.
    ///
    /// ## Note
    /// Only call this function from queries in the same file or your
    /// query depends on the AST of another file (bad!).
    fn explicit_metaclass(self, db: &'db dyn Db) -> Option<Type<'db>> {
        let class_stmt = self.node(db);
        let metaclass_node = &class_stmt
            .arguments
            .as_ref()?
            .find_keyword("metaclass")?
            .value;
        let class_definition = semantic_index(db, self.file(db)).definition(class_stmt);
        let metaclass_ty = definition_expression_ty(db, class_definition, metaclass_node);
        Some(metaclass_ty)
    }

    /// Return the metaclass of this class, or `type[Unknown]` if the metaclass cannot be inferred.
    pub(crate) fn metaclass(self, db: &'db dyn Db) -> Type<'db> {
        self.try_metaclass(db)
            .unwrap_or_else(|_| SubclassOfType::subclass_of_unknown())
    }

    /// Return the metaclass of this class, or an error if the metaclass cannot be inferred.
    #[salsa::tracked]
    pub(crate) fn try_metaclass(self, db: &'db dyn Db) -> Result<Type<'db>, MetaclassError<'db>> {
        // Identify the class's own metaclass (or take the first base class's metaclass).
        let mut base_classes = self.fully_static_explicit_bases(db).peekable();

        if base_classes.peek().is_some() && self.is_cyclically_defined(db) {
            // We emit diagnostics for cyclic class definitions elsewhere.
            // Avoid attempting to infer the metaclass if the class is cyclically defined:
            // it would be easy to enter an infinite loop.
            return Ok(SubclassOfType::subclass_of_unknown());
        }

        let explicit_metaclass = self.explicit_metaclass(db);
        let (metaclass, class_metaclass_was_from) = if let Some(metaclass) = explicit_metaclass {
            (metaclass, self)
        } else if let Some(base_class) = base_classes.next() {
            (base_class.metaclass(db), base_class)
        } else {
            (KnownClass::Type.to_class_literal(db), self)
        };

        let mut candidate = if let Type::ClassLiteral(metaclass_ty) = metaclass {
            MetaclassCandidate {
                metaclass: metaclass_ty.class,
                explicit_metaclass_of: class_metaclass_was_from,
            }
        } else {
            let name = Type::string_literal(db, self.name(db));
            let bases = TupleType::from_elements(db, self.explicit_bases(db));
            // TODO: Should be `dict[str, Any]`
            let namespace = KnownClass::Dict.to_instance(db);

            // TODO: Other keyword arguments?
            let arguments = CallArguments::positional([name, bases, namespace]);

            let return_ty_result = match metaclass.call(db, &arguments) {
                CallOutcome::NotCallable { not_callable_ty } => Err(MetaclassError {
                    kind: MetaclassErrorKind::NotCallable(not_callable_ty),
                }),

                CallOutcome::Union {
                    outcomes,
                    called_ty,
                } => {
                    let mut partly_not_callable = false;

                    let return_ty = outcomes
                        .iter()
                        .fold(None, |acc, outcome| {
                            let ty = outcome.return_ty(db);

                            match (acc, ty) {
                                (acc, None) => {
                                    partly_not_callable = true;
                                    acc
                                }
                                (None, Some(ty)) => Some(UnionBuilder::new(db).add(ty)),
                                (Some(builder), Some(ty)) => Some(builder.add(ty)),
                            }
                        })
                        .map(UnionBuilder::build);

                    if partly_not_callable {
                        Err(MetaclassError {
                            kind: MetaclassErrorKind::PartlyNotCallable(called_ty),
                        })
                    } else {
                        Ok(return_ty.unwrap_or(Type::unknown()))
                    }
                }

                CallOutcome::PossiblyUnboundDunderCall { called_ty, .. } => Err(MetaclassError {
                    kind: MetaclassErrorKind::PartlyNotCallable(called_ty),
                }),

                // TODO we should also check for binding errors that would indicate the metaclass
                // does not accept the right arguments
                CallOutcome::Callable { binding }
                | CallOutcome::RevealType { binding, .. }
                | CallOutcome::StaticAssertionError { binding, .. }
                | CallOutcome::AssertType { binding, .. } => Ok(binding.return_ty()),
            };

            return return_ty_result.map(|ty| ty.to_meta_type(db));
        };

        // Reconcile all base classes' metaclasses with the candidate metaclass.
        //
        // See:
        // - https://docs.python.org/3/reference/datamodel.html#determining-the-appropriate-metaclass
        // - https://github.com/python/cpython/blob/83ba8c2bba834c0b92de669cac16fcda17485e0e/Objects/typeobject.c#L3629-L3663
        for base_class in base_classes {
            let metaclass = base_class.metaclass(db);
            let Type::ClassLiteral(metaclass) = metaclass else {
                continue;
            };
            if metaclass.class.is_subclass_of(db, candidate.metaclass) {
                candidate = MetaclassCandidate {
                    metaclass: metaclass.class,
                    explicit_metaclass_of: base_class,
                };
                continue;
            }
            if candidate.metaclass.is_subclass_of(db, metaclass.class) {
                continue;
            }
            return Err(MetaclassError {
                kind: MetaclassErrorKind::Conflict {
                    candidate1: candidate,
                    candidate2: MetaclassCandidate {
                        metaclass: metaclass.class,
                        explicit_metaclass_of: base_class,
                    },
                    candidate1_is_base_class: explicit_metaclass.is_none(),
                },
            });
        }

        Ok(Type::class_literal(candidate.metaclass))
    }

    /// Returns the class member of this class named `name`.
    ///
    /// The member resolves to a member on the class itself or any of its proper superclasses.
    pub(crate) fn class_member(self, db: &'db dyn Db, name: &str) -> Symbol<'db> {
        if name == "__mro__" {
            let tuple_elements = self.iter_mro(db).map(Type::from);
            return TupleType::from_elements(db, tuple_elements).into();
        }

        for superclass in self.iter_mro(db) {
            match superclass {
                // TODO we may instead want to record the fact that we encountered dynamic, and intersect it with
                // the type found on the next "real" class.
                ClassBase::Dynamic(_) => return Type::from(superclass).member(db, name),
                ClassBase::Class(class) => {
                    let member = class.own_class_member(db, name);
                    if !member.is_unbound() {
                        return member;
                    }
                }
            }
        }

        Symbol::Unbound
    }

    /// Returns the inferred type of the class member named `name`.
    ///
    /// Returns [`Symbol::Unbound`] if `name` cannot be found in this class's scope
    /// directly. Use [`Class::class_member`] if you require a method that will
    /// traverse through the MRO until it finds the member.
    pub(crate) fn own_class_member(self, db: &'db dyn Db, name: &str) -> Symbol<'db> {
        let scope = self.body_scope(db);
        symbol(db, scope, name)
    }

    /// Return `true` if this class appears to be a cyclic definition,
    /// i.e., it inherits either directly or indirectly from itself.
    ///
    /// A class definition like this will fail at runtime,
    /// but we must be resilient to it or we could panic.
    #[salsa::tracked]
    fn is_cyclically_defined(self, db: &'db dyn Db) -> bool {
        fn is_cyclically_defined_recursive<'db>(
            db: &'db dyn Db,
            class: Class<'db>,
            classes_to_watch: &mut IndexSet<Class<'db>>,
        ) -> bool {
            if !classes_to_watch.insert(class) {
                return true;
            }
            for explicit_base_class in class.fully_static_explicit_bases(db) {
                // Each base must be considered in isolation.
                // This is due to the fact that if a class uses multiple inheritance,
                // there could easily be a situation where two bases have the same class in their MROs;
                // that isn't enough to constitute the class being cyclically defined.
                let classes_to_watch_len = classes_to_watch.len();
                if is_cyclically_defined_recursive(db, explicit_base_class, classes_to_watch) {
                    return true;
                }
                classes_to_watch.truncate(classes_to_watch_len);
            }
            false
        }

        self.fully_static_explicit_bases(db)
            .any(|base_class| is_cyclically_defined_recursive(db, base_class, &mut IndexSet::new()))
    }
}

#[salsa::interned]
pub struct TypeAliasType<'db> {
    #[return_ref]
    pub name: ast::name::Name,

    rhs_scope: ScopeId<'db>,
}

#[salsa::tracked]
impl<'db> TypeAliasType<'db> {
    #[salsa::tracked]
    pub fn value_ty(self, db: &'db dyn Db) -> Type<'db> {
        let scope = self.rhs_scope(db);

        let type_alias_stmt_node = scope.node(db).expect_type_alias();
        let definition = semantic_index(db, scope.file(db)).definition(type_alias_stmt_node);

        definition_expression_ty(db, definition, &type_alias_stmt_node.value)
    }
}

/// Either the explicit `metaclass=` keyword of the class, or the inferred metaclass of one of its base classes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MetaclassCandidate<'db> {
    metaclass: Class<'db>,
    explicit_metaclass_of: Class<'db>,
}

/// A singleton type representing a single class object at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, salsa::Update)]
pub struct ClassLiteralType<'db> {
    class: Class<'db>,
}

impl<'db> ClassLiteralType<'db> {
    fn member(self, db: &'db dyn Db, name: &str) -> Symbol<'db> {
        self.class.class_member(db, name)
    }
}

impl<'db> From<ClassLiteralType<'db>> for Type<'db> {
    fn from(value: ClassLiteralType<'db>) -> Self {
        Self::ClassLiteral(value)
    }
}

/// A type representing the set of runtime objects which are instances of a certain class.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, salsa::Update)]
pub struct InstanceType<'db> {
    class: Class<'db>,
}

impl<'db> InstanceType<'db> {
    fn is_subtype_of(self, db: &'db dyn Db, other: InstanceType<'db>) -> bool {
        // N.B. The subclass relation is fully static
        self.class.is_subclass_of(db, other.class)
    }
}

impl<'db> From<InstanceType<'db>> for Type<'db> {
    fn from(value: InstanceType<'db>) -> Self {
        Self::Instance(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MetaclassError<'db> {
    kind: MetaclassErrorKind<'db>,
}

impl<'db> MetaclassError<'db> {
    /// Return an [`MetaclassErrorKind`] variant describing why we could not resolve the metaclass for this class.
    pub(super) fn reason(&self) -> &MetaclassErrorKind<'db> {
        &self.kind
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum MetaclassErrorKind<'db> {
    /// The class has incompatible metaclasses in its inheritance hierarchy.
    ///
    /// The metaclass of a derived class must be a (non-strict) subclass of the metaclasses of all
    /// its bases.
    Conflict {
        /// `candidate1` will either be the explicit `metaclass=` keyword in the class definition,
        /// or the inferred metaclass of a base class
        candidate1: MetaclassCandidate<'db>,

        /// `candidate2` will always be the inferred metaclass of a base class
        candidate2: MetaclassCandidate<'db>,

        /// Flag to indicate whether `candidate1` is the explicit `metaclass=` keyword or the
        /// inferred metaclass of a base class. This helps us give better error messages in diagnostics.
        candidate1_is_base_class: bool,
    },
    /// The metaclass is not callable
    NotCallable(Type<'db>),
    /// The metaclass is of a union type whose some members are not callable
    PartlyNotCallable(Type<'db>),
}

#[salsa::interned]
pub struct UnionType<'db> {
    /// The union type includes values in any of these types.
    #[return_ref]
    elements_boxed: Box<[Type<'db>]>,
}

impl<'db> UnionType<'db> {
    fn elements(self, db: &'db dyn Db) -> &'db [Type<'db>] {
        self.elements_boxed(db)
    }

    /// Create a union from a list of elements
    /// (which may be eagerly simplified into a different variant of [`Type`] altogether).
    pub fn from_elements<T: Into<Type<'db>>>(
        db: &'db dyn Db,
        elements: impl IntoIterator<Item = T>,
    ) -> Type<'db> {
        elements
            .into_iter()
            .fold(UnionBuilder::new(db), |builder, element| {
                builder.add(element.into())
            })
            .build()
    }

    /// Apply a transformation function to all elements of the union,
    /// and create a new union from the resulting set of types.
    pub fn map(
        &self,
        db: &'db dyn Db,
        transform_fn: impl FnMut(&Type<'db>) -> Type<'db>,
    ) -> Type<'db> {
        Self::from_elements(db, self.elements(db).iter().map(transform_fn))
    }
}

#[salsa::interned]
pub struct IntersectionType<'db> {
    /// The intersection type includes only values in all of these types.
    #[return_ref]
    positive: FxOrderSet<Type<'db>>,

    /// The intersection type does not include any value in any of these types.
    ///
    /// Negation types aren't expressible in annotations, and are most likely to arise from type
    /// narrowing along with intersections (e.g. `if not isinstance(...)`), so we represent them
    /// directly in intersections rather than as a separate type.
    #[return_ref]
    negative: FxOrderSet<Type<'db>>,
}

#[salsa::interned]
pub struct StringLiteralType<'db> {
    #[return_ref]
    value: Box<str>,
}

impl<'db> StringLiteralType<'db> {
    /// The length of the string, as would be returned by Python's `len()`.
    pub fn python_len(&self, db: &'db dyn Db) -> usize {
        self.value(db).chars().count()
    }
}

#[salsa::interned]
pub struct BytesLiteralType<'db> {
    #[return_ref]
    value: Box<[u8]>,
}

impl<'db> BytesLiteralType<'db> {
    pub fn python_len(&self, db: &'db dyn Db) -> usize {
        self.value(db).len()
    }
}

#[salsa::interned]
pub struct SliceLiteralType<'db> {
    start: Option<i32>,
    stop: Option<i32>,
    step: Option<i32>,
}

impl SliceLiteralType<'_> {
    fn as_tuple(self, db: &dyn Db) -> (Option<i32>, Option<i32>, Option<i32>) {
        (self.start(db), self.stop(db), self.step(db))
    }
}
#[salsa::interned]
pub struct TupleType<'db> {
    #[return_ref]
    elements: Box<[Type<'db>]>,
}

impl<'db> TupleType<'db> {
    pub fn from_elements<T: Into<Type<'db>>>(
        db: &'db dyn Db,
        types: impl IntoIterator<Item = T>,
    ) -> Type<'db> {
        let mut elements = vec![];

        for ty in types {
            let ty = ty.into();
            if ty.is_never() {
                return Type::Never;
            }
            elements.push(ty);
        }

        Type::Tuple(Self::new(db, elements.into_boxed_slice()))
    }

    pub fn get(&self, db: &'db dyn Db, index: usize) -> Option<Type<'db>> {
        self.elements(db).get(index).copied()
    }

    pub fn len(&self, db: &'db dyn Db) -> usize {
        self.elements(db).len()
    }
}

// Make sure that the `Type` enum does not grow unexpectedly.
#[cfg(not(debug_assertions))]
#[cfg(target_pointer_width = "64")]
static_assertions::assert_eq_size!(Type, [u8; 16]);

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::tests::{setup_db, TestDb, TestDbBuilder};
    use crate::stdlib::typing_symbol;
    use crate::PythonVersion;
    use ruff_db::files::system_path_to_file;
    use ruff_db::parsed::parsed_module;
    use ruff_db::system::DbWithTestSystem;
    use ruff_db::testing::assert_function_query_was_not_run;
    use ruff_python_ast as ast;
    use test_case::test_case;

    /// A test representation of a type that can be transformed unambiguously into a real Type,
    /// given a db.
    #[derive(Debug, Clone, PartialEq)]
    pub(crate) enum Ty {
        Never,
        Unknown,
        None,
        Any,
        Todo,
        IntLiteral(i64),
        BooleanLiteral(bool),
        StringLiteral(&'static str),
        LiteralString,
        BytesLiteral(&'static str),
        // BuiltinInstance("str") corresponds to an instance of the builtin `str` class
        BuiltinInstance(&'static str),
        /// Members of the `abc` stdlib module
        AbcInstance(&'static str),
        AbcClassLiteral(&'static str),
        TypingLiteral,
        // BuiltinClassLiteral("str") corresponds to the builtin `str` class object itself
        BuiltinClassLiteral(&'static str),
        KnownClassInstance(KnownClass),
        Union(Vec<Ty>),
        Intersection {
            pos: Vec<Ty>,
            neg: Vec<Ty>,
        },
        Tuple(Vec<Ty>),
        SubclassOfAny,
        SubclassOfBuiltinClass(&'static str),
        SubclassOfAbcClass(&'static str),
        SliceLiteral(i32, i32, i32),
        AlwaysTruthy,
        AlwaysFalsy,
    }

    impl Ty {
        pub(crate) fn into_type(self, db: &TestDb) -> Type<'_> {
            match self {
                Ty::Never => Type::Never,
                Ty::Unknown => Type::unknown(),
                Ty::None => Type::none(db),
                Ty::Any => Type::any(),
                Ty::Todo => todo_type!("Ty::Todo"),
                Ty::IntLiteral(n) => Type::IntLiteral(n),
                Ty::StringLiteral(s) => Type::string_literal(db, s),
                Ty::BooleanLiteral(b) => Type::BooleanLiteral(b),
                Ty::LiteralString => Type::LiteralString,
                Ty::BytesLiteral(s) => Type::bytes_literal(db, s.as_bytes()),
                Ty::BuiltinInstance(s) => builtins_symbol(db, s).expect_type().to_instance(db),
                Ty::AbcInstance(s) => known_module_symbol(db, KnownModule::Abc, s)
                    .expect_type()
                    .to_instance(db),
                Ty::AbcClassLiteral(s) => {
                    known_module_symbol(db, KnownModule::Abc, s).expect_type()
                }
                Ty::TypingLiteral => Type::KnownInstance(KnownInstanceType::Literal),
                Ty::BuiltinClassLiteral(s) => builtins_symbol(db, s).expect_type(),
                Ty::KnownClassInstance(known_class) => known_class.to_instance(db),
                Ty::Union(tys) => {
                    UnionType::from_elements(db, tys.into_iter().map(|ty| ty.into_type(db)))
                }
                Ty::Intersection { pos, neg } => {
                    let mut builder = IntersectionBuilder::new(db);
                    for p in pos {
                        builder = builder.add_positive(p.into_type(db));
                    }
                    for n in neg {
                        builder = builder.add_negative(n.into_type(db));
                    }
                    builder.build()
                }
                Ty::Tuple(tys) => {
                    let elements = tys.into_iter().map(|ty| ty.into_type(db));
                    TupleType::from_elements(db, elements)
                }
                Ty::SubclassOfAny => SubclassOfType::subclass_of_any(),
                Ty::SubclassOfBuiltinClass(s) => SubclassOfType::from(
                    db,
                    builtins_symbol(db, s)
                        .expect_type()
                        .expect_class_literal()
                        .class,
                ),
                Ty::SubclassOfAbcClass(s) => SubclassOfType::from(
                    db,
                    known_module_symbol(db, KnownModule::Abc, s)
                        .expect_type()
                        .expect_class_literal()
                        .class,
                ),
                Ty::SliceLiteral(start, stop, step) => Type::SliceLiteral(SliceLiteralType::new(
                    db,
                    Some(start),
                    Some(stop),
                    Some(step),
                )),
                Ty::AlwaysTruthy => Type::AlwaysTruthy,
                Ty::AlwaysFalsy => Type::AlwaysFalsy,
            }
        }
    }

    #[test_case(Ty::Never, Ty::Never)]
    #[test_case(Ty::Never, Ty::None)]
    #[test_case(Ty::Never, Ty::BuiltinInstance("int"))]
    #[test_case(Ty::None, Ty::BooleanLiteral(true))]
    #[test_case(Ty::None, Ty::IntLiteral(1))]
    #[test_case(Ty::None, Ty::StringLiteral("test"))]
    #[test_case(Ty::None, Ty::BytesLiteral("test"))]
    #[test_case(Ty::None, Ty::LiteralString)]
    #[test_case(Ty::None, Ty::BuiltinInstance("int"))]
    #[test_case(Ty::None, Ty::Tuple(vec![Ty::None]))]
    #[test_case(Ty::BooleanLiteral(true), Ty::BooleanLiteral(false))]
    #[test_case(Ty::BooleanLiteral(true), Ty::Tuple(vec![Ty::None]))]
    #[test_case(Ty::BooleanLiteral(true), Ty::IntLiteral(1))]
    #[test_case(Ty::BooleanLiteral(false), Ty::IntLiteral(0))]
    #[test_case(Ty::IntLiteral(1), Ty::IntLiteral(2))]
    #[test_case(Ty::IntLiteral(1), Ty::Tuple(vec![Ty::None]))]
    #[test_case(Ty::StringLiteral("a"), Ty::StringLiteral("b"))]
    #[test_case(Ty::StringLiteral("a"), Ty::Tuple(vec![Ty::None]))]
    #[test_case(Ty::LiteralString, Ty::BytesLiteral("a"))]
    #[test_case(Ty::BytesLiteral("a"), Ty::BytesLiteral("b"))]
    #[test_case(Ty::BytesLiteral("a"), Ty::Tuple(vec![Ty::None]))]
    #[test_case(Ty::BytesLiteral("a"), Ty::StringLiteral("a"))]
    #[test_case(Ty::Union(vec![Ty::IntLiteral(1), Ty::IntLiteral(2)]), Ty::IntLiteral(3))]
    #[test_case(Ty::Union(vec![Ty::IntLiteral(1), Ty::IntLiteral(2)]), Ty::Union(vec![Ty::IntLiteral(3), Ty::IntLiteral(4)]))]
    #[test_case(Ty::Intersection{pos: vec![Ty::BuiltinInstance("int"),  Ty::IntLiteral(1)], neg: vec![]}, Ty::IntLiteral(2))]
    #[test_case(Ty::Tuple(vec![Ty::IntLiteral(1)]), Ty::Tuple(vec![Ty::IntLiteral(2)]))]
    #[test_case(Ty::Tuple(vec![Ty::IntLiteral(1), Ty::IntLiteral(2)]), Ty::Tuple(vec![Ty::IntLiteral(1)]))]
    #[test_case(Ty::Tuple(vec![Ty::IntLiteral(1), Ty::IntLiteral(2)]), Ty::Tuple(vec![Ty::IntLiteral(1), Ty::IntLiteral(3)]))]
    #[test_case(Ty::Tuple(vec![]), Ty::BuiltinClassLiteral("object"))]
    #[test_case(Ty::SubclassOfBuiltinClass("object"), Ty::None)]
    #[test_case(Ty::SubclassOfBuiltinClass("str"), Ty::LiteralString)]
    #[test_case(Ty::AlwaysFalsy, Ty::AlwaysTruthy)]
    #[test_case(Ty::Tuple(vec![]), Ty::TypingLiteral)]
    #[test_case(Ty::TypingLiteral, Ty::SubclassOfBuiltinClass("object"))]
    fn is_disjoint_from(a: Ty, b: Ty) {
        let db = setup_db();
        let a = a.into_type(&db);
        let b = b.into_type(&db);

        assert!(a.is_disjoint_from(&db, b));
        assert!(b.is_disjoint_from(&db, a));
    }

    #[test_case(Ty::Any, Ty::BuiltinInstance("int"))]
    #[test_case(Ty::None, Ty::None)]
    #[test_case(Ty::None, Ty::BuiltinInstance("object"))]
    #[test_case(Ty::BuiltinInstance("int"), Ty::BuiltinInstance("int"))]
    #[test_case(Ty::BuiltinInstance("str"), Ty::LiteralString)]
    #[test_case(Ty::BooleanLiteral(true), Ty::BooleanLiteral(true))]
    #[test_case(Ty::BooleanLiteral(false), Ty::BooleanLiteral(false))]
    #[test_case(Ty::BooleanLiteral(true), Ty::BuiltinInstance("bool"))]
    #[test_case(Ty::BooleanLiteral(true), Ty::BuiltinInstance("int"))]
    #[test_case(Ty::IntLiteral(1), Ty::IntLiteral(1))]
    #[test_case(Ty::StringLiteral("a"), Ty::StringLiteral("a"))]
    #[test_case(Ty::StringLiteral("a"), Ty::LiteralString)]
    #[test_case(Ty::StringLiteral("a"), Ty::BuiltinInstance("str"))]
    #[test_case(Ty::LiteralString, Ty::LiteralString)]
    #[test_case(Ty::Union(vec![Ty::IntLiteral(1), Ty::IntLiteral(2)]), Ty::IntLiteral(2))]
    #[test_case(Ty::Union(vec![Ty::IntLiteral(1), Ty::IntLiteral(2)]), Ty::Union(vec![Ty::IntLiteral(2), Ty::IntLiteral(3)]))]
    #[test_case(Ty::Intersection{pos: vec![Ty::BuiltinInstance("int"), Ty::IntLiteral(2)], neg: vec![]}, Ty::IntLiteral(2))]
    #[test_case(Ty::Tuple(vec![Ty::IntLiteral(1), Ty::IntLiteral(2)]), Ty::Tuple(vec![Ty::IntLiteral(1), Ty::BuiltinInstance("int")]))]
    #[test_case(Ty::BuiltinClassLiteral("str"), Ty::BuiltinInstance("type"))]
    #[test_case(Ty::BuiltinClassLiteral("str"), Ty::SubclassOfAny)]
    #[test_case(Ty::AbcClassLiteral("ABC"), Ty::AbcInstance("ABCMeta"))]
    #[test_case(Ty::BuiltinInstance("str"), Ty::AlwaysTruthy)]
    #[test_case(Ty::BuiltinInstance("str"), Ty::AlwaysFalsy)]
    fn is_not_disjoint_from(a: Ty, b: Ty) {
        let db = setup_db();
        let a = a.into_type(&db);
        let b = b.into_type(&db);

        assert!(!a.is_disjoint_from(&db, b));
        assert!(!b.is_disjoint_from(&db, a));
    }

    #[test]
    fn is_disjoint_from_union_of_class_types() {
        let mut db = setup_db();
        db.write_dedented(
            "/src/module.py",
            "
            class A: ...
            class B: ...
            U = A if flag else B
        ",
        )
        .unwrap();
        let module = ruff_db::files::system_path_to_file(&db, "/src/module.py").unwrap();

        let type_a = super::global_symbol(&db, module, "A").expect_type();
        let type_u = super::global_symbol(&db, module, "U").expect_type();

        assert!(type_a.is_class_literal());
        assert!(type_u.is_union());

        assert!(!type_a.is_disjoint_from(&db, type_u));
    }

    #[test]
    fn is_disjoint_type_subclass_of() {
        let mut db = setup_db();
        db.write_dedented(
            "/src/module.py",
            "
            class A: ...
            class B: ...
        ",
        )
        .unwrap();
        let module = ruff_db::files::system_path_to_file(&db, "/src/module.py").unwrap();

        let literal_a = super::global_symbol(&db, module, "A").expect_type();
        let literal_b = super::global_symbol(&db, module, "B").expect_type();

        let subclass_of_a = SubclassOfType::from(&db, literal_a.expect_class_literal().class);
        let subclass_of_b = SubclassOfType::from(&db, literal_b.expect_class_literal().class);

        // Class literals are always disjoint. They are singleton types
        assert!(literal_a.is_disjoint_from(&db, literal_b));

        // The class A is a subclass of A, so A is not disjoint from type[A]
        assert!(!literal_a.is_disjoint_from(&db, subclass_of_a));

        // The class A is disjoint from type[B] because it's not a subclass
        // of B:
        assert!(literal_a.is_disjoint_from(&db, subclass_of_b));

        // However, type[A] is not disjoint from type[B], as there could be
        // classes that inherit from both A and B:
        assert!(!subclass_of_a.is_disjoint_from(&db, subclass_of_b));
    }

    #[test]
    fn is_disjoint_module_literals() {
        let mut db = setup_db();
        db.write_dedented(
            "/src/module.py",
            "
            import random
            import math
        ",
        )
        .unwrap();

        let module = ruff_db::files::system_path_to_file(&db, "/src/module.py").unwrap();

        let module_literal_random = super::global_symbol(&db, module, "random").expect_type();
        let module_literal_math = super::global_symbol(&db, module, "math").expect_type();

        assert!(module_literal_random.is_disjoint_from(&db, module_literal_math));

        assert!(!module_literal_random.is_disjoint_from(
            &db,
            Ty::KnownClassInstance(KnownClass::ModuleType).into_type(&db)
        ));
        assert!(!module_literal_random.is_disjoint_from(
            &db,
            Ty::KnownClassInstance(KnownClass::Object).into_type(&db)
        ));
    }

    #[test]
    fn is_disjoint_function_literals() {
        let mut db = setup_db();
        db.write_dedented(
            "/src/module.py",
            "
            def f(): ...
            def g(): ...
        ",
        )
        .unwrap();

        let module = ruff_db::files::system_path_to_file(&db, "/src/module.py").unwrap();

        let function_literal_f = super::global_symbol(&db, module, "f").expect_type();
        let function_literal_g = super::global_symbol(&db, module, "g").expect_type();

        assert!(function_literal_f.is_disjoint_from(&db, function_literal_g));

        assert!(!function_literal_f.is_disjoint_from(
            &db,
            Ty::KnownClassInstance(KnownClass::FunctionType).into_type(&db)
        ));
        assert!(!function_literal_f.is_disjoint_from(
            &db,
            Ty::KnownClassInstance(KnownClass::Object).into_type(&db)
        ));
    }

    #[test_case(Ty::None)]
    #[test_case(Ty::BooleanLiteral(true))]
    #[test_case(Ty::BooleanLiteral(false))]
    #[test_case(Ty::SubclassOfBuiltinClass("bool"))] // a `@final` class
    fn is_singleton(from: Ty) {
        let db = setup_db();

        assert!(from.into_type(&db).is_singleton(&db));
    }

    /// Explicitly test for Python version <3.13 and >=3.13, to ensure that
    /// the fallback to `typing_extensions` is working correctly.
    /// See [`KnownClass::canonical_module`] for more information.
    #[test_case(PythonVersion::PY312)]
    #[test_case(PythonVersion::PY313)]
    fn no_default_type_is_singleton(python_version: PythonVersion) {
        let db = TestDbBuilder::new()
            .with_python_version(python_version)
            .build()
            .unwrap();

        let no_default = Ty::KnownClassInstance(KnownClass::NoDefaultType).into_type(&db);

        assert!(no_default.is_singleton(&db));
    }

    #[test_case(Ty::None)]
    #[test_case(Ty::BooleanLiteral(true))]
    #[test_case(Ty::IntLiteral(1))]
    #[test_case(Ty::StringLiteral("abc"))]
    #[test_case(Ty::BytesLiteral("abc"))]
    #[test_case(Ty::Tuple(vec![]))]
    #[test_case(Ty::Tuple(vec![Ty::BooleanLiteral(true), Ty::IntLiteral(1)]))]
    fn is_single_valued(from: Ty) {
        let db = setup_db();

        assert!(from.into_type(&db).is_single_valued(&db));
    }

    #[test_case(Ty::Never)]
    #[test_case(Ty::Any)]
    #[test_case(Ty::Union(vec![Ty::IntLiteral(1), Ty::IntLiteral(2)]))]
    #[test_case(Ty::Tuple(vec![Ty::None, Ty::BuiltinInstance("int")]))]
    #[test_case(Ty::BuiltinInstance("str"))]
    #[test_case(Ty::LiteralString)]
    fn is_not_single_valued(from: Ty) {
        let db = setup_db();

        assert!(!from.into_type(&db).is_single_valued(&db));
    }

    #[test_case(Ty::Never)]
    #[test_case(Ty::IntLiteral(345))]
    #[test_case(Ty::BuiltinInstance("str"))]
    #[test_case(Ty::Union(vec![Ty::IntLiteral(1), Ty::IntLiteral(2)]))]
    #[test_case(Ty::Tuple(vec![]))]
    #[test_case(Ty::Tuple(vec![Ty::None]))]
    #[test_case(Ty::Tuple(vec![Ty::None, Ty::BooleanLiteral(true)]))]
    fn is_not_singleton(from: Ty) {
        let db = setup_db();

        assert!(!from.into_type(&db).is_singleton(&db));
    }

    #[test_case(Ty::Never)]
    #[test_case(Ty::None)]
    #[test_case(Ty::IntLiteral(1))]
    #[test_case(Ty::BooleanLiteral(true))]
    #[test_case(Ty::StringLiteral("abc"))]
    #[test_case(Ty::LiteralString)]
    #[test_case(Ty::BytesLiteral("abc"))]
    #[test_case(Ty::KnownClassInstance(KnownClass::Str))]
    #[test_case(Ty::KnownClassInstance(KnownClass::Object))]
    #[test_case(Ty::KnownClassInstance(KnownClass::Type))]
    #[test_case(Ty::BuiltinClassLiteral("str"))]
    #[test_case(Ty::TypingLiteral)]
    #[test_case(Ty::Union(vec![Ty::KnownClassInstance(KnownClass::Str), Ty::None]))]
    #[test_case(Ty::Intersection{pos: vec![Ty::KnownClassInstance(KnownClass::Str)], neg: vec![Ty::LiteralString]})]
    #[test_case(Ty::Tuple(vec![]))]
    #[test_case(Ty::Tuple(vec![Ty::KnownClassInstance(KnownClass::Int), Ty::KnownClassInstance(KnownClass::Object)]))]
    #[test_case(Ty::BuiltinInstance("type"))]
    #[test_case(Ty::SubclassOfBuiltinClass("object"))]
    #[test_case(Ty::SubclassOfBuiltinClass("str"))]
    fn is_fully_static(from: Ty) {
        let db = setup_db();

        assert!(from.into_type(&db).is_fully_static(&db));
    }

    #[test_case(Ty::Any)]
    #[test_case(Ty::Unknown)]
    #[test_case(Ty::Todo)]
    #[test_case(Ty::Union(vec![Ty::Any, Ty::KnownClassInstance(KnownClass::Str)]))]
    #[test_case(Ty::Union(vec![Ty::KnownClassInstance(KnownClass::Str), Ty::Unknown]))]
    #[test_case(Ty::Intersection{pos: vec![Ty::Any], neg: vec![Ty::LiteralString]})]
    #[test_case(Ty::Tuple(vec![Ty::KnownClassInstance(KnownClass::Int), Ty::Any]))]
    #[test_case(Ty::SubclassOfAny)]
    fn is_not_fully_static(from: Ty) {
        let db = setup_db();

        assert!(!from.into_type(&db).is_fully_static(&db));
    }

    #[test_case(Ty::Todo, Ty::Todo)]
    #[test_case(Ty::Any, Ty::Any)]
    #[test_case(Ty::Unknown, Ty::Unknown)]
    #[test_case(Ty::Any, Ty::Unknown)]
    #[test_case(Ty::Todo, Ty::Unknown)]
    #[test_case(Ty::Todo, Ty::Any)]
    #[test_case(Ty::Never, Ty::Never)]
    #[test_case(Ty::AlwaysTruthy, Ty::AlwaysTruthy)]
    #[test_case(Ty::AlwaysFalsy, Ty::AlwaysFalsy)]
    #[test_case(Ty::LiteralString, Ty::LiteralString)]
    #[test_case(Ty::BooleanLiteral(true), Ty::BooleanLiteral(true))]
    #[test_case(Ty::BooleanLiteral(false), Ty::BooleanLiteral(false))]
    #[test_case(Ty::SliceLiteral(0, 1, 2), Ty::SliceLiteral(0, 1, 2))]
    #[test_case(Ty::BuiltinClassLiteral("str"), Ty::BuiltinClassLiteral("str"))]
    #[test_case(Ty::BuiltinInstance("type"), Ty::SubclassOfBuiltinClass("object"))]
    // TODO: Compare unions/intersections with different orders
    // #[test_case(
    //     Ty::Union(vec![Ty::BuiltinInstance("str"), Ty::BuiltinInstance("int")]),
    //     Ty::Union(vec![Ty::BuiltinInstance("int"), Ty::BuiltinInstance("str")])
    // )]
    // #[test_case(
    //     Ty::Intersection {
    //         pos: vec![Ty::BuiltinInstance("str"), Ty::BuiltinInstance("int")],
    //         neg: vec![Ty::BuiltinInstance("bytes"), Ty::None]
    //     },
    //     Ty::Intersection {
    //         pos: vec![Ty::BuiltinInstance("int"), Ty::BuiltinInstance("str")],
    //         neg: vec![Ty::None, Ty::BuiltinInstance("bytes")]
    //     }
    // )]
    // #[test_case(
    //     Ty::Intersection {
    //         pos: vec![Ty::Union(vec![Ty::BuiltinInstance("str"), Ty::BuiltinInstance("int")])],
    //         neg: vec![Ty::SubclassOfAny]
    //     },
    //     Ty::Intersection {
    //         pos: vec![Ty::Union(vec![Ty::BuiltinInstance("int"), Ty::BuiltinInstance("str")])],
    //         neg: vec![Ty::SubclassOfUnknown]
    //     }
    // )]
    fn is_gradual_equivalent_to(a: Ty, b: Ty) {
        let db = setup_db();
        let a = a.into_type(&db);
        let b = b.into_type(&db);

        assert!(a.is_gradual_equivalent_to(&db, b));
        assert!(b.is_gradual_equivalent_to(&db, a));
    }

    #[test_case(Ty::BuiltinInstance("type"), Ty::SubclassOfAny)]
    #[test_case(Ty::SubclassOfBuiltinClass("object"), Ty::SubclassOfAny)]
    #[test_case(
        Ty::Union(vec![Ty::BuiltinInstance("str"), Ty::BuiltinInstance("int")]),
        Ty::Union(vec![Ty::BuiltinInstance("int"), Ty::BuiltinInstance("str"), Ty::BuiltinInstance("bytes")])
    )]
    #[test_case(
        Ty::Union(vec![Ty::BuiltinInstance("str"), Ty::BuiltinInstance("int"), Ty::BuiltinInstance("bytes")]),
        Ty::Union(vec![Ty::BuiltinInstance("int"), Ty::BuiltinInstance("str"), Ty::BuiltinInstance("dict")])
    )]
    #[test_case(
        Ty::Tuple(vec![Ty::BuiltinInstance("str"), Ty::BuiltinInstance("int")]),
        Ty::Tuple(vec![Ty::BuiltinInstance("str"), Ty::BuiltinInstance("int"), Ty::BuiltinInstance("bytes")])
    )]
    #[test_case(
        Ty::Tuple(vec![Ty::BuiltinInstance("str"), Ty::BuiltinInstance("int")]),
        Ty::Tuple(vec![Ty::BuiltinInstance("int"), Ty::BuiltinInstance("str")])
    )]
    fn is_not_gradual_equivalent_to(a: Ty, b: Ty) {
        let db = setup_db();
        let a = a.into_type(&db);
        let b = b.into_type(&db);

        assert!(!a.is_gradual_equivalent_to(&db, b));
        assert!(!b.is_gradual_equivalent_to(&db, a));
    }

    #[test_case(Ty::IntLiteral(1); "is_int_literal_truthy")]
    #[test_case(Ty::IntLiteral(-1))]
    #[test_case(Ty::StringLiteral("foo"))]
    #[test_case(Ty::Tuple(vec![Ty::IntLiteral(0)]))]
    #[test_case(Ty::Union(vec![Ty::IntLiteral(1), Ty::IntLiteral(2)]))]
    fn is_truthy(ty: Ty) {
        let db = setup_db();
        assert_eq!(ty.into_type(&db).bool(&db), Truthiness::AlwaysTrue);
    }

    #[test_case(Ty::Tuple(vec![]))]
    #[test_case(Ty::IntLiteral(0))]
    #[test_case(Ty::StringLiteral(""))]
    #[test_case(Ty::Union(vec![Ty::IntLiteral(0), Ty::IntLiteral(0)]))]
    fn is_falsy(ty: Ty) {
        let db = setup_db();
        assert_eq!(ty.into_type(&db).bool(&db), Truthiness::AlwaysFalse);
    }

    #[test_case(Ty::BuiltinInstance("str"))]
    #[test_case(Ty::Union(vec![Ty::IntLiteral(1), Ty::IntLiteral(0)]))]
    #[test_case(Ty::Union(vec![Ty::BuiltinInstance("str"), Ty::IntLiteral(0)]))]
    #[test_case(Ty::Union(vec![Ty::BuiltinInstance("str"), Ty::IntLiteral(1)]))]
    fn boolean_value_is_unknown(ty: Ty) {
        let db = setup_db();
        assert_eq!(ty.into_type(&db).bool(&db), Truthiness::Ambiguous);
    }

    #[test_case(Ty::IntLiteral(1), Ty::StringLiteral("1"))]
    #[test_case(Ty::BooleanLiteral(true), Ty::StringLiteral("True"))]
    #[test_case(Ty::BooleanLiteral(false), Ty::StringLiteral("False"))]
    #[test_case(Ty::StringLiteral("ab'cd"), Ty::StringLiteral("ab'cd"))] // no quotes
    #[test_case(Ty::LiteralString, Ty::LiteralString)]
    #[test_case(Ty::BuiltinInstance("int"), Ty::BuiltinInstance("str"))]
    fn has_correct_str(ty: Ty, expected: Ty) {
        let db = setup_db();

        assert_eq!(ty.into_type(&db).str(&db), expected.into_type(&db));
    }

    #[test_case(Ty::IntLiteral(1), Ty::StringLiteral("1"))]
    #[test_case(Ty::BooleanLiteral(true), Ty::StringLiteral("True"))]
    #[test_case(Ty::BooleanLiteral(false), Ty::StringLiteral("False"))]
    #[test_case(Ty::StringLiteral("ab'cd"), Ty::StringLiteral("'ab\\'cd'"))] // single quotes
    #[test_case(Ty::LiteralString, Ty::LiteralString)]
    #[test_case(Ty::BuiltinInstance("int"), Ty::BuiltinInstance("str"))]
    fn has_correct_repr(ty: Ty, expected: Ty) {
        let db = setup_db();

        assert_eq!(ty.into_type(&db).repr(&db), expected.into_type(&db));
    }

    #[test]
    fn typing_vs_typeshed_no_default() {
        let db = TestDbBuilder::new()
            .with_python_version(PythonVersion::PY313)
            .build()
            .unwrap();

        let typing_no_default = typing_symbol(&db, "NoDefault").expect_type();
        let typing_extensions_no_default = typing_extensions_symbol(&db, "NoDefault").expect_type();

        assert_eq!(typing_no_default.display(&db).to_string(), "NoDefault");
        assert_eq!(
            typing_extensions_no_default.display(&db).to_string(),
            "NoDefault"
        );
    }

    #[test]
    fn module_type_symbols_includes_declared_types_but_not_referenced_types() {
        let db = setup_db();
        let symbol_names = module_type_symbols(&db);

        let dunder_name_symbol_name = ast::name::Name::new_static("__name__");
        assert!(symbol_names.contains(&dunder_name_symbol_name));

        let property_symbol_name = ast::name::Name::new_static("property");
        assert!(!symbol_names.contains(&property_symbol_name));
    }

    /// Inferring the result of a call-expression shouldn't need to re-run after
    /// a trivial change to the function's file (e.g. by adding a docstring to the function).
    #[test]
    fn call_type_doesnt_rerun_when_only_callee_changed() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "src/foo.py",
            r#"
            def foo() -> int:
                return 5
        "#,
        )?;
        db.write_dedented(
            "src/bar.py",
            r#"
            from foo import foo

            a = foo()
            "#,
        )?;

        let bar = system_path_to_file(&db, "src/bar.py")?;
        let a = global_symbol(&db, bar, "a");

        assert_eq!(a.expect_type(), KnownClass::Int.to_instance(&db));

        // Add a docstring to foo to trigger a re-run.
        // The bar-call site of foo should not be re-run because of that
        db.write_dedented(
            "src/foo.py",
            r#"
            def foo() -> int:
                "Computes a value"
                return 5
            "#,
        )?;
        db.clear_salsa_events();

        let a = global_symbol(&db, bar, "a");

        assert_eq!(a.expect_type(), KnownClass::Int.to_instance(&db));
        let events = db.take_salsa_events();

        let call = &*parsed_module(&db, bar).syntax().body[1]
            .as_assign_stmt()
            .unwrap()
            .value;
        let foo_call = semantic_index(&db, bar).expression(call);

        assert_function_query_was_not_run(&db, infer_expression_types, foo_call, &events);

        Ok(())
    }

    #[test]
    fn type_alias_types() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "src/mod.py",
            r#"
            type Alias1 = int
            type Alias2 = int
        "#,
        )?;

        let mod_py = system_path_to_file(&db, "src/mod.py")?;
        let ty_alias1 = global_symbol(&db, mod_py, "Alias1").expect_type();
        let ty_alias2 = global_symbol(&db, mod_py, "Alias2").expect_type();

        let Type::KnownInstance(KnownInstanceType::TypeAliasType(alias1)) = ty_alias1 else {
            panic!("Expected TypeAliasType, got {ty_alias1:?}");
        };
        assert_eq!(alias1.name(&db), "Alias1");
        assert_eq!(alias1.value_ty(&db), KnownClass::Int.to_instance(&db));

        // Two type aliases are distinct and disjoint, even if they refer to the same type
        assert!(!ty_alias1.is_equivalent_to(&db, ty_alias2));
        assert!(ty_alias1.is_disjoint_from(&db, ty_alias2));

        Ok(())
    }

    /// All other tests also make sure that `Type::Todo` works as expected. This particular
    /// test makes sure that we handle `Todo` types correctly, even if they originate from
    /// different sources.
    #[test]
    fn todo_types() {
        let db = setup_db();

        let todo1 = todo_type!("1");
        let todo2 = todo_type!("2");
        let todo3 = todo_type!();
        let todo4 = todo_type!();

        let int = KnownClass::Int.to_instance(&db);

        assert!(int.is_assignable_to(&db, todo1));
        assert!(int.is_assignable_to(&db, todo3));

        assert!(todo1.is_assignable_to(&db, int));
        assert!(todo3.is_assignable_to(&db, int));

        // We lose information when combining several `Todo` types. This is an
        // acknowledged limitation of the current implementation. We can not
        // easily store the meta information of several `Todo`s in a single
        // variant, as `TodoType` needs to implement `Copy`, meaning it can't
        // contain `Vec`/`Box`/etc., and can't be boxed itself.
        //
        // Lifting this restriction would require us to intern `TodoType` in
        // salsa, but that would mean we would have to pass in `db` everywhere.

        // A union of several `Todo` types collapses to a single `Todo` type:
        assert!(UnionType::from_elements(&db, vec![todo1, todo2, todo3, todo4]).is_todo());

        // And similar for intersection types:
        assert!(IntersectionBuilder::new(&db)
            .add_positive(todo1)
            .add_positive(todo2)
            .add_positive(todo3)
            .add_positive(todo4)
            .build()
            .is_todo());
        assert!(IntersectionBuilder::new(&db)
            .add_positive(todo1)
            .add_negative(todo2)
            .add_positive(todo3)
            .add_negative(todo4)
            .build()
            .is_todo());
    }
}
