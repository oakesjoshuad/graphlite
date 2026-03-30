use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Language {
    Rust,
    TypeScript,
    JavaScript,
    Svelte,
    Html,
    Css,
}

impl Language {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Language::Rust),
            "ts" | "tsx" | "jsx" => Some(Language::TypeScript),
            "js" | "mjs" | "cjs" => Some(Language::JavaScript),
            "svelte" => Some(Language::Svelte),
            "html" | "htm" => Some(Language::Html),
            "css" => Some(Language::Css),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::TypeScript => "typescript",
            Language::JavaScript => "javascript",
            Language::Svelte => "svelte",
            Language::Html => "html",
            Language::Css => "css",
        }
    }

    pub fn tree_sitter_language(&self) -> tree_sitter::Language {
        match self {
            Language::Rust => tree_sitter_rust::LANGUAGE.into(),
            Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Language::Svelte => tree_sitter_svelte_ng::LANGUAGE.into(),
            Language::Html => tree_sitter_html::LANGUAGE.into(),
            Language::Css => tree_sitter_css::LANGUAGE.into(),
        }
    }

    pub fn query_source(&self) -> &'static str {
        match self {
            Language::Rust => include_str!("../queries/rust.scm"),
            Language::TypeScript => include_str!("../queries/typescript.scm"),
            Language::JavaScript => include_str!("../queries/javascript.scm"),
            Language::Svelte => include_str!("../queries/svelte.scm"),
            Language::Html => include_str!("../queries/html.scm"),
            Language::Css => include_str!("../queries/css.scm"),
        }
    }
}

pub fn detect_language(path: &Path) -> Option<Language> {
    path.extension()
        .and_then(|e| e.to_str())
        .and_then(Language::from_extension)
}
