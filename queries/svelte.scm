; Svelte 5 named snippet definitions
(snippet_statement
  (snippet_start
    (snippet_name) @name)) @definition.function

; Component references — PascalCase tag names are Svelte component imports
(
  (start_tag
    (tag_name) @name) @reference.class
  (#match? @name "^[A-Z]")
)

(
  (self_closing_tag
    (tag_name) @name) @reference.class
  (#match? @name "^[A-Z]")
)
