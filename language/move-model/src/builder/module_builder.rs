// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use itertools::Itertools;
use regex::Regex;

use bytecode_source_map::source_map::SourceMap;
use move_ir_types::{ast::ConstantName, location::Spanned};
use move_lang::{
    compiled_unit::{FunctionInfo, SpecInfo},
    expansion::ast as EA,
    parser::ast as PA,
    shared::{unique_map::UniqueMap, Name},
};
use vm::{
    access::ModuleAccess,
    file_format::{AbilitySet, Constant, FunctionDefinitionIndex, StructDefinitionIndex},
    views::{FunctionHandleView, StructHandleView},
    CompiledModule,
};

use crate::{
    ast::{
        Condition, ConditionKind, Exp, GlobalInvariant, ModuleName, Operation, PropertyBag,
        PropertyValue, QualifiedSymbol, Spec, SpecBlockInfo, SpecBlockTarget, SpecFunDecl,
        SpecVarDecl, Value,
    },
    builder::{
        exp_translator::ExpTranslator,
        model_builder::{ConstEntry, LocalVarEntry, ModelBuilder, SpecFunEntry},
    },
    exp_rewriter::{ExpRewriter, RewriteTarget},
    model::{
        AbilityConstraint, FieldId, FunId, FunctionData, Loc, ModuleId, MoveIrLoc,
        NamedConstantData, NamedConstantId, NodeId, QualifiedId, SchemaId, SpecFunId, SpecVarId,
        StructData, StructId, TypeParameter, SCRIPT_BYTECODE_FUN_NAME,
    },
    pragmas::{
        is_pragma_valid_for_block, is_property_valid_for_condition, CONDITION_DEACTIVATED_PROP,
        CONDITION_GLOBAL_PROP, CONDITION_INJECTED_PROP,
    },
    project_1st, project_2nd,
    symbol::{Symbol, SymbolPool},
    ty::{PrimitiveType, Type, BOOL_TYPE},
};

#[derive(Debug)]
pub(crate) struct ModuleBuilder<'env, 'translator> {
    pub parent: &'translator mut ModelBuilder<'env>,
    /// Id of the currently build module.
    pub module_id: ModuleId,
    /// Name of the currently build module.
    pub module_name: ModuleName,
    /// Translated specification functions.
    pub spec_funs: Vec<SpecFunDecl>,
    /// During the definition analysis, the index into `spec_funs` we are currently
    /// handling
    pub spec_fun_index: usize,
    /// Translated specification variables.
    pub spec_vars: Vec<SpecVarDecl>,
    /// Translated function specifications.
    pub fun_specs: BTreeMap<Symbol, Spec>,
    /// Translated struct specifications.
    pub struct_specs: BTreeMap<Symbol, Spec>,
    /// Translated module spec
    pub module_spec: Spec,
    /// Spec block infos.
    pub spec_block_infos: Vec<SpecBlockInfo>,
    /// Let bindings for the current spec block, pointing to the function generated for the let.
    pub spec_block_lets: BTreeMap<Symbol, SpecFunId>,
}

/// A value which we pass in to spec block analyzers, describing the resolved target of the spec
/// block.
#[derive(Debug)]
pub enum SpecBlockContext<'a> {
    Module,
    Struct(QualifiedSymbol),
    Function(QualifiedSymbol),
    FunctionCode(QualifiedSymbol, &'a SpecInfo),
    Schema(QualifiedSymbol),
}

impl<'a> fmt::Display for SpecBlockContext<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use SpecBlockContext::*;
        match self {
            Module => write!(f, "module context")?,
            Struct(..) => write!(f, "struct context")?,
            Function(..) => write!(f, "function context")?,
            FunctionCode(..) => write!(f, "code context")?,
            Schema(..) => write!(f, "schema context")?,
        }
        Ok(())
    }
}

/// # Entry Points

impl<'env, 'translator> ModuleBuilder<'env, 'translator> {
    pub fn new(
        parent: &'translator mut ModelBuilder<'env>,
        module_id: ModuleId,
        module_name: ModuleName,
    ) -> Self {
        Self {
            parent,
            module_id,
            module_name,
            spec_funs: vec![],
            spec_fun_index: 0,
            spec_vars: vec![],
            fun_specs: BTreeMap::new(),
            struct_specs: BTreeMap::new(),
            module_spec: Spec::default(),
            spec_block_infos: Default::default(),
            spec_block_lets: BTreeMap::new(),
        }
    }

    /// Translates the given module definition from the Move compiler's expansion phase,
    /// combined with a compiled module (bytecode) and a source map, and enters it into
    /// this global environment. Any type check or others errors encountered will be collected
    /// in the environment for later processing. Dependencies of this module are guaranteed to
    /// have been analyzed and being already part of the environment.
    ///
    /// Translation happens in three phases:
    ///
    /// 1. In the *declaration analysis*, we collect all information about structs, functions,
    ///    spec functions, spec vars, and schemas in a module. We do not yet analyze function
    ///    bodies, conditions, and invariants, which we can only analyze after we know all
    ///    global declarations (declaration of globals is order independent, and they can have
    ///    cyclic references).
    /// 2. In the *definition analysis*, we visit the definitions we have skipped in step (1),
    ///    specifically analyzing and type checking expressions and schema inclusions.
    /// 3. In the *population phase*, we populate the global environment with the information
    ///    from this module.
    pub fn translate(
        &mut self,
        loc: Loc,
        module_def: EA::ModuleDefinition,
        compiled_module: CompiledModule,
        source_map: SourceMap<MoveIrLoc>,
        function_infos: UniqueMap<PA::FunctionName, FunctionInfo>,
    ) {
        self.decl_ana(&module_def, &compiled_module, &source_map);
        self.def_ana(&module_def, function_infos);
        self.collect_spec_block_infos(&module_def);
        self.populate_env_from_result(loc, compiled_module, source_map);
    }
}

impl<'env, 'translator> ModuleBuilder<'env, 'translator> {
    /// Shortcut for accessing the symbol pool.
    fn symbol_pool(&self) -> &SymbolPool {
        self.parent.env.symbol_pool()
    }

    /// Qualifies the given symbol by the current module.
    pub fn qualified_by_module(&self, sym: Symbol) -> QualifiedSymbol {
        QualifiedSymbol {
            module_name: self.module_name.clone(),
            symbol: sym,
        }
    }

    /// Qualifies the given name by the current module.
    fn qualified_by_module_from_name(&self, name: &Name) -> QualifiedSymbol {
        let sym = self.symbol_pool().make(&name.value);
        self.qualified_by_module(sym)
    }

    /// Converts a ModuleAccess into its parts, an optional ModuleName and base name.
    pub fn module_access_to_parts(
        &self,
        access: &EA::ModuleAccess,
    ) -> (Option<ModuleName>, Symbol) {
        match &access.value {
            EA::ModuleAccess_::Name(n) => (None, self.symbol_pool().make(n.value.as_str())),
            EA::ModuleAccess_::ModuleAccess(m, n) => {
                let module_name = ModuleName::from_str(
                    &m.value.0.to_string(),
                    self.symbol_pool().make(m.value.1.as_str()),
                );
                (Some(module_name), self.symbol_pool().make(n.value.as_str()))
            }
        }
    }

    /// Converts a ModuleAccess into a qualified symbol which can be used for lookup of
    /// types or functions.
    pub fn module_access_to_qualified(&self, access: &EA::ModuleAccess) -> QualifiedSymbol {
        let (module_name_opt, symbol) = self.module_access_to_parts(access);
        let module_name = module_name_opt.unwrap_or_else(|| self.module_name.clone());
        QualifiedSymbol {
            module_name,
            symbol,
        }
    }

    /// Creates a SpecBlockContext from the given SpecBlockTarget. The context is used during
    /// definition analysis when visiting a schema block member (condition, invariant, etc.).
    /// This returns None if the SpecBlockTarget cannnot be resolved; error reporting happens
    /// at caller side.
    fn get_spec_block_context<'pa>(
        &self,
        target: &'pa PA::SpecBlockTarget,
    ) -> Option<SpecBlockContext<'pa>> {
        match &target.value {
            PA::SpecBlockTarget_::Code => None,
            PA::SpecBlockTarget_::Function(name) => {
                let qsym = self.qualified_by_module_from_name(&name.0);
                if self.parent.fun_table.contains_key(&qsym) {
                    Some(SpecBlockContext::Function(qsym))
                } else {
                    None
                }
            }
            PA::SpecBlockTarget_::Structure(name) => {
                let qsym = self.qualified_by_module_from_name(&name.0);
                if self.parent.struct_table.contains_key(&qsym) {
                    Some(SpecBlockContext::Struct(qsym))
                } else {
                    None
                }
            }
            PA::SpecBlockTarget_::Schema(name, _) => {
                let qsym = self.qualified_by_module_from_name(&name);
                if self.parent.spec_schema_table.contains_key(&qsym) {
                    Some(SpecBlockContext::Schema(qsym))
                } else {
                    None
                }
            }
            PA::SpecBlockTarget_::Module => Some(SpecBlockContext::Module),
        }
    }
}

/// # Declaration Analysis

impl<'env, 'translator> ModuleBuilder<'env, 'translator> {
    fn decl_ana(
        &mut self,
        module_def: &EA::ModuleDefinition,
        compiled_module: &CompiledModule,
        source_map: &SourceMap<MoveIrLoc>,
    ) {
        for (name, struct_def) in module_def.structs.key_cloned_iter() {
            self.decl_ana_struct(&name, struct_def);
        }
        for (name, fun_def) in module_def.functions.key_cloned_iter() {
            self.decl_ana_fun(&name, fun_def);
        }
        for (name, const_def) in module_def.constants.key_cloned_iter() {
            self.decl_ana_const(&name, const_def, compiled_module, source_map);
        }
        for spec in &module_def.specs {
            self.decl_ana_spec_block(spec);
        }
    }

    fn decl_ana_const(
        &mut self,
        name: &PA::ConstantName,
        def: &EA::Constant,
        compiled_module: &CompiledModule,
        source_map: &SourceMap<MoveIrLoc>,
    ) {
        let qsym = self.qualified_by_module_from_name(&name.0);
        let name = qsym.symbol;
        let const_name = ConstantName::new(self.symbol_pool().string(name).to_string());
        let const_idx = source_map
            .constant_map
            .get(&const_name)
            .expect("constant not in source map");
        let move_value =
            Constant::deserialize_constant(&compiled_module.constant_pool()[*const_idx as usize])
                .unwrap();
        let mut et = ExpTranslator::new(self);
        let loc = et.to_loc(&def.loc);
        let value = et.translate_from_move_value(&loc, &move_value);
        let ty = et.translate_type(&def.signature);
        et.parent
            .parent
            .define_const(qsym, ConstEntry { loc, ty, value });
    }

    fn decl_ana_struct(&mut self, name: &PA::StructName, def: &EA::StructDefinition) {
        let qsym = self.qualified_by_module_from_name(&name.0);
        let struct_id = StructId::new(qsym.symbol);
        let is_resource =
            // TODO migrate to abilities
            def.abilities.has_ability_(PA::Ability_::Key)|| (
                !def.abilities.has_ability_(PA::Ability_::Copy) &&
                !def.abilities.has_ability_(PA::Ability_::Drop)
            );
        let mut et = ExpTranslator::new(self);
        let type_params = et.analyze_and_add_type_params(&def.type_parameters);
        et.parent.parent.define_struct(
            et.to_loc(&def.loc),
            qsym,
            et.parent.module_id,
            struct_id,
            is_resource,
            type_params,
            None, // will be filled in during definition analysis
        );
    }

