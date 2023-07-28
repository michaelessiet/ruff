use std::borrow::Cow;
use std::path::Path;

use crate::{
    self as ast, Arguments, Constant, ExceptHandler, Expr, Keyword, MatchCase, Pattern, Ranged,
    Stmt, TypeParam,
};
use num_traits::Zero;
use ruff_text_size::TextRange;
use smallvec::SmallVec;

use crate::call_path::CallPath;
use crate::statement_visitor::{walk_body, walk_stmt, StatementVisitor};

/// Return `true` if the `Stmt` is a compound statement (as opposed to a simple statement).
pub const fn is_compound_statement(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::FunctionDef(_)
            | Stmt::AsyncFunctionDef(_)
            | Stmt::ClassDef(_)
            | Stmt::While(_)
            | Stmt::For(_)
            | Stmt::AsyncFor(_)
            | Stmt::Match(_)
            | Stmt::With(_)
            | Stmt::AsyncWith(_)
            | Stmt::If(_)
            | Stmt::Try(_)
            | Stmt::TryStar(_)
    )
}

fn is_iterable_initializer<F>(id: &str, is_builtin: F) -> bool
where
    F: Fn(&str) -> bool,
{
    matches!(id, "list" | "tuple" | "set" | "dict" | "frozenset") && is_builtin(id)
}

/// Return `true` if the `Expr` contains an expression that appears to include a
/// side-effect (like a function call).
///
/// Accepts a closure that determines whether a given name (e.g., `"list"`) is a Python builtin.
pub fn contains_effect<F>(expr: &Expr, is_builtin: F) -> bool
where
    F: Fn(&str) -> bool,
{
    any_over_expr(expr, &|expr| {
        // Accept empty initializers.
        if let Expr::Call(ast::ExprCall {
            func,
            args,
            keywords,
            range: _range,
        }) = expr
        {
            // Ex) `list()`
            if args.is_empty() && keywords.is_empty() {
                if let Expr::Name(ast::ExprName { id, .. }) = func.as_ref() {
                    if !is_iterable_initializer(id.as_str(), |id| is_builtin(id)) {
                        return true;
                    }
                    return false;
                }
            }
        }

        // Avoid false positive for overloaded operators.
        if let Expr::BinOp(ast::ExprBinOp { left, right, .. }) = expr {
            if !matches!(
                left.as_ref(),
                Expr::Constant(_)
                    | Expr::JoinedStr(_)
                    | Expr::List(_)
                    | Expr::Tuple(_)
                    | Expr::Set(_)
                    | Expr::Dict(_)
                    | Expr::ListComp(_)
                    | Expr::SetComp(_)
                    | Expr::DictComp(_)
            ) {
                return true;
            }
            if !matches!(
                right.as_ref(),
                Expr::Constant(_)
                    | Expr::JoinedStr(_)
                    | Expr::List(_)
                    | Expr::Tuple(_)
                    | Expr::Set(_)
                    | Expr::Dict(_)
                    | Expr::ListComp(_)
                    | Expr::SetComp(_)
                    | Expr::DictComp(_)
            ) {
                return true;
            }
            return false;
        }

        // Otherwise, avoid all complex expressions.
        matches!(
            expr,
            Expr::Await(_)
                | Expr::Call(_)
                | Expr::DictComp(_)
                | Expr::GeneratorExp(_)
                | Expr::ListComp(_)
                | Expr::SetComp(_)
                | Expr::Subscript(_)
                | Expr::Yield(_)
                | Expr::YieldFrom(_)
        )
    })
}

