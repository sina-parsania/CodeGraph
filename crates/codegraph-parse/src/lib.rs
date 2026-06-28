//! Grammar-driven parser: one generic tree-sitter walk parameterized by a
//! per-language `LangSpec` (label map, call-node kinds, callee fields, name
//! extraction mode). Adding a language = one grammar dep + one `LangSpec`.

use codegraph_core::{InheritKind, Metadata, Node, NodeLabel, QualifiedName, RawCall, RawField, RawInherit, Receiver};
use tree_sitter::{Language, Node as TsNode, Parser};

pub struct ParsedFile {
    pub nodes: Vec<Node>,
    pub calls: Vec<RawCall>,
    pub inherits: Vec<RawInherit>,
    pub fields: Vec<RawField>,
}

impl ParsedFile {
    fn empty() -> Self {
        ParsedFile { nodes: Vec::new(), calls: Vec::new(), inherits: Vec::new(), fields: Vec::new() }
    }
}

#[derive(Clone, Copy)]
enum NameMode {
    Field,
    CDeclarator,
}

type InheritFn = fn(TsNode, &[u8]) -> Vec<RawInherit>;

struct LangSpec {
    name: &'static str,
    language: fn() -> Language,
    label_for: fn(&str) -> Option<NodeLabel>,
    call_kinds: &'static [&'static str],
    callee_fields: &'static [&'static str],
    name_mode: NameMode,
    inherit_fn: Option<InheritFn>,
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

static RUST: LangSpec = LangSpec { name: "rust", language: || tree_sitter_rust::LANGUAGE.into(), label_for: rust_label, call_kinds: &["call_expression"], callee_fields: &["function"], name_mode: NameMode::Field, inherit_fn: Some(rust_inherits) };
static PYTHON: LangSpec = LangSpec { name: "python", language: || tree_sitter_python::LANGUAGE.into(), label_for: python_label, call_kinds: &["call"], callee_fields: &["function"], name_mode: NameMode::Field, inherit_fn: Some(py_inherits) };
static JS: LangSpec = LangSpec { name: "javascript", language: || tree_sitter_javascript::LANGUAGE.into(), label_for: js_label, call_kinds: &["call_expression"], callee_fields: &["function"], name_mode: NameMode::Field, inherit_fn: None };
static TS: LangSpec = LangSpec { name: "typescript", language: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), label_for: ts_label, call_kinds: &["call_expression"], callee_fields: &["function"], name_mode: NameMode::Field, inherit_fn: Some(ts_inherits) };
static TSX: LangSpec = LangSpec { name: "typescript", language: || tree_sitter_typescript::LANGUAGE_TSX.into(), label_for: ts_label, call_kinds: &["call_expression"], callee_fields: &["function"], name_mode: NameMode::Field, inherit_fn: Some(ts_inherits) };
static GO: LangSpec = LangSpec { name: "go", language: || tree_sitter_go::LANGUAGE.into(), label_for: go_label, call_kinds: &["call_expression"], callee_fields: &["function"], name_mode: NameMode::Field, inherit_fn: None };
static SWIFT: LangSpec = LangSpec { name: "swift", language: || tree_sitter_swift::LANGUAGE.into(), label_for: swift_label, call_kinds: &["call_expression"], callee_fields: &[], name_mode: NameMode::Field, inherit_fn: None };
static JAVA: LangSpec = LangSpec { name: "java", language: || tree_sitter_java::LANGUAGE.into(), label_for: java_label, call_kinds: &["method_invocation"], callee_fields: &["name"], name_mode: NameMode::Field, inherit_fn: Some(java_inherits) };
static C: LangSpec = LangSpec { name: "c", language: || tree_sitter_c::LANGUAGE.into(), label_for: c_label, call_kinds: &["call_expression"], callee_fields: &["function"], name_mode: NameMode::CDeclarator, inherit_fn: None };
static CPP: LangSpec = LangSpec { name: "cpp", language: || tree_sitter_cpp::LANGUAGE.into(), label_for: cpp_label, call_kinds: &["call_expression"], callee_fields: &["function"], name_mode: NameMode::CDeclarator, inherit_fn: None };
static RUBY: LangSpec = LangSpec { name: "ruby", language: || tree_sitter_ruby::LANGUAGE.into(), label_for: ruby_label, call_kinds: &["call"], callee_fields: &["method"], name_mode: NameMode::Field, inherit_fn: None };
static CSHARP: LangSpec = LangSpec { name: "csharp", language: || tree_sitter_c_sharp::LANGUAGE.into(), label_for: csharp_label, call_kinds: &["invocation_expression"], callee_fields: &["function"], name_mode: NameMode::Field, inherit_fn: None };
static KOTLIN: LangSpec = LangSpec { name: "kotlin", language: || tree_sitter_kotlin_ng::LANGUAGE.into(), label_for: kotlin_label, call_kinds: &["call_expression"], callee_fields: &[], name_mode: NameMode::Field, inherit_fn: None };
static BASH: LangSpec = LangSpec { name: "bash", language: || tree_sitter_bash::LANGUAGE.into(), label_for: bash_label, call_kinds: &[], callee_fields: &[], name_mode: NameMode::Field, inherit_fn: None };

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
    let ctx = Ctx { spec, project, segs: &file_segs, rel_path, file_id: &file_id };
    collect(tree.root_node(), bytes, &ctx, None, None, &mut nodes, &mut calls, &mut inherits, &mut fields);
    ParsedFile { nodes, calls, inherits, fields }
}

