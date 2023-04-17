use std::{
    collections::{BTreeMap, BTreeSet},
    sync::RwLock,
};

use etrace::some_or;
use futures::{future, FutureExt};
use lang_c::{
    ast::Identifier,
    span::{Node, Span},
};

use crate::{
    c_parser::{
        self, CustomType, Function, Program, Struct, TypeDependency, TypeSort, Typedef, Variable,
    },
    compiler::{self, ItemSort, ParsedItem, TypeCheckingResult},
    graph,
    graph::Id,
    openai_client::OpenAIClient,
};

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub try_multiple_signatures: bool,
    pub provide_signatures: bool,
    pub fix_errors: bool,
}

pub struct Translator<'ast> {
    program: &'ast Program,
    typedefs: BTreeMap<&'ast str, Typedef<'ast>>,
    structs: BTreeMap<&'ast str, Struct<'ast>>,
    variables: BTreeMap<&'ast str, Variable<'ast>>,
    functions: BTreeMap<&'ast str, Function<'ast>>,

    custom_types: Vec<CustomType<'ast>>,
    transitive_types: BTreeMap<CustomType<'ast>, BTreeSet<CustomType<'ast>>>,
    type_graph: BTreeMap<Id, BTreeSet<Id>>,
    type_elem_map: BTreeMap<Id, BTreeSet<CustomType<'ast>>>,
    variable_graph: BTreeMap<Id, BTreeSet<Id>>,
    variable_elem_map: BTreeMap<Id, BTreeSet<&'ast str>>,
    function_graph: BTreeMap<Id, BTreeSet<Id>>,
    function_elem_map: BTreeMap<Id, BTreeSet<&'ast str>>,

    client: OpenAIClient,

    new_type_names: BTreeMap<CustomType<'ast>, String>,
    new_term_names: BTreeMap<&'ast str, String>,

    inner: RwLock<TranslatorInner<'ast>>,

    config: Config,
}

#[derive(Default)]
pub struct TranslatorInner<'ast> {
    translated_type_names: BTreeSet<String>,
    translated_term_names: BTreeSet<String>,

    translated_types: BTreeMap<CustomType<'ast>, TranslationResult>,
    translated_variables: BTreeMap<&'ast str, TranslationResult>,
    translated_functions: BTreeMap<&'ast str, TranslationResult>,

    uses: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct TranslationResult {
    items: Vec<ParsedItem>,
    uses: BTreeSet<String>,
    errors: usize,
    copied: bool,
    signature_only: bool,
}

impl TranslationResult {
    #[inline]
    fn mk_code<F>(&self, f: F) -> String
    where F: FnMut(&ParsedItem) -> String {
        self.items.iter().map(f).collect::<Vec<_>>().join("\n")
    }

    fn code(&self) -> String {
        self.mk_code(|i| i.get_code())
    }

    fn simple_code(&self) -> String {
        self.mk_code(|i| i.get_simple_code())
    }

    fn checking_code(&self) -> String {
        self.mk_code(|i| {
            if self.errors == 0 && matches!(i.sort, ItemSort::Variable(_)) {
                i.get_code()
            } else {
                i.get_checking_code()
            }
        })
    }
}

#[derive(Debug, Clone)]
struct FixContext<'a> {
    uses: BTreeSet<String>,
    prefix: &'a str,
    code: String,
    names: &'a BTreeSet<String>,
    result: Option<TypeCheckingResult>,
}

impl<'a> FixContext<'a> {
    fn new(
        uses: BTreeSet<String>,
        prefix: &'a str,
        code: String,
        names: &'a BTreeSet<String>,
    ) -> Self {
        let mut this = Self {
            uses,
            prefix,
            code,
            names,
            result: None,
        };
        this.check();
        this
    }

    fn check(&mut self) {
        self.result = compiler::type_check(&self.code());
        tracing::info!("{:?}", self.result);
        if let Some(res) = &self.result {
            let prefix_lines = self.prefix_lines();
            for error in &res.errors {
                assert!(error.line > prefix_lines, "{}", error.message);
            }
        }
    }

    fn add_uses(&mut self) -> bool {
        fn get_name(s: &str) -> Option<String> {
            let i = s.rfind("::")?;
            Some(s[i + 2..].strip_suffix(';')?.to_string())
        }
        let uses = std::mem::take(&mut self.result.as_mut().unwrap().uses);
        let names: BTreeSet<_> = self.uses.iter().filter_map(|s| get_name(s)).collect();

        let mut updated = false;
        for u in uses {
            if u.ends_with('*') || u.contains('{') || names.contains(&get_name(&u).unwrap()) {
                continue;
            }
            if self.uses.insert(u) {
                updated = true;
            }
        }
        if updated {
            self.check();
        }
        updated
    }

