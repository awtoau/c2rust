//! This module implements the translation of functions in C.

use super::*;
use c2rust_ast_builder::CaptureBy;
use failure::format_err;
use proc_macro2::{Literal, TokenStream, TokenTree};
use syn::__private::ToTokens;

impl<'c> Translation<'c> {
    pub fn convert_function(
        &self,
        ctx: ExprContext,
        decl_id: CDeclId,
        span: Span,
        is_global: bool,
        is_inline: bool,
        is_extern: bool,
        typ: CTypeId,
        name: &str,
        parameters: &[CDeclId],
        body: Option<CStmtId>,
        attrs: &IndexSet<c_ast::Attribute>,
    ) -> TranslationResult<ConvertedDecl> {
        let new_name = &self
            .renamer
            .borrow()
            .get(&decl_id)
            .expect("Functions should already be renamed");

        if self.import_simd_function(new_name)? {
            return Ok(ConvertedDecl::NoItem);
        }

        let (ret, is_variadic): (Option<CQualTypeId>, bool) =
            match self.ast_context.resolve_type(typ).kind {
                CTypeKind::Function(ret, _, is_var, is_noreturn, _) => {
                    (if is_noreturn { None } else { Some(ret) }, is_var)
                }
                ref k => {
                    return Err(format_err!(
                        "Type of function {:?} was not a function type, got {:?}",
                        decl_id,
                        k
                    )
                    .into());
                }
            };

        let mut args: Vec<(CDeclId, String, CQualTypeId)> = vec![];
        for param_id in parameters {
            if let CDeclKind::Variable { ref ident, typ, .. } =
                self.ast_context.index(*param_id).kind
            {
                args.push((*param_id, ident.clone(), typ))
            } else {
                return Err(TranslationError::generic(
                    "Parameter is not variable declaration",
                ));
            }
        }

        let is_main = self.ast_context.c_main == Some(decl_id);

        let converted_function = self.convert_function_inner(
            ctx,
            decl_id,
            span,
            is_global,
            is_inline,
            is_main,
            is_variadic,
            is_extern,
            new_name,
            name,
            &args,
            ret,
            body,
            attrs,
        );

        converted_function.or_else(|e| match self.tcfg.replace_unsupported_decls {
            // `body.is_none()` originally covered only functions that never
            // had a body to begin with (e.g. a prototype whose parameter
            // type failed to convert) - retrying with `body: None` there is
            // a no-op-shaped fallback that just re-attempts the same
            // declaration-only path.
            //
            // Also take this path when a body *was* present but failed to
            // convert (e.g. `Cannot translate GNU asm goto ...`, see
            // `assembly.rs`'s `convert_asm`): dropping the whole decl in
            // that case, as happened before this arm existed, leaves every
            // other declaration in the file that calls this function with a
            // dangling reference to a symbol that was never declared at
            // all - not a translation gap callers can spot by inspection,
            // but a hard compile error one file-scope decl away from the
            // real problem. Falling back to an `extern "C"` declaration
            // instead keeps that call site type-checking (same as any other
            // genuinely-external C symbol c2rust doesn't have a definition
            // for) and confines the actual information loss to precisely
            // the one function whose body failed, which is what the
            // existing "containing function is skipped with a warning"
            // comment on the asm-goto check already claims happens.
            ReplaceMode::Extern => {
                if body.is_some() {
                    log::warn!(
                        "Falling back to an extern declaration for '{}': body failed to translate: {}",
                        name, e
                    );
                }
                self.convert_function_inner(
                    ctx,
                    decl_id,
                    span,
                    is_global,
                    false,
                    is_main,
                    is_variadic,
                    is_extern,
                    new_name,
                    name,
                    &args,
                    ret,
                    None,
                    attrs,
                )
            }
            _ => Err(e),
        })
    }

