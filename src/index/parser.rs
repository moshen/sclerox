use std::collections::HashSet;
use std::path::Path;
use tree_sitter::{Language, Node, Parser};

/// A parsed symbol extracted from source code.
#[derive(Debug, Clone)]
pub struct ParsedSymbol {
    pub kind: String,
    pub name: String,
    pub signature: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
}

/// A chunk of source code for embedding.
#[derive(Debug, Clone)]
pub struct CodeChunk {
    pub text: String,
    pub start_line: u32,
    pub end_line: u32,
}

/// A directed edge in the call/inheritance graph.
#[derive(Debug, Clone)]
pub struct ParsedEdge {
    /// "calls", "inherits", or "implements"
    pub kind: String,
    pub from_name: String,
    pub to_name: String,
    pub line: u32,
}

/// Detect language from file extension.
pub fn detect_language(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()? {
        "rs" => Some("rust"),
        "py" | "pyi" => Some("python"),
        "ts" | "tsx" => Some("typescript"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "go" => Some("go"),
        "cs" => Some("csharp"),
        "java" => Some("java"),
        "cpp" | "cc" | "cxx" | "c" | "h" | "hpp" | "hxx" => Some("cpp"),
        "rb" => Some("ruby"),
        "swift" => Some("swift"),
        "kt" | "kts" => Some("kotlin"),
        "scala" => Some("scala"),
        "php" => Some("php"),
        "sh" | "bash" | "zsh" => Some("shell"),
        "sql" => Some("sql"),
        "md" | "mdx" => Some("markdown"),
        _ => None,
    }
}

fn get_tree_sitter_language(lang: &str) -> Option<Language> {
    match lang {
        "rust" => Some(tree_sitter_rust::LANGUAGE.into()),
        "python" => Some(tree_sitter_python::LANGUAGE.into()),
        "typescript" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "javascript" => Some(tree_sitter_javascript::LANGUAGE.into()),
        "go" => Some(tree_sitter_go::LANGUAGE.into()),
        "csharp" => Some(tree_sitter_c_sharp::LANGUAGE.into()),
        "java" => Some(tree_sitter_java::LANGUAGE.into()),
        "cpp" => Some(tree_sitter_cpp::LANGUAGE.into()),
        "ruby" => Some(tree_sitter_ruby::LANGUAGE.into()),
        "kotlin" => Some(tree_sitter_kotlin_ng::LANGUAGE.into()),
        "sql" => Some(tree_sitter_sequel::LANGUAGE.into()),
        "shell" => Some(tree_sitter_bash::LANGUAGE.into()),
        "lua" => Some(tree_sitter_lua::LANGUAGE.into()),
        _ => None,
    }
}

/// Parse a source file, extracting symbols, code chunks, and call graph edges.
/// Falls back to line-based chunking (no symbols, no edges) for unknown languages.
pub fn parse_file(
    source: &str,
    language: &str,
    chunk_size_lines: usize,
) -> (Vec<ParsedSymbol>, Vec<CodeChunk>, Vec<ParsedEdge>) {
    if let Some(ts_lang) = get_tree_sitter_language(language) {
        parse_with_tree_sitter(source, language, &ts_lang, chunk_size_lines)
    } else {
        (vec![], chunk_by_lines(source, chunk_size_lines), vec![])
    }
}

fn parse_with_tree_sitter(
    source: &str,
    language: &str,
    ts_lang: &Language,
    chunk_size_lines: usize,
) -> (Vec<ParsedSymbol>, Vec<CodeChunk>, Vec<ParsedEdge>) {
    let mut parser = Parser::new();
    if parser.set_language(ts_lang).is_err() {
        return (vec![], chunk_by_lines(source, chunk_size_lines), vec![]);
    }

    let tree = match parser.parse(source.as_bytes(), None) {
        Some(t) => t,
        None => return (vec![], chunk_by_lines(source, chunk_size_lines), vec![]),
    };

    let root = tree.root_node();
    let mut out = ParseOutput {
        symbols: Vec::new(),
        chunks: Vec::new(),
        edges: Vec::new(),
        seen_edges: HashSet::new(),
    };

    collect_symbols_and_edges(&root, source, language, &mut out, None);

    if out.chunks.is_empty() {
        out.chunks = chunk_by_lines(source, chunk_size_lines);
    }

    (out.symbols, out.chunks, out.edges)
}

fn is_symbol_node(kind: &str, language: &str) -> bool {
    match language {
        "rust" => matches!(
            kind,
            "function_item"
                | "struct_item"
                | "enum_item"
                | "impl_item"
                | "trait_item"
                | "type_item"
                | "const_item"
                | "static_item"
                | "macro_definition"
        ),
        "python" => matches!(
            kind,
            "function_definition" | "class_definition" | "decorated_definition"
        ),
        "typescript" | "javascript" => matches!(
            kind,
            "function_declaration"
                | "class_declaration"
                | "method_definition"
                | "arrow_function"
                | "export_statement"
                | "lexical_declaration"
        ),
        "go" => matches!(
            kind,
            "function_declaration"
                | "method_declaration"
                | "type_declaration"
                | "var_declaration"
                | "const_declaration"
        ),
        "csharp" => matches!(
            kind,
            "class_declaration"
                | "interface_declaration"
                | "method_declaration"
                | "constructor_declaration"
                | "enum_declaration"
                | "record_declaration"
                | "struct_declaration"
                | "namespace_declaration"
                | "property_declaration"
        ),
        "java" => matches!(
            kind,
            "method_declaration"
                | "constructor_declaration"
                | "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
                | "annotation_type_declaration"
        ),
        "cpp" => matches!(
            kind,
            "function_definition" | "class_specifier" | "struct_specifier"
        ),
        "ruby" => matches!(kind, "method" | "singleton_method" | "class" | "module"),
        "kotlin" => matches!(
            kind,
            "function_declaration" | "class_declaration" | "object_declaration" | "companion_object"
        ),
        "sql" => matches!(
            kind,
            "create_function" | "create_table" | "create_view" | "create_type"
        ),
        "shell" => kind == "function_definition",
        "lua" => matches!(kind, "function_declaration" | "local_function"),
        _ => false,
    }
}

struct ParseOutput {
    symbols: Vec<ParsedSymbol>,
    chunks: Vec<CodeChunk>,
    edges: Vec<ParsedEdge>,
    seen_edges: HashSet<(String, String)>,
}

fn collect_symbols_and_edges(
    node: &Node,
    source: &str,
    language: &str,
    out: &mut ParseOutput,
    enclosing: Option<&str>,
) {
    let kind = node.kind();
    let is_symbol = is_symbol_node(kind, language);

    if is_symbol {
        let start = node.start_position();
        let end = node.end_position();
        let name = extract_name(node, source, language)
            .unwrap_or_else(|| "<anonymous>".to_string());
        let text = &source[node.start_byte()..node.end_byte()];
        let signature = extract_signature(text, language);

        out.symbols.push(ParsedSymbol {
            kind: kind.to_string(),
            name: name.clone(),
            signature,
            start_line: start.row as u32 + 1,
            end_line: end.row as u32 + 1,
        });

        out.chunks.push(CodeChunk {
            text: text.to_string(),
            start_line: start.row as u32 + 1,
            end_line: end.row as u32 + 1,
        });

        // Collect inheritance/implements edges for the current node itself.
        if name != "<anonymous>" {
            for edge in extract_structural_edges(node, source, language, &name) {
                let key = (edge.from_name.clone(), edge.to_name.clone());
                if out.seen_edges.insert(key) {
                    out.edges.push(edge);
                }
            }
        }

        // Recurse into the symbol's body to find nested symbols and call edges.
        let enc = if name == "<anonymous>" { enclosing } else { Some(name.as_str()) };
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            collect_symbols_and_edges(&child, source, language, out, enc);
        }
        return;
    }

    // If we're inside a named symbol, look for call expressions at this node.
    if let Some(enc) = enclosing {
        if let Some(callee) = extract_call_target(node, source, language) {
            if !callee.is_empty() && callee != "<anonymous>" && !is_noise_call(&callee, language) {
                let key = (enc.to_string(), callee.clone());
                if out.seen_edges.insert(key) {
                    out.edges.push(ParsedEdge {
                        kind: "calls".to_string(),
                        from_name: enc.to_string(),
                        to_name: callee,
                        line: node.start_position().row as u32 + 1,
                    });
                }
            }
        }
    }

    // Recurse into non-symbol children, preserving the current enclosing scope.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_symbols_and_edges(&child, source, language, out, enclosing);
    }
}