/// Call `func` over every `Expr` in `expr`, returning `true` if any expression
/// returns `true`..
pub fn any_over_expr<F>(expr: &Expr, func: &F) -> bool
where
    F: Fn(&Expr) -> bool,
{
    if func(expr) {
        return true;
    }
    match expr {
        Expr::BoolOp(ast::ExprBoolOp {
            values,
            range: _range,
            ..
        })
        | Expr::JoinedStr(ast::ExprJoinedStr {
            values,
            range: _range,
        }) => values.iter().any(|expr| any_over_expr(expr, func)),
        Expr::NamedExpr(ast::ExprNamedExpr {
            target,
            value,
            range: _range,
        }) => any_over_expr(target, func) || any_over_expr(value, func),
        Expr::BinOp(ast::ExprBinOp { left, right, .. }) => {
            any_over_expr(left, func) || any_over_expr(right, func)
        }
        Expr::UnaryOp(ast::ExprUnaryOp { operand, .. }) => any_over_expr(operand, func),
        Expr::Lambda(ast::ExprLambda { body, .. }) => any_over_expr(body, func),
        Expr::IfExp(ast::ExprIfExp {
            test,
            body,
            orelse,
            range: _range,
        }) => any_over_expr(test, func) || any_over_expr(body, func) || any_over_expr(orelse, func),
        Expr::Dict(ast::ExprDict {
            keys,
            values,
            range: _range,
        }) => values
            .iter()
            .chain(keys.iter().flatten())
            .any(|expr| any_over_expr(expr, func)),
        Expr::Set(ast::ExprSet {
            elts,
            range: _range,
        })
        | Expr::List(ast::ExprList {
            elts,
            range: _range,
            ..
        })
        | Expr::Tuple(ast::ExprTuple {
            elts,
            range: _range,
            ..
        }) => elts.iter().any(|expr| any_over_expr(expr, func)),
        Expr::ListComp(ast::ExprListComp {
            elt,
            generators,
            range: _range,
        })
        | Expr::SetComp(ast::ExprSetComp {
            elt,
            generators,
            range: _range,
        })
        | Expr::GeneratorExp(ast::ExprGeneratorExp {
            elt,
            generators,
            range: _range,
        }) => {
            any_over_expr(elt, func)
                || generators.iter().any(|generator| {
                    any_over_expr(&generator.target, func)
                        || any_over_expr(&generator.iter, func)
                        || generator.ifs.iter().any(|expr| any_over_expr(expr, func))
                })
        }
        Expr::DictComp(ast::ExprDictComp {
            key,
            value,
            generators,
            range: _range,
        }) => {
            any_over_expr(key, func)
                || any_over_expr(value, func)
                || generators.iter().any(|generator| {
                    any_over_expr(&generator.target, func)
                        || any_over_expr(&generator.iter, func)
                        || generator.ifs.iter().any(|expr| any_over_expr(expr, func))
                })
        }
        Expr::Await(ast::ExprAwait {
            value,
            range: _range,
        })
        | Expr::YieldFrom(ast::ExprYieldFrom {
            value,
            range: _range,
        })
        | Expr::Attribute(ast::ExprAttribute {
            value,
            range: _range,
            ..
        })
        | Expr::Starred(ast::ExprStarred {
            value,
            range: _range,
            ..
        }) => any_over_expr(value, func),
        Expr::Yield(ast::ExprYield {
            value,
            range: _range,
        }) => value
            .as_ref()
            .map_or(false, |value| any_over_expr(value, func)),
        Expr::Compare(ast::ExprCompare {
            left, comparators, ..
        }) => any_over_expr(left, func) || comparators.iter().any(|expr| any_over_expr(expr, func)),
        Expr::Call(ast::ExprCall {
            func: call_func,
            args,
            keywords,
            range: _range,
        }) => {
            any_over_expr(call_func, func)
                || args.iter().any(|expr| any_over_expr(expr, func))
                || keywords
                    .iter()
                    .any(|keyword| any_over_expr(&keyword.value, func))
        }
        Expr::FormattedValue(ast::ExprFormattedValue {
            value, format_spec, ..
        }) => {
            any_over_expr(value, func)
                || format_spec
                    .as_ref()
                    .map_or(false, |value| any_over_expr(value, func))
        }
        Expr::Subscript(ast::ExprSubscript { value, slice, .. }) => {
            any_over_expr(value, func) || any_over_expr(slice, func)
        }
        Expr::Slice(ast::ExprSlice {
            lower,
            upper,
            step,
            range: _range,
        }) => {
            lower
                .as_ref()
                .map_or(false, |value| any_over_expr(value, func))
                || upper
                    .as_ref()
                    .map_or(false, |value| any_over_expr(value, func))
                || step
                    .as_ref()
                    .map_or(false, |value| any_over_expr(value, func))
        }
        Expr::Name(_) | Expr::Constant(_) => false,
        Expr::LineMagic(_) => false,
    }
}

pub fn any_over_type_param<F>(type_param: &TypeParam, func: &F) -> bool
where
    F: Fn(&Expr) -> bool,
{
    match type_param {
        TypeParam::TypeVar(ast::TypeParamTypeVar { bound, .. }) => bound
            .as_ref()
            .map_or(false, |value| any_over_expr(value, func)),
        TypeParam::TypeVarTuple(ast::TypeParamTypeVarTuple { .. }) => false,
        TypeParam::ParamSpec(ast::TypeParamParamSpec { .. }) => false,
    }
}

pub fn any_over_pattern<F>(pattern: &Pattern, func: &F) -> bool
where
    F: Fn(&Expr) -> bool,
{
    match pattern {
        Pattern::MatchValue(ast::PatternMatchValue {
            value,
            range: _range,
        }) => any_over_expr(value, func),
        Pattern::MatchSingleton(_) => false,
        Pattern::MatchSequence(ast::PatternMatchSequence {
            patterns,
            range: _range,
        }) => patterns
            .iter()
            .any(|pattern| any_over_pattern(pattern, func)),
        Pattern::MatchMapping(ast::PatternMatchMapping { keys, patterns, .. }) => {
            keys.iter().any(|key| any_over_expr(key, func))
                || patterns
                    .iter()
                    .any(|pattern| any_over_pattern(pattern, func))
        }
        Pattern::MatchClass(ast::PatternMatchClass {
            cls,
            patterns,
            kwd_patterns,
            ..
        }) => {
            any_over_expr(cls, func)
                || patterns
                    .iter()
                    .any(|pattern| any_over_pattern(pattern, func))
                || kwd_patterns
                    .iter()
                    .any(|pattern| any_over_pattern(pattern, func))
        }
        Pattern::MatchStar(_) => false,
        Pattern::MatchAs(ast::PatternMatchAs { pattern, .. }) => pattern
            .as_ref()
            .map_or(false, |pattern| any_over_pattern(pattern, func)),
        Pattern::MatchOr(ast::PatternMatchOr {
            patterns,
            range: _range,
        }) => patterns
            .iter()
            .any(|pattern| any_over_pattern(pattern, func)),
    }
}