    fn decl_ana_fun(&mut self, name: &PA::FunctionName, def: &EA::Function) {
        let qsym = self.qualified_by_module_from_name(&name.0);
        let fun_id = FunId::new(qsym.symbol);
        let mut et = ExpTranslator::new(self);
        et.enter_scope();
        let type_params = et.analyze_and_add_type_params(&def.signature.type_parameters);
        et.enter_scope();
        let params = et.analyze_and_add_params(&def.signature.parameters, true);
        let result_type = et.translate_type(&def.signature.return_type);
        let is_public = matches!(def.visibility, PA::FunctionVisibility::Public(..));
        let loc = et.to_loc(&def.loc);
        et.parent.parent.define_fun(
            loc.clone(),
            qsym.clone(),
            et.parent.module_id,
            fun_id,
            is_public,
            type_params.clone(),
            params.clone(),
            result_type.clone(),
        );

        // Add function as a spec fun entry as well.
        let spec_fun_id = SpecFunId::new(self.spec_funs.len());
        self.parent.define_spec_fun(
            qsym,
            SpecFunEntry {
                loc: loc.clone(),
                oper: Operation::Function(self.module_id, spec_fun_id, None),
                type_params: type_params.iter().map(|(_, ty)| ty.clone()).collect(),
                arg_types: params.iter().map(|(_, ty)| ty.clone()).collect(),
                result_type: result_type.clone(),
            },
        );

        // Add $ to the name so the spec version does not name clash with the Move version.
        let name = self.symbol_pool().make(&format!("${}", name.0.value));
        let mut fun_decl = SpecFunDecl {
            loc,
            name,
            type_params,
            params,
            context_params: None,
            result_type,
            used_spec_vars: BTreeSet::new(),
            used_memory: BTreeSet::new(),
            uninterpreted: false,
            is_move_fun: true,
            is_native: false,
            body: None,
        };
        if let EA::FunctionBody_::Native = def.body.value {
            fun_decl.is_native = true;
        }
        self.spec_funs.push(fun_decl);
    }

    fn decl_ana_spec_block(&mut self, block: &EA::SpecBlock) {
        use EA::SpecBlockMember_::*;
        // Process any spec block members which introduce global declarations.
        for member in &block.value.members {
            let loc = self.parent.env.to_loc(&member.loc);
            match &member.value {
                Function {
                    uninterpreted,
                    name,
                    signature,
                    ..
                } => self.decl_ana_spec_fun(&loc, *uninterpreted, name, signature),
                Variable {
                    is_global: true,
                    name,
                    type_,
                    type_parameters,
                } => self.decl_ana_global_var(&loc, name, type_parameters, type_),
                _ => {}
            }
        }
        // If this is a schema spec block, process its declaration.
        if let PA::SpecBlockTarget_::Schema(name, type_params) = &block.value.target.value {
            self.decl_ana_schema(&block, &name, &type_params);
        }
    }

    fn decl_ana_spec_fun(
        &mut self,
        loc: &Loc,
        uninterpreted: bool,
        name: &PA::FunctionName,
        signature: &EA::FunctionSignature,
    ) {
        let name = self.symbol_pool().make(&name.0.value);
        let (type_params, params, result_type) = {
            let et = &mut ExpTranslator::new(self);
            let type_params = et.analyze_and_add_type_params(&signature.type_parameters);
            et.enter_scope();
            let params = et.analyze_and_add_params(&signature.parameters, false);
            let result_type = et.translate_type(&signature.return_type);
            et.finalize_types();
            (type_params, params, result_type)
        };

        // Add the function to the symbol table.
        let fun_id = SpecFunId::new(self.spec_funs.len());
        self.parent.define_spec_fun(
            self.qualified_by_module(name),
            SpecFunEntry {
                loc: loc.clone(),
                oper: Operation::Function(self.module_id, fun_id, None),
                type_params: type_params.iter().map(|(_, ty)| ty.clone()).collect(),
                arg_types: params.iter().map(|(_, ty)| ty.clone()).collect(),
                result_type: result_type.clone(),
            },
        );

        // Add a prototype of the SpecFunDecl to the module build. This
        // will for now have an empty body which we fill in during a 2nd pass.
        let fun_decl = SpecFunDecl {
            loc: loc.clone(),
            name,
            type_params,
            params,
            context_params: None,
            result_type,
            used_spec_vars: BTreeSet::new(),
            used_memory: BTreeSet::new(),
            uninterpreted,
            is_move_fun: false,
            is_native: false,
            body: None,
        };
        self.spec_funs.push(fun_decl);
    }

    fn decl_ana_global_var(
        &mut self,
        loc: &Loc,
        name: &Name,
        type_params: &[(Name, EA::AbilitySet)],
        type_: &EA::Type,
    ) {
        let name = self.symbol_pool().make(name.value.as_str());
        let (type_params, type_) = {
            let et = &mut ExpTranslator::new(self);
            let type_params = et.analyze_and_add_type_params(type_params);
            let type_ = et.translate_type(type_);
            (type_params, type_)
        };
        if type_.is_reference() {
            self.parent.error(
                loc,
                &format!(
                    "`{}` cannot have reference type",
                    name.display(self.symbol_pool())
                ),
            )
        }
        // Add the variable to the symbol table.
        let var_id = SpecVarId::new(self.spec_vars.len());
        self.parent.define_spec_var(
            loc,
            self.qualified_by_module(name),
            self.module_id,
            var_id,
            project_2nd(&type_params),
            type_.clone(),
        );
        // Add the variable to the module build.
        let var_decl = SpecVarDecl {
            loc: loc.clone(),
            name,
            type_params,
            type_,
        };
        self.spec_vars.push(var_decl);
    }

    fn decl_ana_schema<T>(
        &mut self,
        block: &EA::SpecBlock,
        name: &Name,
        type_params: &[(Name, T)],
    ) {
        let qsym = self.qualified_by_module_from_name(name);
        let mut et = ExpTranslator::new(self);
        et.enter_scope();
        let type_params = et.analyze_and_add_type_params(&type_params);
        // Extract local variables.
        let mut vars = vec![];
        for member in &block.value.members {
            if let EA::SpecBlockMember_::Variable {
                is_global: false,
                name,
                type_,
                type_parameters,
            } = &member.value
            {
                if !type_parameters.is_empty() {
                    et.error(
                        &et.to_loc(&member.loc),
                        "schema variable cannot have type parameters",
                    );
                }
                let name = et.symbol_pool().make(&name.value);
                let type_ = et.translate_type(type_);
                vars.push((name, type_));
            }
        }
        // Add schema declaration prototype to the symbol table.
        let loc = et.to_loc(&block.loc);
        self.parent
            .define_spec_schema(&loc, qsym, self.module_id, type_params, vars);
    }
}

/// # Definition Analysis

impl<'env, 'translator> ModuleBuilder<'env, 'translator> {
    fn def_ana(
        &mut self,
        module_def: &EA::ModuleDefinition,
        function_infos: UniqueMap<PA::FunctionName, FunctionInfo>,
    ) {
        // Analyze all structs.
        for (name, def) in module_def.structs.key_cloned_iter() {
            self.def_ana_struct(&name, def);
        }

        // Analyze all functions.
        for (idx, (name, fun_def)) in module_def.functions.key_cloned_iter().enumerate() {
            self.def_ana_fun(&name, &fun_def.body, idx);
        }

        // Propagate the impurity of functions: a Move function which calls an
        // impure Move function is also considered impure.
        let mut visited = BTreeMap::new();
        for (idx, (name, _)) in module_def.functions.key_cloned_iter().enumerate() {
            let is_pure = self.propagate_function_impurity(&mut visited, SpecFunId::new(idx));
            let full_name = self.qualified_by_module_from_name(&name.0);
            if is_pure {
                // Modify the types of parameters, return values and expressions
                // of pure Move functions so they no longer have references.
                self.deref_move_fun_types(full_name.clone(), idx);
            }
            self.parent
                .fun_table
                .entry(full_name)
                .and_modify(|e| e.is_pure = is_pure);
        }

        // Analyze all schemas. This must be done before other things because schemas need to be
        // ready for inclusion. We also must do this recursively, so use a visited set to detect
        // cycles.
        {
            let schema_defs: BTreeMap<QualifiedSymbol, &EA::SpecBlock> = module_def
                .specs
                .iter()
                .filter_map(|block| {
                    if let PA::SpecBlockTarget_::Schema(name, ..) = &block.value.target.value {
                        let qsym = self.qualified_by_module_from_name(name);
                        Some((qsym, block))
                    } else {
                        None
                    }
                })
                .collect();
            let mut visited = BTreeSet::new();
            let mut visiting = vec![];
            for (name, block) in schema_defs.iter() {
                self.def_ana_schema(
                    &schema_defs,
                    &mut visited,
                    &mut visiting,
                    name.clone(),
                    block,
                );
            }
        }

        // Analyze all module level spec blocks (except schemas)
        for spec in &module_def.specs {
            if matches!(spec.value.target.value, PA::SpecBlockTarget_::Schema(..)) {
                continue;
            }
            match self.get_spec_block_context(&spec.value.target) {
                Some(context) => self.def_ana_spec_block(&context, spec),
                None => {
                    let loc = self.parent.env.to_loc(&spec.value.target.loc);
                    self.parent.error(&loc, "unresolved spec target");
                }
            }
        }

        // Analyze in-function spec blocks.
        for (name, fun_def) in module_def.functions.key_cloned_iter() {
            let fun_spec_info = &function_infos.get(&name).unwrap().spec_info;
            let qsym = self.qualified_by_module_from_name(&name.0);
            for (spec_id, spec_block) in fun_def.specs.iter() {
                for member in &spec_block.value.members {
                    let loc = &self.parent.env.to_loc(&member.loc);
                    match &member.value {
                        EA::SpecBlockMember_::Condition {
                            kind,
                            properties,
                            exp,
                            additional_exps,
                        } => {
                            let context = SpecBlockContext::FunctionCode(
                                qsym.clone(),
                                &fun_spec_info[spec_id],
                            );
                            if let Some((kind, exp)) =
                                self.extract_condition_kind(&context, kind, exp)
                            {
                                let properties = self.translate_properties(properties, &|prop| {
                                    if !is_property_valid_for_condition(&kind, prop) {
                                        Some(loc.clone())
                                    } else {
                                        None
                                    }
                                });
                                self.def_ana_condition(
                                    loc,
                                    &context,
                                    kind,
                                    properties,
                                    exp,
                                    additional_exps,
                                );
                            }
                        }
                        _ => {
                            self.parent.error(&loc, "item not allowed");
                        }
                    }
                }
            }
        }

        // Perform post analyzes of state usage in spec functions.
        self.compute_state_usage();
        // Perform post reduction of module invariants.
        self.reduce_module_invariants();
    }
}

/// ## Struct Definition Analysis

impl<'env, 'translator> ModuleBuilder<'env, 'translator> {
    fn def_ana_struct(&mut self, name: &PA::StructName, def: &EA::StructDefinition) {
        let qsym = self.qualified_by_module_from_name(&name.0);
        let type_params = self
            .parent
            .struct_table
            .get(&qsym)
            .expect("struct invalid")
            .type_params
            .clone();
        let mut et = ExpTranslator::new(self);
        let loc = et.to_loc(&name.0.loc);
        for (name, ty) in type_params {
            et.define_type_param(&loc, name, ty);
        }
        let fields = match &def.fields {
            EA::StructFields::Defined(fields) => {
                let mut field_map = BTreeMap::new();
                for (_name_loc, field_name_, (idx, ty)) in fields {
                    let field_sym = et.symbol_pool().make(field_name_);
                    let field_ty = et.translate_type(&ty);
                    field_map.insert(field_sym, (*idx, field_ty));
                }
                Some(field_map)
            }
            EA::StructFields::Native(_) => None,
        };
        self.parent
            .struct_table
            .get_mut(&qsym)
            .expect("struct invalid")
            .fields = fields;
    }
}

