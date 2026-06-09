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

/// Detect language from file extension.
pub fn detect_language(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()? {
        "rs" => Some("rust"),
        "py" => Some("python"),
        "ts" | "tsx" => Some("typescript"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "go" => Some("go"),
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
        _ => None,
    }
}

/// Parse a source file, extracting top-level symbols and code chunks.
/// Falls back to line-based chunking if no tree-sitter grammar is available.
pub fn parse_file(
    source: &str,
    language: &str,
    chunk_size_lines: usize,
) -> (Vec<ParsedSymbol>, Vec<CodeChunk>) {
    if let Some(ts_lang) = get_tree_sitter_language(language) {
        parse_with_tree_sitter(source, language, &ts_lang, chunk_size_lines)
    } else {
        (vec![], chunk_by_lines(source, chunk_size_lines))
    }
}

fn parse_with_tree_sitter(
    source: &str,
    language: &str,
    ts_lang: &Language,
    chunk_size_lines: usize,
) -> (Vec<ParsedSymbol>, Vec<CodeChunk>) {
    let mut parser = Parser::new();
    if parser.set_language(ts_lang).is_err() {
        return (vec![], chunk_by_lines(source, chunk_size_lines));
    }

    let tree = match parser.parse(source.as_bytes(), None) {
        Some(t) => t,
        None => return (vec![], chunk_by_lines(source, chunk_size_lines)),
    };

    let root = tree.root_node();
    let mut symbols = Vec::new();
    let mut chunks = Vec::new();

    collect_symbols(&root, source, language, &mut symbols, &mut chunks);

    // If no symbols found, fall back to line-based chunks
    if chunks.is_empty() {
        chunks = chunk_by_lines(source, chunk_size_lines);
    }

    (symbols, chunks)
}

fn collect_symbols(
    node: &Node,
    source: &str,
    language: &str,
    symbols: &mut Vec<ParsedSymbol>,
    chunks: &mut Vec<CodeChunk>,
) {
    let kind = node.kind();
    let is_symbol = match language {
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
        _ => false,
    };

    if is_symbol {
        let start = node.start_position();
        let end = node.end_position();
        let name = extract_name(node, source, language);
        let text = &source[node.start_byte()..node.end_byte()];
        let signature = extract_signature(text, language);

        symbols.push(ParsedSymbol {
            kind: kind.to_string(),
            name: name.unwrap_or_else(|| "<anonymous>".to_string()),
            signature,
            start_line: start.row as u32 + 1,
            end_line: end.row as u32 + 1,
        });

        // Use each top-level symbol as a chunk
        chunks.push(CodeChunk {
            text: text.to_string(),
            start_line: start.row as u32 + 1,
            end_line: end.row as u32 + 1,
        });
        return; // Don't recurse into the symbol's children for chunking
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_symbols(&child, source, language, symbols, chunks);
    }
}

fn extract_name(node: &Node, source: &str, language: &str) -> Option<String> {
    let name_kinds: &[&str] = match language {
        "rust" => &["identifier"],
        "python" => &["identifier"],
        "typescript" | "javascript" => &["identifier", "property_identifier"],
        "go" => &["identifier"],
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

fn extract_signature(text: &str, language: &str) -> Option<String> {
    let sig = match language {
        "rust" => {
            // Take everything up to the first `{` or the whole thing if no brace
            text.lines()
                .next()
                .map(|l| l.trim_end_matches('{').trim().to_string())
        }
        "python" | "go" | "typescript" | "javascript" => text.lines().next().map(|l| l.to_string()),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_language() {
        assert_eq!(detect_language(Path::new("main.rs")), Some("rust"));
        assert_eq!(detect_language(Path::new("app.py")), Some("python"));
        assert_eq!(detect_language(Path::new("index.ts")), Some("typescript"));
        assert_eq!(detect_language(Path::new("main.go")), Some("go"));
        assert_eq!(detect_language(Path::new("readme.md")), None);
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
        let (symbols, chunks) = parse_file(source, "rust", 50);
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
    fn test_parse_python_functions() {
        let source = r#"
def greet(name: str) -> str:
    return f"Hello, {name}"

class MyClass:
    def method(self):
        pass
"#;
        let (symbols, _chunks) = parse_file(source, "python", 50);
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
        let (_, chunks) = parse_file(&source, "unknown_lang", 50);
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
        let (symbols, _) = parse_file(source, "rust", 50);
        for sym in &symbols {
            assert!(sym.start_line > 0);
            assert!(sym.end_line >= sym.start_line);
        }
    }
}