pub fn any_over_stmt<F>(stmt: &Stmt, func: &F) -> bool
where
    F: Fn(&Expr) -> bool,
{
    match stmt {
        Stmt::FunctionDef(ast::StmtFunctionDef {
            args,
            body,
            decorator_list,
            returns,
            ..
        })
        | Stmt::AsyncFunctionDef(ast::StmtAsyncFunctionDef {
            args,
            body,
            decorator_list,
            returns,
            ..
        }) => {
            args.posonlyargs
                .iter()
                .chain(args.args.iter().chain(args.kwonlyargs.iter()))
                .any(|arg_with_default| {
                    arg_with_default
                        .default
                        .as_ref()
                        .map_or(false, |expr| any_over_expr(expr, func))
                        || arg_with_default
                            .def
                            .annotation
                            .as_ref()
                            .map_or(false, |expr| any_over_expr(expr, func))
                })
                || args.vararg.as_ref().map_or(false, |arg| {
                    arg.annotation
                        .as_ref()
                        .map_or(false, |expr| any_over_expr(expr, func))
                })
                || args.kwarg.as_ref().map_or(false, |arg| {
                    arg.annotation
                        .as_ref()
                        .map_or(false, |expr| any_over_expr(expr, func))
                })
                || body.iter().any(|stmt| any_over_stmt(stmt, func))
                || decorator_list
                    .iter()
                    .any(|decorator| any_over_expr(&decorator.expression, func))
                || returns
                    .as_ref()
                    .map_or(false, |value| any_over_expr(value, func))
        }
        Stmt::ClassDef(ast::StmtClassDef {
            bases,
            keywords,
            body,
            decorator_list,
            ..
        }) => {
            bases.iter().any(|expr| any_over_expr(expr, func))
                || keywords
                    .iter()
                    .any(|keyword| any_over_expr(&keyword.value, func))
                || body.iter().any(|stmt| any_over_stmt(stmt, func))
                || decorator_list
                    .iter()
                    .any(|decorator| any_over_expr(&decorator.expression, func))
        }
        Stmt::Return(ast::StmtReturn {
            value,
            range: _range,
        }) => value
            .as_ref()
            .map_or(false, |value| any_over_expr(value, func)),
        Stmt::Delete(ast::StmtDelete {
            targets,
            range: _range,
        }) => targets.iter().any(|expr| any_over_expr(expr, func)),
        Stmt::TypeAlias(ast::StmtTypeAlias {
            name,
            type_params,
            value,
            ..
        }) => {
            any_over_expr(name, func)
                || type_params
                    .iter()
                    .any(|type_param| any_over_type_param(type_param, func))
                || any_over_expr(value, func)
        }
        Stmt::Assign(ast::StmtAssign { targets, value, .. }) => {
            targets.iter().any(|expr| any_over_expr(expr, func)) || any_over_expr(value, func)
        }
        Stmt::AugAssign(ast::StmtAugAssign { target, value, .. }) => {
            any_over_expr(target, func) || any_over_expr(value, func)
        }
        Stmt::AnnAssign(ast::StmtAnnAssign {
            target,
            annotation,
            value,
            ..
        }) => {
            any_over_expr(target, func)
                || any_over_expr(annotation, func)
                || value
                    .as_ref()
                    .map_or(false, |value| any_over_expr(value, func))
        }
        Stmt::For(ast::StmtFor {
            target,
            iter,
            body,
            orelse,
            ..
        })
        | Stmt::AsyncFor(ast::StmtAsyncFor {
            target,
            iter,
            body,
            orelse,
            ..
        }) => {
            any_over_expr(target, func)
                || any_over_expr(iter, func)
                || any_over_body(body, func)
                || any_over_body(orelse, func)
        }
        Stmt::While(ast::StmtWhile {
            test,
            body,
            orelse,
            range: _range,
        }) => any_over_expr(test, func) || any_over_body(body, func) || any_over_body(orelse, func),
        Stmt::If(ast::StmtIf {
            test,
            body,
            elif_else_clauses,
            range: _range,
        }) => {
            any_over_expr(test, func)
                || any_over_body(body, func)
                || elif_else_clauses.iter().any(|clause| {
                    clause
                        .test
                        .as_ref()
                        .map_or(false, |test| any_over_expr(test, func))
                        || any_over_body(&clause.body, func)
                })
        }
        Stmt::With(ast::StmtWith { items, body, .. })
        | Stmt::AsyncWith(ast::StmtAsyncWith { items, body, .. }) => {
            items.iter().any(|with_item| {
                any_over_expr(&with_item.context_expr, func)
                    || with_item
                        .optional_vars
                        .as_ref()
                        .map_or(false, |expr| any_over_expr(expr, func))
            }) || any_over_body(body, func)
        }
        Stmt::Raise(ast::StmtRaise {
            exc,
            cause,
            range: _range,
        }) => {
            exc.as_ref()
                .map_or(false, |value| any_over_expr(value, func))
                || cause
                    .as_ref()
                    .map_or(false, |value| any_over_expr(value, func))
        }
        Stmt::Try(ast::StmtTry {
            body,
            handlers,
            orelse,
            finalbody,
            range: _range,
        })
        | Stmt::TryStar(ast::StmtTryStar {
            body,
            handlers,
            orelse,
            finalbody,
            range: _range,
        }) => {
            any_over_body(body, func)
                || handlers.iter().any(|handler| {
                    let ExceptHandler::ExceptHandler(ast::ExceptHandlerExceptHandler {
                        type_,
                        body,
                        ..
                    }) = handler;
                    type_
                        .as_ref()
                        .map_or(false, |expr| any_over_expr(expr, func))
                        || any_over_body(body, func)
                })
                || any_over_body(orelse, func)
                || any_over_body(finalbody, func)
        }
        Stmt::Assert(ast::StmtAssert {
            test,
            msg,
            range: _range,
        }) => {
            any_over_expr(test, func)
                || msg
                    .as_ref()
                    .map_or(false, |value| any_over_expr(value, func))
        }
        Stmt::Match(ast::StmtMatch {
            subject,
            cases,
            range: _range,
        }) => {
            any_over_expr(subject, func)
                || cases.iter().any(|case| {
                    let MatchCase {
                        pattern,
                        guard,
                        body,
                        range: _range,
                    } = case;
                    any_over_pattern(pattern, func)
                        || guard
                            .as_ref()
                            .map_or(false, |expr| any_over_expr(expr, func))
                        || any_over_body(body, func)
                })
        }
        Stmt::Import(_) => false,
        Stmt::ImportFrom(_) => false,
        Stmt::Global(_) => false,
        Stmt::Nonlocal(_) => false,
        Stmt::Expr(ast::StmtExpr {
            value,
            range: _range,
        }) => any_over_expr(value, func),
        Stmt::Pass(_) | Stmt::Break(_) | Stmt::Continue(_) => false,
        Stmt::LineMagic(_) => false,
    }
}