/// ## Move Function Definition Analysis

impl<'env, 'translator> ModuleBuilder<'env, 'translator> {
    /// Definition analysis for Move functions.
    /// If the function is pure, we translate its body.
    fn def_ana_fun(&mut self, name: &PA::FunctionName, body: &EA::FunctionBody, fun_idx: usize) {
        if let EA::FunctionBody_::Defined(seq) = &body.value {
            let full_name = self.qualified_by_module_from_name(&name.0);
            let entry = self
                .parent
                .fun_table
                .get(&full_name)
                .expect("function defined");
            let type_params = entry.type_params.clone();
            let params = entry.params.clone();
            let result_type = entry.result_type.clone();
            let mut et = ExpTranslator::new(self);
            et.translate_fun_as_spec_fun();
            let loc = et.to_loc(&body.loc);
            for (n, ty) in &type_params {
                et.define_type_param(&loc, *n, ty.clone());
            }
            et.enter_scope();
            for (idx, (n, ty)) in params.iter().enumerate() {
                et.define_local(&loc, *n, ty.clone(), None, Some(idx));
            }
            let translated = et.translate_seq(&loc, &seq, &result_type);
            et.finalize_types();
            // If no errors were generated, then the function is considered pure.
            if !*et.errors_generated.borrow() {
                // Rewrite all type annotations in expressions to skip references.
                for node_id in translated.node_ids() {
                    let ty = et.get_node_type(node_id);
                    et.set_node_type(node_id, ty.skip_reference().clone());
                }
                et.called_spec_funs.iter().for_each(|(mid, fid)| {
                    self.parent.add_edge_to_move_fun_call_graph(
                        self.module_id,
                        SpecFunId::new(fun_idx),
                        *mid,
                        *fid,
                    );
                });
                self.spec_funs[self.spec_fun_index].body = Some(translated);
            }
        }
        self.spec_fun_index += 1;
    }

    /// Propagate the impurity of Move functions from callees to callers so
    /// that we can detect pure-looking Move functions which calls impure
    /// Move functions.
    fn propagate_function_impurity(
        &mut self,
        mut visited: &mut BTreeMap<SpecFunId, bool>,
        spec_fun_id: SpecFunId,
    ) -> bool {
        if let Some(is_pure) = visited.get(&spec_fun_id) {
            return *is_pure;
        }
        let spec_fun_idx = spec_fun_id.as_usize();
        let body = if self.spec_funs[spec_fun_idx].body.is_some() {
            std::mem::replace(&mut self.spec_funs[spec_fun_idx].body, None).unwrap()
        } else {
            // If the function is native and contains no mutable references
            // as parameters, consider it pure.
            // Otherwise the function is non-native, its body cannot be parsed
            // so we consider it impure.
            // TODO(emmazzz) right now all the native Move functions without
            // parameters of type mutable references are considered pure.
            // In the future we might want to only allow a certain subset of the
            // native Move functions, through something similar to an allow list or
            // a pragma.
            let no_mut_ref_param = self.spec_funs[spec_fun_idx]
                .params
                .iter()
                .map(|(_, ty)| !ty.is_mutable_reference())
                .all(|b| b); // `no_mut_ref_param` if none of the types are mut refs.
            return self.spec_funs[spec_fun_idx].is_native && no_mut_ref_param;
        };
        let mut is_pure = true;
        body.visit(&mut |e: &Exp| {
            if let Exp::Call(_, Operation::Function(mid, fid, _), _) = e {
                if mid.to_usize() < self.module_id.to_usize() {
                    // This is calling a function from another module we already have
                    // translated. In this case, the impurity has already been propagated
                    // in translate_call.
                } else {
                    // This is calling a function from the module we are currently translating.
                    // Need to recursively ensure we have propagated impurity because of
                    // arbitrary call graphs, including cyclic.
                    if !self.propagate_function_impurity(&mut visited, *fid) {
                        is_pure = false;
                    }
                }
            }
        });
        if is_pure {
            // Restore the function body if the Move function is pure.
            self.spec_funs[spec_fun_idx].body = Some(body);
        }
        visited.insert(spec_fun_id, is_pure);
        is_pure
    }

    fn deref_move_fun_types(&mut self, full_name: QualifiedSymbol, spec_fun_idx: usize) {
        self.parent.spec_fun_table.entry(full_name).and_modify(|e| {
            assert!(e.len() == 1);
            e[0].arg_types = e[0]
                .arg_types
                .iter()
                .map(|ty| ty.skip_reference().clone())
                .collect_vec();
            e[0].type_params = e[0]
                .type_params
                .iter()
                .map(|ty| ty.skip_reference().clone())
                .collect_vec();
            e[0].result_type = e[0].result_type.skip_reference().clone();
        });

        let spec_fun_decl = &mut self.spec_funs[spec_fun_idx];
        spec_fun_decl.params = spec_fun_decl
            .params
            .iter()
            .map(|(s, ty)| (*s, ty.skip_reference().clone()))
            .collect_vec();
        spec_fun_decl.type_params = spec_fun_decl
            .type_params
            .iter()
            .map(|(s, ty)| (*s, ty.skip_reference().clone()))
            .collect_vec();
        spec_fun_decl.result_type = spec_fun_decl.result_type.skip_reference().clone();
    }
}

/// ## Spec Block Definition Analysis

impl<'env, 'translator> ModuleBuilder<'env, 'translator> {
    fn def_ana_spec_block(&mut self, context: &SpecBlockContext<'_>, block: &EA::SpecBlock) {
        use EA::SpecBlockMember_::*;

        let block_loc = self.parent.env.to_loc(&block.loc);
        self.update_spec(context, move |spec| spec.loc = Some(block_loc));

        assert!(self.spec_block_lets.is_empty());

        for member in &block.value.members {
            let loc = &self.parent.env.to_loc(&member.loc);
            match &member.value {
                Condition {
                    kind,
                    properties,
                    exp,
                    additional_exps,
                } => {
                    if let Some((kind, exp)) = self.extract_condition_kind(context, kind, exp) {
                        let properties = self.translate_properties(properties, &|prop| {
                            if !is_property_valid_for_condition(&kind, prop) {
                                Some(loc.clone())
                            } else {
                                None
                            }
                        });
                        self.def_ana_condition(loc, context, kind, properties, exp, additional_exps)
                    }
                }
                Function {
                    signature, body, ..
                } => self.def_ana_spec_fun(signature, body),
                Let { name, def } => self.def_ana_let(context, loc, name, def),
                Include { properties, exp } => {
                    let properties = self.translate_properties(properties, &|_| None);
                    self.def_ana_schema_inclusion_outside_schema(
                        loc, context, None, properties, exp,
                    )
                }
                Apply {
                    exp,
                    patterns,
                    exclusion_patterns,
                } => self.def_ana_schema_apply(loc, context, exp, patterns, exclusion_patterns),
                Pragma { properties } => self.def_ana_pragma(loc, context, properties),
                Variable { .. } => { /* nothing to do right now */ }
            }
        }

        // clear the let bindings stored in the build.
        self.spec_block_lets.clear();
    }
}

/// ## Let Definition Analysis

impl<'env, 'translator> ModuleBuilder<'env, 'translator> {
    fn def_ana_let(
        &mut self,
        context: &SpecBlockContext<'_>,
        loc: &Loc,
        name: &Name,
        def: &EA::Exp,
    ) {
        // Check the expression and extract results. The let expression can access parameters,
        // but not use the `old(..)` function, therefore we treat it like a requires or aborts_if.
        let mut et = self
            .exp_translator_for_context(loc, context, Some(&ConditionKind::Requires), true)
            .set_in_let()
            .mark_context_scopes();
        let (_, mut def) = et.translate_exp_free(def);
        let mut generated_type_params = et.get_type_params();
        et.fix_types(&mut generated_type_params);
        let accessed_locals = et.get_accessed_context_locals();

        // Rewrite all type annotations in expressions to skip references. References
        // will be removed by the caller to the spec function representing the let.
        // In the body of a spec function, references do not appear.
        for node_id in def.node_ids() {
            let ty = et.get_node_type(node_id);
            et.set_node_type(node_id, ty.skip_reference().clone());
        }

        // Prepare the type_params field for SpecFunDecl.
        let mut type_params = vec![];
        for (i, param_type) in generated_type_params.into_iter().enumerate() {
            let name = et.symbol_pool().make(&format!("T{}", i));
            type_params.push((name, param_type))
        }

        // Prepare the params field for SpecFunDecl.
        let mut params = vec![];
        for (local, in_old) in &accessed_locals {
            let ty = et
                .lookup_local(*local, false)
                .expect("tracked local defined")
                .type_
                .skip_reference()
                .clone();
            let local = et.make_context_local_name(*local, *in_old);
            params.push((local, et.subs.specialize(&ty)))
        }

        // If the let definition is a lambda, add the parameters from the lambda, and
        // remove it from the definition.
        if let Exp::Lambda(_, vars, body) = def {
            for v in vars {
                let ty = et
                    .parent
                    .parent
                    .env
                    .get_node_type(v.id)
                    .skip_reference()
                    .clone();
                params.push((v.name, ty));
            }
            def = *body;
        }

        // Prepare the result_type for SpecFunDecl.
        let result_type = et.parent.parent.env.get_node_type(def.node_id());

        // Generate an id and name for the function representing this let.
        let fid = SpecFunId::new(et.parent.spec_funs.len());
        let fname = et
            .symbol_pool()
            .make(&format!("{}${}", name.value, fid.as_usize()));

        // Add a function declaration.
        self.spec_funs.push(SpecFunDecl {
            loc: loc.clone(),
            name: fname,
            type_params,
            params,
            context_params: Some(accessed_locals),
            result_type,
            used_spec_vars: Default::default(),
            used_memory: Default::default(),
            uninterpreted: false,
            is_move_fun: false,
            is_native: false,
            body: Some(def),
        });

        // Finally, check whether a let of this name is already defined, and add it to the
        // map which tracks lets in this block.
        let sym = self.symbol_pool().make(&name.value);
        if self.spec_block_lets.insert(sym, fid).is_some() {
            self.parent.error(
                &self.parent.to_loc(&name.loc),
                &format!("duplicate declaration of `{}`", name.value),
            );
        }
    }
}

/// ## Pragma Definition Analysis

impl<'env, 'translator> ModuleBuilder<'env, 'translator> {
    /// Definition analysis for a pragma.
    fn def_ana_pragma(
        &mut self,
        loc: &Loc,
        context: &SpecBlockContext,
        properties: &[EA::PragmaProperty],
    ) {
        let properties = self.translate_properties(properties, &|prop| {
            if !is_pragma_valid_for_block(context, prop) {
                Some(loc.clone())
            } else {
                None
            }
        });
        self.update_spec(context, move |spec| {
            spec.properties.extend(properties);
        });
    }

