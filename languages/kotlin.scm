; foxdb tags for Kotlin (tree-sitter-kotlin-ng). Captures follow the tree-sitter tags.scm convention.
(class_declaration name: (identifier) @name) @definition.class
(object_declaration name: (identifier) @name) @definition.object
(companion_object) @definition.object
(function_declaration name: (identifier) @name) @definition.function
(property_declaration (variable_declaration (identifier) @name)) @definition.property
(type_alias type: (identifier) @name) @definition.type
(enum_entry (identifier) @name) @definition.constant
(call_expression . (identifier) @name) @reference.call
(call_expression . (navigation_expression (identifier) @name .)) @reference.call
(delegation_specifiers (delegation_specifier (constructor_invocation (user_type (identifier) @name)))) @reference.implementation
(delegation_specifiers (delegation_specifier (user_type (identifier) @name))) @reference.implementation
(import (qualified_identifier) @name) @reference.import
(class_parameter ["val" "var"] (identifier) @name) @definition.property
; A function reference (`::step`, `Type::method`): a reference to its last name.
(callable_reference (identifier) @name .) @reference.ref