    fn convert_function_inner(
        &self,
        ctx: ExprContext,
        decl_id: CDeclId,
        span: Span,
        is_global: bool,
        is_inline: bool,
        is_main: bool,
        is_variadic: bool,
        is_extern: bool,
        new_name: &str,
        name: &str,
        arguments: &[(CDeclId, String, CQualTypeId)],
        return_type: Option<CQualTypeId>,
        body: Option<CStmtId>,
        attrs: &IndexSet<c_ast::Attribute>,
    ) -> TranslationResult<ConvertedDecl> {
        self.function_context.borrow_mut().enter_new(name);

        self.with_scope(|| {
            let mut args: Vec<FnArg> = vec![];

            // handle regular (non-variadic) arguments
            for &(decl_id, ref var, typ) in arguments {
                let ConvertedFunctionParam { ty, mutbl } = self.convert_function_param(ctx, typ)?;

                let pat = if var.is_empty() {
                    mk().wild_pat()
                } else {
                    // extern function declarations don't support/require mut patterns
                    let mutbl = if body.is_none() {
                        Mutability::Immutable
                    } else {
                        mutbl
                    };

                    let new_var = self
                        .renamer
                        .borrow_mut()
                        .insert(decl_id, var.as_str())
                        .unwrap_or_else(|| {
                            panic!(
                                "Failed to insert argument '{}' while converting '{}'",
                                var, name
                            )
                        });

                    mk().set_mutbl(mutbl).ident_pat(new_var)
                };

                args.push(mk().arg(ty, pat))
            }

            let variadic = if is_variadic {
                // function definitions
                let mut builder = mk();
                let arg_va_list_name = if let Some(body_id) = body {
                    // FIXME: detect mutability requirements.
                    builder = builder.set_mutbl(Mutability::Mutable);
                    Some(self.register_va_decls(body_id))
                } else {
                    None
                };

                Some(builder.variadic_arg(arg_va_list_name))
            } else {
                None
            };

            // handle return type
            let ret = match return_type {
                Some(return_type) => self.convert_type(return_type.ctype)?,
                None => mk().never_ty(),
            };
            let is_void_ret = return_type
                .map(|qty| self.ast_context[qty.ctype].kind == CTypeKind::Void)
                .unwrap_or(false);

            // If a return type is void, we should instead omit the unit type return,
            // -> (), to be more idiomatic
            let ret = if is_void_ret {
                ReturnType::Default
            } else {
                ReturnType::Type(Default::default(), ret)
            };

            let decl = mk().fn_decl(new_name, args, variadic, ret);

            if let Some(body) = body {
                // Translating an actual function

                let ret = match return_type {
                    Some(return_type) => {
                        let ret_type_id: CTypeId =
                            self.ast_context.resolve_type_id(return_type.ctype);
                        if let CTypeKind::Void = self.ast_context.index(ret_type_id).kind {
                            cfg::ImplicitReturnType::Void
                        } else if is_main {
                            cfg::ImplicitReturnType::Main
                        } else {
                            cfg::ImplicitReturnType::NoImplicitReturnType
                        }
                    }
                    _ => cfg::ImplicitReturnType::Void,
                };

                let mut body_stmts = vec![];
                for &(_, _, typ) in arguments {
                    body_stmts.append(&mut self.compute_variable_array_sizes(ctx, typ.ctype)?);
                }

                let mut converted_body = if let Some(register_accessor_body) =
                    self.register_var_accessor_body(decl_id)?
                {
                    register_accessor_body
                } else if let Some(bit_scan_body) =
                    self.bit_scan_idiom_body(name, arguments, return_type)
                {
                    bit_scan_body
                } else {
                    let body_ids = match self.ast_context.index(body).kind {
                        CStmtKind::Compound(ref stmts) => stmts,
                        _ => panic!("function body expects to be a compound statement"),
                    };
                    let mut converted_body =
                        self.convert_block_with_scope(ctx, name, body_ids, return_type, ret)?;
                    strip_tail_return(&mut converted_body);
                    converted_body
                };

                // If `alloca` was used in the function body, include a variable to hold the
                // allocations.
                if let Some(alloca_allocations_name) = self
                    .function_context
                    .borrow_mut()
                    .alloca_allocations_name
                    .take()
                {
                    // let mut c2rust_alloca_allocations: Vec<Vec<u8>> = Vec::new();
                    let inner_vec = mk().path_ty(vec![mk().path_segment_with_args(
                        "Vec",
                        mk().angle_bracketed_args(vec![mk().ident_ty("u8")]),
                    )]);
                    let outer_vec = mk().path_ty(vec![mk().path_segment_with_args(
                        "Vec",
                        mk().angle_bracketed_args(vec![inner_vec]),
                    )]);
                    let alloca_allocations_stmt = mk().local_stmt(Box::new(mk().local(
                        mk().mutbl().ident_pat(alloca_allocations_name),
                        Some(outer_vec),
                        Some(mk().call_expr(mk().path_expr(vec!["Vec", "new"]), vec![])),
                    )));

                    body_stmts.push(alloca_allocations_stmt);
                }

                body_stmts.append(&mut converted_body);
                let mut block = stmts_block(body_stmts);
                if let Some(span) = self.get_span(SomeId::Stmt(body)) {
                    block.set_span(span);
                }

                // c99 extern inline functions should be pub, but not gnu_inline attributed
                // extern inlines, which become subject to their gnu89 visibility (private)
                let is_extern_inline =
                    is_inline && is_extern && !attrs.contains(&c_ast::Attribute::GnuInline);

                // Only add linkage attributes if the function is `extern`
                let mut mk_ = if is_main {
                    // Cross-check this function as if it was called `main`
                    // FIXME: pass in a vector of NestedMetaItem elements,
                    // but strings have to do for now
                    self.mk_cross_check(mk(), vec!["entry(djb2=\"main\")", "exit(djb2=\"main\")"])
                } else if (is_global && !is_inline) || is_extern_inline {
                    // A function that would otherwise get a plain
                    // `#[no_mangle]` (i.e. the renamer left its name
                    // unchanged — same condition `mk_linkage` itself uses
                    // to choose the `no_mangle` vs `export_name` branch)
                    // gets the kernel's signature-checked `#[export]`
                    // attribute instead, but ONLY when the C original was
                    // `EXPORT_SYMBOL_GPL`/etc: `#[export]` unconditionally
                    // emits `EXPORT_SYMBOL_GPL` semantics
                    // (`rust/macros/export.rs` has no non-GPL variant), so
                    // applying it to a plain-`EXPORT_SYMBOL` (non-GPL)
                    // original would silently tighten that export's
                    // license — see `KernelIdiomRule::ExportSymbol`.
                    if new_name == name
                        && self
                            .tcfg
                            .kernel_idiom_rules
                            .is_enabled(crate::KernelIdiomRule::ExportSymbol)
                        && self.is_gpl_export_symbol(name)
                    {
                        self.with_cur_file_item_store(|item_store| {
                            item_store.add_use(true, vec!["macros".into()], "export");
                        });
                        mk().unsafety(attr_unsafety(self.tcfg.edition))
                            .single_attr("export")
                            .unsafety(Unsafety::Normal)
                            .extern_("C")
                            .pub_()
                    } else {
                        mk_linkage(false, new_name, name, self.tcfg.edition)
                            .extern_("C")
                            .pub_()
                    }
                } else if self.cur_file.get().is_some() {
                    mk().extern_("C").pub_()
                } else {
                    mk().extern_("C")
                };

                // In Edition2024, `unsafe_op_in_unsafe_fn` is deny-by-default so we emit an allow pragma
                // to silence warnings. Was this overridden by the `--deny_unsafe_op_in_unsafe_fn` flag?
                if self.tcfg.deny_unsafe_op_in_unsafe_fn {
                    mk_ = mk_.deny_unsafe_op_in_unsafe_fn();
                }

                for attr in attrs {
                    mk_ = match attr {
                        c_ast::Attribute::AlwaysInline => mk_.call_attr("inline", vec!["always"]),
                        c_ast::Attribute::Cold => mk_.single_attr("cold"),
                        c_ast::Attribute::NoInline => mk_.call_attr("inline", vec!["never"]),
                        _ => continue,
                    };
                }

                // If this function is just a regular inline
                if is_inline && !attrs.contains(&c_ast::Attribute::AlwaysInline) {
                    mk_ = mk_.single_attr("inline");

                    // * In C99, a function defined inline will never, and a function defined extern
                    //   inline will always, emit an externally visible function.
                    // * If a non-static function is declared inline, then it must be defined in the
                    //   same translation unit. The inline definition that does not use extern is
                    //   not externally visible and does not prevent other translation units from
                    //   defining the same function. This makes the inline keyword an alternative to
                    //   static for defining functions inside header files, which may be included in
                    //   multiple translation units of the same program.
                    // * always_inline implies inline -
                    //   https://gcc.gnu.org/ml/gcc-help/2007-01/msg00051.html
                    //   even if the `inline` keyword isn't present
                    // * gnu_inline instead applies gnu89 rules. extern inline will not emit an
                    //   externally visible function.
                    if is_global && is_extern && !attrs.contains(&c_ast::Attribute::GnuInline) {
                        self.use_feature("linkage");
                        // ensures that public inlined rust function can be used in other modules
                        mk_ = mk_.str_attr("linkage", "external");
                    }
                    // NOTE: it does not seem necessary to have an else branch here that
                    // specifies internal linkage in all other cases due to name mangling by rustc.
                }

                Ok(ConvertedDecl::Item(
                    mk_.span(span).unsafe_().fn_item(decl, block),
                ))
            } else {
                // Translating an extern function declaration
                let mut mk_ = mk_linkage(true, new_name, name, self.tcfg.edition).span(span);

                // When putting extern fns into submodules, they need to be public to be accessible
                if self.tcfg.reorganize_definitions {
                    mk_ = mk_.pub_();
                };

                for attr in attrs {
                    mk_ = match attr {
                        c_ast::Attribute::Alias(aliasee) => mk_.str_attr("link_name", aliasee),
                        _ => continue,
                    };
                }

                let mk_ = mk_.unsafety(extern_block_unsafety(self.tcfg.edition));
                let function_decl = mk_.fn_foreign_item(decl);

                Ok(ConvertedDecl::ForeignItem(function_decl))
            }
        })
    }

