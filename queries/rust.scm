; Canonical tree-sitter-rust tags.scm

; ADT definitions
(struct_item
    name: (type_identifier) @name) @definition.class

(enum_item
    name: (type_identifier) @name) @definition.class

(union_item
    name: (type_identifier) @name) @definition.class

; type aliases
(type_item
    name: (type_identifier) @name) @definition.class

; method definitions — functions with a body inside impl or trait blocks
; canonical tags.scm pattern; dedup handles overlap with definition.function below
(declaration_list
    (function_item
        name: (identifier) @name) @definition.method)

; trait method signatures — required methods with no body (ends with ;)
; not in canonical tags.scm; critical for graphlite: rust-analyzer resolves
; generic dispatch (E: EventStore, A: Aggregate) to these declaration positions
(declaration_list
    (function_signature_item
        name: (identifier) @name) @definition.method)

; function definitions — free functions at module level
(function_item
    name: (identifier) @name) @definition.function

; trait definitions
(trait_item
    name: (type_identifier) @name) @definition.interface

; module definitions
(mod_item
    name: (identifier) @name) @definition.module

; macro definitions
(macro_definition
    name: (identifier) @name) @definition.macro

; references
(call_expression
    function: (identifier) @name) @reference.call

(call_expression
    function: (field_expression
        field: (field_identifier) @name)) @reference.call

(macro_invocation
    macro: (identifier) @name) @reference.call

; implementations
(impl_item
    trait: (type_identifier) @name) @reference.implementation

(impl_item
    type: (type_identifier) @name
    !trait) @reference.implementation

; graphlite extension: path calls not in canonical (Foo::bar(), Type::new())
(call_expression
    function: (scoped_identifier
        name: (identifier) @name)) @reference.call

; graphlite extension: generic (turbofish) calls — parse::<T>(), Vec::<T>::new(),
; self.method::<T>() — from highlights.scm; not in canonical tags.scm
(call_expression
    function: (generic_function
        function: (identifier) @name)) @reference.call

(call_expression
    function: (generic_function
        function: (scoped_identifier
            name: (identifier) @name))) @reference.call

(call_expression
    function: (generic_function
        function: (field_expression
            field: (field_identifier) @name))) @reference.call

; graphlite extension: impl blocks as searchable symbols
; always captures the implementing type (Bar in `impl Foo for Bar`, Foo in `impl Foo`)
(impl_item
    type: (type_identifier) @name) @definition.impl
