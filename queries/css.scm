; Class selectors (.class-name)
(class_selector
  (class_name) @name) @definition.class

; ID selectors (#id-name)
(id_selector
  (id_name) @name) @definition.module

; CSS custom properties / design tokens (--var-name)
(
  (declaration
    (property_name) @name) @definition.macro
  (#match? @name "^--")
)

; Keyframe animation names
(keyframes_statement
  (keyframes_name) @name) @definition.function