    /// If `decl_id` is a same-TU accessor for a register-variable-extension
    /// variable (see `find_register_var_accessors`'s doc comment - the
    /// `return riscv_current_is_tp;`-shaped `get_current()` pattern from
    /// arch/riscv's asm/current.h, awtoau/c2rust#22), return a one-statement
    /// body that reads the register directly via inline asm instead of the
    /// caller falling through to the normal translation (which would read
    /// back a fabricated, always-null `.bss` static — see
    /// `find_register_var_accessors` and the `Variable` arm of
    /// `convert_decl` for the other half of this fix).
    ///
    /// This project's only confirmed use case is RISC-V (`tp`/`sp`
    /// register-variable current-pointer bindings). For any other target
    /// the register/mnemonic mapping is unverified, so rather than guess an
    /// asm mnemonic that might be silently wrong, this returns `None` and
    /// lets the caller fall through to today's fabricated-static behavior;
    /// the accessor keeps compiling, it just isn't fixed for that target.
    fn register_var_accessor_body(
        &self,
        decl_id: CDeclId,
    ) -> TranslationResult<Option<Vec<Stmt>>> {
        let Some(fix) = self
            .register_var_accessors
            .borrow()
            .get(&decl_id)
            .cloned()
        else {
            return Ok(None);
        };

        if !self.ast_context.target.starts_with("riscv") {
            // Non-RISC-V target: the register name was captured, but this
            // fork has no evidence for what asm mnemonic/register-class
            // convention is correct there. Leave a breadcrumb in the
            // generated output rather than emitting an unverified guess or
            // silently doing nothing.
            log::warn!(
                "register-variable accessor detected for '{}' (register \"{}\") on non-RISC-V \
                 target '{}': leaving today's fabricated-static behavior in place, since this \
                 fork only has verified inline-asm codegen for RISC-V. See awtoau/c2rust#22.",
                decl_id.0,
                fix.register_name,
                self.ast_context.target
            );
            return Ok(None);
        }

        let ret_ty = self.convert_type(fix.return_type.ctype)?;

        // `Renamer::pick_name` asserts its basename starts with
        // "c2rust_"/"C2Rust_" (see `check_c2rust_name` in renamer.rs) -
        // matches the convention `convert_asm` already uses for its own
        // synthesized temporaries (e.g. "c2rust_input").
        let result_name = self.renamer.borrow_mut().pick_name("c2rust_result");

        // let result: <ty>;
        let let_result = mk().local_stmt(Box::new(mk().local(
            mk().ident_pat(&result_name),
            Some(ret_ty),
            None,
        )));

        // asm!("mv {0}, <reg>", out(reg) result, options(nomem, nostack, preserves_flags));
        let asm_template = format!("mv {{0}}, {}", fix.register_name);
        let mut tokens: Vec<TokenTree> = vec![TokenTree::Literal(Literal::string(&asm_template))];
        tokens.push(TokenTree::Punct(Punct::new(',', Alone)));
        tokens.extend(mk().ident_expr("out").to_token_stream());
        tokens.extend(mk().paren_expr(mk().ident_expr("reg")).to_token_stream());
        tokens.extend(mk().ident_expr(&result_name).to_token_stream());
        tokens.push(TokenTree::Punct(Punct::new(',', Alone)));
        tokens.extend(
            mk().call_expr(
                mk().ident_expr("options"),
                vec![
                    mk().ident_expr("nomem"),
                    mk().ident_expr("nostack"),
                    mk().ident_expr("preserves_flags"),
                ],
            )
            .to_token_stream(),
        );

        self.with_cur_file_item_store(|item_store| {
            item_store.add_use(true, vec!["core".into(), "arch".into()], "asm");
        });

        let asm_mac = mk().mac(
            mk().path(vec!["asm"]),
            tokens.into_iter().collect::<TokenStream>(),
            MacroDelimiter::Paren(Default::default()),
        );
        let asm_stmt = mk().semi_stmt(mk().mac_expr(asm_mac));

        let result_expr = mk().ident_expr(&result_name);

        // unsafe { let result: <ty>; asm!(...); result }
        let unsafe_block = mk().unsafe_block_expr(vec![
            let_result,
            asm_stmt,
            mk().expr_stmt(result_expr),
        ]);

        // `stmts_block` (used by the normal function-body assembly path)
        // always appends a semicolon to the last statement, so a bare tail
        // expression here would discard the unsafe block's value against a
        // non-unit return type - use an explicit `return`, same as
        // `bit_scan_idiom_body` does for the same reason.
        Ok(Some(vec![mk().semi_stmt(mk().return_expr(Some(
            unsafe_block,
        )))]))
    }

