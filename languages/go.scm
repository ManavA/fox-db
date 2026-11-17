; foxdb tags for Go (adapted from tree-sitter-go tags.scm).
(function_declaration name: (identifier) @name) @definition.function
(method_declaration receiver: (parameter_list (parameter_declaration type: [(type_identifier) @container (pointer_type (type_identifier) @container)])) name: (field_identifier) @name) @definition.method
(call_expression function: [(identifier) @name (parenthesized_expression (identifier) @name)
  (selector_expression field: (field_identifier) @name) (parenthesized_expression (selector_expression field: (field_identifier) @name))]) @reference.call
(type_spec name: (type_identifier) @name type: (interface_type)) @definition.interface
(type_spec name: (type_identifier) @name type: (struct_type)) @definition.struct
(type_spec name: (type_identifier) @name type: [(type_identifier) (qualified_type) (function_type) (map_type) (slice_type) (pointer_type) (generic_type)]) @definition.type
(method_elem name: (field_identifier) @name) @definition.method
(import_spec path: (interpreted_string_literal) @name) @reference.import
; A function value in a keyed composite literal (`RunE: runCmd`).
(keyed_element value: (literal_element (identifier) @name)) @reference.ref