    /// Translate properties (of conditions or in pragmas), using the provided function
    /// to check their validness.
    fn translate_properties<F>(
        &mut self,
        properties: &[EA::PragmaProperty],
        check_prop: &F,
    ) -> PropertyBag
    where
        // Returns the location if not valid
        F: Fn(&str) -> Option<Loc>,
    {
        // For now we pass properties just on. We may want to check against a set of known
        // property names and types in the future.
        let mut props = PropertyBag::default();
        for prop in properties {
            let prop_str = prop.value.name.value.as_str();
            if let Some(loc) = check_prop(prop_str) {
                self.parent.error(
                    &loc,
                    &format!("property `{}` is not valid in this context", prop_str),
                );
            }
            let prop_name = self.symbol_pool().make(&prop.value.name.value);
            let value = if let Some(pv) = &prop.value.value {
                match pv {
                    EA::PragmaValue::Literal(ev) => {
                        let mut et = ExpTranslator::new(self);
                        if let Some((v, _)) = et.translate_value(ev) {
                            PropertyValue::Value(v)
                        } else {
                            // Error reported
                            continue;
                        }
                    }
                    EA::PragmaValue::Ident(ema) => match self.module_access_to_parts(ema) {
                        (None, sym) => PropertyValue::Symbol(sym),
                        _ => PropertyValue::QualifiedSymbol(self.module_access_to_qualified(ema)),
                    },
                }
            } else {
                PropertyValue::Value(Value::Bool(true))
            };
            props.insert(prop_name, value);
        }
        props
    }

    fn add_bool_property(&self, mut properties: PropertyBag, name: &str, val: bool) -> PropertyBag {
        let sym = self.symbol_pool().make(name);
        properties.insert(sym, PropertyValue::Value(Value::Bool(val)));
        properties
    }
}

/// ## General Helpers for Definition Analysis

impl<'env, 'translator> ModuleBuilder<'env, 'translator> {
    /// Updates the Spec of a given context via an update function.
    fn update_spec<F>(&mut self, context: &SpecBlockContext, update: F)
    where
        F: FnOnce(&mut Spec),
    {
        use SpecBlockContext::*;
        match context {
            Function(name) => update(
                &mut self
                    .fun_specs
                    .entry(name.symbol)
                    .or_insert_with(Spec::default),
            ),
            FunctionCode(name, spec_info) => update(
                &mut self
                    .fun_specs
                    .entry(name.symbol)
                    .or_insert_with(Spec::default)
                    .on_impl
                    .entry(spec_info.offset)
                    .or_insert_with(Spec::default),
            ),
            Schema(name) => update(
                &mut self
                    .parent
                    .spec_schema_table
                    .get_mut(name)
                    .expect("schema defined")
                    .spec,
            ),
            Struct(name) => update(
                &mut self
                    .struct_specs
                    .entry(name.symbol)
                    .or_insert_with(Spec::default),
            ),
            Module => update(&mut self.module_spec),
        }
    }

    /// Sets up an expression translator for the given spec block context. If kind
    /// is given, includes all the symbols which can be consumed by the condition,
    /// otherwise only defines type parameters.
    fn exp_translator_for_context<'module_translator>(
        &'module_translator mut self,
        loc: &Loc,
        context: &SpecBlockContext,
        kind_opt: Option<&ConditionKind>,
        result_as_symbol: bool,
    ) -> ExpTranslator<'env, 'translator, 'module_translator> {
        use SpecBlockContext::*;
        let allow_old = if let Some(kind) = kind_opt {
            kind.allows_old()
        } else {
            false
        };
        match context {
            Function(name) => {
                let entry = &self
                    .parent
                    .fun_table
                    .get(name)
                    .expect("invalid spec block context")
                    .clone();
                let mut et = ExpTranslator::new_with_old(self, allow_old);
                for (n, ty) in &entry.type_params {
                    et.define_type_param(loc, *n, ty.clone());
                }
                if let Some(kind) = kind_opt {
                    et.enter_scope();
                    for (idx, (n, ty)) in entry.params.iter().enumerate() {
                        et.define_local(loc, *n, ty.clone(), None, Some(idx));
                    }
                    // Define the placeholders for the result values of a function if this is an
                    // Ensures condition.
                    if matches!(kind, ConditionKind::Ensures) {
                        et.enter_scope();
                        if let Type::Tuple(ts) = &entry.result_type {
                            for (i, ty) in ts.iter().enumerate() {
                                let name = et.symbol_pool().make(&format!("result_{}", i + 1));
                                let oper = if result_as_symbol {
                                    None
                                } else {
                                    Some(Operation::Result(i))
                                };
                                et.define_local(loc, name, ty.clone(), oper, None);
                            }
                        } else {
                            let name = et.symbol_pool().make("result");
                            let oper = if result_as_symbol {
                                None
                            } else {
                                Some(Operation::Result(0))
                            };
                            et.define_local(loc, name, entry.result_type.clone(), oper, None);
                        }
                    }
                }
                et
            }
            FunctionCode(name, spec_info) => {
                let entry = &self
                    .parent
                    .fun_table
                    .get(name)
                    .expect("invalid spec block context")
                    .clone();
                let mut et = ExpTranslator::new_with_old(self, allow_old);
                for (n, ty) in &entry.type_params {
                    et.define_type_param(loc, *n, ty.clone());
                }
                if kind_opt.is_some() {
                    et.enter_scope();
                    for (_n_loc, n_, info) in &spec_info.used_locals {
                        let sym = et.symbol_pool().make(n_);
                        let ty = et.translate_hlir_single_type(&info.type_);
                        if ty == Type::Error {
                            et.error(
                                loc,
                                "[internal] error in translating hlir type to prover type",
                            );
                        }
                        et.define_local(loc, sym, ty, None, Some(info.index as usize));
                    }
                }
                et
            }
            Struct(name) => {
                let entry = &self
                    .parent
                    .struct_table
                    .get(name)
                    .expect("invalid spec block context")
                    .clone();

                let mut et = ExpTranslator::new_with_old(self, allow_old);
                for (n, ty) in &entry.type_params {
                    et.define_type_param(loc, *n, ty.clone());
                }
                if kind_opt.is_some() {
                    if let Some(fields) = &entry.fields {
                        et.enter_scope();
                        for (n, (_, ty)) in fields {
                            et.define_local(
                                loc,
                                *n,
                                ty.clone(),
                                Some(Operation::Select(
                                    entry.module_id,
                                    entry.struct_id,
                                    FieldId::new(*n),
                                )),
                                None,
                            );
                        }
                    }
                }
                et
            }
            Module => {
                if let Some(ConditionKind::InvariantUpdate) = &kind_opt {
                    ExpTranslator::new_with_old(self, true)
                } else {
                    ExpTranslator::new(self)
                }
            }
            Schema(name) => {
                let entry = self
                    .parent
                    .spec_schema_table
                    .get(name)
                    .expect("schema defined");
                // Unfortunately need to clone elements from the entry because we need mut borrow
                // of self for expression build.
                let type_params = entry.type_params.clone();
                let all_vars = entry.all_vars.clone();
                let mut et = ExpTranslator::new_with_old(self, allow_old);
                for (n, ty) in type_params {
                    et.define_type_param(loc, n, ty);
                }
                if kind_opt.is_some() {
                    et.enter_scope();
                    for (n, entry) in all_vars {
                        et.define_local(loc, n, entry.type_, None, None);
                    }
                }
                et
            }
        }
    }
}

/// ## Condition Definition Analysis

impl<'env, 'translator> ModuleBuilder<'env, 'translator> {
    /// Check whether the condition is allowed in the given context. Return true if so, otherwise
    /// report an error and return false.
    fn check_condition_is_valid(
        &mut self,
        context: &SpecBlockContext,
        loc: &Loc,
        kind: &ConditionKind,
        detail: &str,
    ) -> bool {
        use SpecBlockContext::*;
        let mut notes = vec![];
        let ok = match context {
            Module => kind.allowed_on_module(),
            Struct(s) => {
                let mut ok = true;
                if !kind.allowed_on_struct() {
                    ok = false;
                } else if matches!(
                    kind,
                    ConditionKind::VarPack(..)
                        | ConditionKind::VarUnpack(..)
                        | ConditionKind::VarUpdate(..)
                ) {
                    // The struct must be a resource.
                    if let Some(entry) = self.parent.struct_table.get(s) {
                        if !entry.is_resource {
                            notes.push(
                                "global var updates can only be used with \
                                       resources since they require linear semantics"
                                    .to_string(),
                            );
                            ok = false
                        }
                    }
                };
                ok
            }
            Function(name) => {
                let entry = self.parent.fun_table.get(name).expect("function defined");
                if entry.is_public {
                    kind.allowed_on_public_fun_decl()
                } else {
                    kind.allowed_on_private_fun_decl()
                }
            }
            FunctionCode(_, _) => kind.allowed_on_fun_impl(),
            Schema(_) => true,
        };
        if !ok {
            self.parent.error_with_notes(
                loc,
                &format!("`{}` not allowed in {} {}", kind, context, detail),
                notes,
            );
        }
        ok
    }

    /// Add the given conditions to the context, after checking whether they are valid in the
    /// context. Reports errors for invalid conditions.
    fn add_conditions_to_context(
        &mut self,
        context: &SpecBlockContext,
        loc: &Loc,
        conditions: Vec<Condition>,
        context_properties: PropertyBag,
        error_msg: &str,
    ) {
        for cond in conditions {
            // If this is an invariant on a function decl, transform it into a pair of
            // requires and ensures.
            let derived_conds = match cond {
                Condition {
                    loc,
                    kind: kind @ ConditionKind::Invariant,
                    properties,
                    exp,
                    additional_exps,
                }
                | Condition {
                    loc,
                    kind: kind @ ConditionKind::InvariantModule,
                    properties,
                    exp,
                    additional_exps,
                } if matches!(context, SpecBlockContext::Function(..)) => {
                    let requires_kind = if kind == ConditionKind::InvariantModule {
                        ConditionKind::RequiresModule
                    } else {
                        ConditionKind::Requires
                    };
                    let mut merged_properties = context_properties.clone();
                    merged_properties.extend(properties);
                    vec![
                        Condition {
                            loc: loc.clone(),
                            kind: requires_kind,
                            properties: merged_properties.clone(),
                            exp: exp.clone(),
                            additional_exps: additional_exps.clone(),
                        },
                        Condition {
                            loc,
                            kind: ConditionKind::Ensures,
                            properties: merged_properties,
                            exp,
                            additional_exps,
                        },
                    ]
                }
                Condition {
                    loc,
                    kind,
                    properties,
                    exp,
                    additional_exps,
                } => {
                    let mut merged_properties = context_properties.clone();
                    merged_properties.extend(properties);
                    vec![Condition {
                        loc,
                        kind,
                        properties: merged_properties,
                        exp,
                        additional_exps,
                    }]
                }
            };
            for derived_cond in derived_conds {
                if self.check_condition_is_valid(context, loc, &derived_cond.kind, error_msg)
                    && !self
                        .parent
                        .env
                        .is_property_true(&derived_cond.properties, CONDITION_DEACTIVATED_PROP)
                        .unwrap_or(false)
                {
                    self.update_spec(context, |spec| spec.conditions.push(derived_cond));
                }
            }
        }
    }