struct Ctx<'a> {
    spec: &'a LangSpec,
    project: &'a str,
    segs: &'a [&'a str],
    rel_path: &'a str,
    file_id: &'a str,
}

#[allow(clippy::too_many_arguments)]
fn collect(node: TsNode, src: &[u8], ctx: &Ctx, current_fn: Option<&str>, this_class: Option<&str>, nodes: &mut Vec<Node>, calls: &mut Vec<RawCall>, inherits: &mut Vec<RawInherit>, fields: &mut Vec<RawField>) {
    let mut my_fn_id: Option<String> = None;
    let mut my_class_id: Option<String> = None;

    if let Some(label) = (ctx.spec.label_for)(node.kind()) {
        if let Some(name) = name_of(node, src, ctx.spec.name_mode) {
            if !name.is_empty() {
                let id = QualifiedName::build(ctx.project, ctx.segs, &name);
                match label {
                    NodeLabel::Function | NodeLabel::Method => my_fn_id = Some(id.clone()),
                    NodeLabel::Class | NodeLabel::Interface | NodeLabel::Enum => my_class_id = Some(id.clone()),
                    _ => {}
                }
                nodes.push(Node {
                    id,
                    label,
                    name,
                    file_path: ctx.rel_path.to_string(),
                    line_start: node.start_position().row as u32 + 1,
                    line_end: node.end_position().row as u32 + 1,
                    language: ctx.spec.name.to_string(),
                    metadata: Metadata::new(),
                    community: None,
                    pagerank: 0.0,
                    betweenness: 0.0,
                });
            }
        }
    }

    // At a TS class, capture its typed fields for T3 (this.field.method()) resolution.
    if let Some(cls_id) = my_class_id.as_deref() {
        if ctx.spec.name == "typescript" {
            ts_extract_fields(node, src, cls_id, fields);
        }
    }

    if ctx.spec.call_kinds.contains(&node.kind()) {
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
                    }
                }
            }
            // Receiver-aware capture (all languages): self/this binds to the
            // enclosing class so the resolver can do Class-Hierarchy-Analysis.
            let receiver = detect_receiver(node, src);
            let enclosing_class = match receiver {
                Receiver::SelfThis | Receiver::Super | Receiver::Field(_) => this_class.map(str::to_string),
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
        collect(child, src, ctx, next_fn, next_this_class, nodes, calls, inherits, fields);
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
fn ts_extract_fields(class: TsNode, src: &[u8], class_id: &str, out: &mut Vec<RawField>) {
    let text = |n: TsNode| std::str::from_utf8(&src[n.byte_range()]).ok().map(str::to_string);
    let type_in = |n: TsNode| -> Option<String> {
        let mut c = n.walk();
        let ann = n.named_children(&mut c).find(|ch| ch.kind() == "type_annotation")?;
        let mut c2 = ann.walk();
        let ty = ann.named_children(&mut c2).find(|t| t.kind() == "type_identifier")?;
        text(ty)
    };
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

/// In TS/JS a nested non-arrow function literal rebinds `this`. In Swift/Kotlin/
/// Python/etc. the "function" kind IS the method (self stays the instance), so we
/// only treat these kinds as rebinding for TS/JS.
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
    // Fallback: rightmost child yielding an identifier — handles member/navigation
    // chains whose parts are unnamed (Swift navigation_expression/navigation_suffix).
    for i in (0..node.child_count() as u32).rev() {
        let Some(c) = node.child(i) else { continue };
        if c.id() != node.id() && !c.is_extra() {
            if let Some(s) = trailing_ident(c, src) {
                return Some(s);
            }
        }
    }
    None
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
}