pub fn any_over_body<F>(body: &[Stmt], func: &F) -> bool
where
    F: Fn(&Expr) -> bool,
{
    body.iter().any(|stmt| any_over_stmt(stmt, func))
}

pub fn is_dunder(id: &str) -> bool {
    id.starts_with("__") && id.ends_with("__")
}

/// Return `true` if the [`Stmt`] is an assignment to a dunder (like `__all__`).
pub fn is_assignment_to_a_dunder(stmt: &Stmt) -> bool {
    // Check whether it's an assignment to a dunder, with or without a type
    // annotation. This is what pycodestyle (as of 2.9.1) does.
    match stmt {
        Stmt::Assign(ast::StmtAssign { targets, .. }) => {
            if let [Expr::Name(ast::ExprName { id, .. })] = targets.as_slice() {
                is_dunder(id)
            } else {
                false
            }
        }
        Stmt::AnnAssign(ast::StmtAnnAssign { target, .. }) => {
            if let Expr::Name(ast::ExprName { id, .. }) = target.as_ref() {
                is_dunder(id)
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Return `true` if the [`Expr`] is a singleton (`None`, `True`, `False`, or
/// `...`).
pub const fn is_singleton(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Constant(ast::ExprConstant {
            value: Constant::None | Constant::Bool(_) | Constant::Ellipsis,
            ..
        })
    )
}

/// Return `true` if the [`Expr`] is a constant or tuple of constants.
pub fn is_constant(expr: &Expr) -> bool {
    match expr {
        Expr::Constant(_) => true,
        Expr::Tuple(ast::ExprTuple { elts, .. }) => elts.iter().all(is_constant),
        _ => false,
    }
}

/// Return `true` if the [`Expr`] is a non-singleton constant.
pub fn is_constant_non_singleton(expr: &Expr) -> bool {
    is_constant(expr) && !is_singleton(expr)
}

/// Return the [`Keyword`] with the given name, if it's present in the list of
/// [`Keyword`] arguments.
pub fn find_keyword<'a>(keywords: &'a [Keyword], keyword_name: &str) -> Option<&'a Keyword> {
    keywords.iter().find(|keyword| {
        let Keyword { arg, .. } = keyword;
        arg.as_ref().map_or(false, |arg| arg == keyword_name)
    })
}

/// Return `true` if an [`Expr`] is `None`.
pub const fn is_const_none(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Constant(ast::ExprConstant {
            value: Constant::None,
            kind: None,
            ..
        }),
    )
}

/// Return `true` if an [`Expr`] is `True`.
pub const fn is_const_true(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Constant(ast::ExprConstant {
            value: Constant::Bool(true),
            kind: None,
            ..
        }),
    )
}

/// Return `true` if an [`Expr`] is `False`.
pub const fn is_const_false(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Constant(ast::ExprConstant {
            value: Constant::Bool(false),
            kind: None,
            ..
        }),
    )
}

/// Return `true` if a keyword argument is present with a non-`None` value.
pub fn has_non_none_keyword(keywords: &[Keyword], keyword: &str) -> bool {
    find_keyword(keywords, keyword).map_or(false, |keyword| {
        let Keyword { value, .. } = keyword;
        !is_const_none(value)
    })
}

/// Extract the names of all handled exceptions.
pub fn extract_handled_exceptions(handlers: &[ExceptHandler]) -> Vec<&Expr> {
    let mut handled_exceptions = Vec::new();
    for handler in handlers {
        match handler {
            ExceptHandler::ExceptHandler(ast::ExceptHandlerExceptHandler { type_, .. }) => {
                if let Some(type_) = type_ {
                    if let Expr::Tuple(ast::ExprTuple { elts, .. }) = &type_.as_ref() {
                        for type_ in elts {
                            handled_exceptions.push(type_);
                        }
                    } else {
                        handled_exceptions.push(type_);
                    }
                }
            }
        }
    }
    handled_exceptions
}

/// Returns `true` if the given name is included in the given [`Arguments`].
pub fn includes_arg_name(name: &str, arguments: &Arguments) -> bool {
    if arguments
        .posonlyargs
        .iter()
        .chain(&arguments.args)
        .chain(&arguments.kwonlyargs)
        .any(|arg| arg.def.arg.as_str() == name)
    {
        return true;
    }
    if let Some(arg) = &arguments.vararg {
        if arg.arg.as_str() == name {
            return true;
        }
    }
    if let Some(arg) = &arguments.kwarg {
        if arg.arg.as_str() == name {
            return true;
        }
    }
    false
}

/// Given an [`Expr`] that can be callable or not (like a decorator, which could
/// be used with or without explicit call syntax), return the underlying
/// callable.
pub fn map_callable(decorator: &Expr) -> &Expr {
    if let Expr::Call(ast::ExprCall { func, .. }) = decorator {
        // Ex) `@decorator()`
        func
    } else {
        // Ex) `@decorator`
        decorator
    }
}

/// Given an [`Expr`] that can be callable or not (like a decorator, which could
/// be used with or without explicit call syntax), return the underlying
/// callable.
pub fn map_subscript(expr: &Expr) -> &Expr {
    if let Expr::Subscript(ast::ExprSubscript { value, .. }) = expr {
        // Ex) `Iterable[T]`
        value
    } else {
        // Ex) `Iterable`
        expr
    }
}