    /// Definition analysis for a condition.
    fn def_ana_condition(
        &mut self,
        loc: &Loc,
        context: &SpecBlockContext,
        kind: ConditionKind,
        properties: PropertyBag,
        exp: &EA::Exp,
        additional_exps: &[EA::Exp],
    ) {
        if kind == ConditionKind::Decreases {
            self.parent
                .error(loc, "decreases specification not supported currently");
            return;
        }
        if matches!(kind, ConditionKind::SucceedsIf) {
            self.parent.error(loc, "condition kind is not supported");
            return;
        }
        let expected_type = self.expected_type_for_condition(&kind);
        let mut et = self.exp_translator_for_context(loc, context, Some(&kind), false);
        let (translated, translated_additional) = match kind {
            ConditionKind::AbortsIf => (
                et.translate_exp(exp, &expected_type),
                additional_exps
                    .iter()
                    .map(|code| et.translate_exp(code, &Type::Primitive(PrimitiveType::Num)))
                    .collect_vec(),
            ),
            ConditionKind::AbortsWith => {
                // Parser has created a dummy exp, codes are all in additional_exps
                let mut exps = additional_exps
                    .iter()
                    .map(|code| et.translate_exp(code, &Type::Primitive(PrimitiveType::Num)))
                    .collect_vec();
                let first = exps.remove(0);
                (first, exps)
            }
            ConditionKind::Modifies => {
                // Parser has created a dummy exp, targets are all in additional_exps
                let mut exps = additional_exps
                    .iter()
                    .map(|target| et.translate_modify_target(target))
                    .collect_vec();
                let first = exps.remove(0);
                (first, exps)
            }
            ConditionKind::Emits => {
                // TODO: `first` is the "message" part, and `second` is the "handle" part.
                //       `second` should have type 0x1::Event::EventHandle<T>, and `first`
                //       should have type T.
                let (_, first) = et.translate_exp_free(exp);
                let (_, second) = et.translate_exp_free(&additional_exps[0]);
                let mut exps = vec![second];
                if additional_exps.len() > 1 {
                    exps.push(et.translate_exp(&additional_exps[1], &BOOL_TYPE));
                }
                (first, exps)
            }
            _ => {
                if !additional_exps.is_empty() {
                    et.error(
                       loc,
                       "additional expressions only allowed with `aborts_if`, `aborts_with`, `modifies`, or `emits`",
                   );
                }
                (et.translate_exp(exp, &expected_type), vec![])
            }
        };
        et.finalize_types();
        self.add_conditions_to_context(
            context,
            loc,
            vec![Condition {
                loc: loc.clone(),
                kind,
                properties,
                exp: translated,
                additional_exps: translated_additional,
            }],
            PropertyBag::default(),
            "",
        );
    }

    /// Compute the expected type for the expression in a condition.
    fn expected_type_for_condition(&mut self, kind: &ConditionKind) -> Type {
        if let Some((mid, vid, ty_args)) = kind.get_spec_var_target() {
            if mid == self.module_id {
                self.spec_vars[vid.as_usize()]
                    .type_
                    .clone()
                    .instantiate(&ty_args)
            } else {
                let module_env = self.parent.env.get_module(mid);
                module_env
                    .get_spec_var(vid)
                    .type_
                    .clone()
                    .instantiate(&ty_args)
            }
        } else {
            BOOL_TYPE.clone()
        }
    }

    /// Extracts a condition kind based on the parsed kind and the associated expression. This
    /// identifies a spec var assignment expression and moves the var to the SpecConditionKind enum
    /// we use in our AST, returning the rhs expression of the assignment; otherwise it returns
    /// the passed expression.
    fn extract_condition_kind<'a>(
        &mut self,
        context: &SpecBlockContext,
        kind: &PA::SpecConditionKind,
        exp: &'a EA::Exp,
    ) -> Option<(ConditionKind, &'a EA::Exp)> {
        use ConditionKind::*;
        use PA::SpecConditionKind as PK;
        let loc = self.parent.env.to_loc(&exp.loc);
        match kind {
            PK::Assert => Some((Assert, exp)),
            PK::Assume => Some((Assume, exp)),
            PK::Decreases => Some((Decreases, exp)),
            PK::Modifies => Some((Modifies, exp)),
            PK::Emits => Some((Emits, exp)),
            PK::Ensures => Some((Ensures, exp)),
            PK::Requires => Some((Requires, exp)),
            PK::AbortsIf => Some((AbortsIf, exp)),
            PK::AbortsWith => Some((AbortsWith, exp)),
            PK::SucceedsIf => Some((SucceedsIf, exp)),
            PK::RequiresModule => Some((RequiresModule, exp)),
            PK::Invariant => Some((Invariant, exp)),
            PK::InvariantModule => Some((InvariantModule, exp)),
            PK::InvariantUpdate => {
                if let Some((mid, vid, tys, exp1)) = self.extract_assignment(context, exp) {
                    Some((VarUpdate(mid, vid, tys), exp1))
                } else {
                    Some((InvariantUpdate, exp))
                }
            }
            PK::InvariantPack => {
                if let Some((mid, vid, tys, exp1)) = self.extract_assignment(context, exp) {
                    Some((VarPack(mid, vid, tys), exp1))
                } else {
                    self.parent
                        .error(&loc, "expected assignment to spec variable");
                    None
                }
            }
            PK::InvariantUnpack => {
                if let Some((mid, vid, tys, exp1)) = self.extract_assignment(context, exp) {
                    Some((VarUnpack(mid, vid, tys), exp1))
                } else {
                    self.parent
                        .error(&loc, "expected assignment to spec variable");
                    None
                }
            }
        }
    }

    /// Extracts an assignment from an expression, returning the assigned spec var and
    /// rhs expression.
    fn extract_assignment<'a>(
        &mut self,
        context: &SpecBlockContext,
        exp: &'a EA::Exp,
    ) -> Option<(ModuleId, SpecVarId, Vec<Type>, &'a EA::Exp)> {
        if let EA::Exp_::Assign(list, rhs) = &exp.value {
            let var_loc = self.parent.to_loc(&list.loc);
            if list.value.len() != 1 {
                self.parent.error(
                    &var_loc,
                    "[current restriction] tuples not supported in assignment",
                );
                return None;
            }
            if let EA::LValue_::Var(maccess, tys_opt) = &list.value[0].value {
                let var_name = self.module_access_to_qualified(maccess);
                let mut et = self.exp_translator_for_context(&var_loc, context, None, false);
                let tys = tys_opt
                    .as_ref()
                    .map(|tys| et.translate_types(tys))
                    .unwrap_or_else(Vec::new);
                if let Some(spec_var) = et.parent.parent.spec_var_table.get(&var_name) {
                    Some((spec_var.module_id, spec_var.var_id, tys, rhs.as_ref()))
                } else {
                    et.error(
                        &var_loc,
                        &format!(
                            "spec global `{}` undeclared",
                            var_name.display(et.symbol_pool())
                        ),
                    );
                    None
                }
            } else {
                self.parent.error(
                    &var_loc,
                    "[current restriction] unpack not supported in assignment",
                );
                None
            }
        } else {
            None
        }
    }
}

/// ## Spec Function Definition Analysis

impl<'env, 'translator> ModuleBuilder<'env, 'translator> {
    /// Definition analysis for a specification helper function.
    fn def_ana_spec_fun(&mut self, _signature: &EA::FunctionSignature, body: &EA::FunctionBody) {
        if let EA::FunctionBody_::Defined(seq) = &body.value {
            let entry = &self.spec_funs[self.spec_fun_index];
            let type_params = entry.type_params.clone();
            let params = entry.params.clone();
            let result_type = entry.result_type.clone();
            let mut et = ExpTranslator::new(self);
            let loc = et.to_loc(&body.loc);
            for (n, ty) in type_params {
                et.define_type_param(&loc, n, ty);
            }
            et.enter_scope();
            for (n, ty) in params {
                et.define_local(&loc, n, ty, None, None);
            }
            let translated = et.translate_seq(&loc, seq, &result_type);
            et.finalize_types();
            self.spec_funs[self.spec_fun_index].body = Some(translated);
        }
        self.spec_fun_index += 1;
    }
}

/// ## Schema Definition Analysis

impl<'env, 'translator> ModuleBuilder<'env, 'translator> {
    /// Definition analysis for a schema. This proceeds in two steps: first we ensure recursively
    /// that all included schemas are analyzed, checking for cycles. Then we actually analyze this
    /// schema's content.
    fn def_ana_schema(
        &mut self,
        schema_defs: &BTreeMap<QualifiedSymbol, &EA::SpecBlock>,
        visited: &mut BTreeSet<QualifiedSymbol>,
        visiting: &mut Vec<QualifiedSymbol>,
        name: QualifiedSymbol,
        block: &EA::SpecBlock,
    ) {
        if !visited.insert(name.clone()) {
            // Already analyzed.
            return;
        }
        visiting.push(name.clone());

        // First recursively visit all schema includes and ensure they are analyzed.
        for included_name in self
            .iter_schema_includes(&block.value.members)
            .map(|(_, _, exp)| {
                let mut res = vec![];
                extract_schema_access(exp, &mut res);
                res
            })
            .flatten()
        {
            let included_loc = self.parent.env.to_loc(&included_name.loc);
            let included_name = self.module_access_to_qualified(included_name);
            if included_name.module_name == self.module_name {
                // A schema in the module we are currently analyzing. We need to check
                // for cycles before recursively analyzing it.
                if visiting.contains(&included_name) {
                    self.parent.error(
                        &included_loc,
                        &format!(
                            "cyclic schema dependency: {} -> {}",
                            visiting
                                .iter()
                                .map(|name| format!("{}", name.display_simple(self.symbol_pool())))
                                .join(" -> "),
                            included_name.display_simple(self.symbol_pool())
                        ),
                    )
                } else if let Some(included_block) = schema_defs.get(&included_name) {
                    // Recursively analyze it, if its defined. If not, we report an undeclared
                    // error in 2nd phase.
                    self.def_ana_schema(
                        schema_defs,
                        visited,
                        visiting,
                        included_name,
                        included_block,
                    );
                }
            }
        }

        // Now actually analyze this schema.
        self.def_ana_schema_content(name, block);

        // Remove from visiting list
        visiting.pop();
    }

    /// Analysis of schema after it is ensured that all included schemas are fully analyzed.
    fn def_ana_schema_content(&mut self, name: QualifiedSymbol, block: &EA::SpecBlock) {
        let loc = self.parent.env.to_loc(&block.loc);
        let entry = self
            .parent
            .spec_schema_table
            .get(&name)
            .expect("schema defined");
        let type_params = entry.type_params.clone();
        let mut all_vars: BTreeMap<Symbol, LocalVarEntry> = entry
            .vars
            .iter()
            .map(|(n, ty)| {
                (
                    *n,
                    LocalVarEntry {
                        loc: loc.clone(),
                        type_: ty.clone(),
                        operation: None,
                        temp_index: None,
                    },
                )
            })
            .collect();
        let mut included_spec = Spec::default();

        // Store back all_vars computed so far (which does not include those coming from
        // included schemas). This is needed so we can analyze lets.
        {
            let entry = self
                .parent
                .spec_schema_table
                .get_mut(&name)
                .expect("schema defined");
            entry.all_vars = all_vars.clone();
        }

        // Process all lets. We need to do this before includes so we have them available
        // in schema arguments of includes. This unfortunately means we can't refer in
        // lets to variables included from schemas, but this seems to be a rare use case.
        assert!(self.spec_block_lets.is_empty());
        for member in &block.value.members {
            let member_loc = self.parent.to_loc(&member.loc);
            if let EA::SpecBlockMember_::Let {
                name: let_name,
                def,
            } = &member.value
            {
                let context = SpecBlockContext::Schema(name.clone());
                self.def_ana_let(&context, &member_loc, let_name, def);
            }
        }

        // Process all schema includes. We need to do this before we type check expressions to have
        // all variables from includes in the environment.
        for (_, included_props, included_exp) in self.iter_schema_includes(&block.value.members) {
            let included_props = self.translate_properties(included_props, &|_| None);
            self.def_ana_schema_exp(
                &type_params,
                &mut all_vars,
                &mut included_spec,
                true,
                &included_props,
                included_exp,
            );
        }
        // Store the results back to the schema entry.
        {
            let entry = self
                .parent
                .spec_schema_table
                .get_mut(&name)
                .expect("schema defined");
            entry.all_vars = all_vars;
            entry.included_spec = included_spec;
        }

        // Now process all conditions and invariants.
        for member in &block.value.members {
            let member_loc = self.parent.to_loc(&member.loc);
            match &member.value {
                EA::SpecBlockMember_::Variable {
                    is_global: false, ..
                } => { /* handled during decl analysis */ }
                EA::SpecBlockMember_::Include { .. } => { /* handled above */ }
                EA::SpecBlockMember_::Let { .. } => { /* handled above */ }
                EA::SpecBlockMember_::Condition {
                    kind,
                    properties,
                    exp,
                    additional_exps,
                } => {
                    let context = SpecBlockContext::Schema(name.clone());
                    if let Some((kind, exp)) = self.extract_condition_kind(&context, kind, exp) {
                        let properties = self.translate_properties(properties, &|prop| {
                            if !is_property_valid_for_condition(&kind, prop) {
                                Some(member_loc.clone())
                            } else {
                                None
                            }
                        });
                        self.def_ana_condition(
                            &member_loc,
                            &context,
                            kind,
                            properties,
                            exp,
                            additional_exps,
                        );
                    } else {
                        // Error reported.
                    }
                }
                _ => {
                    self.parent.error(&member_loc, "item not allowed in schema");
                }
            };
        }
        self.spec_block_lets.clear();
    }

