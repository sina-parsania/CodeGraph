//! Grammar-driven parser: one generic tree-sitter walk parameterized by a
//! per-language `LangSpec` (label map, call-node kinds, callee fields, name
//! extraction mode). Adding a language = one grammar dep + one `LangSpec`.

use codegraph_core::{
    InheritKind, Metadata, Node, NodeLabel, QualifiedName, RawCall, RawField, RawImport, RawInherit, RawLocal, Receiver,
};
use tree_sitter::{Language, Node as TsNode, Parser};

/// Bump whenever parse OUTPUT changes shape (new node kinds, name filters,
/// import handling). A stamped-version mismatch forces a full reparse so one
/// graph never mixes two parser behaviors (incremental == full would break).
pub const PARSER_VERSION: u32 = 3;

pub struct ParsedFile {
    pub nodes: Vec<Node>,
    pub calls: Vec<RawCall>,
    pub inherits: Vec<RawInherit>,
    pub fields: Vec<RawField>,
    pub locals: Vec<RawLocal>,
    pub imports: Vec<RawImport>,
}

impl ParsedFile {
    fn empty() -> Self {
        ParsedFile {
            nodes: Vec::new(),
            calls: Vec::new(),
            inherits: Vec::new(),
            fields: Vec::new(),
            locals: Vec::new(),
            imports: Vec::new(),
        }
    }
}

#[derive(Clone, Copy)]
enum NameMode {
    Field,
    CDeclarator,
}

type InheritFn = fn(TsNode, &[u8]) -> Vec<RawInherit>;
type ExtraDefFn = fn(TsNode, &[u8]) -> Option<String>;

struct LangSpec {
    name: &'static str,
    language: fn() -> Language,
    label_for: fn(&str) -> Option<NodeLabel>,
    call_kinds: &'static [&'static str],
    callee_fields: &'static [&'static str],
    name_mode: NameMode,
    inherit_fn: Option<InheritFn>,
    /// Definitions `label_for` can't see because the name lives on a sibling:
    /// e.g. TS `const Foo = () => {…}` (arrow/function-expression components).
    /// Returns the bound name; the node is emitted as a Function.
    extra_def: Option<ExtraDefFn>,
}

/// Byte ranges (+1-based line) of every identifier-kind token whose text == `name`,
/// in source order. The occurrence set a `rename-symbol` must fully account for
/// before it may rewrite — strings/comments are excluded by construction
/// (tree-sitter classifies them as non-identifier nodes).
pub fn identifier_spans(rel_path: &str, source: &str, name: &str) -> Vec<(usize, usize, u32)> {
    let ext = rel_path.rsplit('.').next().unwrap_or("");
    let Some(spec) = spec_for_ext(ext) else { return Vec::new() };
    let mut parser = Parser::new();
    if parser.set_language(&(spec.language)()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else { return Vec::new() };
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        let k = n.kind();
        if (k == "identifier" || k == "simple_identifier" || k.ends_with("_identifier"))
            && std::str::from_utf8(&bytes[n.byte_range()]).ok() == Some(name)
        {
            out.push((n.start_byte(), n.end_byte(), n.start_position().row as u32 + 1));
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    out.sort_unstable();
    out
}

pub fn parse_file(project: &str, rel_path: &str, source: &str) -> ParsedFile {
    let ext = rel_path.rsplit('.').next().unwrap_or("");
    match spec_for_ext(ext) {
        Some(spec) => parse_with(spec, project, rel_path, source),
        None => ParsedFile::empty(),
    }
}

fn spec_for_ext(ext: &str) -> Option<&'static LangSpec> {
    let s = match ext {
        "rs" => &RUST,
        "py" | "pyi" => &PYTHON,
        "js" | "jsx" | "mjs" | "cjs" => &JS,
        "ts" | "mts" | "cts" => &TS,
        "tsx" => &TSX,
        "go" => &GO,
        "kt" | "kts" => &KOTLIN,
        "swift" => &SWIFT,
        "java" => &JAVA,
        "c" | "h" => &C,
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => &CPP,
        "rb" => &RUBY,
        "cs" => &CSHARP,
        "sh" | "bash" => &BASH,
        _ => return None,
    };
    Some(s)
}

// Back-compat single-language entry points (used by tests).
pub fn parse_rust(p: &str, r: &str, s: &str) -> ParsedFile { parse_with(&RUST, p, r, s) }
pub fn parse_python(p: &str, r: &str, s: &str) -> ParsedFile { parse_with(&PYTHON, p, r, s) }
pub fn parse_js(p: &str, r: &str, s: &str) -> ParsedFile { parse_with(&JS, p, r, s) }
pub fn parse_ts(p: &str, r: &str, s: &str) -> ParsedFile { parse_with(&TS, p, r, s) }
pub fn parse_tsx(p: &str, r: &str, s: &str) -> ParsedFile { parse_with(&TSX, p, r, s) }
pub fn parse_go(p: &str, r: &str, s: &str) -> ParsedFile { parse_with(&GO, p, r, s) }
pub fn parse_swift(p: &str, r: &str, s: &str) -> ParsedFile { parse_with(&SWIFT, p, r, s) }
pub fn parse_java(p: &str, r: &str, s: &str) -> ParsedFile { parse_with(&JAVA, p, r, s) }
pub fn parse_kotlin(p: &str, r: &str, s: &str) -> ParsedFile { parse_with(&KOTLIN, p, r, s) }

static RUST: LangSpec = LangSpec { name: "rust", language: || tree_sitter_rust::LANGUAGE.into(), label_for: rust_label, call_kinds: &["call_expression"], callee_fields: &["function"], name_mode: NameMode::Field, inherit_fn: Some(rust_inherits), extra_def: None, };
static PYTHON: LangSpec = LangSpec { name: "python", language: || tree_sitter_python::LANGUAGE.into(), label_for: python_label, call_kinds: &["call"], callee_fields: &["function"], name_mode: NameMode::Field, inherit_fn: Some(py_inherits), extra_def: None, };
static JS: LangSpec = LangSpec { name: "javascript", language: || tree_sitter_javascript::LANGUAGE.into(), label_for: js_label, call_kinds: &["call_expression"], callee_fields: &["function"], name_mode: NameMode::Field, inherit_fn: None, extra_def: Some(js_extra_def), };
static TS: LangSpec = LangSpec { name: "typescript", language: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), label_for: ts_label, call_kinds: &["call_expression"], callee_fields: &["function"], name_mode: NameMode::Field, inherit_fn: Some(ts_inherits), extra_def: Some(js_extra_def), };
static TSX: LangSpec = LangSpec { name: "typescript", language: || tree_sitter_typescript::LANGUAGE_TSX.into(), label_for: ts_label, call_kinds: &["call_expression"], callee_fields: &["function"], name_mode: NameMode::Field, inherit_fn: Some(ts_inherits), extra_def: Some(js_extra_def), };
static GO: LangSpec = LangSpec { name: "go", language: || tree_sitter_go::LANGUAGE.into(), label_for: go_label, call_kinds: &["call_expression"], callee_fields: &["function"], name_mode: NameMode::Field, inherit_fn: None, extra_def: None, };
static SWIFT: LangSpec = LangSpec { name: "swift", language: || tree_sitter_swift::LANGUAGE.into(), label_for: swift_label, call_kinds: &["call_expression"], callee_fields: &[], name_mode: NameMode::Field, inherit_fn: Some(swift_inherits), extra_def: None, };
static JAVA: LangSpec = LangSpec { name: "java", language: || tree_sitter_java::LANGUAGE.into(), label_for: java_label, call_kinds: &["method_invocation"], callee_fields: &["name"], name_mode: NameMode::Field, inherit_fn: Some(java_inherits), extra_def: None, };
static C: LangSpec = LangSpec { name: "c", language: || tree_sitter_c::LANGUAGE.into(), label_for: c_label, call_kinds: &["call_expression"], callee_fields: &["function"], name_mode: NameMode::CDeclarator, inherit_fn: None, extra_def: None, };
static CPP: LangSpec = LangSpec { name: "cpp", language: || tree_sitter_cpp::LANGUAGE.into(), label_for: cpp_label, call_kinds: &["call_expression"], callee_fields: &["function"], name_mode: NameMode::CDeclarator, inherit_fn: None, extra_def: None, };
static RUBY: LangSpec = LangSpec { name: "ruby", language: || tree_sitter_ruby::LANGUAGE.into(), label_for: ruby_label, call_kinds: &["call"], callee_fields: &["method"], name_mode: NameMode::Field, inherit_fn: None, extra_def: None, };
static CSHARP: LangSpec = LangSpec { name: "csharp", language: || tree_sitter_c_sharp::LANGUAGE.into(), label_for: csharp_label, call_kinds: &["invocation_expression"], callee_fields: &["function"], name_mode: NameMode::Field, inherit_fn: None, extra_def: None, };
static KOTLIN: LangSpec = LangSpec { name: "kotlin", language: || tree_sitter_kotlin_ng::LANGUAGE.into(), label_for: kotlin_label, call_kinds: &["call_expression"], callee_fields: &[], name_mode: NameMode::Field, inherit_fn: Some(kotlin_inherits), extra_def: None, };
static BASH: LangSpec = LangSpec { name: "bash", language: || tree_sitter_bash::LANGUAGE.into(), label_for: bash_label, call_kinds: &[], callee_fields: &[], name_mode: NameMode::Field, inherit_fn: None, extra_def: None, };