/// Return `true` if the body uses `locals()`, `globals()`, `vars()`, `eval()`.
///
/// Accepts a closure that determines whether a given name (e.g., `"list"`) is a Python builtin.
pub fn uses_magic_variable_access<F>(body: &[Stmt], is_builtin: F) -> bool
where
    F: Fn(&str) -> bool,
{
    any_over_body(body, &|expr| {
        if let Expr::Call(ast::ExprCall { func, .. }) = expr {
            if let Expr::Name(ast::ExprName { id, .. }) = func.as_ref() {
                if matches!(id.as_str(), "locals" | "globals" | "vars" | "exec" | "eval") {
                    if is_builtin(id.as_str()) {
                        return true;
                    }
                }
            }
        }
        false
    })
}

/// Format the module reference name for a relative import.
///
/// # Examples
///
/// ```rust
/// # use ruff_python_ast::helpers::format_import_from;
///
/// assert_eq!(format_import_from(None, None), "".to_string());
/// assert_eq!(format_import_from(Some(1), None), ".".to_string());
/// assert_eq!(format_import_from(Some(1), Some("foo")), ".foo".to_string());
/// ```
pub fn format_import_from(level: Option<u32>, module: Option<&str>) -> String {
    let mut module_name = String::with_capacity(16);
    if let Some(level) = level {
        for _ in 0..level {
            module_name.push('.');
        }
    }
    if let Some(module) = module {
        module_name.push_str(module);
    }
    module_name
}

/// Format the member reference name for a relative import.
///
/// # Examples
///
/// ```rust
/// # use ruff_python_ast::helpers::format_import_from_member;
///
/// assert_eq!(format_import_from_member(None, None, "bar"), "bar".to_string());
/// assert_eq!(format_import_from_member(Some(1), None, "bar"), ".bar".to_string());
/// assert_eq!(format_import_from_member(Some(1), Some("foo"), "bar"), ".foo.bar".to_string());
/// ```
pub fn format_import_from_member(level: Option<u32>, module: Option<&str>, member: &str) -> String {
    let mut qualified_name = String::with_capacity(
        (level.unwrap_or(0) as usize)
            + module.as_ref().map_or(0, |module| module.len())
            + 1
            + member.len(),
    );
    if let Some(level) = level {
        for _ in 0..level {
            qualified_name.push('.');
        }
    }
    if let Some(module) = module {
        qualified_name.push_str(module);
        qualified_name.push('.');
    }
    qualified_name.push_str(member);
    qualified_name
}

/// Create a module path from a (package, path) pair.
///
/// For example, if the package is `foo/bar` and the path is `foo/bar/baz.py`,
/// the call path is `["baz"]`.
pub fn to_module_path(package: &Path, path: &Path) -> Option<Vec<String>> {
    path.strip_prefix(package.parent()?)
        .ok()?
        .iter()
        .map(Path::new)
        .map(Path::file_stem)
        .map(|path| path.and_then(|path| path.to_os_string().into_string().ok()))
        .collect::<Option<Vec<String>>>()
}

/// Create a [`CallPath`] from a relative import reference name (like `".foo.bar"`).
///
/// Returns an empty [`CallPath`] if the import is invalid (e.g., a relative import that
/// extends beyond the top-level module).
///
/// # Examples
///
/// ```rust
/// # use smallvec::{smallvec, SmallVec};
/// # use ruff_python_ast::helpers::from_relative_import;
///
/// assert_eq!(from_relative_import(&[], "bar"), SmallVec::from_buf(["bar"]));
/// assert_eq!(from_relative_import(&["foo".to_string()], "bar"), SmallVec::from_buf(["foo", "bar"]));
/// assert_eq!(from_relative_import(&["foo".to_string()], "bar.baz"), SmallVec::from_buf(["foo", "bar", "baz"]));
/// assert_eq!(from_relative_import(&["foo".to_string()], ".bar"), SmallVec::from_buf(["bar"]));
/// assert!(from_relative_import(&["foo".to_string()], "..bar").is_empty());
/// assert!(from_relative_import(&["foo".to_string()], "...bar").is_empty());
/// ```
pub fn from_relative_import<'a>(module: &'a [String], name: &'a str) -> CallPath<'a> {
    let mut call_path: CallPath = SmallVec::with_capacity(module.len() + 1);

    // Start with the module path.
    call_path.extend(module.iter().map(String::as_str));

    // Remove segments based on the number of dots.
    for _ in 0..name.chars().take_while(|c| *c == '.').count() {
        if call_path.is_empty() {
            return SmallVec::new();
        }
        call_path.pop();
    }

    // Add the remaining segments.
    call_path.extend(name.trim_start_matches('.').split('.'));

    call_path
}

pub fn from_relative_import_parts<'a>(
    module_path: &'a [String],
    level: Option<u32>,
    module: Option<&'a str>,
    member: &'a str,
) -> Option<CallPath<'a>> {
    let mut call_path: CallPath = SmallVec::with_capacity(module_path.len() + 1);

    // Remove segments based on the number of dots.
    if let Some(level) = level {
        if level > 0 {
            call_path.extend(module_path.iter().map(String::as_str));

            for _ in 0..level {
                if call_path.is_empty() {
                    break;
                }
                call_path.pop();
            }
        }
    }

    // Add the remaining segments.
    if let Some(module) = module {
        call_path.extend(module.split('.'));
    }

    // Add the member.
    call_path.push(member);

    Some(call_path)
}