    /// Extracts all schema inclusions from a list of spec block members.
    fn iter_schema_includes<'a>(
        &self,
        members: &'a [EA::SpecBlockMember],
    ) -> impl Iterator<Item = (&'a MoveIrLoc, &'a Vec<EA::PragmaProperty>, &'a EA::Exp)> {
        members.iter().filter_map(|m| {
            if let EA::SpecBlockMember_::Include { properties, exp } = &m.value {
                Some((&m.loc, properties, exp))
            } else {
                None
            }
        })
    }

    /// Analyzes a schema expression. Depending on whether `allow_new_vars` is true, this will
    /// add new variables to `vars` and match types of existing ones. All conditions
    /// from the schema are rewritten for the inclusion context and added to the provided spec.
    ///
    /// We accept a very restricted set of Move expressions for schemas:
    ///
    /// - `P ==> SchemaExp`: all conditions in the schema will be prefixed with `P ==> ..`.
    ///   Conditions which are not based on boolean expressions (as VarUpdate et. al) will
    ///   be rejected.
    /// - `if (P) SchemaExp else SchemaExp`: this is treated similar as one include for
    ///   `P ==> SchemaExp` and one for `!P ==> SchemaExp`.
    /// - `SchemaExp1 && SchemaExp2`: this is treated as two includes for the both expressions.
    /// - `SchemaExp1 || SchemaExp2`: this could be treated as
    ///   `exists b: bool :: if (b) SchemaExp1 else SchemaExp2` (but as we do not have the
    ///   existential quantifier yet in the spec language, it is actually not supported..)
    ///
    /// The implementation works via a recursive function which accumulates a path condition
    /// leading to a Move "pack" expression which is interpreted as a schema reference.
    fn def_ana_schema_exp(
        &mut self,
        context_type_params: &[(Symbol, Type)],
        vars: &mut BTreeMap<Symbol, LocalVarEntry>,
        spec: &mut Spec,
        allow_new_vars: bool,
        properties: &PropertyBag,
        exp: &EA::Exp,
    ) {
        self.def_ana_schema_exp_oper(
            context_type_params,
            vars,
            spec,
            allow_new_vars,
            None,
            properties,
            exp,
        )
    }

    /// Analyzes operations in schema expressions. This extends the path condition as needed
    /// and continues recursively.
    fn def_ana_schema_exp_oper(
        &mut self,
        context_type_params: &[(Symbol, Type)],
        vars: &mut BTreeMap<Symbol, LocalVarEntry>,
        spec: &mut Spec,
        allow_new_vars: bool,
        path_cond: Option<Exp>,
        properties: &PropertyBag,
        exp: &EA::Exp,
    ) {
        let loc = self.parent.to_loc(&exp.loc);
        match &exp.value {
            EA::Exp_::BinopExp(
                lhs,
                Spanned {
                    value: PA::BinOp_::Implies,
                    ..
                },
                rhs,
            ) => {
                let mut et = self.exp_translator_for_schema(&loc, context_type_params, vars);
                let lhs_exp = et.translate_exp(lhs, &BOOL_TYPE);
                et.finalize_types();
                let path_cond = Some(self.extend_path_condition(&loc, path_cond, lhs_exp));
                self.def_ana_schema_exp_oper(
                    context_type_params,
                    vars,
                    spec,
                    allow_new_vars,
                    path_cond,
                    properties,
                    rhs,
                );
            }
            EA::Exp_::BinopExp(
                lhs,
                Spanned {
                    value: PA::BinOp_::And,
                    ..
                },
                rhs,
            ) => {
                self.def_ana_schema_exp_oper(
                    context_type_params,
                    vars,
                    spec,
                    allow_new_vars,
                    path_cond.clone(),
                    properties,
                    lhs,
                );
                self.def_ana_schema_exp_oper(
                    context_type_params,
                    vars,
                    spec,
                    allow_new_vars,
                    path_cond,
                    properties,
                    rhs,
                );
            }
            EA::Exp_::IfElse(c, t, e) => {
                let mut et = self.exp_translator_for_schema(&loc, context_type_params, vars);
                let c_exp = et.translate_exp(c, &BOOL_TYPE);
                et.finalize_types();
                let t_path_cond =
                    Some(self.extend_path_condition(&loc, path_cond.clone(), c_exp.clone()));
                self.def_ana_schema_exp_oper(
                    context_type_params,
                    vars,
                    spec,
                    allow_new_vars,
                    t_path_cond,
                    properties,
                    t,
                );
                let node_id = self.parent.env.new_node(loc.clone(), BOOL_TYPE.clone());
                let not_c_exp = Exp::Call(node_id, Operation::Not, vec![c_exp]);
                let e_path_cond = Some(self.extend_path_condition(&loc, path_cond, not_c_exp));
                self.def_ana_schema_exp_oper(
                    context_type_params,
                    vars,
                    spec,
                    allow_new_vars,
                    e_path_cond,
                    properties,
                    e,
                );
            }
            EA::Exp_::Name(maccess, type_args_opt) => self.def_ana_schema_exp_leaf(
                context_type_params,
                vars,
                spec,
                allow_new_vars,
                path_cond,
                properties,
                &loc,
                maccess,
                type_args_opt,
                None,
            ),
            EA::Exp_::Pack(maccess, type_args_opt, fields) => self.def_ana_schema_exp_leaf(
                context_type_params,
                vars,
                spec,
                allow_new_vars,
                path_cond,
                properties,
                &loc,
                maccess,
                type_args_opt,
                Some(fields),
            ),
            _ => self
                .parent
                .error(&loc, "expression construct not supported for schemas"),
        }
    }

    /// Analyzes a schema leaf expression.
    fn def_ana_schema_exp_leaf(
        &mut self,
        context_type_params: &[(Symbol, Type)],
        vars: &mut BTreeMap<Symbol, LocalVarEntry>,
        spec: &mut Spec,
        allow_new_vars: bool,
        path_cond: Option<Exp>,
        schema_properties: &PropertyBag,
        loc: &Loc,
        maccess: &EA::ModuleAccess,
        type_args_opt: &Option<Vec<EA::Type>>,
        args_opt: Option<&EA::Fields<EA::Exp>>,
    ) {
        let schema_name = self.module_access_to_qualified(maccess);

        // Remove schema from unused table since it is used in an expression
        self.parent.unused_schema_set.remove(&schema_name);

        // We need to temporarily detach the schema entry from the parent table because of
        // borrowing problems, as we need to traverse it while at the same time mutate self.
        let schema_entry = if let Some(e) = self.parent.spec_schema_table.remove(&schema_name) {
            e
        } else {
            self.parent.error(
                loc,
                &format!(
                    "schema `{}` undeclared",
                    schema_name.display(self.symbol_pool())
                ),
            );
            return;
        };

        // Translate type arguments
        let mut et = self.exp_translator_for_schema(&loc, context_type_params, vars);
        let type_arguments = &et.translate_types_opt(type_args_opt);
        if schema_entry.type_params.len() != type_arguments.len() {
            self.parent.error(
                loc,
                &format!(
                    "wrong number of type arguments (expected {}, got {})",
                    schema_entry.type_params.len(),
                    type_arguments.len()
                ),
            );
            // Don't forget to put schema back.
            self.parent
                .spec_schema_table
                .insert(schema_name, schema_entry);
            return;
        }

        // Translate schema arguments.
        let mut argument_map: BTreeMap<Symbol, Exp> = args_opt
            .map(|args| {
                args.iter()
                    .map(|(var_loc, schema_var_, (_, exp))| {
                        let pool = et.symbol_pool();
                        let schema_sym = pool.make(schema_var_);
                        let schema_type = if let Some(LocalVarEntry { type_, .. }) =
                            schema_entry.all_vars.get(&schema_sym)
                        {
                            type_.instantiate(type_arguments)
                        } else {
                            et.error(
                                &et.to_loc(&var_loc),
                                &format!("`{}` not declared in schema", schema_sym.display(pool)),
                            );
                            Type::Error
                        };
                        // Check the expression in the argument list.
                        // Note we currently only use the vars defined so far in this context. Variables
                        // which are introduced by schemas after the inclusion of this one are not in scope.
                        let exp = et.translate_exp(exp, &schema_type);
                        et.finalize_types();
                        (schema_sym, exp)
                    })
                    .collect()
            })
            .unwrap_or_else(BTreeMap::new);

        // Go over all variables in the schema which are not in the argument map and either match
        // them against existing one or declare new, if allowed.
        for (name, LocalVarEntry { type_, .. }) in &schema_entry.all_vars {
            if argument_map.contains_key(name) {
                continue;
            }
            let ty = type_.instantiate(type_arguments);
            let pool = et.symbol_pool();
            if let Some(entry) = vars.get(name) {
                // Name already exists in inclusion context, check its type.
                et.check_type(
                    loc,
                    &ty,
                    &entry.type_,
                    &format!(
                        "for `{}` included from schema",
                        name.display(et.symbol_pool())
                    ),
                );
                // Put into argument map.
                let node_id = et.new_node_id_with_type_loc(&entry.type_, loc);
                let exp = if let Some(oper) = &entry.operation {
                    Exp::Call(node_id, oper.clone(), vec![])
                } else if let Some(index) = &entry.temp_index {
                    Exp::Temporary(node_id, *index)
                } else {
                    Exp::LocalVar(node_id, *name)
                };
                argument_map.insert(*name, exp);
            } else if allow_new_vars {
                // Name does not yet exists in inclusion context, but is allowed to be introduced.
                // This happens if we include a schema in another schema.
                vars.insert(
                    *name,
                    LocalVarEntry {
                        loc: loc.clone(),
                        type_: ty.clone(),
                        operation: None,
                        temp_index: None,
                    },
                );
            } else {
                et.error(
                    loc,
                    &format!(
                        "`{}` cannot be matched to an existing name in inclusion context",
                        name.display(pool)
                    ),
                );
            }
        }
        // Done with expression build; ensure all types are inferred correctly.
        et.finalize_types();

        // Go over all conditions in the schema, rewrite them, and add to the inclusion conditions.
        for Condition {
            loc,
            kind,
            properties,
            exp,
            additional_exps,
        } in schema_entry
            .spec
            .conditions
            .iter()
            .chain(schema_entry.included_spec.conditions.iter())
        {
            let mut replacer = |_, target: RewriteTarget| {
                if let RewriteTarget::LocalVar(sym) = target {
                    argument_map.get(&sym).cloned()
                } else {
                    None
                }
            };
            let mut rewriter =
                ExpRewriter::new(self.parent.env, &mut replacer).set_type_args(type_arguments);
            let mut exp = rewriter.rewrite(exp);
            let mut additional_exps = rewriter.rewrite_vec(additional_exps);
            if let Some(cond) = &path_cond {
                // There is a path condition to be added. This is only possible for proper
                // boolean conditions.
                if kind.get_spec_var_target().is_some() {
                    self.parent
                        .error(loc, &format!("`{}` cannot be included conditionally", kind));
                } else if kind == &ConditionKind::Emits {
                    let cond_exp = if additional_exps.len() < 2 {
                        cond.clone()
                    } else {
                        self.make_path_expr(
                            Operation::And,
                            cond.node_id(),
                            cond.clone(),
                            additional_exps.pop().unwrap(),
                        )
                    };
                    additional_exps.push(cond_exp);
                } else {
                    // In case of AbortsIf, the path condition is combined with the predicate using
                    // &&, otherwise ==>.
                    exp = self.make_path_expr(
                        if kind == &ConditionKind::AbortsIf {
                            Operation::And
                        } else {
                            Operation::Implies
                        },
                        cond.node_id(),
                        cond.clone(),
                        exp,
                    );
                }
            }
            let mut effective_properties = schema_properties.clone();
            effective_properties.extend(properties.clone());
            spec.conditions.push(Condition {
                loc: loc.clone(),
                kind: kind.clone(),
                properties: effective_properties,
                exp,
                additional_exps,
            });
        }

        // Put schema entry back.
        self.parent
            .spec_schema_table
            .insert(schema_name, schema_entry);
    }

    /// Make a path expression.
    fn make_path_expr(&mut self, oper: Operation, node_id: NodeId, cond: Exp, exp: Exp) -> Exp {
        let env = &self.parent.env;
        let path_cond_loc = env.get_node_loc(node_id);
        let new_node_id = env.new_node(path_cond_loc, BOOL_TYPE.clone());
        Exp::Call(new_node_id, oper, vec![cond, exp])
    }

    /// Creates an expression translator for use in schema expression. This defines the context
    /// type parameters and the variables.
    fn exp_translator_for_schema<'module_translator>(
        &'module_translator mut self,
        loc: &Loc,
        context_type_params: &[(Symbol, Type)],
        vars: &mut BTreeMap<Symbol, LocalVarEntry>,
    ) -> ExpTranslator<'env, 'translator, 'module_translator> {
        let mut et = ExpTranslator::new(self);
        for (n, ty) in context_type_params {
            et.define_type_param(loc, *n, ty.clone())
        }
        et.enter_scope();
        for (n, entry) in vars.iter() {
            et.define_local(
                &entry.loc,
                *n,
                entry.type_.clone(),
                entry.operation.clone(),
                entry.temp_index,
            );
        }
        et
    }

    /// Extends a path condition for schema expression analysis.
    fn extend_path_condition(&mut self, loc: &Loc, path_cond: Option<Exp>, exp: Exp) -> Exp {
        if let Some(cond) = path_cond {
            let node_id = self.parent.env.new_node(loc.clone(), BOOL_TYPE.clone());
            Exp::Call(node_id, Operation::And, vec![cond, exp])
        } else {
            exp
        }
    }

    /// Analyze schema inclusion in the spec block for a function, struct or module. This
    /// instantiates the schema and adds all conditions and invariants it contains to the context.
    ///
    /// The `alt_context_type_params` allows to use different type parameter names as would
    /// otherwise be inferred from the SchemaBlockContext. This is used for the apply weaving
    /// operator which allows to use different type parameter names than the function declarations
    /// to which it is applied to.
    fn def_ana_schema_inclusion_outside_schema(
        &mut self,
        loc: &Loc,
        context: &SpecBlockContext,
        alt_context_type_params: Option<&[(Symbol, Type)]>,
        context_properties: PropertyBag,
        exp: &EA::Exp,
    ) {
        // Compute the type parameters and variables this spec block uses. We do this by constructing
        // an expression build and immediately extracting  from it. Depending on whether in
        // function or struct context, we use a condition kind which defines the maximum
        // of available symbols. We need to potentially revise this to only declare variables which
        // have a proper use in a condition/invariant, depending on what is actually included in
        // the block.
        let (mut vars, context_type_params) = match context {
            SpecBlockContext::Function(..) | SpecBlockContext::FunctionCode(..) => {
                let et = self.exp_translator_for_context(
                    loc,
                    context,
                    Some(&ConditionKind::Ensures),
                    false,
                );
                (et.extract_var_map(), et.get_type_params_with_name())
            }
            SpecBlockContext::Struct(..) => {
                let et = self.exp_translator_for_context(
                    loc,
                    context,
                    Some(&ConditionKind::InvariantUpdate),
                    false,
                );
                (et.extract_var_map(), et.get_type_params_with_name())
            }
            SpecBlockContext::Module => (BTreeMap::new(), vec![]),
            SpecBlockContext::Schema { .. } => panic!("unexpected schema context"),
        };
        let mut spec = Spec::default();

        // Analyze the schema inclusion. This will instantiate conditions for
        // this block.
        self.def_ana_schema_exp(
            if let Some(type_params) = alt_context_type_params {
                type_params
            } else {
                &context_type_params
            },
            &mut vars,
            &mut spec,
            false,
            &PropertyBag::default(),
            exp,
        );

        // Write the conditions to the context item.
        self.add_conditions_to_context(
            context,
            loc,
            spec.conditions,
            context_properties,
            "(included from schema)",
        );
    }

    /// Analyzes a schema apply weaving operator.
    fn def_ana_schema_apply(
        &mut self,
        loc: &Loc,
        context: &SpecBlockContext,
        exp: &EA::Exp,
        patterns: &[PA::SpecApplyPattern],
        exclusion_patterns: &[PA::SpecApplyPattern],
    ) {
        if !matches!(context, SpecBlockContext::Module) {
            self.parent.error(
                loc,
                "the `apply` schema weaving operator can only be used inside a `spec module` block",
            );
            return;
        }
        for fun_name in self.parent.fun_table.keys().cloned().collect_vec() {
            // Note we need the vector clone above to avoid borrowing self for the
            // whole loop.
            let entry = self.parent.fun_table.get(&fun_name).unwrap();
            if entry.module_id != self.module_id {
                // Not a function from this module
                continue;
            }
            let is_public = entry.is_public;
            let type_arg_count = entry.type_params.len();
            let is_excluded = exclusion_patterns.iter().any(|p| {
                self.apply_pattern_matches(fun_name.symbol, is_public, type_arg_count, true, p)
            });
            if is_excluded {
                // Explicitly excluded from matching.
                continue;
            }
            if let Some(matched) = patterns.iter().find(|p| {
                self.apply_pattern_matches(fun_name.symbol, is_public, type_arg_count, false, p)
            }) {
                // This is a match, so apply this schema to this function.
                let type_params = {
                    let mut et = ExpTranslator::new(self);
                    et.analyze_and_add_type_params(&matched.value.type_parameters);
                    et.get_type_params_with_name()
                };
                // Create a property marking this as injected.
                let context_properties =
                    self.add_bool_property(PropertyBag::default(), CONDITION_INJECTED_PROP, true);
                self.def_ana_schema_inclusion_outside_schema(
                    loc,
                    &SpecBlockContext::Function(fun_name),
                    Some(&type_params),
                    context_properties,
                    exp,
                );
            }
        }
    }

    /// Returns true if the pattern matches the function of name, type arity, and
    /// visibility.
    ///
    /// The `ignore_type_args` parameter is used for exclusion matches. In exclusion matches we
    /// do not want to include type args because its to easy for a user to get this wrong, so
    /// we match based only on visibility and name pattern. On the other hand, we want a user
    /// in inclusion matches to use a pattern like `*<X>` to match any generic function with
    /// one type argument.
    fn apply_pattern_matches(
        &self,
        name: Symbol,
        is_public: bool,
        type_arg_count: usize,
        ignore_type_args: bool,
        pattern: &PA::SpecApplyPattern,
    ) -> bool {
        if !ignore_type_args && pattern.value.type_parameters.len() != type_arg_count {
            return false;
        }
        if let Some(v) = &pattern.value.visibility {
            match v {
                PA::FunctionVisibility::Public(..) => {
                    if !is_public {
                        return false;
                    }
                }
                PA::FunctionVisibility::Internal => {
                    if is_public {
                        return false;
                    }
                }
                PA::FunctionVisibility::Script(..) => {
                    // TODO: model script visibility properly
                    unimplemented!("Script visibility not supported yet")
                }
                PA::FunctionVisibility::Friend(..) => {
                    // TODO: model friend visibility properly
                    unimplemented!("Friend visibility not supported yet")
                }
            }
        }
        let rex = Regex::new(&format!(
            "^{}$",
            pattern
                .value
                .name_pattern
                .iter()
                .map(|p| match &p.value {
                    PA::SpecApplyFragment_::Wildcard => ".*".to_string(),
                    PA::SpecApplyFragment_::NamePart(n) => n.value.clone(),
                })
                .join("")
        ))
        .expect("regex valid");
        rex.is_match(self.symbol_pool().string(name).as_str())
    }
}