fn rust_label(k: &str) -> Option<NodeLabel> {
    match k { "function_item" => Some(NodeLabel::Function), "struct_item" | "union_item" => Some(NodeLabel::Class), "enum_item" => Some(NodeLabel::Enum), "trait_item" => Some(NodeLabel::Interface), "type_item" => Some(NodeLabel::Type), "mod_item" => Some(NodeLabel::Module), _ => None }
}
fn python_label(k: &str) -> Option<NodeLabel> {
    match k { "function_definition" => Some(NodeLabel::Function), "class_definition" => Some(NodeLabel::Class), _ => None }
}
fn js_label(k: &str) -> Option<NodeLabel> {
    match k { "function_declaration" | "generator_function_declaration" => Some(NodeLabel::Function), "class_declaration" => Some(NodeLabel::Class), "method_definition" => Some(NodeLabel::Method), _ => None }
}
fn ts_label(k: &str) -> Option<NodeLabel> {
    match k { "interface_declaration" => Some(NodeLabel::Interface), "type_alias_declaration" => Some(NodeLabel::Type), "enum_declaration" => Some(NodeLabel::Enum), "abstract_class_declaration" => Some(NodeLabel::Class), other => js_label(other) }
}
/// `const Foo = () => {…}` / `const f = function () {…}` — the dominant way React
/// components and module-level helpers are written. The declarator binds the
/// name; the fn literal is the definition. Only fires when the initializer IS
/// the function (a `useMemo(...)` call is not a definition).
fn js_extra_def(node: TsNode, src: &[u8]) -> Option<String> {
    if node.kind() != "variable_declarator" {
        return None;
    }
    let value = node.child_by_field_name("value")?;
    if !matches!(value.kind(), "arrow_function" | "function_expression" | "function") {
        return None;
    }
    let name = node.child_by_field_name("name").filter(|n| n.kind() == "identifier")?;
    std::str::from_utf8(&src[name.byte_range()]).ok().map(str::to_string)
}
fn go_label(k: &str) -> Option<NodeLabel> {
    match k { "function_declaration" => Some(NodeLabel::Function), "method_declaration" => Some(NodeLabel::Method), "type_spec" => Some(NodeLabel::Class), _ => None }
}
fn swift_label(k: &str) -> Option<NodeLabel> {
    // tree-sitter-swift parses class/struct/enum/actor all as `class_declaration`.
    match k { "function_declaration" => Some(NodeLabel::Function), "class_declaration" => Some(NodeLabel::Class), "protocol_declaration" => Some(NodeLabel::Interface), "typealias_declaration" => Some(NodeLabel::Type), _ => None }
}
fn java_label(k: &str) -> Option<NodeLabel> {
    match k { "method_declaration" | "constructor_declaration" => Some(NodeLabel::Method), "class_declaration" | "record_declaration" => Some(NodeLabel::Class), "interface_declaration" | "annotation_type_declaration" => Some(NodeLabel::Interface), "enum_declaration" => Some(NodeLabel::Enum), _ => None }
}
fn c_label(k: &str) -> Option<NodeLabel> {
    match k { "function_definition" => Some(NodeLabel::Function), "struct_specifier" | "union_specifier" => Some(NodeLabel::Class), "enum_specifier" => Some(NodeLabel::Enum), "type_definition" => Some(NodeLabel::Type), _ => None }
}
fn cpp_label(k: &str) -> Option<NodeLabel> {
    match k { "class_specifier" => Some(NodeLabel::Class), "namespace_definition" => Some(NodeLabel::Module), other => c_label(other) }
}
fn ruby_label(k: &str) -> Option<NodeLabel> {
    match k { "method" | "singleton_method" => Some(NodeLabel::Method), "class" => Some(NodeLabel::Class), "module" => Some(NodeLabel::Module), _ => None }
}
fn csharp_label(k: &str) -> Option<NodeLabel> {
    match k { "method_declaration" | "constructor_declaration" => Some(NodeLabel::Method), "class_declaration" | "record_declaration" | "struct_declaration" => Some(NodeLabel::Class), "interface_declaration" => Some(NodeLabel::Interface), "enum_declaration" => Some(NodeLabel::Enum), _ => None }
}
fn kotlin_label(k: &str) -> Option<NodeLabel> {
    // tree-sitter-kotlin parses class/interface/object/enum as class_declaration/object_declaration.
    match k {
        "function_declaration" => Some(NodeLabel::Function),
        "class_declaration" | "object_declaration" => Some(NodeLabel::Class),
        _ => None,
    }
}
fn bash_label(k: &str) -> Option<NodeLabel> {
    match k { "function_definition" => Some(NodeLabel::Function), _ => None }
}

fn rust_inherits(node: TsNode, src: &[u8]) -> Vec<RawInherit> {
    if node.kind() != "impl_item" {
        return Vec::new();
    }
    let Some(typ) = node.child_by_field_name("type").and_then(|t| trailing_ident(t, src)) else {
        return Vec::new();
    };
    if let Some(tr) = node.child_by_field_name("trait").and_then(|t| trailing_ident(t, src)) {
        return vec![RawInherit { impl_name: typ, super_name: tr, kind: InheritKind::Implements }];
    }
    Vec::new()
}

fn py_inherits(node: TsNode, src: &[u8]) -> Vec<RawInherit> {
    if node.kind() != "class_definition" {
        return Vec::new();
    }
    let Some(cls) = field_text(node, "name", src) else { return Vec::new() };
    let Some(args) = node.child_by_field_name("superclasses") else { return Vec::new() };
    let mut out = Vec::new();
    let mut c = args.walk();
    for ch in args.named_children(&mut c) {
        if let Some(sup) = trailing_ident(ch, src) {
            out.push(RawInherit { impl_name: cls.clone(), super_name: sup, kind: InheritKind::Extends });
        }
    }
    out
}

fn ts_inherits(node: TsNode, src: &[u8]) -> Vec<RawInherit> {
    if node.kind() != "class_declaration" {
        return Vec::new();
    }
    let Some(cls) = field_text(node, "name", src) else { return Vec::new() };
    let mut out = Vec::new();
    let mut c = node.walk();
    for ch in node.children(&mut c) {
        if ch.kind() != "class_heritage" {
            continue;
        }
        let mut h = ch.walk();
        for clause in ch.children(&mut h) {
            let kind = match clause.kind() {
                "extends_clause" => InheritKind::Extends,
                "implements_clause" => InheritKind::Implements,
                _ => continue,
            };
            for sup in type_idents(clause, src) {
                out.push(RawInherit { impl_name: cls.clone(), super_name: sup, kind });
            }
        }
    }
    out
}

fn java_inherits(node: TsNode, src: &[u8]) -> Vec<RawInherit> {
    let Some(cls) = field_text(node, "name", src) else { return Vec::new() };
    let mut out = Vec::new();
    match node.kind() {
        "class_declaration" => {
            if let Some(sc) = node.child_by_field_name("superclass") {
                for s in type_idents(sc, src) {
                    out.push(RawInherit { impl_name: cls.clone(), super_name: s, kind: InheritKind::Extends });
                }
            }
            if let Some(ifs) = node.child_by_field_name("interfaces") {
                for s in type_idents(ifs, src) {
                    out.push(RawInherit { impl_name: cls.clone(), super_name: s, kind: InheritKind::Implements });
                }
            }
        }
        "interface_declaration" => {
            if let Some(ifs) = node.child_by_field_name("interfaces") {
                for s in type_idents(ifs, src) {
                    out.push(RawInherit { impl_name: cls.clone(), super_name: s, kind: InheritKind::Extends });
                }
            }
        }
        _ => {}
    }
    out
}

