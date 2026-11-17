; foxdb tags for C (adapted from tree-sitter-c tags.scm; definitions span the body).
(struct_specifier name: (type_identifier) @name body: (_)) @definition.struct
(declaration type: (union_specifier name: (type_identifier) @name)) @definition.class
(function_definition declarator: [(function_declarator declarator: (identifier) @name)
  (pointer_declarator declarator: (function_declarator declarator: (identifier) @name))]) @definition.function
(declaration declarator: (function_declarator declarator: (identifier) @name)) @definition.function
(type_definition declarator: (type_identifier) @name) @definition.type
(enum_specifier name: (type_identifier) @name) @definition.enum
(call_expression function: (identifier) @name) @reference.call
(call_expression function: (field_expression field: (field_identifier) @name)) @reference.call
; A function named as a value in a designated initializer (`.open = foo_open`): a reference.
(initializer_pair value: (identifier) @name) @reference.ref