pub fn literal_path<'a>(
    level: Option<u32>,
    module: Option<&'a str>,
    member: &'a str,
) -> CallPath<'a> {
    let mut call_path: CallPath = SmallVec::with_capacity(
        level.unwrap_or_default() as usize
            + module
                .map(|module| module.split('.').count())
                .unwrap_or_default()
            + 1,
    );

    // Include the dots
    if let Some(level) = level {
        if level > 0 {
            for _ in 0..level {
                call_path.push(".");
            }
        }
    }

    // Add the remaining segments.
    if let Some(module) = module {
        call_path.extend(module.split('.'));
    }

    // Add the member.
    call_path.push(member);

    call_path
}

/// Given an imported module (based on its relative import level and module name), return the
/// fully-qualified module path.
pub fn resolve_imported_module_path<'a>(
    level: Option<u32>,
    module: Option<&'a str>,
    module_path: Option<&[String]>,
) -> Option<Cow<'a, str>> {
    let Some(level) = level else {
        return Some(Cow::Borrowed(module.unwrap_or("")));
    };

    if level == 0 {
        return Some(Cow::Borrowed(module.unwrap_or("")));
    }

    let Some(module_path) = module_path else {
        return None;
    };

    if level as usize >= module_path.len() {
        return None;
    }

    let mut qualified_path = module_path[..module_path.len() - level as usize].join(".");
    if let Some(module) = module {
        if !qualified_path.is_empty() {
            qualified_path.push('.');
        }
        qualified_path.push_str(module);
    }
    Some(Cow::Owned(qualified_path))
}

/// A [`StatementVisitor`] that collects all `return` statements in a function or method.
#[derive(Default)]
pub struct ReturnStatementVisitor<'a> {
    pub returns: Vec<&'a ast::StmtReturn>,
}

impl<'a, 'b> StatementVisitor<'b> for ReturnStatementVisitor<'a>
where
    'b: 'a,
{
    fn visit_stmt(&mut self, stmt: &'b Stmt) {
        match stmt {
            Stmt::FunctionDef(_) | Stmt::AsyncFunctionDef(_) | Stmt::ClassDef(_) => {
                // Don't recurse.
            }
            Stmt::Return(stmt) => self.returns.push(stmt),
            _ => walk_stmt(self, stmt),
        }
    }
}

/// A [`StatementVisitor`] that collects all `raise` statements in a function or method.
#[derive(Default)]
pub struct RaiseStatementVisitor<'a> {
    pub raises: Vec<(TextRange, Option<&'a Expr>, Option<&'a Expr>)>,
}

impl<'a, 'b> StatementVisitor<'b> for RaiseStatementVisitor<'b>
where
    'b: 'a,
{
    fn visit_stmt(&mut self, stmt: &'b Stmt) {
        match stmt {
            Stmt::Raise(ast::StmtRaise {
                exc,
                cause,
                range: _range,
            }) => {
                self.raises
                    .push((stmt.range(), exc.as_deref(), cause.as_deref()));
            }
            Stmt::ClassDef(_)
            | Stmt::FunctionDef(_)
            | Stmt::AsyncFunctionDef(_)
            | Stmt::Try(_)
            | Stmt::TryStar(_) => {}
            Stmt::If(ast::StmtIf {
                body,
                elif_else_clauses,
                ..
            }) => {
                walk_body(self, body);
                for clause in elif_else_clauses {
                    self.visit_elif_else_clause(clause);
                }
            }
            Stmt::While(ast::StmtWhile { body, .. })
            | Stmt::With(ast::StmtWith { body, .. })
            | Stmt::AsyncWith(ast::StmtAsyncWith { body, .. })
            | Stmt::For(ast::StmtFor { body, .. })
            | Stmt::AsyncFor(ast::StmtAsyncFor { body, .. }) => {
                walk_body(self, body);
            }
            Stmt::Match(ast::StmtMatch { cases, .. }) => {
                for case in cases {
                    walk_body(self, &case.body);
                }
            }
            _ => {}
        }
    }
}

/// Return `true` if a `Stmt` is a docstring.
pub fn is_docstring_stmt(stmt: &Stmt) -> bool {
    if let Stmt::Expr(ast::StmtExpr {
        value,
        range: _range,
    }) = stmt
    {
        matches!(
            value.as_ref(),
            Expr::Constant(ast::ExprConstant {
                value: Constant::Str { .. },
                ..
            })
        )
    } else {
        false
    }
}

/// A representation of a function call's positional and keyword arguments that ignores
/// starred expressions.
#[derive(Default)]
pub struct CallArguments<'a> {
    args: &'a [Expr],
    keywords: &'a [Keyword],
}

impl<'a> CallArguments<'a> {
    pub fn new(args: &'a [Expr], keywords: &'a [Keyword]) -> Self {
        Self { args, keywords }
    }

    /// Get the argument with the given name or position, or `None` if no such
    /// argument exists.
    pub fn argument(&self, name: &str, position: usize) -> Option<&'a Expr> {
        self.keywords
            .iter()
            .find(|keyword| {
                let Keyword { arg, .. } = keyword;
                arg.as_ref().map_or(false, |arg| arg == name)
            })
            .map(|keyword| &keyword.value)
            .or_else(|| {
                self.args
                    .iter()
                    .take_while(|expr| !expr.is_starred_expr())
                    .nth(position)
            })
    }

    /// Return the number of arguments.
    pub fn len(&self) -> usize {
        self.args.len() + self.keywords.len()
    }

    /// Return `true` if there are no arguments.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Return the number of positional arguments.
    pub fn num_args(&self) -> usize {
        self.args
            .iter()
            .take_while(|expr| !expr.is_starred_expr())
            .count()
    }

    /// Return the number of keyword arguments.
    pub fn num_kwargs(&self) -> usize {
        self.keywords
            .iter()
            .filter(|keyword| keyword.arg.is_some())
            .count()
    }
}