    fn update(&mut self, code: String) {
        self.code = code;
        self.check();
    }

    fn update_whole(&mut self, code: &str) {
        let prefix = self.uses_and_prefix();
        let code = code
            .strip_prefix(&prefix)
            .unwrap_or_else(|| panic!("{}\n{}", prefix, code))
            .strip_prefix('\n')
            .unwrap()
            .to_string();
        self.update(code);
    }

    fn prefix_lines(&self) -> usize {
        self.uses_and_prefix().split('\n').count()
    }

    fn code(&self) -> String {
        format!("{}\n{}", self.uses_and_prefix(), self.code)
    }

    fn uses_and_prefix(&self) -> String {
        format!("{}{}", self.uses_str(), self.prefix)
    }

    fn uses_str(&self) -> String {
        self.uses
            .iter()
            .map(|s| s.as_str())
            .intersperse("\n")
            .collect()
    }
}

static DERIVES: [&str; 9] = [
    "Clone",
    "Copy",
    "Debug",
    "Default",
    "PartialOrd",
    "Ord",
    "PartialEq",
    "Eq",
    "Hash",
];

impl<'ast> Translator<'ast> {
    pub fn new(program: &'ast Program, client: OpenAIClient, config: Config) -> Self {
        let typedefs = program.typedefs();
        let structs = program.structs();
        let variables = program.variables();
        let functions = program.functions();

        let mut cg: BTreeMap<_, BTreeSet<_>> = BTreeMap::new();
        for (name, t) in &typedefs {
            cg.insert(
                CustomType::mk_typedef(name),
                t.dependencies.iter().map(|t| t.typ).collect(),
            );
        }
        for (name, s) in &structs {
            let x = if s.strct {
                CustomType::mk_struct(name)
            } else {
                CustomType::mk_union(name)
            };
            cg.insert(x, s.dependencies.iter().map(|t| t.typ).collect());
        }
        let custom_types = cg.keys().copied().collect();
        let (type_graph, type_elem_map) = graph::compute_sccs(&cg);
        let transitive_types = graph::transitive_closure(cg);

        let cg = variables
            .iter()
            .map(|(name, variable)| {
                (
                    *name,
                    variable
                        .dependencies
                        .iter()
                        .map(|callee| callee.node.name.as_str())
                        .collect(),
                )
            })
            .collect();
        let (variable_graph, variable_elem_map) = graph::compute_sccs(&cg);

        let cg = functions
            .iter()
            .map(|(name, function)| {
                (
                    *name,
                    function
                        .callees
                        .iter()
                        .map(|callee| callee.node.name.as_str())
                        .collect(),
                )
            })
            .collect();
        let (function_graph, function_elem_map) = graph::compute_sccs(&cg);
        let mut inner = TranslatorInner::default();
        inner.uses.insert("extern crate once_cell;".to_string());
        inner.uses.insert("extern crate libc;".to_string());

        Self {
            program,
            typedefs,
            structs,
            variables,
            functions,
            custom_types,
            transitive_types,
            type_graph,
            type_elem_map,
            variable_graph,
            variable_elem_map,
            function_graph,
            function_elem_map,
            client,
            new_type_names: BTreeMap::new(),
            new_term_names: BTreeMap::new(),
            inner: RwLock::new(inner),
            config,
        }
    }

