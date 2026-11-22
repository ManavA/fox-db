; foxdb tags for C++ (adapted from tree-sitter-cpp tags.scm; definitions span the body).
(struct_specifier name: (type_identifier) @name body: (_)) @definition.struct
(class_specifier name: (type_identifier) @name) @definition.class
(namespace_definition name: (namespace_identifier) @name) @definition.module
(function_definition declarator: (function_declarator declarator: (identifier) @name)) @definition.function
(function_definition declarator: (function_declarator declarator: (field_identifier) @name)) @definition.method
(function_definition declarator: (function_declarator declarator: (qualified_identifier scope: (namespace_identifier) @container name: (identifier) @name))) @definition.method
(field_declaration declarator: (function_declarator declarator: (field_identifier) @name)) @definition.method
(type_definition declarator: (type_identifier) @name) @definition.type
; A typedef of a function pointer or a function (`typedef int (*op_fn)(int);`, `typedef int op_t(int);`):
; a member of that type is a function-pointer member.
(type_definition declarator: [(function_declarator declarator: [(type_identifier) @name
    (parenthesized_declarator (pointer_declarator declarator: (type_identifier) @name))])
  (pointer_declarator declarator: (function_declarator declarator: (parenthesized_declarator (pointer_declarator declarator: (type_identifier) @name))))]) @definition.type
(enum_specifier name: (type_identifier) @name) @definition.enum
(call_expression function: (identifier) @name) @reference.call
(call_expression function: (field_expression field: (field_identifier) @name)) @reference.call
(call_expression function: (qualified_identifier name: (identifier) @name)) @reference.call
; A function named as a value in a designated initializer (`.open = foo_open`): a reference.
(initializer_pair value: (identifier) @name) @reference.ref
; A function named in a positional initializer (`{ "add", cmd_add }`) or passed as an argument
; (`walk(handle_one, &cb)`): a reference. Parameters and locals are dropped at extraction.
(initializer_list (identifier) @name) @reference.ref
(argument_list (identifier) @name) @reference.ref
; A data member, also behind pointers, references and arrays: found by name, never a call target.
(field_declaration declarator: [(field_identifier) @name
  (pointer_declarator declarator: [(field_identifier) @name (pointer_declarator declarator: (field_identifier) @name)])
  (reference_declarator (field_identifier) @name)
  (array_declarator declarator: (field_identifier) @name)
  ; A function-pointer member (`void (*irq_mask)(struct irq_data *)`, also returning a pointer):
  ; indexed as a method of its struct, the target of calls through it.
  (function_declarator declarator: (parenthesized_declarator (pointer_declarator declarator: (field_identifier) @name)))
  (pointer_declarator declarator: (function_declarator declarator: (parenthesized_declarator (pointer_declarator declarator: (field_identifier) @name))))]) @definition.field