/// Check if a node is part of a conditional branch.
pub fn on_conditional_branch<'a>(parents: &mut impl Iterator<Item = &'a Stmt>) -> bool {
    parents.any(|parent| {
        if matches!(parent, Stmt::If(_) | Stmt::While(_) | Stmt::Match(_)) {
            return true;
        }
        if let Stmt::Expr(ast::StmtExpr {
            value,
            range: _range,
        }) = parent
        {
            if value.is_if_exp_expr() {
                return true;
            }
        }
        false
    })
}

/// Check if a node is in a nested block.
pub fn in_nested_block<'a>(mut parents: impl Iterator<Item = &'a Stmt>) -> bool {
    parents.any(|parent| {
        matches!(
            parent,
            Stmt::Try(_) | Stmt::TryStar(_) | Stmt::If(_) | Stmt::With(_) | Stmt::Match(_)
        )
    })
}

/// Check if a node represents an unpacking assignment.
pub fn is_unpacking_assignment(parent: &Stmt, child: &Expr) -> bool {
    match parent {
        Stmt::With(ast::StmtWith { items, .. }) => items.iter().any(|item| {
            if let Some(optional_vars) = &item.optional_vars {
                if optional_vars.is_tuple_expr() {
                    if any_over_expr(optional_vars, &|expr| expr == child) {
                        return true;
                    }
                }
            }
            false
        }),
        Stmt::Assign(ast::StmtAssign { targets, value, .. }) => {
            // In `(a, b) = (1, 2)`, `(1, 2)` is the target, and it is a tuple.
            let value_is_tuple = matches!(
                value.as_ref(),
                Expr::Set(_) | Expr::List(_) | Expr::Tuple(_)
            );
            // In `(a, b) = coords = (1, 2)`, `(a, b)` and `coords` are the targets, and
            // `(a, b)` is a tuple. (We use "tuple" as a placeholder for any
            // unpackable expression.)
            let targets_are_tuples = targets
                .iter()
                .all(|item| matches!(item, Expr::Set(_) | Expr::List(_) | Expr::Tuple(_)));
            // If we're looking at `a` in `(a, b) = coords = (1, 2)`, then we should
            // identify that the current expression is in a tuple.
            let child_in_tuple = targets_are_tuples
                || targets.iter().any(|item| {
                    matches!(item, Expr::Set(_) | Expr::List(_) | Expr::Tuple(_))
                        && any_over_expr(item, &|expr| expr == child)
                });

            // If our child is a tuple, and value is not, it's always an unpacking
            // expression. Ex) `x, y = tup`
            if child_in_tuple && !value_is_tuple {
                return true;
            }

            // If our child isn't a tuple, but value is, it's never an unpacking expression.
            // Ex) `coords = (1, 2)`
            if !child_in_tuple && value_is_tuple {
                return false;
            }

            // If our target and the value are both tuples, then it's an unpacking
            // expression assuming there's at least one non-tuple child.
            // Ex) Given `(x, y) = coords = 1, 2`, `(x, y)` is considered an unpacking
            // expression. Ex) Given `(x, y) = (a, b) = 1, 2`, `(x, y)` isn't
            // considered an unpacking expression.
            if child_in_tuple && value_is_tuple {
                return !targets_are_tuples;
            }

            false
        }
        _ => false,
    }
}

#[derive(Copy, Clone, Debug, PartialEq, is_macro::Is)]
pub enum Truthiness {
    // An expression evaluates to `False`.
    Falsey,
    // An expression evaluates to `True`.
    Truthy,
    // An expression evaluates to an unknown value (e.g., a variable `x` of unknown type).
    Unknown,
}

impl From<Option<bool>> for Truthiness {
    fn from(value: Option<bool>) -> Self {
        match value {
            Some(true) => Truthiness::Truthy,
            Some(false) => Truthiness::Falsey,
            None => Truthiness::Unknown,
        }
    }
}

impl From<Truthiness> for Option<bool> {
    fn from(truthiness: Truthiness) -> Self {
        match truthiness {
            Truthiness::Truthy => Some(true),
            Truthiness::Falsey => Some(false),
            Truthiness::Unknown => None,
        }
    }
}