    pub fn signature_only(&self) -> Vec<&str> {
        let inner = self.inner.read().unwrap();
        inner
            .translated_variables
            .iter()
            .chain(&inner.translated_functions)
            .filter_map(|(name, res)| {
                if res.signature_only {
                    Some(*name)
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn errors(&self) -> usize {
        let inner = self.inner.read().unwrap();
        inner
            .translated_types
            .values()
            .chain(inner.translated_variables.values())
            .chain(inner.translated_functions.values())
            .map(|res| res.errors)
            .sum()
    }

    #[inline]
    fn mk_code<F>(&self, f: F) -> String
    where F: FnMut(&TranslationResult) -> String {
        let this = self.inner.read().unwrap();
        let mut v: Vec<_> = this
            .uses
            .iter()
            .cloned()
            .chain(
                this.translated_types
                    .values()
                    .chain(this.translated_variables.values())
                    .chain(this.translated_functions.values())
                    .filter(|r| !r.copied)
                    .map(f),
            )
            .collect();
        v.push("fn main() {}".to_string());
        v.join("\n")
    }

    pub fn code(&self) -> String {
        self.mk_code(|r| r.code())
    }

    fn checking_code(&self) -> String {
        self.mk_code(|r| r.checking_code())
    }

    fn make_replace_vec<'a>(
        &'a self,
        types: Option<&[TypeDependency<'a>]>,
        vars: Option<&[&Node<Identifier>]>,
        callees: Option<&[&Node<Identifier>]>,
    ) -> Vec<(Span, &'a str)> {
        let mut vec = vec![];

        if let Some(deps) = types {
            for d in deps {
                if let Some(x) = self.new_type_names.get(&d.typ) {
                    vec.push((d.span, x.as_str()));
                }
            }
        }

        if let Some(deps) = vars {
            for d in deps {
                if let Some(x) = self.new_term_names.get(d.node.name.as_str()) {
                    vec.push((d.span, x.as_str()));
                }
            }
        }

        if let Some(deps) = callees {
            for d in deps {
                if let Some(x) = self.new_term_names.get(d.node.name.as_str()) {
                    vec.push((d.span, x.as_str()));
                }
            }
        }

        vec
    }

    fn make_translation_prefix(
        &self,
        types: Option<&[TypeDependency<'_>]>,
        vars: Option<&[&Node<Identifier>]>,
        callees: Option<&[&Node<Identifier>]>,
        transitive: bool,
    ) -> Vec<String> {
        let this = self.inner.read().unwrap();

        let mut vec = vec![];

        if let Some(deps) = types {
            let deps: BTreeSet<_> = if transitive {
                deps.iter()
                    .flat_map(|t| self.transitive_types.get(&t.typ).unwrap())
                    .copied()
                    .chain(deps.iter().map(|t| t.typ))
                    .collect()
            } else {
                deps.iter().map(|t| t.typ).collect()
            };
            let types: BTreeSet<_> = deps
                .into_iter()
                .filter_map(|d| Some(this.translated_types.get(&d)?.simple_code()))
                .collect();
            for t in types {
                vec.push(t);
            }
        }

        if let Some(vars) = vars {
            let vars: BTreeSet<_> = vars.iter().map(|x| x.node.name.as_str()).collect();
            for x in vars {
                if let Some(t) = this.translated_variables.get(x) {
                    vec.push(t.simple_code());
                }
            }
        }

        if let Some(callees) = callees {
            let callees: BTreeSet<_> = callees.iter().map(|x| x.node.name.as_str()).collect();
            for x in callees {
                if let Some(t) = this.translated_functions.get(x) {
                    vec.push(t.simple_code());
                }
            }
        }

        vec
    }

    fn dedup_and_check(&self, items: &mut Vec<ParsedItem>, new_name: &str) {
        let this = self.inner.read().unwrap();
        items.retain(|i| {
            if matches!(i.sort, ItemSort::Type(_)) {
                !this.translated_type_names.contains(&i.name)
            } else {
                !this.translated_term_names.contains(&i.name)
            }
        });
        assert!(items.iter().any(|i| i.name == new_name), "{}", new_name);
    }

    fn take_uses(items: &mut Vec<ParsedItem>) -> BTreeSet<String> {
        items
            .drain_filter(|i| matches!(i.sort, ItemSort::Use))
            .filter_map(|i| {
                let res = compiler::type_check(&format!("{}\nfn main() {{}}", i.code));
                if res.map(|r| r.passed()).unwrap_or(false) {
                    Some(i.code.trim().to_string())
                } else {
                    None
                }
            })
            .collect()
    }

    fn fix_by_suggestions(ctxt: &mut FixContext<'_>) {
        while let Some(res) = &ctxt.result {
            if res.suggestions.is_empty() {
                break;
            }
            let code = rustfix::apply_suggestions(&ctxt.code(), &res.suggestions).unwrap();
            ctxt.update_whole(&code);
        }
    }

    fn fix_by_compiler(ctxt: &mut FixContext<'_>) {
        Self::fix_by_suggestions(ctxt);
        while let Some(res) = &ctxt.result {
            if res.uses.is_empty() {
                break;
            }
            if !ctxt.add_uses() {
                break;
            }
            Self::fix_by_suggestions(ctxt);
        }
    }

    async fn fix_by_llm(&self, ctxt: &mut FixContext<'_>) {
        Self::fix_by_compiler(ctxt);
        let mut failed = BTreeSet::new();
        while let Some(res) = &ctxt.result {
            if res.errors.is_empty() {
                break;
            }

            let current_errors = res.errors.len();
            let msgs: BTreeSet<_> = res
                .errors
                .iter()
                .filter_map(|e| {
                    if failed.contains(&e.message) {
                        None
                    } else {
                        Some(e.message.clone())
                    }
                })
                .collect();

            let futures = msgs.clone().into_iter().map(|msg| {
                async {
                    let msg = msg;
                    let fix = self.client.fix(&ctxt.code, &msg).await.ok()?;
                    let mut fixed_items = compiler::parse(&fix)?;
                    fixed_items.retain(|i| ctxt.names.contains(&i.name));
                    if ctxt.names.len() != fixed_items.len() {
                        return None;
                    }
                    let fix = TranslationResult {
                        items: fixed_items,
                        uses: BTreeSet::new(),
                        errors: 0,
                        copied: false,
                        signature_only: false,
                    }
                    .code();
                    if ctxt.code == fix {
                        return None;
                    }
                    let mut new_ctxt = ctxt.clone();
                    new_ctxt.update(fix);
                    Self::fix_by_compiler(&mut new_ctxt);
                    Some(new_ctxt)
                }
                .boxed()
            });
            let results = future::join_all(futures).await;
            let (successes, failures): (Vec<_>, _) = results
                .into_iter()
                .zip(msgs)
                .map(|(new_ctxt, error)| {
                    let new_res = new_ctxt
                        .as_ref()
                        .and_then(|new_ctxt| new_ctxt.result.as_ref());
                    let new_errors = new_res
                        .map(|new_res| new_res.errors.len())
                        .unwrap_or(current_errors);
                    (new_ctxt, new_errors, error)
                })
                .partition(|(_, new_errors, _)| *new_errors < current_errors);

            for (_, _, msg) in failures {
                failed.insert(msg);
            }

            if let Some((new_ctxt, _, _)) = successes
                .into_iter()
                .min_by_key(|(_, new_errors, _)| *new_errors)
            {
                *ctxt = new_ctxt.unwrap();
            } else {
                break;
            }
        }
    }

    pub async fn translate_names(&mut self) {
        let type_names = future::join_all(
            self.custom_types
                .iter()
                .map(|ty| self.client.rename_type(ty.name)),
        )
        .await;

        for (ty, new_name) in self.custom_types.iter().zip(type_names) {
            let new_name = if new_name == "Option" {
                format!("My{}", new_name)
            } else {
                new_name
            };
            self.new_type_names.insert(*ty, new_name);
        }

        let var_names = future::join_all(
            self.variables
                .keys()
                .map(|var| self.client.rename_variable(var)),
        )
        .await;
        for (var, new_name) in self.variables.keys().zip(var_names) {
            self.new_term_names.insert(*var, new_name);
        }

        let func_names = future::join_all(
            self.functions
                .keys()
                .map(|func| self.client.rename_function(func)),
        )
        .await;
        for (func, new_name) in self.functions.keys().zip(func_names) {
            let new_name = if new_name == "main" {
                format!("my_{}", new_name)
            } else {
                new_name
            };
            self.new_term_names.insert(*func, new_name);
        }
    }

    async fn translate_typedef(&self, typedef: &Typedef<'_>, new_name: &str) -> TranslationResult {
        let deps = &typedef.dependencies;

        if typedef.is_struct_alias {
            let this = self.inner.read().unwrap();
            let aliased = this.translated_types.get(&deps[0].typ).unwrap().clone();
            return TranslationResult {
                copied: true,
                ..aliased
            };
        }

        let vec = self.make_replace_vec(Some(deps), None, None);
        let (code, sort) = match self
            .program
            .typedef_to_struct_string(typedef, vec, new_name)
        {
            Ok((s, sort)) => (s, sort),
            Err(mut vec) => {
                vec.push((typedef.identifier.span, new_name));
                (self.program.typedef_to_string(typedef, vec), "type")
            }
        };
        tracing::info!("translate_typedef code ({})\n{}", new_name, code);

        let prefix = self.make_translation_prefix(Some(deps), None, None, false);
        tracing::info!(
            "translate_typedef prefix ({})\n{}",
            new_name,
            prefix.join("\n")
        );

        let translated = self.client.translate_type(&code, sort, &prefix).await;
        tracing::info!(
            "translate_typedef translated ({})\n{}",
            new_name,
            translated
        );

        let items = compiler::parse(&translated).unwrap();
        TranslationResult {
            items,
            uses: BTreeSet::new(),
            errors: 0,
            copied: false,
            signature_only: false,
        }
    }

    async fn translate_struct(&self, strct: &Struct<'_>, new_name: &str) -> TranslationResult {
        let deps = &strct.dependencies;
        let mut vec = self.make_replace_vec(Some(deps), None, None);
        vec.push((
            strct.struct_type.node.identifier.as_ref().unwrap().span,
            new_name,
        ));
        let code = self.program.struct_to_string(strct, vec);
        tracing::info!("translate_struct code ({})\n{}", new_name, code);

        let prefix = self.make_translation_prefix(Some(deps), None, None, false);
        tracing::info!(
            "translate_struct prefix ({})\n{}",
            new_name,
            prefix.join("\n")
        );

        let sort = if strct.strct { "struct" } else { "union" };
        let translated = self.client.translate_type(&code, sort, &prefix).await;
        tracing::info!("translate_struct translated ({})\n{}", new_name, translated);

        let items = compiler::parse(&translated).unwrap();
        TranslationResult {
            items,
            uses: BTreeSet::new(),
            errors: 0,
            copied: false,
            signature_only: false,
        }
    }

    async fn translate_type(&self, ty: &CustomType<'_>) -> TranslationResult {
        let new_name = self.new_type_names.get(ty).unwrap();
        tracing::info!("translate_type: {}", new_name);

        let mut translated = if matches!(ty.sort, TypeSort::Typedef) {
            let typedef = self.typedefs.get(ty.name).unwrap();
            self.translate_typedef(typedef, new_name).await
        } else {
            let strct = self.structs.get(ty.name).unwrap();
            self.translate_struct(strct, new_name).await
        };

        if translated.copied {
            return translated;
        }

        self.dedup_and_check(&mut translated.items, new_name);
        Self::take_uses(&mut translated.items);

        let checking_prefix = self.checking_code();
        tracing::info!("translate_type prefix ({})\n{}", new_name, checking_prefix);
        tracing::info!("translate_type code ({})\n{}", new_name, translated.code());

        let item_names: BTreeSet<_> = translated.items.iter().map(|i| i.name.clone()).collect();
        let translated_code = translated.code();
        let mut ctxt = FixContext::new(
            translated.uses,
            &checking_prefix,
            translated_code.clone(),
            &item_names,
        );
        self.fix_by_llm(&mut ctxt).await;
        assert!(ctxt.result.as_ref().unwrap().passed());
        translated.uses = ctxt.uses;
        translated.errors = ctxt.result.as_ref().unwrap().errors.len();
        if translated_code != ctxt.code {
            tracing::info!(
                "translate_type diff ({})\n{}",
                new_name,
                difference(&translated_code, &ctxt.code)
            );

            let fixed_items = compiler::parse(&ctxt.code).unwrap();
            let fixed_item_names: BTreeSet<_> =
                fixed_items.iter().map(|i| i.name.clone()).collect();
            assert_eq!(item_names, fixed_item_names);
            translated.items = fixed_items;
        }

        for item in &mut translated.items {
            if let ItemSort::Type(t) = &mut item.sort {
                let ds = match t.sort {
                    TypeSort::Typedef => &DERIVES[..0],
                    TypeSort::Union => &DERIVES[..2],
                    _ => &DERIVES[..],
                };
                for d in ds {
                    t.derives.insert(d.to_string());
                }
            }
        }
        Self::remove_wrong_derives(&mut translated, &checking_prefix);
        tracing::info!("translate_type code ({})\n{}", new_name, translated.code());
        println!("type: {} ({})", new_name, translated.errors);

        translated
    }

    fn remove_wrong_derives(translated: &mut TranslationResult, checking_prefix: &str) {
        loop {
            let mut errors =
                compiler::check_derive(&format!("{}\n{}", checking_prefix, translated.code()));
            if errors.is_empty() {
                break;
            }
            for item in &mut translated.items {
                if let ItemSort::Type(t) = &mut item.sort {
                    if let Some(ds) = errors.remove(&item.name) {
                        t.derives.retain(|d| !ds.contains(d));
                    }
                }
            }
            assert!(errors.is_empty());
        }
    }

    pub async fn translate_types(&mut self) {
        let mut graph = self.type_graph.clone();
        let mut futures = vec![];

        loop {
            let mut new_futures: Vec<_> = graph
                .drain_filter(|_, s| s.is_empty())
                .map(|(id, _)| self.type_elem_map.get(&id).unwrap())
                .map(|set| {
                    async {
                        assert_eq!(set.len(), 1);
                        let typ = set.first().unwrap();
                        let translated = self.translate_type(typ).await;
                        (typ, translated)
                    }
                    .boxed()
                })
                .collect();
            futures.append(&mut new_futures);

            if futures.is_empty() {
                break;
            }

            let ((typ, translated), _, remaining) = future::select_all(futures).await;
            futures = remaining;

            let id = self
                .type_elem_map
                .iter()
                .find_map(|(id, set)| if set.contains(typ) { Some(id) } else { None })
                .unwrap();
            for ids in graph.values_mut() {
                ids.remove(id);
            }

            let mut this = self.inner.write().unwrap();
            for i in &translated.items {
                let name = i.name.clone();
                if matches!(i.sort, ItemSort::Type(_)) {
                    this.translated_type_names.insert(name);
                } else {
                    this.translated_term_names.insert(name);
                }
            }
            for u in &translated.uses {
                this.uses.insert(u.trim().to_string());
            }
            this.translated_types.insert(*typ, translated);
        }
    }

    async fn translate_variable(&self, name: &str) -> TranslationResult {
        let var = self.variables.get(name).unwrap();
        let new_name = self.new_term_names.get(name).unwrap();
        tracing::info!("translate_variable: {}", new_name);

        let tdeps = &var.type_dependencies;
        let deps = &var.dependencies;
        let mut vec = self.make_replace_vec(Some(tdeps), Some(deps), None);
        vec.push((var.identifier.span, new_name));
        let code = self.program.variable_to_string(var, vec.clone(), false);
        tracing::info!("translate_variable code ({})\n{}", new_name, code);

        let prefix = self.make_translation_prefix(Some(tdeps), Some(deps), None, true);
        tracing::info!(
            "translate_variable prefix ({})\n{}",
            new_name,
            prefix.join("\n")
        );

        let (translated, signature_only) =
            match self.client.translate_variable(&code, &prefix).await {
                Ok(translated) => (translated, false),
                Err(_) => {
                    let code = self.program.variable_to_string(var, vec, true);
                    (
                        self.client
                            .translate_variable(&code, &prefix)
                            .await
                            .unwrap(),
                        true,
                    )
                }
            };
        tracing::info!(
            "translate_variable translated ({})\n{}",
            new_name,
            translated
        );

        let mut items = compiler::parse(&translated).unwrap();
        self.dedup_and_check(&mut items, new_name);
        Self::take_uses(&mut items);
        let item_names: BTreeSet<_> = items.iter().map(|i| i.name.clone()).collect();
        let mut translated = TranslationResult {
            items,
            uses: BTreeSet::new(),
            errors: 0,
            copied: false,
            signature_only,
        };
        tracing::info!(
            "translate_variable translated ({})\n{}",
            new_name,
            translated.code()
        );

        let checking_prefix = self.checking_code();
        tracing::info!(
            "translate_variable checking_prefix ({})\n{}",
            new_name,
            checking_prefix
        );

        let translated_code = translated.code();
        let mut ctxt = FixContext::new(
            translated.uses.clone(),
            &checking_prefix,
            translated_code.clone(),
            &item_names,
        );
        if self.config.fix_errors {
            self.fix_by_llm(&mut ctxt).await;
            if translated_code != ctxt.code {
                let fixed_items = compiler::parse(&ctxt.code).unwrap();
                let fixed_item_names: BTreeSet<_> =
                    fixed_items.iter().map(|i| i.name.clone()).collect();
                assert_eq!(item_names, fixed_item_names);

                tracing::info!(
                    "translate_variable diff ({})\n{}",
                    new_name,
                    difference(&translated_code, &ctxt.code)
                );
                translated.items = fixed_items;
            }
        }
        translated.uses = ctxt.uses;
        translated.errors = ctxt.result.as_ref().unwrap().errors.len();

        for e in &ctxt.result.unwrap().errors {
            tracing::info!("translate_variable error ({})\n{}", new_name, e.message);
        }
        println!("variable: {} ({})", new_name, translated.errors);

        translated
    }

    pub async fn translate_variables(&mut self) {
        let mut graph = self.variable_graph.clone();
        let mut futures = vec![];

        loop {
            let mut new_futures: Vec<_> = graph
                .drain_filter(|_, s| s.is_empty())
                .map(|(id, _)| self.variable_elem_map.get(&id).unwrap())
                .map(|set| {
                    async {
                        assert_eq!(set.len(), 1);
                        let var = *set.first().unwrap();
                        let translated = self.translate_variable(var).await;
                        (var, translated)
                    }
                    .boxed()
                })
                .collect();
            futures.append(&mut new_futures);

            if futures.is_empty() {
                break;
            }

            let ((var, translated), _, remaining) = future::select_all(futures).await;
            futures = remaining;

            let id = self
                .variable_elem_map
                .iter()
                .find_map(|(id, set)| if set.contains(var) { Some(id) } else { None })
                .unwrap();
            for ids in graph.values_mut() {
                ids.remove(id);
            }

            let mut this = self.inner.write().unwrap();
            for i in &translated.items {
                let name = i.name.clone();
                if matches!(i.sort, ItemSort::Type(_)) {
                    this.translated_type_names.insert(name);
                } else {
                    this.translated_term_names.insert(name);
                }
            }
            for u in &translated.uses {
                this.uses.insert(u.trim().to_string());
            }
            this.translated_variables.insert(var, translated);
        }
    }

    async fn translate_function(&self, name: &str) -> TranslationResult {
        let func = self.functions.get(name).unwrap();
        let new_name = self.new_term_names.get(name).unwrap();
        tracing::info!("translate_function: {}", new_name);

        let tdeps = &func.type_dependencies;
        let deps = &func.dependencies;
        let callees = &func.callees;
        let mut vec = self.make_replace_vec(Some(tdeps), Some(deps), Some(callees));
        let in_spans = c_parser::find_names(func.definition, "in");
        for span in in_spans {
            vec.push((span, "in_data"));
        }
        vec.push((func.identifier.span, new_name));
        let code = self.program.function_to_string(func, vec.clone());
        tracing::info!("translate_function code ({})\n{}", new_name, code);

        let prefix = self.make_translation_prefix(Some(tdeps), Some(deps), Some(callees), true);
        tracing::info!(
            "translate_function prefix ({})\n{}",
            new_name,
            prefix.join("\n")
        );

        let sigs = self
            .client
            .translate_signature(&code, new_name, &prefix, 3)
            .await;
        tracing::info!(
            "translate_function sigs ({})\n{}",
            new_name,
            sigs.join("\n")
        );
        let mut sig_map = BTreeMap::new();
        for sig in sigs {
            let s = sig.replace("->", "");
            if s.chars().filter(|c| *c == '<').count() != s.chars().filter(|c| *c == '>').count() {
                continue;
            }
            let mut parsed_items = some_or!(compiler::parse(&format!("{}{{}}", sig)), continue);
            assert_eq!(parsed_items.len(), 1);
            let item = parsed_items.pop().unwrap();
            assert_eq!(&item.name, new_name);
            if let ItemSort::Function(f) = item.sort {
                sig_map
                    .entry(f.normalized_signature_ty)
                    .or_insert(f.normalized_signature);
            } else {
                panic!()
            };
        }
        let param_len = func.params;
        if sig_map.keys().any(|sig| sig.params.len() <= param_len) {
            sig_map.retain(|sig, _| sig.params.len() <= param_len);
        }
        tracing::info!(
            "translate_function sigs ({})\n{}",
            new_name,
            sig_map
                .values()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        );

        let checking_prefix = self.checking_code();
        tracing::info!(
            "translate_function checking_prefix ({})\n{}",
            new_name,
            checking_prefix
        );

        let candidates = future::join_all(
            sig_map
                .values()
                .map(|sig| self.try_signature(sig, new_name, &code, &prefix, &checking_prefix)),
        )
        .await;
        let mut candidates = candidates.into_iter().flatten().collect::<Vec<_>>();
        if candidates.is_empty() {
            let code = self.program.function_to_signature_string(func, vec.clone());
            let new_candidates = future::join_all(
                sig_map
                    .values()
                    .map(|sig| self.try_signature(sig, new_name, &code, &[], &checking_prefix)),
            )
            .await;
            candidates = new_candidates.into_iter().flatten().collect::<Vec<_>>();
            for c in &mut candidates {
                c.signature_only = true;
            }
        }

        let min_errors = candidates.iter().map(|c| c.errors).min().expect(new_name);
        candidates.retain(|c| c.errors == min_errors);
        for (i, c) in candidates.iter().enumerate() {
            tracing::info!(
                "translate_function candidate {} ({})\n{}",
                i + 1,
                new_name,
                c.code()
            );
        }
        candidates.reverse();
        let mut best = candidates.pop().unwrap();
        while let Some(cand) = candidates.pop() {
            if self.client.compare(&best.code(), &cand.code()).await == std::cmp::Ordering::Less {
                best = cand;
            }
        }
        tracing::info!("translate_function ({})\n{}", new_name, best.code());
        println!("function: {} ({})", new_name, best.errors);
        best
    }

    async fn try_signature(
        &self,
        sig: &str,
        new_name: &str,
        code: &str,
        prefix: &[String],
        checking_prefix: &str,
    ) -> Option<TranslationResult> {
        let translated = self
            .client
            .translate_function(code, sig, prefix)
            .await
            .ok()?;

        let mut items = compiler::parse(&translated)?;
        self.dedup_and_check(&mut items, new_name);
        Self::take_uses(&mut items);
        let item_names: BTreeSet<_> = items.iter().map(|i| i.name.clone()).collect();
        let mut translated = TranslationResult {
            items,
            uses: BTreeSet::new(),
            errors: 0,
            copied: false,
            signature_only: false,
        };
        tracing::info!("translate_function translated\n{}", translated.code());

        let translated_code = translated.code();
        let mut ctxt = FixContext::new(
            translated.uses.clone(),
            checking_prefix,
            translated_code.clone(),
            &item_names,
        );
        if self.config.fix_errors {
            self.fix_by_llm(&mut ctxt).await;
            if translated_code != ctxt.code {
                let fixed_items = compiler::parse(&ctxt.code).unwrap();
                let fixed_item_names: BTreeSet<_> =
                    fixed_items.iter().map(|i| i.name.clone()).collect();
                assert_eq!(item_names, fixed_item_names);

                tracing::info!(
                    "try_signature diff\n{}",
                    difference(&translated_code, &ctxt.code)
                );
                translated.items = fixed_items;
            }
        }
        let res = ctxt.result?;
        translated.errors = res.errors.len();
        translated.uses = ctxt.uses;

        tracing::info!(
            "try_signature translated ({})\n{}\n{}",
            new_name,
            sig,
            translated.code()
        );
        for (i, e) in res.errors.iter().enumerate() {
            tracing::info!(
                "try_signature error {} ({})\n{}\n{}",
                i + 1,
                new_name,
                sig,
                e.message
            );
        }
        Some(translated)
    }

    pub async fn translate_functions(&mut self) {
        let mut graph = self.function_graph.clone();
        let mut futures = vec![];

        loop {
            let mut new_futures: Vec<_> = graph
                .drain_filter(|_, s| s.is_empty())
                .map(|(id, _)| self.function_elem_map.get(&id).unwrap())
                .map(|set| {
                    async {
                        assert_eq!(set.len(), 1);
                        let func = *set.first().unwrap();
                        let translated = self.translate_function(func).await;
                        (func, translated)
                    }
                    .boxed()
                })
                .collect();
            futures.append(&mut new_futures);

            if futures.is_empty() {
                break;
            }

            let ((func, translated), _, remaining) = future::select_all(futures).await;
            futures = remaining;

            let id = self
                .function_elem_map
                .iter()
                .find_map(|(id, set)| if set.contains(func) { Some(id) } else { None })
                .unwrap();
            for ids in graph.values_mut() {
                ids.remove(id);
            }

            let mut this = self.inner.write().unwrap();
            for i in &translated.items {
                let name = i.name.clone();
                if matches!(i.sort, ItemSort::Type(_)) {
                    this.translated_type_names.insert(name);
                } else {
                    this.translated_term_names.insert(name);
                }
            }
            for u in &translated.uses {
                this.uses.insert(u.trim().to_string());
            }
            this.translated_functions.insert(func, translated);
        }
    }
}

fn difference(s1: &str, s2: &str) -> String {
    let mut result = String::new();
    for (i, diff) in diff::lines(s1, s2).iter().enumerate() {
        if i != 0 {
            result.push('\n');
        }
        let line = match diff {
            diff::Result::Left(l) => format!("-{}", l),
            diff::Result::Both(l, _) => format!(" {}", l),
            diff::Result::Right(r) => format!("+{}", r),
        };
        result.push_str(&line);
    }
    result
}