    /// If `name`/`arguments`/`return_type` exactly matches one of the kernel's
    /// generic bit-scan primitives (`asm-generic/bitops/{fls,__fls,__ffs,fls64}.h`),
    /// return a one-statement body built from `u32`/`u64::leading_zeros()` or
    /// `trailing_zeros()` in place of the C header's byte-at-a-time scan loop.
    ///
    /// The name match alone is not enough to fire this — a user's own function
    /// that happens to be called `fls64` with an unrelated signature (different
    /// parameter count, wrong integer width, wrong return type) falls through
    /// to the normal per-statement translation below. Matching both name and
    /// the exact parameter/return C type kinds is what makes this specific to
    /// the kernel primitives rather than any identifier that happens to collide.
    ///
    /// Each formula is checked against the width of the *actual* parameter
    /// type rather than a fixed width, since `unsigned long`/`unsigned long
    /// long` are both in play here (`__fls`/`__ffs` take `unsigned long`,
    /// which is 64 bits on this target; `fls64` takes `__u64`, i.e. `unsigned
    /// long long`, also 64 bits; plain `fls` takes `unsigned int`, 32 bits).
    /// Getting the width wrong silently shifts the result by a constant, so
    /// the check on parameter type kind is load-bearing, not decorative.
    fn bit_scan_idiom_body(
        &self,
        name: &str,
        arguments: &[(CDeclId, String, CQualTypeId)],
        return_type: Option<CQualTypeId>,
    ) -> Option<Vec<Stmt>> {
        if !self
            .tcfg
            .kernel_idiom_rules
            .is_enabled(crate::KernelIdiomRule::FlsFamily)
        {
            return None;
        }

        #[derive(Clone, Copy)]
        enum Kind {
            // fls(x): 1-based MSB index, defined at 0 -> 0.
            // Loop invariant in generic_fls is `r = 32 - leading_zeros(x)`,
            // with the `x == 0` special case coinciding with that formula
            // already (0u32.leading_zeros() == 32, so 32 - 32 == 0).
            Fls,
            // __fls(word): 0-based MSB index, UB at word == 0 in C. The loop
            // starts `num` at `BITS - 1` and only ever subtracts, so
            // `num = (BITS - 1) - leading_zeros(word)`.
            UnderscoreFls,
            // __ffs(word): 0-based LSB index, UB at word == 0 in C. The loop
            // counts exactly the trailing zero bits before the first set bit.
            UnderscoreFfs,
            // fls64(x): defined at 0 -> 0. `__fls(x) + 1` for x != 0 folds
            // into the same shape as Fls but at 64 bits: `64 - leading_zeros(x)`
            // (0u64.leading_zeros() == 64, so the x == 0 case matches too).
            Fls64,
        }

        let kind = match name {
            "generic_fls" => Kind::Fls,
            "generic___fls" => Kind::UnderscoreFls,
            "generic___ffs" => Kind::UnderscoreFfs,
            "fls64" => Kind::Fls64,
            _ => return None,
        };

        let [(param_decl_id, _, param_qty)] = arguments else {
            return None;
        };
        let param_type_kind = self.ast_context.resolve_type(param_qty.ctype).kind.clone();
        let ret_qty = return_type?;
        let ret_type_kind = self.ast_context.resolve_type(ret_qty.ctype).kind.clone();

        // (unsigned Rust type to compute in, expected C parameter kind, expected C return kind)
        let (rust_uty, expected_param, expected_ret): (&str, CTypeKind, CTypeKind) = match kind {
            Kind::Fls => ("u32", CTypeKind::UInt, CTypeKind::Int),
            Kind::UnderscoreFls => ("u64", CTypeKind::ULong, CTypeKind::UInt),
            Kind::UnderscoreFfs => ("u64", CTypeKind::ULong, CTypeKind::UInt),
            Kind::Fls64 => ("u64", CTypeKind::ULongLong, CTypeKind::Int),
        };
        if param_type_kind != expected_param || ret_type_kind != expected_ret {
            return None;
        }

        let param_name = self.renamer.borrow().get(param_decl_id)?;
        let param_expr = mk().ident_expr(&param_name);
        let bits = mk().path_expr(vec![rust_uty, "BITS"]);
        let lz = mk().method_call_expr(param_expr.clone(), "leading_zeros", vec![]);

        let value_u: Box<Expr> = match kind {
            // u32::BITS - x.leading_zeros()
            Kind::Fls | Kind::Fls64 => mk().binary_expr(BinOp::Sub(Default::default()), bits, lz),
            // u64::BITS - 1 - x.leading_zeros(), i.e. (u64::BITS - 1) - lz
            Kind::UnderscoreFls => {
                let bits_minus_one = mk().binary_expr(
                    BinOp::Sub(Default::default()),
                    bits,
                    mk().lit_expr(mk().int_unsuffixed_lit(1)),
                );
                mk().binary_expr(BinOp::Sub(Default::default()), bits_minus_one, lz)
            }
            // x.trailing_zeros()
            Kind::UnderscoreFfs => mk().method_call_expr(param_expr, "trailing_zeros", vec![]),
        };

        // leading_zeros()/trailing_zeros() both return u32; the two functions
        // that return a C `int` (fls, fls64) need an explicit cast, since the
        // computed value is always non-negative here (no C code path in the
        // originals produces a negative result) and c_int is 32 bits, wide
        // enough for the u32-domain fls result and for fls64's 0..=64 range.
        let final_expr: Box<Expr> = match kind {
            Kind::Fls | Kind::Fls64 => {
                mk().cast_expr(value_u, mk().abs_path_ty(vec!["core", "ffi", "c_int"]))
            }
            Kind::UnderscoreFls | Kind::UnderscoreFfs => value_u,
        };

        // `stmts_block` (used by the normal function-body assembly path below)
        // always appends a semicolon to the last statement, so a bare tail
        // expression here would be turned into a statement discarding its
        // value, leaving the function body evaluate to `()` against a
        // non-unit return type. An explicit `return` avoids relying on tail
        // position surviving that rewrite.
        Some(vec![mk().semi_stmt(mk().return_expr(Some(final_expr)))])
    }

