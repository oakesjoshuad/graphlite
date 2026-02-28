; Symbol captures
(function_item name: (identifier) @name) @symbol
(struct_item name: (type_identifier) @name) @symbol
(enum_item name: (type_identifier) @name) @symbol
(trait_item name: (type_identifier) @name) @symbol
(type_item name: (type_identifier) @name) @symbol
(impl_item type: (type_identifier) @name) @symbol
(mod_item name: (identifier) @name) @symbol

; Call edges
(call_expression function: (identifier) @call.target)
(call_expression function: (scoped_identifier name: (identifier) @call.target))
(call_expression function: (field_expression field: (field_identifier) @call.target))
