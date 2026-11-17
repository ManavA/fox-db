; foxdb tags for Swift (tree-sitter fallback when no Xcode index store covers a file).
(class_declaration name: (type_identifier) @name) @definition.class
(protocol_declaration name: (type_identifier) @name) @definition.interface
(function_declaration name: (simple_identifier) @name) @definition.function
(protocol_function_declaration name: (simple_identifier) @name) @definition.method
(init_declaration "init" @name) @definition.method
(property_declaration name: (pattern (simple_identifier) @name)) @definition.property
(call_expression . (simple_identifier) @name) @reference.call
(call_expression . (navigation_expression suffix: (navigation_suffix suffix: (simple_identifier) @name))) @reference.call
(import_declaration (identifier) @name) @reference.import