    fn convert_function_param(
        &self,
        ctx: ExprContext,
        typ: CQualTypeId,
    ) -> TranslationResult<ConvertedFunctionParam> {
        if self.ast_context.is_va_list(typ.ctype) {
            let mutbl = typ.mutability();
            let ty = mk().abs_path_ty(vec!["core", "ffi", "VaList"]);
            return Ok(ConvertedFunctionParam { mutbl, ty });
        }

        self.convert_variable(ctx, None, typ)
            .map(|ConvertedVariable { ty, mutbl, .. }| ConvertedFunctionParam { ty, mutbl })
    }

    pub fn convert_function_call(
        &self,
        mut ctx: ExprContext,
        func: CExprId,
        args: &[CExprId],
        call_expr_ty: CQualTypeId,
        override_ty: Option<CQualTypeId>,
    ) -> TranslationResult<WithStmts<Box<Expr>>> {
        let fn_ty = self
            .ast_context
            .get_pointee_qual_type(
                self.ast_context
                    .index_unwrap_parens(func)
                    .kind
                    .get_type()
                    .ok_or_else(|| format_err!("Invalid callee expression {:?}", func))?,
            )
            .map(|ty| &self.ast_context.resolve_type(ty.ctype).kind);
        let is_variadic = match fn_ty {
            Some(CTypeKind::Function(_, _, is_variadic, _, _)) => *is_variadic,
            _ => false,
        };

        let mut arg_tys = if let Some(CDeclKind::Function { parameters, .. }) =
            self.ast_context.fn_declref_decl(func)
        {
            self.ast_context.tys_of_params(parameters)
        } else {
            None
        };

        let func = match self.ast_context.index_unwrap_parens(func).kind {
            // Direct function call
            CExprKind::ImplicitCast(_, fexp, CastKind::FunctionToPointerDecay, _, _)
            // Only a direct function call with pointer decay if the
            // callee is a declref
            if matches!(self.ast_context.index_unwrap_parens(fexp).kind, CExprKind::DeclRef(..)) =>
                {
                    self.convert_expr(ctx.used(), fexp, None)?
                }

            // Builtin function call
            CExprKind::ImplicitCast(_, fexp, CastKind::BuiltinFnToFnPtr, _, _) => {
                return self.convert_builtin(ctx, override_ty, call_expr_ty, fexp, args);
            }

            // Function pointer call
            _ => {
                let callee = self.convert_expr(ctx.used(), func, None)?;
                let make_fn_ty = |ret_ty: Box<Type>| {
                    let ret_ty = match *ret_ty {
                        Type::Tuple(TypeTuple { elems: ref v, .. }) if v.is_empty() => ReturnType::Default,
                        _ => ReturnType::Type(Default::default(), ret_ty),
                    };
                    let bare_ty = (
                        vec![mk().bare_arg(mk().infer_ty(), None::<Box<Ident>>); args.len()],
                        None::<BareVariadic>,
                        ret_ty
                    );
                    mk().barefn_ty(bare_ty)
                };
                match fn_ty {
                    Some(CTypeKind::Function(ret_ty, _, _, _, false)) => {
                        // K&R function pointer without arguments
                        let ret_ty = self.convert_type(ret_ty.ctype)?;
                        let target_ty = make_fn_ty(ret_ty);
                        callee.map(|fn_ptr| {
                            let fn_ptr = unwrap_function_pointer(fn_ptr);
                            transmute_expr(mk().infer_ty(), target_ty, fn_ptr)
                        }).set_unsafe()
                    }
                    None => {
                        // We have to infer the return type from our expression type
                        let ret_ty = self.convert_type(call_expr_ty.ctype)?;
                        let target_ty = make_fn_ty(ret_ty);
                        callee.map(|fn_ptr| {
                            transmute_expr(mk().infer_ty(), target_ty, fn_ptr)
                        }).set_unsafe()
                    }
                    Some(CTypeKind::Function(_, ty_arg_tys, ..)) => {
                        arg_tys = Some(ty_arg_tys.clone());
                        // Normal function pointer
                        callee.map(unwrap_function_pointer)
                    }
                    Some(_) => panic!(
                        "function pointer did not point to CTYpeKind::Function: \
                        {fn_ty:?}"
                    ),
                }
            }
        };

        let call = func.and_then_try(|func| {
            // We want to decay refs only when function is variadic
            ctx.decay_ref = DecayRef::from(is_variadic);

            let args = self.convert_call_args(ctx.used(), args, arg_tys.as_deref(), is_variadic)?;

            let call_expr = args.map(|args| mk().call_expr(func, args));
            self.make_cast(
                ctx,
                call_expr_ty,
                override_ty.unwrap_or(call_expr_ty),
                call_expr,
            )
        })?;

        self.convert_side_effects_expr(
            ctx,
            call,
            "Function call expression is not supposed to be used",
        )
    }