/// ## Spec Var Usage Analysis

impl<'env, 'translator> ModuleBuilder<'env, 'translator> {
    /// Compute state usage of spec funs.
    fn compute_state_usage(&mut self) {
        let mut visited = BTreeSet::new();
        for idx in 0..self.spec_funs.len() {
            self.compute_state_usage_for_fun(&mut visited, idx);
        }
        // Check for purity requirements. All data invariants must be pure expressions and
        // not depend on global state.
        let check_pure = |mid: ModuleId, fid: SpecFunId| {
            if mid.to_usize() < self.parent.env.get_module_count() {
                // This is calling a function from another module we already have
                // translated.
                let module_env = self.parent.env.get_module(mid);
                let fun_decl = module_env.get_spec_fun(fid);
                fun_decl.used_spec_vars.is_empty() && fun_decl.used_memory.is_empty()
            } else {
                // This is calling a function from the module we are currently translating.
                let fun_decl = &self.spec_funs[fid.as_usize()];
                fun_decl.used_spec_vars.is_empty() && fun_decl.used_memory.is_empty()
            }
        };
        for struct_spec in self.struct_specs.values() {
            for cond in &struct_spec.conditions {
                if cond.kind == ConditionKind::Invariant && !cond.exp.is_pure(&check_pure) {
                    self.parent.error(
                        &cond.loc,
                        "data invariants cannot depend on global state \
                        (directly or indirectly uses a global spec var or resource storage).",
                    );
                }
            }
        }
    }

    /// Compute state usage for a given spec fun, defined via its index into the spec_funs
    /// vector of the currently translated module. This recursively computes the values for
    /// functions called from this one; the visited set is there to break cycles.
    fn compute_state_usage_for_fun(&mut self, visited: &mut BTreeSet<usize>, fun_idx: usize) {
        if !visited.insert(fun_idx) {
            return;
        }

        // Detach the current SpecFunDecl body so we can traverse it while at the same time mutating
        // the full self. Rust requires us to do so (at least the author doesn't know better yet),
        // but moving it should be not too expensive.
        let body = if self.spec_funs[fun_idx].body.is_some() {
            std::mem::replace(&mut self.spec_funs[fun_idx].body, None).unwrap()
        } else {
            // No body: assume it is pure.
            return;
        };

        let (used_spec_vars, used_memory) = self.compute_state_usage_for_exp(Some(visited), &body);
        let fun_decl = &mut self.spec_funs[fun_idx];
        fun_decl.body = Some(body);
        fun_decl.used_spec_vars = used_spec_vars;
        fun_decl.used_memory = used_memory;
    }