/// Extract call target name from a call expression node.
fn extract_call_target(node: &Node, source: &str, language: &str) -> Option<String> {
    match language {
        "rust" | "typescript" | "javascript" | "go" | "cpp" => {
            if node.kind() != "call_expression" {
                return None;
            }
            let fn_node = node.child_by_field_name("function")?;
            extract_leaf_name(&fn_node, source)
        }
        "python" => {
            if node.kind() != "call" {
                return None;
            }
            let fn_node = node.child_by_field_name("function")?;
            extract_leaf_name(&fn_node, source)
        }
        "csharp" => {
            if node.kind() != "invocation_expression" {
                return None;
            }
            let fn_node = node.child_by_field_name("expression")?;
            extract_leaf_name(&fn_node, source)
        }
        "java" => match node.kind() {
            "method_invocation" => node
                .child_by_field_name("name")
                .map(|n| source[n.start_byte()..n.end_byte()].to_string()),
            "object_creation_expression" => node
                .child_by_field_name("type")
                .and_then(|n| extract_leaf_name(&n, source)),
            _ => None,
        },
        "ruby" => {
            if node.kind() != "call" {
                return None;
            }
            let method_node = node.child_by_field_name("method")?;
            Some(source[method_node.start_byte()..method_node.end_byte()].to_string())
        }
        // SQL: function/procedure call via invocation node
        "sql" => {
            if node.kind() != "invocation" {
                return None;
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "object_reference" {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        return Some(
                            source[name_node.start_byte()..name_node.end_byte()].to_string(),
                        );
                    }
                }
            }
            None
        }
        // Bash: any command is a "call" to the command name
        "shell" => {
            if node.kind() != "command" {
                return None;
            }
            let name_node = node.child_by_field_name("name")?;
            // command_name wraps the actual name token
            let text = source[name_node.start_byte()..name_node.end_byte()].to_string();
            // Filter built-ins and common external commands that aren't navigation targets
            if is_shell_builtin(&text) {
                return None;
            }
            Some(text)
        }
        // Lua: function_call.name → identifier or dot/method index expression
        "lua" => {
            if node.kind() != "function_call" {
                return None;
            }
            let name_node = node.child_by_field_name("name")?;
            match name_node.kind() {
                "identifier" | "variable" => {
                    Some(source[name_node.start_byte()..name_node.end_byte()].to_string())
                }
                "dot_index_expression" => name_node
                    .child_by_field_name("field")
                    .map(|f| source[f.start_byte()..f.end_byte()].to_string()),
                "method_index_expression" => name_node
                    .child_by_field_name("method")
                    .map(|m| source[m.start_byte()..m.end_byte()].to_string()),
                // chained call: already-resolved function_call as callee — skip
                _ => None,
            }
        }
        "kotlin" => {
            if node.kind() != "call_expression" {
                return None;
            }
            let callee = node.named_child(0)?;
            match callee.kind() {
                "identifier" | "simple_identifier" => {
                    Some(source[callee.start_byte()..callee.end_byte()].to_string())
                }
                "navigation_expression" => {
                    // obj.method() — last named child is the method identifier
                    let n = callee.named_child_count();
                    let last = callee.named_child(n.checked_sub(1)?)?;
                    Some(source[last.start_byte()..last.end_byte()].to_string())
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Extract the leaf identifier from a function/expression node.
fn extract_leaf_name(node: &Node, source: &str) -> Option<String> {
    match node.kind() {
        // Simple identifiers
        "identifier"
        | "property_identifier"
        | "field_identifier"
        | "type_identifier"
        | "identifier_name" => Some(source[node.start_byte()..node.end_byte()].to_string()),

        // Rust: self.method() — field_expression { value, field }
        "field_expression" => node
            .child_by_field_name("field")
            .map(|n| source[n.start_byte()..n.end_byte()].to_string()),

        // Rust: path::to::func — scoped_identifier { path, name }
        "scoped_identifier" => node
            .child_by_field_name("name")
            .map(|n| source[n.start_byte()..n.end_byte()].to_string()),

        // JS/TS: obj.method — member_expression { object, property }
        "member_expression" => node
            .child_by_field_name("property")
            .map(|n| source[n.start_byte()..n.end_byte()].to_string()),

        // Python: obj.attr — attribute { object, attribute }
        "attribute" => node
            .child_by_field_name("attribute")
            .map(|n| source[n.start_byte()..n.end_byte()].to_string()),

        // Go: pkg.Func — selector_expression { operand, field }
        "selector_expression" => node
            .child_by_field_name("field")
            .map(|n| source[n.start_byte()..n.end_byte()].to_string()),

        // C#: obj.Method — member_access_expression { expression, name }
        "member_access_expression" => node
            .child_by_field_name("name")
            .map(|n| source[n.start_byte()..n.end_byte()].to_string()),

        // C++: Namespace::func — qualified_identifier { scope, name }
        "qualified_identifier" => node
            .child_by_field_name("name")
            .map(|n| source[n.start_byte()..n.end_byte()].to_string()),

        _ => None,
    }
}

/// Extract structural (non-call) edges: inheritance and interface implementation.
fn extract_structural_edges(
    node: &Node,
    source: &str,
    language: &str,
    symbol_name: &str,
) -> Vec<ParsedEdge> {
    let mut edges = Vec::new();

    match language {
        "python" => {
            if node.kind() == "class_definition" {
                if let Some(bases) = node.child_by_field_name("superclasses") {
                    let mut cursor = bases.walk();
                    for child in bases.named_children(&mut cursor) {
                        if let Some(name) = extract_leaf_name(&child, source) {
                            if !name.is_empty() {
                                edges.push(ParsedEdge {
                                    kind: "inherits".to_string(),
                                    from_name: symbol_name.to_string(),
                                    to_name: name,
                                    line: child.start_position().row as u32 + 1,
                                });
                            }
                        }
                    }
                }
            }
        }

        "typescript" | "javascript" => {
            if node.kind() == "class_declaration" {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "class_heritage" {
                        let mut c2 = child.walk();
                        for item in child.children(&mut c2) {
                            let (kind, name_field) = match item.kind() {
                                "extends_clause" => ("inherits", "value"),
                                "implements_clause" => ("implements", "type"),
                                _ => continue,
                            };
                            // Try field name first, fall back to first named child
                            let target_node = item
                                .child_by_field_name(name_field)
                                .or_else(|| item.named_child(0));
                            if let Some(n) = target_node {
                                if let Some(name) = extract_leaf_name(&n, source) {
                                    if !name.is_empty() {
                                        edges.push(ParsedEdge {
                                            kind: kind.to_string(),
                                            from_name: symbol_name.to_string(),
                                            to_name: name,
                                            line: item.start_position().row as u32 + 1,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        "rust" => {
            // impl Trait for Type — emit (Type → Trait, kind=implements)
            if node.kind() == "impl_item" {
                if let Some(trait_node) = node.child_by_field_name("trait") {
                    if let Some(name) = extract_leaf_name(&trait_node, source) {
                        if !name.is_empty() {
                            edges.push(ParsedEdge {
                                kind: "implements".to_string(),
                                from_name: symbol_name.to_string(),
                                to_name: name,
                                line: trait_node.start_position().row as u32 + 1,
                            });
                        }
                    }
                }
            }
        }

        "csharp" => {
            if matches!(
                node.kind(),
                "class_declaration" | "interface_declaration" | "record_declaration"
            ) {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "base_list" {
                        let mut c2 = child.walk();
                        for item in child.named_children(&mut c2) {
                            if let Some(name) = extract_leaf_name(&item, source) {
                                if name.is_empty() {
                                    continue;
                                }
                                // Heuristic: IFoo pattern = interface
                                let kind = if name.starts_with('I')
                                    && name.len() > 1
                                    && name.chars().nth(1).is_some_and(|c| c.is_uppercase())
                                {
                                    "implements"
                                } else {
                                    "inherits"
                                };
                                edges.push(ParsedEdge {
                                    kind: kind.to_string(),
                                    from_name: symbol_name.to_string(),
                                    to_name: name,
                                    line: item.start_position().row as u32 + 1,
                                });
                            }
                        }
                    }
                }
            }
        }

        "java" => {
            if node.kind() == "class_declaration" {
                // extends: class_declaration.superclass → superclass wrapper → type_identifier
                if let Some(sc) = node.child_by_field_name("superclass") {
                    let mut cursor = sc.walk();
                    for child in sc.named_children(&mut cursor) {
                        if let Some(name) = extract_leaf_name(&child, source) {
                            if !name.is_empty() {
                                edges.push(ParsedEdge {
                                    kind: "inherits".to_string(),
                                    from_name: symbol_name.to_string(),
                                    to_name: name,
                                    line: child.start_position().row as u32 + 1,
                                });
                            }
                        }
                    }
                }
                // implements: class_declaration.interfaces → super_interfaces → type_list children
                if let Some(ifaces) = node.child_by_field_name("interfaces") {
                    let mut c1 = ifaces.walk();
                    for type_list in ifaces.named_children(&mut c1) {
                        let mut c2 = type_list.walk();
                        for type_node in type_list.named_children(&mut c2) {
                            if let Some(name) = extract_leaf_name(&type_node, source) {
                                if !name.is_empty() {
                                    edges.push(ParsedEdge {
                                        kind: "implements".to_string(),
                                        from_name: symbol_name.to_string(),
                                        to_name: name,
                                        line: type_node.start_position().row as u32 + 1,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        "ruby" if node.kind() == "class" => {
            if let Some(sc) = node.child_by_field_name("superclass") {
                if let Some(child) = sc.named_child(0) {
                    let name = source[child.start_byte()..child.end_byte()].to_string();
                    if !name.is_empty() {
                        edges.push(ParsedEdge {
                            kind: "inherits".to_string(),
                            from_name: symbol_name.to_string(),
                            to_name: name,
                            line: child.start_position().row as u32 + 1,
                        });
                    }
                }
            }
        }

        _ => {}
    }

    edges
}

/// Bash built-ins and common external commands that clutter the call graph.
fn is_shell_builtin(name: &str) -> bool {
    const BUILTINS: &[&str] = &[
        "echo", "printf", "cd", "export", "source", ".", "return", "exit",
        "if", "then", "else", "fi", "for", "while", "do", "done", "case", "esac",
        "local", "readonly", "declare", "typeset", "set", "unset", "shift",
        "true", "false", "test", "[", "[[",
        // common external commands unlikely to be project-defined
        "grep", "sed", "awk", "cut", "sort", "uniq", "cat", "ls", "cp", "mv",
        "rm", "mkdir", "touch", "find", "xargs", "curl", "wget",
    ];
    BUILTINS.contains(&name)
}

/// Common method/function names that produce noise in the call graph.
fn is_noise_call(name: &str, language: &str) -> bool {
    if name.len() < 2 {
        return true;
    }
    const UNIVERSAL: &[&str] = &["len", "push", "pop", "get", "set", "fmt"];
    const RUST: &[&str] = &[
        "unwrap", "expect", "ok", "err", "into", "from", "as_ref", "as_mut",
        "map", "filter", "collect", "iter", "into_iter", "next", "clone",
        "to_string", "to_owned", "borrow", "borrow_mut", "deref",
    ];
    const PYTHON: &[&str] = &["append", "extend", "items", "keys", "values", "strip", "split"];
    const JAVA_KOTLIN: &[&str] = &[
        "toString", "equals", "hashCode", "compareTo", "size", "isEmpty", "contains",
        "add", "remove", "put", "println", "print",
    ];
    const RUBY: &[&str] = &[
        "to_s", "to_i", "to_f", "to_a", "inspect", "puts", "print",
    ];
    match language {
        "rust" => UNIVERSAL.contains(&name) || RUST.contains(&name),
        "python" => UNIVERSAL.contains(&name) || PYTHON.contains(&name),
        "java" | "kotlin" => UNIVERSAL.contains(&name) || JAVA_KOTLIN.contains(&name),
        "ruby" => UNIVERSAL.contains(&name) || RUBY.contains(&name),
        _ => UNIVERSAL.contains(&name),
    }
}

fn extract_name(node: &Node, source: &str, language: &str) -> Option<String> {
    // Languages with non-standard name locations.
    match language {
        "cpp" => return extract_name_cpp(node, source),
        // SQL: name is inside an object_reference child (name: identifier field)
        "sql" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "object_reference" {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        return Some(
                            source[name_node.start_byte()..name_node.end_byte()].to_string(),
                        );
                    }
                }
                // create_view sometimes has a bare identifier
                if child.kind() == "identifier" {
                    return Some(source[child.start_byte()..child.end_byte()].to_string());
                }
            }
            return None;
        }
        // Bash: function name is a `word` node
        "shell" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                return Some(source[name_node.start_byte()..name_node.end_byte()].to_string());
            }
            return None;
        }
        // Lua: function_declaration.name can be identifier or dot_index_expression.field
        "lua" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                match name_node.kind() {
                    "identifier" => {
                        return Some(
                            source[name_node.start_byte()..name_node.end_byte()].to_string(),
                        )
                    }
                    "dot_index_expression" | "method_index_expression" => {
                        if let Some(field) = name_node.child_by_field_name("field").or_else(|| name_node.child_by_field_name("method")) {
                            return Some(
                                source[field.start_byte()..field.end_byte()].to_string(),
                            );
                        }
                    }
                    _ => {}
                }
            }
            // local_function: name is a direct identifier child
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "identifier" {
                    return Some(source[child.start_byte()..child.end_byte()].to_string());
                }
            }
            return None;
        }
        _ => {}
    }

    let name_kinds: &[&str] = match language {
        "rust" => &["identifier", "type_identifier"],
        "python" => &["identifier"],
        "typescript" | "javascript" => &["identifier", "property_identifier", "type_identifier"],
        "go" => &["identifier", "type_identifier"],
        "csharp" => &["identifier", "type_identifier"],
        "ruby" => &["identifier", "constant"],
        "java" | "kotlin" => &["identifier"],
        _ => &["identifier"],
    };

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if name_kinds.contains(&child.kind()) {
            return Some(source[child.start_byte()..child.end_byte()].to_string());
        }
    }
    None
}

/// Extract the name from a C++ declaration node.
fn extract_name_cpp(node: &Node, source: &str) -> Option<String> {
    // class_specifier / struct_specifier: direct `type_identifier` child
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "type_identifier" {
            return Some(source[child.start_byte()..child.end_byte()].to_string());
        }
    }
    // function_definition: recurse through declarator chain to find the function name
    if let Some(decl) = node.child_by_field_name("declarator") {
        return extract_cpp_declarator_name(&decl, source);
    }
    None
}

/// Walk a C++ declarator chain to find the final identifier (function/method name).
fn extract_cpp_declarator_name(node: &Node, source: &str) -> Option<String> {
    match node.kind() {
        "identifier" | "field_identifier" | "destructor_name" => {
            Some(source[node.start_byte()..node.end_byte()].to_string())
        }
        // Namespace::method → use `name` field
        "qualified_identifier" => node
            .child_by_field_name("name")
            .map(|n| source[n.start_byte()..n.end_byte()].to_string()),
        // function_declarator.declarator → recurse
        "function_declarator" => node
            .child_by_field_name("declarator")
            .and_then(|d| extract_cpp_declarator_name(&d, source)),
        // *fn or &fn — skip the operator token and look at named children
        "pointer_declarator" | "reference_declarator" | "abstract_pointer_declarator" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if let Some(name) = extract_cpp_declarator_name(&child, source) {
                    return Some(name);
                }
            }
            None
        }
        _ => None,
    }
}

fn extract_signature(text: &str, language: &str) -> Option<String> {
    let sig = match language {
        "rust" | "cpp" | "shell" => text
            .lines()
            .next()
            .map(|l| l.trim_end_matches('{').trim().to_string()),
        "python" | "go" | "typescript" | "javascript" | "java" | "kotlin" | "ruby" | "lua" => {
            text.lines().next().map(|l| l.to_string())
        }
        // SQL: take up to and including the function/table name (first meaningful line)
        "sql" => text.lines().next().map(|l| l.to_string()),
        _ => None,
    };
    sig.filter(|s| !s.is_empty())
}

fn chunk_by_lines(source: &str, chunk_size: usize) -> Vec<CodeChunk> {
    let lines: Vec<&str> = source.lines().collect();
    let overlap = chunk_size / 4;
    let mut chunks = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let end = (i + chunk_size).min(lines.len());
        let text = lines[i..end].join("\n");
        chunks.push(CodeChunk {
            text,
            start_line: i as u32 + 1,
            end_line: end as u32,
        });
        if end == lines.len() {
            break;
        }
        i += chunk_size - overlap;
    }
    chunks
}

/// Split a chunk that exceeds `max_chars` into smaller overlapping pieces.
pub fn split_large_chunk(
    chunk: CodeChunk,
    max_chars: usize,
    overlap_lines: usize,
) -> Vec<CodeChunk> {
    if chunk.text.len() <= max_chars {
        return vec![chunk];
    }

    let lines: Vec<&str> = chunk.text.lines().collect();
    let mut result = Vec::new();
    let mut start = 0usize;

    while start < lines.len() {
        let mut char_count = 0usize;
        let mut end = start;

        while end < lines.len() {
            let line_chars = lines[end].len() + 1;
            if char_count + line_chars > max_chars && end > start {
                break;
            }
            char_count += line_chars;
            end += 1;
        }

        if end == start {
            end += 1;
        }

        result.push(CodeChunk {
            text: lines[start..end].join("\n"),
            start_line: chunk.start_line + start as u32,
            end_line: chunk.start_line + end as u32 - 1,
        });

        if end >= lines.len() {
            break;
        }

        // Always advance by at least 1 line to prevent infinite loop on lines > max_chars.
        let next = end.saturating_sub(overlap_lines);
        start = next.max(start + 1);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_language() {
        assert_eq!(detect_language(Path::new("main.rs")), Some("rust"));
        assert_eq!(detect_language(Path::new("app.py")), Some("python"));
        assert_eq!(detect_language(Path::new("index.ts")), Some("typescript"));
        assert_eq!(detect_language(Path::new("main.go")), Some("go"));
        assert_eq!(detect_language(Path::new("Program.cs")), Some("csharp"));
        assert_eq!(detect_language(Path::new("Main.java")), Some("java"));
        assert_eq!(detect_language(Path::new("readme.md")), Some("markdown"));
        assert_eq!(detect_language(Path::new("makefile")), None);
    }

    #[test]
    fn test_parse_rust_functions() {
        let source = r#"
fn hello(name: &str) -> String {
    format!("Hello, {}!", name)
}

struct Config {
    debug: bool,
    port: u16,
}

fn main() {
    println!("ok");
}
"#;
        let (symbols, _chunks, _edges) = parse_file(source, "rust", 50);
        assert!(!symbols.is_empty(), "should extract symbols");
        let fn_names: Vec<&str> = symbols
            .iter()
            .filter(|s| s.kind == "function_item")
            .map(|s| s.name.as_str())
            .collect();
        assert!(fn_names.contains(&"hello"), "missing fn hello");
        assert!(fn_names.contains(&"main"), "missing fn main");
    }

    #[test]
    fn test_parse_rust_call_edges() {
        let source = r#"
fn process(data: &str) -> String {
    let result = validate(data);
    transform(result)
}

fn validate(_: &str) -> bool { true }
fn transform(_: bool) -> String { String::new() }
"#;
        let (symbols, _chunks, edges) = parse_file(source, "rust", 50);
        assert!(!symbols.is_empty());
        let calls: Vec<(&str, &str)> = edges
            .iter()
            .filter(|e| e.kind == "calls")
            .map(|e| (e.from_name.as_str(), e.to_name.as_str()))
            .collect();
        assert!(
            calls.contains(&("process", "validate")),
            "expected process→validate, got: {calls:?}"
        );
        assert!(
            calls.contains(&("process", "transform")),
            "expected process→transform, got: {calls:?}"
        );
    }

    #[test]
    fn test_parse_python_inheritance() {
        let source = r#"
class Animal:
    def speak(self):
        pass

class Dog(Animal):
    def speak(self):
        bark()
"#;
        let (_symbols, _chunks, edges) = parse_file(source, "python", 50);
        let inherits: Vec<(&str, &str)> = edges
            .iter()
            .filter(|e| e.kind == "inherits")
            .map(|e| (e.from_name.as_str(), e.to_name.as_str()))
            .collect();
        assert!(
            inherits.contains(&("Dog", "Animal")),
            "expected Dog→Animal, got: {inherits:?}"
        );
    }

    #[test]
    fn test_parse_java_symbols_and_calls() {
        let source = r#"
public class PaymentService {
    public void processPayment(String id) {
        validateInput(id);
        chargeCard(id);
    }
    private boolean validateInput(String id) {
        return id != null;
    }
}
"#;
        let (symbols, _chunks, edges) = parse_file(source, "java", 50);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"PaymentService"), "missing class: {names:?}");
        assert!(names.contains(&"processPayment"), "missing method: {names:?}");
        assert!(names.contains(&"validateInput"), "missing method: {names:?}");

        let calls: Vec<(&str, &str)> = edges
            .iter()
            .filter(|e| e.kind == "calls")
            .map(|e| (e.from_name.as_str(), e.to_name.as_str()))
            .collect();
        assert!(
            calls.contains(&("processPayment", "validateInput")),
            "expected processPayment→validateInput, got: {calls:?}"
        );
        assert!(
            calls.contains(&("processPayment", "chargeCard")),
            "expected processPayment→chargeCard, got: {calls:?}"
        );
    }

    #[test]
    fn test_parse_java_inheritance() {
        let source = r#"
public class Dog extends Animal implements Runnable {
    public void run() {}
}
"#;
        let (_symbols, _chunks, edges) = parse_file(source, "java", 50);
        let structural: Vec<(&str, &str, &str)> = edges
            .iter()
            .filter(|e| e.kind != "calls")
            .map(|e| (e.from_name.as_str(), e.kind.as_str(), e.to_name.as_str()))
            .collect();
        assert!(
            structural.iter().any(|(f, k, t)| *f == "Dog" && *k == "inherits" && *t == "Animal"),
            "expected Dog inherits Animal: {structural:?}"
        );
        assert!(
            structural.iter().any(|(f, k, t)| *f == "Dog" && *k == "implements" && *t == "Runnable"),
            "expected Dog implements Runnable: {structural:?}"
        );
    }

    #[test]
    fn test_parse_cpp_symbols_and_calls() {
        let source = r#"
void process(int x) {
    validate(x);
    transform(x);
}

int validate(int x) {
    return x > 0;
}
"#;
        let (symbols, _chunks, edges) = parse_file(source, "cpp", 50);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"process"), "missing fn process: {names:?}");
        assert!(names.contains(&"validate"), "missing fn validate: {names:?}");

        let calls: Vec<(&str, &str)> = edges
            .iter()
            .filter(|e| e.kind == "calls")
            .map(|e| (e.from_name.as_str(), e.to_name.as_str()))
            .collect();
        assert!(
            calls.contains(&("process", "validate")),
            "expected process→validate, got: {calls:?}"
        );
    }

    #[test]
    fn test_parse_ruby_symbols_and_inheritance() {
        let source = r#"
class Dog < Animal
  def bark
    make_sound("woof")
  end
end

module Runnable
  def run
    move
  end
end
"#;
        let (symbols, _chunks, edges) = parse_file(source, "ruby", 50);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Dog"), "missing class Dog: {names:?}");
        assert!(names.contains(&"bark"), "missing method bark: {names:?}");
        assert!(names.contains(&"Runnable"), "missing module: {names:?}");

        let inherits: Vec<(&str, &str)> = edges
            .iter()
            .filter(|e| e.kind == "inherits")
            .map(|e| (e.from_name.as_str(), e.to_name.as_str()))
            .collect();
        assert!(
            inherits.contains(&("Dog", "Animal")),
            "expected Dog→Animal, got: {inherits:?}"
        );
    }

    #[test]
    fn test_parse_kotlin_symbols() {
        let source = r#"
class PaymentProcessor {
    fun processPayment(id: String): Boolean {
        return validate(id)
    }
    fun validate(id: String): Boolean {
        return id.isNotEmpty()
    }
}
"#;
        let (symbols, _chunks, _edges) = parse_file(source, "kotlin", 50);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"PaymentProcessor"), "missing class: {names:?}");
        assert!(names.contains(&"processPayment"), "missing fn: {names:?}");
        assert!(names.contains(&"validate"), "missing fn: {names:?}");
    }

    #[test]
    fn test_parse_bash_functions_and_calls() {
        let source = r#"
setup_database() {
    create_tables
    seed_data
}

create_tables() {
    echo "creating tables"
}
"#;
        let (symbols, _chunks, edges) = parse_file(source, "shell", 50);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"setup_database"), "missing fn: {names:?}");
        assert!(names.contains(&"create_tables"), "missing fn: {names:?}");

        let calls: Vec<(&str, &str)> = edges
            .iter()
            .filter(|e| e.kind == "calls")
            .map(|e| (e.from_name.as_str(), e.to_name.as_str()))
            .collect();
        assert!(
            calls.contains(&("setup_database", "create_tables")),
            "expected setup_database→create_tables, got: {calls:?}"
        );
        assert!(
            calls.contains(&("setup_database", "seed_data")),
            "expected setup_database→seed_data, got: {calls:?}"
        );
    }

    #[test]
    fn test_parse_sql_symbols() {
        let source = r#"
CREATE TABLE users (
    id SERIAL PRIMARY KEY,
    email TEXT NOT NULL
);

CREATE VIEW active_users AS
    SELECT * FROM users WHERE active = true;

CREATE FUNCTION get_user(user_id INT)
RETURNS TABLE(id INT, email TEXT) AS $$
    SELECT id, email FROM users WHERE id = user_id;
$$ LANGUAGE sql;
"#;
        let (symbols, _chunks, _edges) = parse_file(source, "sql", 50);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            !names.is_empty(),
            "should extract at least one SQL symbol, got none"
        );
    }

    #[test]
    fn test_parse_lua_symbols_and_calls() {
        let source = r#"
function process(data)
    local result = validate(data)
    return transform(result)
end

function validate(data)
    return data ~= nil
end
"#;
        let (symbols, _chunks, edges) = parse_file(source, "lua", 50);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"process"), "missing fn process: {names:?}");
        assert!(names.contains(&"validate"), "missing fn validate: {names:?}");

        let calls: Vec<(&str, &str)> = edges
            .iter()
            .filter(|e| e.kind == "calls")
            .map(|e| (e.from_name.as_str(), e.to_name.as_str()))
            .collect();
        assert!(
            calls.contains(&("process", "validate")),
            "expected process→validate, got: {calls:?}"
        );
        assert!(
            calls.contains(&("process", "transform")),
            "expected process→transform, got: {calls:?}"
        );
    }

    #[test]
    fn test_parse_python_functions() {
        let source = r#"
def greet(name: str) -> str:
    return f"Hello, {name}"

class MyClass:
    def method(self):
        pass
"#;
        let (symbols, _chunks, _edges) = parse_file(source, "python", 50);
        assert!(!symbols.is_empty());
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"greet"));
    }

    #[test]
    fn test_chunk_by_lines_fallback() {
        let source = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (_, chunks, _) = parse_file(&source, "unknown_lang", 50);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            let line_count = chunk.text.lines().count();
            assert!(line_count <= 55, "chunk too big: {line_count} lines");
        }
    }

    #[test]
    fn test_line_range_is_set() {
        let source = r#"
fn foo() {}

fn bar() {}
"#;
        let (symbols, _, _) = parse_file(source, "rust", 50);
        for sym in &symbols {
            assert!(sym.start_line > 0);
            assert!(sym.end_line >= sym.start_line);
        }
    }

    #[test]
    fn test_split_large_chunk_small_is_passthrough() {
        let chunk = CodeChunk {
            text: "fn small() { 42 }".to_string(),
            start_line: 1,
            end_line: 1,
        };
        let result = split_large_chunk(chunk, 800, 3);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].text, "fn small() { 42 }");
    }

    #[test]
    fn test_split_large_chunk_splits_oversized() {
        let source = (1..=50u32)
            .map(|i| format!("    let variable_{i} = {i} * {i};"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(source.len() > 800, "test source too short");

        let chunk = CodeChunk {
            text: source.clone(),
            start_line: 10,
            end_line: 59,
        };
        let result = split_large_chunk(chunk, 800, 3);

        assert!(
            result.len() > 1,
            "expected multiple chunks, got {}",
            result.len()
        );

        for c in &result {
            assert!(
                c.text.len() <= 850,
                "sub-chunk too large: {} chars",
                c.text.len()
            );
        }

        let all_text = result
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all_text.contains("variable_1"), "first line missing");
        assert!(all_text.contains("variable_50"), "last line missing");

        assert_eq!(result[0].start_line, 10);
        assert!(result.last().unwrap().end_line <= 59);
    }

    #[test]
    fn test_split_large_chunk_overlap_repeats_lines() {
        let source = (1..=30u32)
            .map(|i| format!("line-content-{i:02}-padding-here"))
            .collect::<Vec<_>>()
            .join("\n");

        let chunk = CodeChunk {
            text: source,
            start_line: 1,
            end_line: 30,
        };
        let chunks = split_large_chunk(chunk, 400, 3);

        if chunks.len() >= 2 {
            let last_lines_of_first: Vec<&str> = chunks[0].text.lines().rev().take(3).collect();
            let first_lines_of_second: Vec<&str> = chunks[1].text.lines().take(3).collect();
            let overlap_count = last_lines_of_first
                .iter()
                .filter(|l| first_lines_of_second.contains(l))
                .count();
            assert!(overlap_count > 0, "no overlap between consecutive chunks");
        }
    }

    #[test]
    fn test_repo_indexer_uses_correct_chunk_size() {
        let max_safe_chars = 1024usize;

        let max_fallback_chars = super::super::CHUNK_SIZE_LINES * 60;
        assert!(
            max_fallback_chars <= max_safe_chars,
            "CHUNK_SIZE_LINES ({}) produces chunks up to {max_fallback_chars} chars, \
             exceeding model limit of {max_safe_chars}",
            super::super::CHUNK_SIZE_LINES,
        );

        assert!(
            super::super::MAX_EMBED_CHARS <= max_safe_chars,
            "MAX_EMBED_CHARS ({}) exceeds model limit",
            super::super::MAX_EMBED_CHARS,
        );
    }

    #[test]
    fn test_split_large_chunk_no_infinite_loop_on_long_lines() {
        let long_line = "x".repeat(2000);
        let source = (0..10)
            .map(|_| long_line.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        let chunk = CodeChunk {
            text: source.clone(),
            start_line: 1,
            end_line: 10,
        };

        let chunks = split_large_chunk(chunk, 800, 3);
        assert!(!chunks.is_empty(), "should produce at least one chunk");
        assert!(
            chunks.len() <= 20,
            "should not loop indefinitely: {} chunks",
            chunks.len()
        );

        for window in chunks.windows(2) {
            assert!(
                window[1].start_line > window[0].start_line,
                "start_line must strictly increase: {} -> {}",
                window[0].start_line,
                window[1].start_line
            );
        }
    }
}