    /// Variant of `convert_exprs` for the arguments of a function call.
    /// Accounts for differences in translation for arguments, and for varargs where only a prefix
    /// of the expression types are known.
    #[allow(clippy::vec_box/*, reason = "not worth a substantial refactor"*/)]
    fn convert_call_args(
        &self,
        ctx: ExprContext,
        exprs: &[CExprId],
        arg_tys: Option<&[CQualTypeId]>,
        is_variadic: bool,
    ) -> TranslationResult<WithStmts<Vec<Box<Expr>>>> {
        let arg_tys = if let Some(arg_tys) = arg_tys {
            if !is_variadic {
                assert!(arg_tys.len() == exprs.len());
            }

            arg_tys
        } else {
            &[]
        };

        exprs
            .iter()
            .enumerate()
            .map(|(n, arg)| self.convert_call_arg(ctx, *arg, arg_tys.get(n).copied()))
            .collect()
    }

    /// Wrapper around `convert_expr` for the arguments of a function call.
    fn convert_call_arg(
        &self,
        ctx: ExprContext,
        expr_id: CExprId,
        override_ty: Option<CQualTypeId>,
    ) -> TranslationResult<WithStmts<Box<Expr>>> {
        let mut val;

        if (self
            .ast_context
            .index_unwrap_parens(expr_id)
            .kind
            .get_qual_type())
        .map_or(false, |qtype| self.ast_context.is_va_list(qtype.ctype))
        {
            // No `override_ty` to avoid unwanted casting.
            val = self.convert_expr(ctx, expr_id, None)?;
            val = val.map(|val| mk_va_list_copy(self.tcfg.edition, val));
        } else {
            val = self.convert_expr(ctx, expr_id, override_ty)?;
        }

        Ok(val)
    }