fn type_idents(node: TsNode, src: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if matches!(n.kind(), "type_identifier" | "identifier") {
            if let Ok(s) = std::str::from_utf8(&src[n.byte_range()]) {
                out.push(s.to_string());
            }
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    out
}

fn parse_with(spec: &LangSpec, project: &str, rel_path: &str, source: &str) -> ParsedFile {
    let mut parser = Parser::new();
    if parser.set_language(&(spec.language)()).is_err() {
        return ParsedFile::empty();
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return ParsedFile::empty(),
    };
    let bytes = source.as_bytes();
    let mut dir: Vec<&str> = rel_path.split('/').filter(|s| !s.is_empty()).collect();
    let filename = dir.pop().unwrap_or("file");
    let mut file_segs = dir.clone();
    file_segs.push(filename);
    let file_id = QualifiedName::build(project, &dir, filename);

    let mut nodes = vec![Node {
        id: file_id.clone(),
        label: NodeLabel::File,
        name: filename.to_string(),
        file_path: rel_path.to_string(),
        line_start: 1,
        line_end: source.lines().count().max(1) as u32,
        language: spec.name.to_string(),
        metadata: Metadata::new(),
        community: None,
        pagerank: 0.0,
        betweenness: 0.0,
    }];
    let mut calls = Vec::new();
    let mut inherits = Vec::new();
    let mut fields = Vec::new();
    let mut locals = Vec::new();
    let mut imports = Vec::new();
    let ctx = Ctx { spec, project, segs: &file_segs, rel_path, file_id: &file_id };
    collect(tree.root_node(), bytes, &ctx, None, None, &mut nodes, &mut calls, &mut inherits, &mut fields, &mut locals, &mut imports);
    ParsedFile { nodes, calls, inherits, fields, locals, imports }
}

struct Ctx<'a> {
    spec: &'a LangSpec,
    project: &'a str,
    segs: &'a [&'a str],
    rel_path: &'a str,
    file_id: &'a str,
}

#[allow(clippy::too_many_arguments)]
fn collect(node: TsNode, src: &[u8], ctx: &Ctx, current_fn: Option<&str>, this_class: Option<&str>, nodes: &mut Vec<Node>, calls: &mut Vec<RawCall>, inherits: &mut Vec<RawInherit>, fields: &mut Vec<RawField>, locals: &mut Vec<RawLocal>, imports: &mut Vec<RawImport>) {
    let mut my_fn_id: Option<String> = None;
    let mut my_class_id: Option<String> = None;

    let labeled: Option<(NodeLabel, String)> = match (ctx.spec.label_for)(node.kind()) {
        Some(label) => name_of(node, src, ctx.spec.name_mode).map(|n| (label, n)),
        // `const Foo = () => {…}` — the name lives on the declarator, not the fn.
        None => ctx.spec.extra_def.and_then(|f| f(node, src)).map(|n| (NodeLabel::Function, n)),
    };
    {
        if let Some((label, name)) = labeled {
            // A digits-only-ish definition name ($200-style object keys, numeric
            // literals-as-methods) is generated noise, never a real call target —
            // it would only pollute search/ranking/communities. Skip the node.
            // Operator definitions (`==`, `+`) have no letters but ARE real —
            // the gate requires a digit with no letter to reject.
            if name.chars().any(char::is_alphabetic)
                || !name.chars().any(|c| c.is_ascii_digit()) {
                // B1: methods nest under their enclosing class so two same-named
                // methods in two classes of ONE file get DISTINCT ids
                // (`project.dir.file.class.method`) — identical ids previously
                // merged them and misattributed members in CHA lookups.
                let id = match (label, this_class) {
                    (NodeLabel::Function | NodeLabel::Method, Some(cls)) => {
                        format!("{}.{}", cls, QualifiedName::build("", &[], &name))
                    }
                    _ => QualifiedName::build(ctx.project, ctx.segs, &name),
                };
                match label {
                    NodeLabel::Function | NodeLabel::Method => my_fn_id = Some(id.clone()),
                    NodeLabel::Class | NodeLabel::Interface | NodeLabel::Enum => my_class_id = Some(id.clone()),
                    _ => {}
                }
                let mut metadata = Metadata::new();
                if matches!(label, NodeLabel::Function | NodeLabel::Method) {
                    metadata.insert("complexity".into(), serde_json::json!(cyclomatic(node, ctx.spec.label_for)));
                }
                nodes.push(Node {
                    id,
                    label,
                    name,
                    file_path: ctx.rel_path.to_string(),
                    line_start: node.start_position().row as u32 + 1,
                    line_end: node.end_position().row as u32 + 1,
                    language: ctx.spec.name.to_string(),
                    metadata,
                    community: None,
                    pagerank: 0.0,
                    betweenness: 0.0,
                });
            }
        }
    }

    // At a class, capture its typed fields for T3 (this.field.method()) resolution.
    if let Some(cls_id) = my_class_id.as_deref() {
        match ctx.spec.name {
            "typescript" => ts_extract_fields(node, src, cls_id, fields),
            "swift" => swift_extract_fields(node, src, cls_id, fields),
            "kotlin" => kotlin_extract_fields(node, src, cls_id, fields),
            "java" => java_extract_fields(node, src, cls_id, fields),
            _ => {}
        }
    }

    // Import bindings (T6 evidence): TS/JS relative imports + Python from-imports.
    match (ctx.spec.name, node.kind()) {
        ("typescript" | "javascript", "import_statement") => ts_extract_import(node, src, ctx.rel_path, imports),
        ("python", "import_from_statement") => py_extract_import(node, src, ctx.rel_path, imports),
        _ => {}
    }

    // At a function/method, infer the static type of its declared-type locals
    // and typed params for T5 (`x.method()` via the variable's type), and
    // record parameter names as SHADOW bindings (all languages) so a call
    // through a parameter never binds a same-named free function elsewhere.
    if let Some(fn_id) = my_fn_id.as_deref() {
        collect_param_shadows(node, src, fn_id, locals);
        match ctx.spec.name {
            "typescript" => ts_infer_locals(node, src, fn_id, locals),
            "kotlin" => kotlin_infer_locals(node, src, fn_id, locals),
            _ => {}
        }
    }

    if ctx.spec.call_kinds.contains(&node.kind()) && !is_subscript(node, src) {
        if let Some(callee) = callee_name(node, src, ctx.spec.callee_fields) {
            if is_http_method(&callee) {
                if let Some(path) = first_string_arg(node, src) {
                    if path.starts_with('/') && path.len() <= 200 {
                        let method = callee.to_ascii_uppercase();
                        let line = node.start_position().row as u32 + 1;
                        let mut md = Metadata::new();
                        md.insert("path".to_string(), serde_json::Value::String(path.clone()));
                        md.insert("method".to_string(), serde_json::Value::String(method.clone()));
                        if let Some(h) = current_fn {
                            md.insert("handler".to_string(), serde_json::Value::String(h.to_string()));
                        }
                        nodes.push(Node {
                            id: format!("route.{}.{}", normalize_path(&path), method),
                            label: NodeLabel::Route,
                            name: format!("{} {}", method, path),
                            file_path: ctx.rel_path.to_string(),
                            line_start: line,
                            line_end: line,
                            language: ctx.spec.name.to_string(),
                            metadata: md,
                            community: None,
                            pagerank: 0.0,
                            betweenness: 0.0,
                        });
                        // Link the enclosing fn to the route HUB node. Route ids are
                        // path+method keyed (project-agnostic), so a backend handler
                        // registration and a frontend client call collapse onto the
                        // SAME node — cross-project connectivity via exact-path match.
                        calls.push(RawCall {
                            caller_id: current_fn.unwrap_or(ctx.file_id).to_string(),
                            callee_name: format!("route.{}.{}", normalize_path(&path), method),
                            line,
                            receiver: Receiver::Bare,
                            enclosing_class: None,
                        });
                    }
                }
            }
            // Receiver-aware capture (all languages): self/this binds to the
            // enclosing class so the resolver can do Class-Hierarchy-Analysis.
            let receiver = detect_receiver(node, src);
            let enclosing_class = match &receiver {
                Receiver::SelfThis | Receiver::Super | Receiver::Field(_) => this_class.map(str::to_string),
                // In implicit-member languages a bare `field.method()` is an
                // implicit `this.field.method()`, so carry the class to let the
                // resolver try its fields (a local var shadows it — resolver order).
                Receiver::Named(_) if has_implicit_member(ctx.spec.name) => this_class.map(str::to_string),
                _ => None,
            };
            calls.push(RawCall {
                caller_id: current_fn.unwrap_or(ctx.file_id).to_string(),
                callee_name: callee,
                line: node.start_position().row as u32 + 1,
                receiver,
                enclosing_class,
            });
        }
    }

    if let Some(inf) = ctx.spec.inherit_fn {
        inherits.extend(inf(node, src));
    }

    let next_fn = my_fn_id.as_deref().or(current_fn);
    // `this` binds to the nearest enclosing class, EXCEPT inside a non-arrow
    // function literal which rebinds it (arrow fns / methods preserve it).
    let next_this_class = match my_class_id.as_deref() {
        Some(c) => Some(c),
        None if rebinds_this(ctx.spec.name, node.kind()) => None,
        None => this_class,
    };
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, src, ctx, next_fn, next_this_class, nodes, calls, inherits, fields, locals, imports);
    }
}

