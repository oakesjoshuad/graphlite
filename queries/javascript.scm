; Symbol captures
(function_declaration name: (identifier) @name) @symbol
(class_declaration name: (identifier) @name) @symbol
(method_definition name: (property_identifier) @name) @symbol
(export_statement declaration: (function_declaration name: (identifier) @name)) @symbol
(lexical_declaration (variable_declarator name: (identifier) @name value: (arrow_function))) @symbol

; Call edges
(call_expression function: (identifier) @call.target)
(call_expression function: (member_expression property: (property_identifier) @call.target))