    /// Translates the C main function.
    ///
    /// Translating main requires us to wrap the C implementation to
    /// a helper that can be called from a generated main function in
    /// Rust.
    pub fn convert_main(&self, main_id: CDeclId) -> TranslationResult<Box<Item>> {
        if let CDeclKind::Function {
            ref parameters,
            typ,
            ..
        } = self.ast_context.index(main_id).kind
        {
            let ret: CTypeKind = match self.ast_context.resolve_type(typ).kind {
                CTypeKind::Function(ret, _, _, _, _) => {
                    self.ast_context.resolve_type(ret.ctype).kind.clone()
                }
                ref k => {
                    return Err(format_err!(
                        "Type of main function {:?} was not a function type, got {:?}",
                        main_id,
                        k
                    )
                    .into())
                }
            };

            let main_fn_name = self
                .renamer
                .borrow()
                .get(&main_id)
                .expect("Could not find main function in renamer");

            let decl = mk().fn_decl("main", vec![], None, ReturnType::Default);

            let main_fn = mk().path_expr(vec![main_fn_name]);

            let exit_fn = mk().abs_path_expr(vec!["std", "process", "exit"]);
            let args_fn = mk().abs_path_expr(vec!["std", "env", "args"]);
            let vars_fn = mk().abs_path_expr(vec!["std", "env", "vars"]);

            let no_args: Vec<Box<Expr>> = vec![];

            let mut stmts: Vec<Stmt> = vec![];
            let mut main_args: Vec<Box<Expr>> = vec![];

            let n = parameters.len();

            if n >= 2 {
                // `argv` and `argc`

                stmts.push(mk().local_stmt(Box::new({
                    // ty = Vec<Vec<u8>>
                    let ty = mk().path_ty(vec![mk().path_segment_with_args(
                        "Vec",
                        mk().angle_bracketed_args(vec![mk().path_ty(vec![mk()
                            .path_segment_with_args(
                                "Vec",
                                mk().angle_bracketed_args(vec![mk().ident_ty("u8")]),
                            )])]),
                    )]);
                    // map_arg = |arg| {
                    //     (::std::ffi::CString::new(arg))
                    //         .expect("Failed to convert argument into CString.")
                    //         .into_bytes_with_nul()
                    // }
                    let cstring_call = mk().call_expr(
                        // TODO(kkysen) change `"std"` to `"alloc"` after `#![feature(alloc_c_string)]` is stabilized in `1.63.0`
                        mk().abs_path_expr(vec!["std", "ffi", "CString", "new"]),
                        vec![mk().path_expr(vec!["arg"])],
                    );
                    let expect_arg = mk().lit_expr("Failed to convert argument into CString.");
                    let map_arg = mk().closure_expr(
                        CaptureBy::Ref,
                        Movability::Movable,
                        vec![mk().ident_pat("arg")],
                        ReturnType::Default,
                        mk().method_chain_expr(
                            cstring_call,
                            vec![
                                (mk().path_segment("expect"), vec![expect_arg]),
                                (mk().path_segment("into_bytes_with_nul"), vec![]),
                            ],
                        ),
                    );
                    // init = args_fn
                    //     .map(map_arg)
                    //     .collect();
                    let init = mk().method_chain_expr(
                        mk().call_expr(args_fn, vec![]),
                        vec![
                            (mk().path_segment("map"), vec![map_arg]),
                            (mk().path_segment("collect"), vec![]),
                        ],
                    );
                    mk().local(mk().mutbl().ident_pat("args_strings"), Some(ty), Some(init))
                })));

                stmts.push(mk().local_stmt(Box::new({
                    // ty = Vec<*mut ::core::ffi::c_char>
                    let ty = mk().path_ty(vec![mk().path_segment_with_args(
                        "Vec",
                        mk().angle_bracketed_args(vec![mk()
                            .mutbl()
                            .ptr_ty(mk().abs_path_ty(vec!["core", "ffi", "c_char"]))]),
                    )]);
                    // map_arg = |arg| arg.as_mut_ptr() as *mut ::core::ffi::c_char
                    let map_arg = mk().closure_expr(
                        CaptureBy::Ref,
                        Movability::Movable,
                        vec![mk().ident_pat("arg")],
                        ReturnType::Default,
                        mk().cast_expr(
                            mk().method_call_expr(mk().ident_expr("arg"), "as_mut_ptr", vec![]),
                            mk().mutbl()
                                .ptr_ty(mk().abs_path_ty(vec!["core", "ffi", "c_char"])),
                        ),
                    );
                    // chain_arg = ::core::iter::once(::core::ptr::null_mut())
                    let chain_arg = mk().call_expr(
                        mk().abs_path_expr(vec!["core", "iter", "once"]),
                        vec![mk().call_expr(
                            mk().abs_path_expr(vec!["core", "ptr", "null_mut"]),
                            vec![],
                        )],
                    );
                    // init = args_strings
                    //     .iter_mut()
                    //     .map(map_arg)
                    //     .chain(chain_arg)
                    //     .collect()
                    let init = mk().method_chain_expr(
                        mk().ident_expr("args_strings"),
                        vec![
                            (mk().path_segment("iter_mut"), vec![]),
                            (mk().path_segment("map"), vec![map_arg]),
                            (mk().path_segment("chain"), vec![chain_arg]),
                            (mk().path_segment("collect"), vec![]),
                        ],
                    );
                    mk().local(mk().mutbl().ident_pat("args_ptrs"), Some(ty), Some(init))
                })));

                let argc_ty: Box<Type> = match self.ast_context.index(parameters[0]).kind {
                    CDeclKind::Variable { ref typ, .. } => self.convert_type(typ.ctype),
                    _ => Err(TranslationError::generic(
                        "Cannot find type of 'argc' argument in main function",
                    )),
                }?;
                let argv_ty: Box<Type> = match self.ast_context.index(parameters[1]).kind {
                    CDeclKind::Variable { ref typ, .. } => self.convert_type(typ.ctype),
                    _ => Err(TranslationError::generic(
                        "Cannot find type of 'argv' argument in main function",
                    )),
                }?;
                let args = mk().ident_expr("args_ptrs");
                let argc = mk().binary_expr(
                    BinOp::Sub(Default::default()),
                    mk().method_call_expr(args.clone(), "len", no_args.clone()),
                    mk().lit_expr(mk().int_lit(1, "")),
                );
                let argv = mk().method_call_expr(args, "as_mut_ptr", no_args.clone());

                main_args.push(mk().cast_expr(argc, argc_ty));
                main_args.push(mk().cast_expr(argv, argv_ty));
            }

            if n >= 3 {
                // non-standard `envp`

                stmts.push(mk().local_stmt(Box::new(mk().local(
                    mk().mutbl().ident_pat("vars"),
                    Some(mk().path_ty(vec![mk().path_segment_with_args(
                        "Vec",
                        mk().angle_bracketed_args(vec![
                            mk().mutbl().ptr_ty(mk().abs_path_ty(vec!["core", "ffi", "c_char"])),
                        ]),
                    )])),
                    Some(mk().call_expr(mk().path_expr(vec!["Vec", "new"]), vec![])),
                ))));
                let var_name_ident = mk().ident("var_name");
                let var_value_ident = mk().ident("var_value");
                stmts.push(mk().semi_stmt(mk().for_expr(
                    mk().tuple_pat(vec![
                        mk().ident_pat("var_name"),
                        mk().ident_pat("var_value"),
                    ]),
                    mk().call_expr(vars_fn, vec![]),
                    mk().block(vec![
                        mk().local_stmt(Box::new(
                            mk().local(
                                mk().ident_pat("var"),
                                Some(mk().path_ty(vec!["String"])),
                                Some(
                                    mk().mac_expr(
                                        mk().mac(
                                            mk().path(vec!["format"]),
                                            vec![
                                                TokenTree::Literal(
                                                    proc_macro2::Literal::string("{}={}"),
                                                ),
                                                TokenTree::Punct(Punct::new(
                                                    ',',
                                                    proc_macro2::Spacing::Alone,
                                                )),
                                                TokenTree::Ident(var_name_ident),
                                                TokenTree::Punct(Punct::new(
                                                    ',',
                                                    proc_macro2::Spacing::Alone,
                                                )),
                                                TokenTree::Ident(var_value_ident),
                                            ]
                                            .into_iter()
                                            .collect::<TokenStream>(),
                                            MacroDelimiter::Paren(Default::default()),
                                        ),
                                    ),
                                ),
                            ),
                        )),
                        mk().semi_stmt(mk().method_call_expr(
                            mk().path_expr(vec!["vars"]),
                            "push",
                            vec![mk().method_call_expr(
                                mk().method_call_expr(
                                    mk().call_expr(
                                        mk().abs_path_expr(vec![
                                            // TODO(kkysen) change `"std"` to `"alloc"` after `#![feature(alloc_c_string)]` is stabilized in `1.63.0`
                                            "std", "ffi", "CString", "new",
                                        ]),
                                        vec![mk().path_expr(vec!["var"])],
                                    ),
                                    "expect",
                                    vec![mk().lit_expr(
                                    "Failed to convert environment variable into CString."
                                )],
                                ),
                                "into_raw",
                                vec![],
                            )],
                        )),
                    ]),
                    None as Option<Ident>,
                )));
                stmts.push(mk().semi_stmt(mk().method_call_expr(
                    mk().path_expr(vec!["vars"]),
                    "push",
                    vec![
                        mk().call_expr(mk().abs_path_expr(vec!["core", "ptr", "null_mut"]), vec![]),
                    ],
                )));

                let envp_ty: Box<Type> = match self.ast_context.index(parameters[2]).kind {
                    CDeclKind::Variable { ref typ, .. } => self.convert_type(typ.ctype),
                    _ => Err(TranslationError::generic(
                        "Cannot find type of 'envp' argument in main function",
                    )),
                }?;

                let envp = mk().method_call_expr(mk().ident_expr("vars"), "as_mut_ptr", no_args);

                main_args.push(mk().cast_expr(envp, envp_ty));
            }

            // Check `main` has the right form
            if n != 0 && n != 2 && n != 3 {
                return Err(format_err!(
                    "Main function should have 0, 2, or 3 parameters, not {}.",
                    n
                )
                .into());
            };

            if let CTypeKind::Void = ret {
                let call_main = mk().call_expr(main_fn, main_args);
                stmts.push(mk().expr_stmt(mk().unsafe_block_expr(vec![mk().expr_stmt(call_main)])));

                let exit_arg = mk().lit_expr(mk().int_lit(0, "i32"));
                let call_exit = mk().call_expr(exit_fn, vec![exit_arg]);

                stmts.push(mk().semi_stmt(call_exit));
            } else {
                let call_main = mk().cast_expr(
                    mk().call_expr(main_fn, main_args),
                    mk().path_ty(vec!["i32"]),
                );

                let call_exit = mk().call_expr(exit_fn, vec![call_main]);
                stmts.push(mk().expr_stmt(mk().unsafe_block_expr(vec![mk().expr_stmt(call_exit)])));
            };

            let block = mk().block(stmts);
            let main_attributes = self.mk_cross_check(mk(), vec!["none"]);
            Ok(main_attributes.pub_().fn_item(decl, block))
        } else {
            Err(TranslationError::generic(
                "Cannot translate non-function main entry point",
            ))
        }
    }
}

struct ConvertedFunctionParam {
    ty: Box<Type>,
    mutbl: Mutability,
}