    /// Computes state usage for an expression. If the visited_opt is available, this recurses
    /// to compute the usage for any functions called. Otherwise it assumes this information is
    /// already computed.
    fn compute_state_usage_for_exp(
        &mut self,
        mut visited_opt: Option<&mut BTreeSet<usize>>,
        exp: &Exp,
    ) -> (
        BTreeSet<QualifiedId<SpecVarId>>,
        BTreeSet<QualifiedId<StructId>>,
    ) {
        let mut used_spec_vars = BTreeSet::new();
        let mut used_memory = BTreeSet::new();
        exp.visit(&mut |e: &Exp| {
            match e {
                Exp::SpecVar(_, mid, vid, _) => {
                    used_spec_vars.insert(mid.qualified(*vid));
                }
                Exp::Call(_, Operation::Function(mid, fid, _), _) => {
                    if mid.to_usize() < self.parent.env.get_module_count() {
                        // This is calling a function from another module we already have
                        // translated.
                        let module_env = self.parent.env.get_module(*mid);
                        let fun_decl = module_env.get_spec_fun(*fid);
                        used_spec_vars.extend(&fun_decl.used_spec_vars);
                        used_memory.extend(&fun_decl.used_memory);
                    } else {
                        // This is calling a function from the module we are currently translating.
                        // Need to recursively ensure we have computed used_spec_vars because of
                        // arbitrary call graphs, including cyclic. If visted_opt is not set,
                        // we know we already computed this.
                        if let Some(visited) = &mut visited_opt {
                            self.compute_state_usage_for_fun(visited, fid.as_usize());
                        }
                        let fun_decl = &self.spec_funs[fid.as_usize()];
                        used_spec_vars.extend(&fun_decl.used_spec_vars);
                        used_memory.extend(&fun_decl.used_memory);
                    }
                }
                Exp::Call(node_id, Operation::Global(_), _)
                | Exp::Call(node_id, Operation::Exists(_), _) => {
                    let env = &self.parent.env;
                    if !env.has_errors() {
                        // We would crash if the type is not valid, so only do this if no errors
                        // have been reported so far.
                        let ty = &env.get_node_instantiation(*node_id)[0];
                        let (mid, sid, _) = ty.require_struct();
                        used_memory.insert(mid.qualified(sid));
                    }
                }
                _ => {}
            }
        });
        (used_spec_vars, used_memory)
    }
}

/// ## Module Invariant Reduction

impl<'env, 'translator> ModuleBuilder<'env, 'translator> {
    /// Reduce module invariants by making them requires/ensures on each function.
    fn reduce_module_invariants(&mut self) {
        for mut cond in self.module_spec.conditions.iter().cloned().collect_vec() {
            if self
                .parent
                .env
                .is_property_true(&cond.properties, CONDITION_GLOBAL_PROP)
                .unwrap_or(false)
            {
                // Global invariant, attach to environment.
                let (spec_var_usage, mem_usage) = self.compute_state_usage_for_exp(None, &cond.exp);
                let id = self.parent.env.new_global_id();
                let Condition { loc, exp, .. } = cond;
                self.parent.env.add_global_invariant(GlobalInvariant {
                    id,
                    loc,
                    kind: cond.kind,
                    spec_var_usage,
                    mem_usage,
                    declaring_module: self.module_id,
                    cond: exp,
                    properties: cond.properties.clone(),
                });
            } else {
                if cond.kind != ConditionKind::Invariant {
                    self.parent.error(
                        &cond.loc,
                        "only `invariant` allowed unless marked as `[global]`",
                    );
                    continue;
                }
                // An Invariant on module level becomes an InvariantModule on function level
                // (which is then further reduced to a pair of RequiresModule and Ensures).
                // Only public functions receive it.
                cond.kind = ConditionKind::InvariantModule;
                for qname in self
                    .parent
                    .fun_table
                    .keys()
                    .filter(|qn| qn.module_name == self.module_name)
                    .cloned()
                    .collect_vec()
                {
                    let entry = self.parent.fun_table.get(&qname).unwrap();
                    if entry.is_public {
                        let context = SpecBlockContext::Function(qname);
                        // Create a property marking this as injected.
                        let context_properties = self.add_bool_property(
                            PropertyBag::default(),
                            CONDITION_INJECTED_PROP,
                            true,
                        );
                        // The below should not generate an error because of the above assert.
                        self.add_conditions_to_context(
                            &context,
                            &cond.loc.clone(),
                            vec![cond.clone()],
                            context_properties,
                            "[internal] (included via module level invariant) not allowed in this context",
                        )
                    }
                }
            }
        }
    }
}

/// # Spec Block Infos

impl<'env, 'translator> ModuleBuilder<'env, 'translator> {
    /// Collect location and target information for all spec blocks. This is used for documentation
    /// generation.
    fn collect_spec_block_infos(&mut self, module_def: &EA::ModuleDefinition) {
        for block in &module_def.specs {
            let block_loc = self.parent.to_loc(&block.loc);
            let member_locs = block
                .value
                .members
                .iter()
                .map(|m| self.parent.to_loc(&m.loc))
                .collect_vec();
            let target = match self.get_spec_block_context(&block.value.target) {
                Some(SpecBlockContext::Module) => SpecBlockTarget::Module,
                Some(SpecBlockContext::Function(qsym)) => {
                    SpecBlockTarget::Function(self.module_id, FunId::new(qsym.symbol))
                }
                Some(SpecBlockContext::FunctionCode(qsym, info)) => SpecBlockTarget::FunctionCode(
                    self.module_id,
                    FunId::new(qsym.symbol),
                    info.offset as usize,
                ),
                Some(SpecBlockContext::Struct(qsym)) => {
                    SpecBlockTarget::Struct(self.module_id, StructId::new(qsym.symbol))
                }
                Some(SpecBlockContext::Schema(qsym)) => {
                    let entry = self
                        .parent
                        .spec_schema_table
                        .get(&qsym)
                        .expect("schema defined");
                    SpecBlockTarget::Schema(
                        self.module_id,
                        SchemaId::new(qsym.symbol),
                        entry
                            .type_params
                            .iter()
                            .map(|(name, _)| {
                                TypeParameter(*name, AbilityConstraint(AbilitySet::EMPTY))
                            })
                            .collect_vec(),
                    )
                }
                None => {
                    // This has been reported as an error. Choose a dummy target.
                    SpecBlockTarget::Module
                }
            };
            self.spec_block_infos.push(SpecBlockInfo {
                loc: block_loc,
                member_locs,
                target,
            })
        }
    }
}

/// # Environment Population

impl<'env, 'translator> ModuleBuilder<'env, 'translator> {
    fn populate_env_from_result(
        &mut self,
        loc: Loc,
        module: CompiledModule,
        source_map: SourceMap<MoveIrLoc>,
    ) {
        let struct_data: BTreeMap<StructId, StructData> = (0..module.struct_defs().len())
            .filter_map(|idx| {
                let def_idx = StructDefinitionIndex(idx as u16);
                let handle_idx = module.struct_def_at(def_idx).struct_handle;
                let handle = module.struct_handle_at(handle_idx);
                let view = StructHandleView::new(&module, handle);
                let name = self.symbol_pool().make(view.name().as_str());
                if let Some(entry) = self
                    .parent
                    .struct_table
                    .get(&self.qualified_by_module(name))
                {
                    let struct_spec = self
                        .struct_specs
                        .remove(&name)
                        .unwrap_or_else(Spec::default);
                    Some((
                        StructId::new(name),
                        self.parent.env.create_struct_data(
                            &module,
                            def_idx,
                            name,
                            entry.loc.clone(),
                            struct_spec,
                        ),
                    ))
                } else {
                    self.parent.error(
                        &self.parent.env.internal_loc(),
                        &format!("[internal] bytecode does not match AST: `{}` in bytecode but not in AST", name.display(self.symbol_pool())));
                    None
                }
            })
            .collect();
        let function_data: BTreeMap<FunId, FunctionData> = (0..module.function_defs().len())
            .filter_map(|idx| {
                let def_idx = FunctionDefinitionIndex(idx as u16);
                let handle_idx = module.function_def_at(def_idx).function;
                let handle = module.function_handle_at(handle_idx);
                let view = FunctionHandleView::new(&module, handle);
                let name_str = view.name().as_str();
                let name = if name_str == SCRIPT_BYTECODE_FUN_NAME {
                    // This is a pseudo script module, which has exactly one function. Determine
                    // the name of this function.
                    self.parent.fun_table.iter().filter_map(|(k, _)| {
                        if k.module_name == self.module_name
                        { Some(k.symbol) } else { None }
                    }).next().expect("unexpected script with multiple or no functions")
                } else {
                    self.symbol_pool().make(name_str)
                };
                let fun_spec = self.fun_specs.remove(&name).unwrap_or_else(Spec::default);
                if let Some(entry) = self.parent.fun_table.get(&self.qualified_by_module(name)) {
                    let arg_names = project_1st(&entry.params);
                    let type_arg_names = project_1st(&entry.type_params);
                    Some((FunId::new(name), self.parent.env.create_function_data(
                        &module,
                        def_idx,
                        name,
                        entry.loc.clone(),
                        arg_names,
                        type_arg_names,
                        fun_spec,
                    )))
                } else {
                    let funs = self.parent.fun_table.iter().map(|(k, _)| {
                        format!("{}", k.display_full(self.symbol_pool()))
                    }).join(", ");
                    self.parent.error(
                        &self.parent.env.internal_loc(),
                        &format!("[internal] bytecode does not match AST: `{}` in bytecode but not in AST (available in AST: {})", name.display(self.symbol_pool()), funs));
                    None
                }
            })
            .collect();
        let named_constants: BTreeMap<NamedConstantId, NamedConstantData> = self
            .parent
            .const_table
            .iter()
            .filter(|(name, _)| name.module_name == self.module_name)
            .map(|(name, const_entry)| {
                let ConstEntry { loc, value, ty } = const_entry.clone();
                (
                    NamedConstantId::new(name.symbol),
                    self.parent
                        .env
                        .create_named_constant_data(name.symbol, loc, ty, value),
                )
            })
            .collect();
        self.parent.env.add(
            loc,
            module,
            source_map,
            named_constants,
            struct_data,
            function_data,
            std::mem::take(&mut self.spec_vars),
            std::mem::take(&mut self.spec_funs),
            std::mem::take(&mut self.module_spec),
            std::mem::take(&mut self.spec_block_infos),
        );
    }
}

/// Extract all accesses of a schema from a schema expression.
pub(crate) fn extract_schema_access<'a>(exp: &'a EA::Exp, res: &mut Vec<&'a EA::ModuleAccess>) {
    match &exp.value {
        EA::Exp_::Name(maccess, _) => res.push(maccess),
        EA::Exp_::Pack(maccess, ..) => res.push(maccess),
        EA::Exp_::BinopExp(_, _, rhs) => extract_schema_access(rhs, res),
        EA::Exp_::IfElse(_, t, e) => {
            extract_schema_access(t, res);
            extract_schema_access(e, res);
        }
        _ => {}
    }
}