fn node_str<'a>(n: TsNode, src: &'a [u8]) -> Option<&'a str> {
    std::str::from_utf8(&src[n.byte_range()]).ok()
}

/// `self`/`this` → SelfThis; `self.field`/`this.field` (one identifier) → Field;
/// `super` → Super; anything else → Bare. The receiver text is validated, so a
/// complex receiver (`(a).x`, `arr[0]`, generics) never becomes a guess.
fn classify_receiver(recv: &str) -> Receiver {
    let recv = recv.trim();
    if recv.is_empty() {
        return Receiver::Bare;
    }
    if recv == "self" || recv == "this" {
        return Receiver::SelfThis;
    }
    if recv == "super" {
        return Receiver::Super;
    }
    for kw in ["self.", "this."] {
        if let Some(field) = recv.strip_prefix(kw) {
            if !field.is_empty() && field.chars().all(|c| c.is_alphanumeric() || c == '_') {
                return Receiver::Field(field.to_string());
            }
        }
    }
    // A qualified call whose receiver type we can't pin statically. Carry the
    // receiver text so the resolver knows it's qualified (→ globally-unique only,
    // never a same-file guess) instead of treating it like an unqualified call.
    Receiver::Named(recv.to_string())
}

/// Language-agnostic receiver detection from the callee expression text. Works
/// for `obj.method()` shapes across all grammars: Java's explicit `object` field,
/// the `function` field (TS/JS/Python/Rust/Go/C#), or the first non-argument child
/// (Swift). The class `self`/`this` is bound to is supplied separately (enclosing_class).
/// tree-sitter-swift models `arr[i]` subscripts as a `call_expression` whose
/// argument-suffix uses `[...]` instead of `(...)`. Those are NOT method calls —
/// detect them by the bracket so we never emit a phantom edge for a subscript.
fn is_subscript(call: TsNode, src: &[u8]) -> bool {
    for i in 0..call.child_count() as u32 {
        let Some(ch) = call.child(i) else { continue };
        if matches!(ch.kind(), "call_suffix" | "value_arguments" | "arguments" | "argument_list") {
            return node_str(ch, src).map(|t| t.trim_start().starts_with('[')).unwrap_or(false);
        }
    }
    false
}

