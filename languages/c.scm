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
; A function named in a positional initializer (`{ "add", cmd_add }`) or passed as an argument
; (`walk(handle_one, &cb)`): a reference. Parameters and locals are dropped at extraction.
(initializer_list (identifier) @name) @reference.ref
(argument_list (identifier) @name) @reference.ref
; A struct or union field, also behind pointers and arrays: found by name, never a call target.
(field_declaration declarator: [(field_identifier) @name
  (pointer_declarator declarator: [(field_identifier) @name (pointer_declarator declarator: (field_identifier) @name)])
  (array_declarator declarator: (field_identifier) @name)]) @definition.field