impl Truthiness {
    pub fn from_expr<F>(expr: &Expr, is_builtin: F) -> Self
    where
        F: Fn(&str) -> bool,
    {
        match expr {
            Expr::Constant(ast::ExprConstant { value, .. }) => match value {
                Constant::Bool(value) => Some(*value),
                Constant::None => Some(false),
                Constant::Str(string) => Some(!string.is_empty()),
                Constant::Bytes(bytes) => Some(!bytes.is_empty()),
                Constant::Int(int) => Some(!int.is_zero()),
                Constant::Float(float) => Some(*float != 0.0),
                Constant::Complex { real, imag } => Some(*real != 0.0 || *imag != 0.0),
                Constant::Ellipsis => Some(true),
            },
            Expr::JoinedStr(ast::ExprJoinedStr {
                values,
                range: _range,
            }) => {
                if values.is_empty() {
                    Some(false)
                } else if values.iter().any(|value| {
                    let Expr::Constant(ast::ExprConstant {
                        value: Constant::Str(string),
                        ..
                    }) = &value
                    else {
                        return false;
                    };
                    !string.is_empty()
                }) {
                    Some(true)
                } else {
                    None
                }
            }
            Expr::List(ast::ExprList {
                elts,
                range: _range,
                ..
            })
            | Expr::Set(ast::ExprSet {
                elts,
                range: _range,
            })
            | Expr::Tuple(ast::ExprTuple {
                elts,
                range: _range,
                ..
            }) => Some(!elts.is_empty()),
            Expr::Dict(ast::ExprDict {
                keys,
                range: _range,
                ..
            }) => Some(!keys.is_empty()),
            Expr::Call(ast::ExprCall {
                func,
                args,
                keywords,
                range: _range,
            }) => {
                if let Expr::Name(ast::ExprName { id, .. }) = func.as_ref() {
                    if is_iterable_initializer(id.as_str(), |id| is_builtin(id)) {
                        if args.is_empty() && keywords.is_empty() {
                            // Ex) `list()`
                            Some(false)
                        } else if args.len() == 1 && keywords.is_empty() {
                            // Ex) `list([1, 2, 3])`
                            Self::from_expr(&args[0], is_builtin).into()
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            _ => None,
        }
        .into()
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::cell::RefCell;
    use std::vec;

    use crate::{
        Constant, Expr, ExprConstant, ExprContext, ExprName, Identifier, Stmt, StmtTypeAlias,
        TypeParam, TypeParamParamSpec, TypeParamTypeVar, TypeParamTypeVarTuple,
    };
    use ruff_text_size::TextRange;

    use crate::helpers::{any_over_stmt, any_over_type_param, resolve_imported_module_path};

    #[test]
    fn resolve_import() {
        // Return the module directly.
        assert_eq!(
            resolve_imported_module_path(None, Some("foo"), None),
            Some(Cow::Borrowed("foo"))
        );

        // Construct the module path from the calling module's path.
        assert_eq!(
            resolve_imported_module_path(
                Some(1),
                Some("foo"),
                Some(&["bar".to_string(), "baz".to_string()])
            ),
            Some(Cow::Owned("bar.foo".to_string()))
        );

        // We can't return the module if it's a relative import, and we don't know the calling
        // module's path.
        assert_eq!(
            resolve_imported_module_path(Some(1), Some("foo"), None),
            None
        );

        // We can't return the module if it's a relative import, and the path goes beyond the
        // calling module's path.
        assert_eq!(
            resolve_imported_module_path(Some(1), Some("foo"), Some(&["bar".to_string()])),
            None,
        );
        assert_eq!(
            resolve_imported_module_path(Some(2), Some("foo"), Some(&["bar".to_string()])),
            None
        );
    }

    #[test]
    fn any_over_stmt_type_alias() {
        let seen = RefCell::new(Vec::new());
        let name = Expr::Name(ExprName {
            id: "x".to_string(),
            range: TextRange::default(),
            ctx: ExprContext::Load,
        });
        let constant_one = Expr::Constant(ExprConstant {
            value: Constant::Int(1.into()),
            kind: Some("x".to_string()),
            range: TextRange::default(),
        });
        let constant_two = Expr::Constant(ExprConstant {
            value: Constant::Int(2.into()),
            kind: Some("y".to_string()),
            range: TextRange::default(),
        });
        let constant_three = Expr::Constant(ExprConstant {
            value: Constant::Int(3.into()),
            kind: Some("z".to_string()),
            range: TextRange::default(),
        });
        let type_var_one = TypeParam::TypeVar(TypeParamTypeVar {
            range: TextRange::default(),
            bound: Some(Box::new(constant_one.clone())),
            name: Identifier::new("x", TextRange::default()),
        });
        let type_var_two = TypeParam::TypeVar(TypeParamTypeVar {
            range: TextRange::default(),
            bound: Some(Box::new(constant_two.clone())),
            name: Identifier::new("x", TextRange::default()),
        });
        let type_alias = Stmt::TypeAlias(StmtTypeAlias {
            name: Box::new(name.clone()),
            type_params: vec![type_var_one, type_var_two],
            value: Box::new(constant_three.clone()),
            range: TextRange::default(),
        });
        assert!(!any_over_stmt(&type_alias, &|expr| {
            seen.borrow_mut().push(expr.clone());
            false
        }));
        assert_eq!(
            seen.take(),
            vec![name, constant_one, constant_two, constant_three]
        );
    }

    #[test]
    fn any_over_type_param_type_var() {
        let type_var_no_bound = TypeParam::TypeVar(TypeParamTypeVar {
            range: TextRange::default(),
            bound: None,
            name: Identifier::new("x", TextRange::default()),
        });
        assert!(!any_over_type_param(&type_var_no_bound, &|_expr| true));

        let bound = Expr::Constant(ExprConstant {
            value: Constant::Int(1.into()),
            kind: Some("x".to_string()),
            range: TextRange::default(),
        });

        let type_var_with_bound = TypeParam::TypeVar(TypeParamTypeVar {
            range: TextRange::default(),
            bound: Some(Box::new(bound.clone())),
            name: Identifier::new("x", TextRange::default()),
        });
        assert!(
            any_over_type_param(&type_var_with_bound, &|expr| {
                assert_eq!(
                    *expr, bound,
                    "the received expression should be the unwrapped bound"
                );
                true
            }),
            "if true is returned from `func` it should be respected"
        );
    }

    #[test]
    fn any_over_type_param_type_var_tuple() {
        let type_var_tuple = TypeParam::TypeVarTuple(TypeParamTypeVarTuple {
            range: TextRange::default(),
            name: Identifier::new("x", TextRange::default()),
        });
        assert!(
            !any_over_type_param(&type_var_tuple, &|_expr| true),
            "type var tuples have no expressions to visit"
        );
    }

    #[test]
    fn any_over_type_param_param_spec() {
        let type_param_spec = TypeParam::ParamSpec(TypeParamParamSpec {
            range: TextRange::default(),
            name: Identifier::new("x", TextRange::default()),
        });
        assert!(
            !any_over_type_param(&type_param_spec, &|_expr| true),
            "param specs have no expressions to visit"
        );
    }
}
