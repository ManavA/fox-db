; foxdb tags shared by JavaScript and TypeScript (adapted from tree-sitter-javascript tags.scm, doc captures dropped).
((method_definition name: (property_identifier) @name) @definition.method (#not-eq? @name "constructor"))
[(class name: (_) @name) (class_declaration name: (_) @name)] @definition.class
[(function_expression name: (identifier) @name) (function_declaration name: (identifier) @name)
 (generator_function name: (identifier) @name) (generator_function_declaration name: (identifier) @name)] @definition.function
(lexical_declaration (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)]) @definition.function)
(variable_declaration (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)]) @definition.function)
(assignment_expression left: [(identifier) @name (member_expression property: (property_identifier) @name)]
  right: [(arrow_function) (function_expression)]) @definition.function
(pair key: (property_identifier) @name value: [(arrow_function) (function_expression)]) @definition.function
((call_expression function: (identifier) @name) @reference.call (#not-match? @name "^(require)$"))
(call_expression function: (member_expression property: (property_identifier) @name) arguments: (_) @reference.call)
(new_expression constructor: (identifier) @name) @reference.class
(import_specifier name: (identifier) @name) @reference.import
; A function or method passed as an argument (`app.onError(onError)`, `on(this.handle)`): a reference.
; Parameters and locals are dropped.
(arguments [(identifier) @name (member_expression property: (property_identifier) @name)]) @reference.ref