fn first_callee(call: TsNode<'_>) -> Option<TsNode<'_>> {
    if let Some(f) = call.child_by_field_name("function") {
        return Some(f);
    }
    // Some grammars (Swift) expose the callee as an unnamed child, so scan ALL
    // children and skip the argument / call-suffix kinds rather than `named_child`.
    for i in 0..call.child_count() as u32 {
        if let Some(ch) = call.child(i) {
            if !matches!(
                ch.kind(),
                "arguments" | "argument_list" | "value_arguments" | "type_arguments" | "call_suffix"
            ) {
                return Some(ch);
            }
        }
    }
    None
}

fn detect_receiver(call: TsNode, src: &[u8]) -> Receiver {
    // Java-style: the receiver is an explicit `object` field (no trailing name).
    if let Some(obj) = call.child_by_field_name("object") {
        return classify_receiver(node_str(obj, src).unwrap_or(""));
    }
    match first_callee(call).and_then(|c| node_str(c, src)).and_then(|t| t.rsplit_once('.')) {
        Some((recv, _name)) => classify_receiver(recv),
        None => Receiver::Bare,
    }
}

/// Extract a TS class's typed fields → `RawField`s: `public foo: T` and constructor
/// parameter-properties `constructor(private foo: T)`. Only simple `type_identifier`
/// types (no generics/unions) so T3 resolution stays precise. Skips nested classes.
/// Base type name of a TS type node — the resolver only needs the class whose
/// members a receiver can have: `Foo` → Foo · `Foo<T>` → Foo (members live on
/// the generic base) · `Foo | null` / `Foo | undefined` → Foo (the only value
/// with members). Anything else (real unions, tuples, functions) → None.
fn ts_base_type(ty: TsNode, src: &[u8]) -> Option<String> {
    let text = |n: TsNode| std::str::from_utf8(&src[n.byte_range()]).ok().map(str::to_string);
    match ty.kind() {
        "type_identifier" => text(ty),
        "generic_type" => {
            let mut c = ty.walk();
            let base = ty.named_children(&mut c).find(|n| n.kind() == "type_identifier")?;
            text(base)
        }
        "union_type" => {
            let mut c = ty.walk();
            let real: Vec<TsNode> = ty
                .named_children(&mut c)
                .filter(|m| !matches!(text(*m).as_deref(), Some("null") | Some("undefined")))
                .collect();
            if real.len() == 1 {
                ts_base_type(real[0], src)
            } else {
                None // a real union has no single member set — drop
            }
        }
        _ => None,
    }
}

/// The declared type of a parameter/declarator via its `type_annotation`.
fn ts_annotation_type(n: TsNode, src: &[u8]) -> Option<String> {
    let mut c = n.walk();
    let ann = n.named_children(&mut c).find(|ch| ch.kind() == "type_annotation")?;
    let mut c2 = ann.walk();
    let ty = ann.named_children(&mut c2).next()?;
    ts_base_type(ty, src)
}

fn ts_extract_fields(class: TsNode, src: &[u8], class_id: &str, out: &mut Vec<RawField>) {
    let text = |n: TsNode| std::str::from_utf8(&src[n.byte_range()]).ok().map(str::to_string);
    let type_in = |n: TsNode| ts_annotation_type(n, src);
    let mut stack = vec![class];
    while let Some(n) = stack.pop() {
        // a nested class owns its own fields — don't attribute them here
        if n.id() != class.id() && matches!(n.kind(), "class_declaration" | "abstract_class_declaration") {
            continue;
        }
        match n.kind() {
            "public_field_definition" => {
                if let (Some(name), Some(ty)) = (n.child_by_field_name("name").and_then(text), type_in(n)) {
                    out.push(RawField { class_id: class_id.into(), field_name: name, type_name: ty });
                }
            }
            "required_parameter" | "optional_parameter" => {
                let mut c = n.walk();
                let is_property = n.children(&mut c).any(|ch| ch.kind() == "accessibility_modifier");
                let name = n.child_by_field_name("pattern").filter(|p| p.kind() == "identifier").and_then(text);
                if let (true, Some(name), Some(ty)) = (is_property, name, type_in(n)) {
                    out.push(RawField { class_id: class_id.into(), field_name: name, type_name: ty });
                }
            }
            _ => {}
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
}

/// Infer the static type of declared-type locals + typed (non-property) params
/// within a TS function body (T5). Flow-insensitive single-assignment: a name
/// declared with two different types in the same body is DROPPED, never guessed.
/// Stops at nested function boundaries (their locals are their own scope) and
/// only accepts simple `type_identifier` types — matching the field extractor.
fn ts_infer_locals(func: TsNode, src: &[u8], fn_id: &str, out: &mut Vec<RawLocal>) {
    let text = |n: TsNode| std::str::from_utf8(&src[n.byte_range()]).ok().map(str::to_string);
    // Declared annotation first; else `const x = new Foo()` — the constructor
    // IS the type (still evidence, not a guess).
    let type_in = |n: TsNode| -> Option<String> {
        if let Some(t) = ts_annotation_type(n, src) {
            return Some(t);
        }
        if n.kind() != "variable_declarator" {
            return None;
        }
        let value = n.child_by_field_name("value")?;
        if value.kind() != "new_expression" {
            return None;
        }
        value.child_by_field_name("constructor").filter(|c| c.kind() == "identifier").and_then(text)
    };
    // BTreeMap → deterministic emit order. `None` = poisoned (conflicting types).
    let mut found: std::collections::BTreeMap<String, Option<String>> = std::collections::BTreeMap::new();
    const FN_KINDS: [&str; 5] = [
        "function_declaration",
        "function_expression",
        "arrow_function",
        "method_definition",
        "generator_function_declaration",
    ];
    // For `const Foo = () => {…}` defs the walk starts at the DECLARATOR; its
    // value literal is Foo's own body, not a nested function — don't skip it.
    let own_body = if func.kind() == "variable_declarator" {
        func.child_by_field_name("value").map(|v| v.id())
    } else {
        None
    };
    let mut stack = vec![func];
    while let Some(n) = stack.pop() {
        if n.id() != func.id() && Some(n.id()) != own_body && FN_KINDS.contains(&n.kind()) {
            continue; // nested function: its own scope
        }
        let name_ty = match n.kind() {
            "variable_declarator" | "required_parameter" | "optional_parameter" => n
                .child_by_field_name(if n.kind() == "variable_declarator" { "name" } else { "pattern" })
                .filter(|p| p.kind() == "identifier")
                .and_then(text)
                .zip(type_in(n)),
            _ => None,
        };
        if let Some((name, ty)) = name_ty {
            found.entry(name).and_modify(|e| {
                if e.as_deref() != Some(ty.as_str()) {
                    *e = None; // conflicting declared types in one scope → drop
                }
            }).or_insert(Some(ty));
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    for (name, ty) in found {
        if let Some(t) = ty {
            out.push(RawLocal { caller_id: fn_id.into(), var_name: name, type_name: t });
        }
    }
}

/// Parameter names are LOCAL BINDINGS: a call through one (`call_next(...)`,
/// `cb()`) must never resolve to a same-named free function elsewhere —
/// audit-caught on a real repo. Emitted as type-less RawLocal rows: shadow
/// evidence only (an empty type can't type a member call, but it proves the
/// name is locally bound). Over-collection (e.g. Swift argument labels) only
/// SUPPRESSES resolution — precision-safe by construction.
fn collect_param_shadows(func: TsNode, src: &[u8], fn_id: &str, out: &mut Vec<RawLocal>) {
    let text = |n: TsNode| std::str::from_utf8(&src[n.byte_range()]).ok().map(str::to_string);
    let is_binding_ident = |k: &str| k == "identifier" || k == "simple_identifier";
    // arrow-component defs start at the declarator; the fn literal holds the params
    let root = if func.kind() == "variable_declarator" {
        func.child_by_field_name("value").unwrap_or(func)
    } else {
        func
    };
    let mut c = root.walk();
    for container in root.children(&mut c) {
        // parameters / formal_parameters / function_value_parameters / parameter_list …
        if !container.kind().contains("parameter") {
            continue;
        }
        let mut pc = container.walk();
        for param in container.named_children(&mut pc) {
            if is_binding_ident(param.kind()) {
                if let Some(name) = text(param) {
                    out.push(RawLocal { caller_id: fn_id.into(), var_name: name, type_name: String::new() });
                }
                continue;
            }
            let mut ic = param.walk();
            for ch in param.named_children(&mut ic) {
                if is_binding_ident(ch.kind()) {
                    if let Some(name) = text(ch) {
                        out.push(RawLocal { caller_id: fn_id.into(), var_name: name, type_name: String::new() });
                    }
                }
            }
        }
    }
}

/// Cyclomatic complexity (lite): 1 + decision points in the function's subtree.
/// Grammar-agnostic by node-kind name — an approximation, consistent across the
/// 13 languages, good for ranking risk (not for exact McCabe compliance).
/// Nested NAMED functions are skipped — they get their own node + count, and
/// counting them here double-bills every ancestor (O(depth × subtree) walks).
fn cyclomatic(func: TsNode, label_for: fn(&str) -> Option<NodeLabel>) -> u32 {
    let mut n = 1u32;
    let mut stack = vec![func];
    while let Some(node) = stack.pop() {
        if node.id() != func.id()
            && matches!(label_for(node.kind()), Some(NodeLabel::Function | NodeLabel::Method))
        {
            continue;
        }
        let k = node.kind();
        if k.contains("if_") || k.starts_with("if") && k.ends_with("statement")
            || k.contains("for_") || k.contains("while")
            || k.contains("case") || k.contains("catch")
            || k.contains("guard_statement") || k.contains("conditional_expression")
            || k.contains("ternary") || k == "match_arm" || k.contains("when_entry")
            || k == "&&" || k == "||" || k == "elif_clause" || k.contains("except_clause")
        {
            n += 1;
        }
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            stack.push(ch);
        }
    }
    n
}

/// First named child of `n` with the given kind (indexing borrows the tree, not
/// a cursor, so the result can be reassigned/returned).
fn named_child_of<'t>(n: TsNode<'t>, kind: &str) -> Option<TsNode<'t>> {
    (0..n.named_child_count() as u32).filter_map(|i| n.named_child(i)).find(|c| c.kind() == kind)
}

/// Kotlin `user_type` (possibly wrapped in `nullable_type`) → its base identifier.
fn kotlin_user_type(n: TsNode, src: &[u8]) -> Option<String> {
    let user = named_child_of(n, "user_type")
        .or_else(|| named_child_of(n, "nullable_type").and_then(|nt| named_child_of(nt, "user_type")))?;
    std::str::from_utf8(&src[named_child_of(user, "identifier")?.byte_range()]).ok().map(str::to_string)
}

/// Capture Kotlin stored-property types for T3 (`val x: T`). Unwraps `T?`
/// nullable; takes a generic's base type; only `user_type` (no function/array).
/// Also captures PRIMARY-CONSTRUCTOR properties `class Foo(val bar: Bar)` — the
/// dominant Kotlin DI pattern — but only `val`/`var` parameters (a plain
/// parameter is not a field).
fn kotlin_extract_fields(class: TsNode, src: &[u8], class_id: &str, out: &mut Vec<RawField>) {
    let text = |n: TsNode| std::str::from_utf8(&src[n.byte_range()]).ok().map(str::to_string);
    let mut stack = vec![class];
    while let Some(n) = stack.pop() {
        if n.id() != class.id() && matches!(n.kind(), "class_declaration" | "object_declaration") {
            continue;
        }
        match n.kind() {
            "property_declaration" => {
                if let Some(vd) = named_child_of(n, "variable_declaration") {
                    // tree-sitter-kotlin-ng names both the var and the type `identifier`.
                    let name = named_child_of(vd, "identifier").and_then(text);
                    if let (Some(name), Some(ty)) = (name, kotlin_user_type(vd, src)) {
                        out.push(RawField { class_id: class_id.into(), field_name: name, type_name: ty });
                    }
                }
            }
            "class_parameter" => {
                let mut c = n.walk();
                let is_property = n.children(&mut c).any(|ch| matches!(ch.kind(), "val" | "var"));
                let name = named_child_of(n, "identifier").and_then(text);
                if let (true, Some(name), Some(ty)) = (is_property, name, kotlin_user_type(n, src)) {
                    out.push(RawField { class_id: class_id.into(), field_name: name, type_name: ty });
                }
            }
            _ => {}
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
}

/// Kotlin locals for T5: `val x: Type = …` (declared annotation — fully safe)
/// and `val x = Foo(...)` where the initializer is a bare CAPITALIZED call —
/// a constructor invocation by Kotlin naming convention. The resolver still
/// gates on unique-class + member-exists, so a misread factory drops out.
fn kotlin_infer_locals(func: TsNode, src: &[u8], fn_id: &str, out: &mut Vec<RawLocal>) {
    let text = |n: TsNode| std::str::from_utf8(&src[n.byte_range()]).ok().map(str::to_string);
    let mut found: std::collections::BTreeMap<String, Option<String>> = std::collections::BTreeMap::new();
    let mut stack = vec![func];
    while let Some(n) = stack.pop() {
        // Lambdas are NOT a boundary: calls inside a lambda attribute to the
        // enclosing function (there is no lambda node), so its locals must be
        // visible here too — otherwise Android's lambda-heavy code loses T5
        // entirely. Shadowing conflicts are handled by the poisoning map.
        if n.id() != func.id() && matches!(n.kind(), "function_declaration" | "class_declaration") {
            continue; // nested named scope owns its locals
        }
        if n.kind() == "property_declaration" {
            let name = named_child_of(n, "variable_declaration").and_then(|vd| named_child_of(vd, "identifier")).and_then(text);
            let ty = named_child_of(n, "variable_declaration")
                .and_then(|vd| kotlin_user_type(vd, src))
                .or_else(|| {
                    let call = named_child_of(n, "call_expression")?;
                    let callee = call.named_child(0).filter(|c| c.kind() == "identifier").and_then(text)?;
                    callee.chars().next().filter(|c| c.is_uppercase()).map(|_| callee)
                });
            if let (Some(name), Some(ty)) = (name, ty) {
                found
                    .entry(name)
                    .and_modify(|e| {
                        if e.as_deref() != Some(ty.as_str()) {
                            *e = None; // conflicting types in one scope → drop
                        }
                    })
                    .or_insert(Some(ty));
            }
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    for (name, ty) in found {
        if let Some(t) = ty {
            out.push(RawLocal { caller_id: fn_id.into(), var_name: name, type_name: t });
        }
    }
}

/// Kotlin supertypes: `class A(…) : Base(), Iface` — a `constructor_invocation`
/// IS the superclass (Kotlin requires the ctor call for classes); a bare
/// `user_type` is an interface. Both feed CHA; the graph builder corrects the
/// edge kind against the resolved target's label anyway.
fn kotlin_inherits(node: TsNode, src: &[u8]) -> Vec<RawInherit> {
    if node.kind() != "class_declaration" && node.kind() != "object_declaration" {
        return Vec::new();
    }
    let Some(cls) = field_text(node, "name", src) else { return Vec::new() };
    let Some(specs) = named_child_of(node, "delegation_specifiers") else { return Vec::new() };
    let text = |n: TsNode| std::str::from_utf8(&src[n.byte_range()]).ok().map(str::to_string);
    let mut out = Vec::new();
    for i in 0..specs.named_child_count() as u32 {
        let Some(spec) = specs.named_child(i) else { continue };
        let (user, kind) = match named_child_of(spec, "constructor_invocation") {
            Some(ci) => (named_child_of(ci, "user_type"), InheritKind::Extends),
            None => (named_child_of(spec, "user_type"), InheritKind::Implements),
        };
        if let Some(sup) = user.and_then(|u| named_child_of(u, "identifier")).and_then(text) {
            out.push(RawInherit { impl_name: cls.clone(), super_name: sup, kind });
        }
    }
    out
}

/// Swift supertypes: `class A: UIViewController, Trackable` — superclass and
/// protocols are syntactically identical, so emit Extends and let the graph
/// builder flip to Implements when the target resolves to a protocol node.
fn swift_inherits(node: TsNode, src: &[u8]) -> Vec<RawInherit> {
    if node.kind() != "class_declaration" {
        return Vec::new();
    }
    let Some(cls) = field_text(node, "name", src) else { return Vec::new() };
    let text = |n: TsNode| std::str::from_utf8(&src[n.byte_range()]).ok().map(str::to_string);
    let mut out = Vec::new();
    for i in 0..node.named_child_count() as u32 {
        let Some(ch) = node.named_child(i) else { continue };
        if ch.kind() != "inheritance_specifier" {
            continue;
        }
        let sup = named_child_of(ch, "user_type")
            .and_then(|u| named_child_of(u, "type_identifier"))
            .and_then(text);
        if let Some(sup) = sup {
            out.push(RawInherit { impl_name: cls.clone(), super_name: sup, kind: InheritKind::Extends });
        }
    }
    out
}

/// Capture Java field types for T3 (`T field;`). Simple `type_identifier` types
/// only (generics/arrays rejected for precision); handles `T a, b;` declarators.
fn java_extract_fields(class: TsNode, src: &[u8], class_id: &str, out: &mut Vec<RawField>) {
    let text = |n: TsNode| std::str::from_utf8(&src[n.byte_range()]).ok().map(str::to_string);
    let mut stack = vec![class];
    while let Some(n) = stack.pop() {
        if n.id() != class.id()
            && matches!(n.kind(), "class_declaration" | "interface_declaration" | "enum_declaration")
        {
            continue;
        }
        if n.kind() == "field_declaration" {
            if let Some(ty) = n.child_by_field_name("type").filter(|t| t.kind() == "type_identifier").and_then(text) {
                let mut c = n.walk();
                for ch in n.children(&mut c) {
                    if ch.kind() == "variable_declarator" {
                        if let Some(name) = ch.child_by_field_name("name").and_then(text) {
                            out.push(RawField { class_id: class_id.into(), field_name: name, type_name: ty.clone() });
                        }
                    }
                }
            }
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
}

/// Capture Swift stored-property types for T3 (`self.field.method()`). Accepts a
/// plain `user_type` (the base of a generic resolves to that base), unwraps one
/// `T?` optional layer, and REJECTS arrays/dictionaries/tuples/closures so a
/// `[Foo]` field never resolves a call to `Foo` — precision over recall.
fn swift_extract_fields(class: TsNode, src: &[u8], class_id: &str, out: &mut Vec<RawField>) {
    let text = |n: TsNode| std::str::from_utf8(&src[n.byte_range()]).ok().map(str::to_string);
    let type_in = |n: TsNode| -> Option<String> {
        let ann = named_child_of(n, "type_annotation")?;
        let mut t = ann.named_child(0)?;
        if t.kind() == "optional_type" {
            t = t.named_child(0)?; // T? -> T
        }
        if t.kind() != "user_type" {
            return None; // arrays / dictionaries / tuples / closures -> drop
        }
        text(named_child_of(t, "type_identifier")?)
    };
    let mut stack = vec![class];
    while let Some(n) = stack.pop() {
        // a nested type owns its own properties — don't attribute them here
        if n.id() != class.id() && n.kind() == "class_declaration" {
            continue;
        }
        if n.kind() == "property_declaration" {
            let name = n
                .child_by_field_name("name")
                .and_then(|pat| named_child_of(pat, "simple_identifier").or(Some(pat)))
                .and_then(text);
            if let (Some(name), Some(ty)) = (name, type_in(n)) {
                out.push(RawField { class_id: class_id.into(), field_name: name, type_name: ty });
            }
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
}

/// In TS/JS a nested non-arrow function literal rebinds `this`. In Swift/Kotlin/
/// Python/etc. the "function" kind IS the method (self stays the instance), so we
/// only treat these kinds as rebinding for TS/JS.
/// Languages where a bare `field.method()` is an implicit `this.field.method()`
/// (member access without an explicit receiver). TS/JS/Python/Rust/Go require an
/// explicit `this`/`self`, so a bare name there is never an implicit field.
fn has_implicit_member(spec: &str) -> bool {
    matches!(spec, "swift" | "kotlin" | "java" | "csharp" | "cpp" | "ruby")
}

fn rebinds_this(spec: &str, kind: &str) -> bool {
    matches!(spec, "typescript" | "javascript")
        && matches!(
            kind,
            "function_declaration" | "function_expression" | "generator_function" | "generator_function_declaration"
        )
}

fn name_of(node: TsNode, src: &[u8], mode: NameMode) -> Option<String> {
    match mode {
        NameMode::Field => field_text(node, "name", src),
        NameMode::CDeclarator => {
            if let Some(s) = field_text(node, "name", src) {
                return Some(s);
            }
            // C/C++ function: descend the declarator chain to the identifier.
            let mut d = node.child_by_field_name("declarator")?;
            loop {
                match d.kind() {
                    "identifier" | "field_identifier" | "type_identifier" | "operator_name" => {
                        return std::str::from_utf8(&src[d.byte_range()]).ok().map(|s| s.to_string());
                    }
                    _ => d = d.child_by_field_name("declarator")?,
                }
            }
        }
    }
}

fn field_text(node: TsNode, field: &str, src: &[u8]) -> Option<String> {
    std::str::from_utf8(&src[node.child_by_field_name(field)?.byte_range()]).ok().map(|s| s.to_string())
}

fn is_http_method(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "get" | "post" | "put" | "delete" | "patch" | "all" | "head" | "options" | "route"
            | "getmapping" | "postmapping" | "putmapping" | "deletemapping" | "patchmapping"
            | "requestmapping"
    )
}

fn normalize_path(p: &str) -> String {
    p.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect()
}

fn first_string_arg(call: TsNode, src: &[u8]) -> Option<String> {
    let args = call.child_by_field_name("arguments")?;
    let mut c = args.walk();
    for a in args.named_children(&mut c) {
        if a.kind().contains("string") {
            let t = std::str::from_utf8(&src[a.byte_range()]).ok()?;
            return Some(t.trim_matches(|ch| ch == '"' || ch == '\'' || ch == '`').to_string());
        }
    }
    None
}

fn callee_name(call: TsNode, src: &[u8], fields: &[&str]) -> Option<String> {
    for f in fields {
        if let Some(c) = call.child_by_field_name(f) {
            if let Some(s) = trailing_ident(c, src) {
                return Some(s);
            }
        }
    }
    let mut cursor = call.walk();
    for child in call.named_children(&mut cursor) {
        if let Some(s) = trailing_ident(child, src) {
            return Some(s);
        }
    }
    // Fallback for grammars where the callee is an unnamed child (Swift's
    // navigation_expression) — otherwise `self.method()` calls are dropped.
    first_callee(call).and_then(|c| trailing_ident(c, src))
}

fn trailing_ident(node: TsNode, src: &[u8]) -> Option<String> {
    let k = node.kind();
    if k == "identifier" || k == "simple_identifier" || k.ends_with("_identifier") {
        return std::str::from_utf8(&src[node.byte_range()]).ok().map(|s| s.to_string());
    }
    for field in ["name", "field", "attribute", "property", "method", "suffix"] {
        if let Some(c) = node.child_by_field_name(field) {
            if let Some(s) = trailing_ident(c, src) {
                return Some(s);
            }
        }
    }
    // Fallback: descend ONLY member/navigation chains whose parts are unnamed
    // (Swift navigation_expression/navigation_suffix). Restricted to these kinds
    // so we never dig into a closure/block body and invent a phantom callee from
    // an IIFE `{ … return x }()` or a call argument.
    if matches!(
        k,
        "navigation_expression"
            | "navigation_suffix"
            | "member_expression"
            | "field_expression"
            | "selector_expression"
            | "member_access_expression"
            | "scoped_identifier"
    ) {
        for i in (0..node.child_count() as u32).rev() {
            let Some(c) = node.child(i) else { continue };
            if c.id() != node.id() && !c.is_extra() {
                if let Some(s) = trailing_ident(c, src) {
                    return Some(s);
                }
            }
        }
    }
    None
}


/// TS/JS `import { A, B as C } from './mod'` / `import X from './mod'` → bindings.
/// Only RELATIVE modules (./ ../) are kept — they map to project files; package
/// imports are external and can never resolve to an internal node.
fn ts_extract_import(node: TsNode, src: &[u8], rel_path: &str, out: &mut Vec<RawImport>) {
    let text = |n: TsNode| std::str::from_utf8(&src[n.byte_range()]).ok().map(str::to_string);
    let Some(module) = node
        .child_by_field_name("source")
        .and_then(text)
        .map(|m| m.trim_matches(|c| c == '"' || c == '\'' || c == '`').to_string())
    else {
        return;
    };
    // Keep non-relative specifiers too: the indexer rewrites tsconfig-path
    // aliases (`@app/x` → `/web/src/x`) and drops true external packages there.
    // Parsing stays IO-free — it can't know the alias map.
    let mut push = |name: String| {
        if !name.is_empty() {
            out.push(RawImport { file_path: rel_path.to_string(), name, module: module.clone() });
        }
    };
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "import_specifier" => {
                // `A as B` binds B locally; plain `A` binds A.
                let bound = n.child_by_field_name("alias").or_else(|| n.child_by_field_name("name"));
                if let Some(name) = bound.and_then(text) {
                    push(name);
                }
                continue;
            }
            "import_clause" => {
                // default import: a bare identifier child
                for i in 0..n.named_child_count() as u32 {
                    if let Some(ch) = n.named_child(i) {
                        if ch.kind() == "identifier" {
                            if let Some(name) = text(ch) {
                                push(name);
                            }
                        }
                    }
                }
            }
            "namespace_import" => {
                // `import * as ns` — binds ns; calls through it are module-bound
                // (externality evidence when the module is an external package).
                if let Some(name) = named_child_of(n, "identifier").and_then(text) {
                    push(name);
                }
                continue;
            }
            _ => {}
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
}

/// Python `from a.b import X, Y as Z` → bindings (module kept dotted; relative
/// `from . import x` keeps the leading dots for the resolver to interpret).
fn py_extract_import(node: TsNode, src: &[u8], rel_path: &str, out: &mut Vec<RawImport>) {
    let text = |n: TsNode| std::str::from_utf8(&src[n.byte_range()]).ok().map(str::to_string);
    let Some(module) = node.child_by_field_name("module_name").and_then(text) else { return };
    for i in 0..node.named_child_count() as u32 {
        let Some(ch) = node.named_child(i) else { continue };
        match ch.kind() {
            "dotted_name" if Some(ch) != node.child_by_field_name("module_name") => {
                if let Some(name) = text(ch) {
                    if !name.contains('.') {
                        out.push(RawImport { file_path: rel_path.to_string(), name, module: module.clone() });
                    }
                }
            }
            "aliased_import" => {
                if let Some(name) = ch.child_by_field_name("alias").and_then(text) {
                    out.push(RawImport { file_path: rel_path.to_string(), name, module: module.clone() });
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(pf: &ParsedFile) -> Vec<(String, NodeLabel)> {
        pf.nodes.iter().map(|n| (n.name.clone(), n.label)).collect()
    }
    fn has(pf: &ParsedFile, name: &str, label: NodeLabel) -> bool {
        names(pf).iter().any(|(n, l)| n == name && *l == label)
    }

    #[test]
    fn kotlin_constructor_properties_locals_and_inherits() {
        let pf = parse_kotlin(
            "p",
            "Repo.kt",
            "class UserRepo(val api: ApiClient, private var cache: Cache?, plain: Int) : BaseRepo(), Syncable {\n  fun load() {\n    val svc = UserService()\n    val typed: Formatter = make()\n    val lower = factory()\n  }\n}\n",
        );
        let field = |n: &str| pf.fields.iter().find(|f| f.field_name == n).map(|f| f.type_name.clone());
        assert_eq!(field("api").as_deref(), Some("ApiClient"), "val ctor property is a field");
        assert_eq!(field("cache").as_deref(), Some("Cache"), "var ctor property unwraps T?");
        assert_eq!(field("plain"), None, "plain param is NOT a field");
        let local = |n: &str| pf.locals.iter().find(|l| l.var_name == n).map(|l| l.type_name.clone());
        assert_eq!(local("svc").as_deref(), Some("UserService"), "capitalized ctor call types the local");
        assert_eq!(local("typed").as_deref(), Some("Formatter"), "declared annotation wins");
        assert_eq!(local("lower"), None, "lowercase factory call is not evidence");
        assert!(pf.inherits.iter().any(|i| i.super_name == "BaseRepo" && i.kind == InheritKind::Extends));
        assert!(pf.inherits.iter().any(|i| i.super_name == "Syncable" && i.kind == InheritKind::Implements));
    }

    #[test]
    fn swift_inheritance_specifiers() {
        let pf = parse_swift(
            "p",
            "VC.swift",
            "class OrderVC: UIViewController, Trackable {\n  func go() {}\n}\n",
        );
        let sups: Vec<&str> = pf.inherits.iter().map(|i| i.super_name.as_str()).collect();
        assert_eq!(sups, vec!["UIViewController", "Trackable"], "both supertypes captured: {sups:?}");
    }

    #[test]
    fn arrow_and_function_expression_components_are_nodes() {
        let pf = parse_tsx(
            "p",
            "App.tsx",
            "export const App = () => { helper(); return null; };\nconst helper = function () {};\nconst notADef = useMemo(() => 1);\n",
        );
        assert!(has(&pf, "App", NodeLabel::Function), "arrow component must be a Function node");
        assert!(has(&pf, "helper", NodeLabel::Function), "function-expression binding must be a node");
        assert!(!has(&pf, "notADef", NodeLabel::Function), "a call initializer is NOT a definition");
        // calls inside the arrow body attribute to the component, not the file
        assert!(pf.calls.iter().any(|c| c.callee_name == "helper" && c.caller_id.ends_with("app")));
    }

    #[test]
    fn junk_names_are_skipped() {
        let pf = parse_ts("p", "codes.ts", "const handlers = { $200() { return 1; }, valid() { return 2; } };\n");
        assert!(!names(&pf).iter().any(|(n, _)| n == "$200"), "no-letter names are noise");
        assert!(has(&pf, "valid", NodeLabel::Method));
    }

    #[test]
    fn ts_locals_new_expression_and_generic_unwrap() {
        let pf = parse_ts(
            "p",
            "svc.ts",
            "class Repo<T> { save(x: T) {} }\nfunction run() {\n  const r = new Repo();\n  const s: Repo<string> = make();\n  const n: Repo | null = maybe();\n  const u: Repo | Error = both();\n  r.save(1); s.save(2); n.save(3);\n}\n",
        );
        let ty = |v: &str| pf.locals.iter().find(|l| l.var_name == v).map(|l| l.type_name.clone());
        assert_eq!(ty("r").as_deref(), Some("Repo"), "new-expression initializer types the local");
        assert_eq!(ty("s").as_deref(), Some("Repo"), "generic annotation unwraps to its base");
        assert_eq!(ty("n").as_deref(), Some("Repo"), "| null unwraps");
        assert_eq!(ty("u"), None, "a real union stays dropped");
    }

    #[test]
    fn ts_field_generic_unwrap() {
        let pf = parse_ts(
            "p",
            "a.ts",
            "class Svc { constructor(private repo: Repo<User>) {} }\n",
        );
        assert!(
            pf.fields.iter().any(|f| f.field_name == "repo" && f.type_name == "Repo"),
            "constructor param property with generic type unwraps to base: {:?}",
            pf.fields
        );
    }

    #[test]
    fn ts_imports_keep_alias_specifiers() {
        let pf = parse_ts("p", "a.ts", "import { doThing } from '@app/svc';\nimport { rel } from './rel';\n");
        assert!(pf.imports.iter().any(|i| i.name == "doThing" && i.module == "@app/svc"));
        assert!(pf.imports.iter().any(|i| i.name == "rel" && i.module == "./rel"));
    }

    #[test]
    fn rust_python_js_ts_go_still_work() {
        assert!(has(&parse_rust("p", "a.rs", "fn helper(){}\nfn main(){ helper(); }\nstruct S;\ntrait T{}\n"), "helper", NodeLabel::Function));
        assert!(has(&parse_python("p", "a.py", "def foo():\n  pass\nclass Bar:\n  pass\n"), "Bar", NodeLabel::Class));
        assert!(has(&parse_ts("p", "a.ts", "interface I{}\nfunction f(){}\n"), "I", NodeLabel::Interface));
        assert!(has(&parse_go("p", "a.go", "package m\nfunc run(){}\ntype S struct{}\n"), "run", NodeLabel::Function));
    }

    #[test]
    fn swift_defs_and_calls() {
        let pf = parse_swift("p", "A.swift", "func helper() {}\nfunc run() { helper() }\nclass C {}\nprotocol P {}\nenum E { case a }\nstruct S {}\n");
        assert!(has(&pf, "helper", NodeLabel::Function));
        assert!(has(&pf, "run", NodeLabel::Function));
        assert!(has(&pf, "C", NodeLabel::Class));
        assert!(has(&pf, "P", NodeLabel::Interface));
        assert!(has(&pf, "E", NodeLabel::Class));
        assert!(has(&pf, "S", NodeLabel::Class));
        assert!(pf.calls.iter().any(|c| c.callee_name == "helper" && c.caller_id.ends_with("run")));
    }

    #[test]
    fn java_defs_and_calls() {
        let pf = parse_java("p", "A.java", "class A {\n  void run() { helper(); }\n  void helper() {}\n}\ninterface I {}\nenum E { X }\n");
        assert!(has(&pf, "A", NodeLabel::Class));
        assert!(has(&pf, "run", NodeLabel::Method));
        assert!(has(&pf, "I", NodeLabel::Interface));
        assert!(has(&pf, "E", NodeLabel::Enum));
        assert!(pf.calls.iter().any(|c| c.callee_name == "helper"));
    }

    #[test]
    fn c_function_name_from_declarator() {
        let pf = parse_file("p", "a.c", "int helper(int x) { return x; }\nint main() { return helper(2); }\nstruct Point { int x; };\n");
        assert!(has(&pf, "helper", NodeLabel::Function));
        assert!(has(&pf, "main", NodeLabel::Function));
        assert!(has(&pf, "Point", NodeLabel::Class));
        assert!(pf.calls.iter().any(|c| c.callee_name == "helper"));
    }

    #[test]
    fn ruby_csharp_bash() {
        assert!(has(&parse_file("p", "a.rb", "def foo\nend\nclass Bar\nend\n"), "Bar", NodeLabel::Class));
        assert!(has(&parse_file("p", "a.cs", "class C { void M() {} }"), "M", NodeLabel::Method));
        assert!(has(&parse_file("p", "a.sh", "greet() { echo hi; }\n"), "greet", NodeLabel::Function));
    }


    #[test]
    fn inheritance_extraction() {
        let py = parse_python("p", "a.py", "class Animal:\n    pass\nclass Dog(Animal):\n    pass\n");
        assert!(py.inherits.iter().any(|i| i.impl_name == "Dog" && i.super_name == "Animal" && i.kind == InheritKind::Extends));
        let ts = parse_ts("p", "a.ts", "interface Service {}\nclass Impl implements Service {}\nclass Base {}\nclass Sub extends Base {}\n");
        assert!(ts.inherits.iter().any(|i| i.impl_name == "Impl" && i.super_name == "Service" && i.kind == InheritKind::Implements));
        assert!(ts.inherits.iter().any(|i| i.impl_name == "Sub" && i.super_name == "Base" && i.kind == InheritKind::Extends));
        let java = parse_java("p", "A.java", "interface I {}\nclass A implements I {}\nclass B extends A {}\n");
        assert!(java.inherits.iter().any(|i| i.impl_name == "A" && i.super_name == "I" && i.kind == InheritKind::Implements));
        assert!(java.inherits.iter().any(|i| i.impl_name == "B" && i.super_name == "A" && i.kind == InheritKind::Extends));
        let rust = parse_rust("p", "a.rs", "struct Foo;\ntrait Show {}\nimpl Show for Foo {}\n");
        assert!(rust.inherits.iter().any(|i| i.impl_name == "Foo" && i.super_name == "Show" && i.kind == InheritKind::Implements));
    }


    #[test]
    fn kotlin_defs_and_calls() {
        let pf = parse_kotlin("p", "A.kt", "fun greet() { say() }\nfun say() {}\nclass Vm {}\ninterface Service {}\nobject Singleton {}\n");
        assert!(has(&pf, "greet", NodeLabel::Function));
        assert!(has(&pf, "Vm", NodeLabel::Class));
        assert!(has(&pf, "Service", NodeLabel::Class));
        assert!(has(&pf, "Singleton", NodeLabel::Class));
        assert!(pf.calls.iter().any(|c| c.callee_name == "say" && c.caller_id.ends_with("greet")));
    }

    #[test]
    fn http_route_extraction() {
        let ts = parse_ts("p", "r.ts", "class C { @Get('/users') list() {} }\napp.post('/login', handler);\n");
        assert!(ts.nodes.iter().any(|n| n.label == NodeLabel::Route && n.name == "GET /users"));
        assert!(ts.nodes.iter().any(|n| n.label == NodeLabel::Route && n.name == "POST /login"));
        let py = parse_python("p", "r.py", "@app.route('/health')\ndef health():\n    pass\n");
        assert!(py.nodes.iter().any(|n| n.label == NodeLabel::Route && n.name.contains("/health")));
    }



    #[test]
    fn unknown_extension_is_empty() {
        assert!(parse_file("p", "a.unknown", "stuff").nodes.is_empty());
    }

    #[test]
    fn identifier_spans_excludes_strings_and_comments() {
        // the def `foo` + the call `foo` are identifier tokens; the string "foo"
        // and the `// foo` comment are NOT — the rename safety gate depends on this.
        let src = "fn foo() {}\nfn bar() { foo(); let s = \"foo\"; }\n// foo here\n";
        let spans = identifier_spans("a.rs", src, "foo");
        assert_eq!(spans.len(), 2, "only identifier tokens, not string/comment occurrences");
        for (s, e, _) in spans {
            assert_eq!(&src[s..e], "foo");
        }
    }

    #[test]
    fn swift_subscript_is_not_a_call() {
        // tree-sitter-swift models `arr[i]` as a call_expression with a `[]`
        // suffix — those are subscripts, not method calls (no phantom edge).
        let pf = parse_swift("p", "a.swift", "class A {\n  func go() {\n    let x = arr[0]\n    cell.configure(with: rows[i])\n  }\n}");
        assert!(!pf.calls.iter().any(|c| c.callee_name == "arr"), "arr[0] subscript is not a call");
        assert!(!pf.calls.iter().any(|c| c.callee_name == "rows"), "rows[i] subscript is not a call");
        assert!(pf.calls.iter().any(|c| c.callee_name == "configure"), "configure(...) IS a call");
    }

    #[test]
    fn swift_iife_closure_invents_no_callee() {
        // `static let f: X = { … return formatter }()` is an immediately-invoked
        // closure — it must NOT mint a call to the closure's last identifier.
        let pf = parse_swift("p", "a.swift", "class A {\n  static let f: X = {\n    let formatter = Y()\n    return formatter\n  }()\n}");
        assert!(!pf.calls.iter().any(|c| c.callee_name == "formatter"), "IIFE body must not become a callee");
    }
}
