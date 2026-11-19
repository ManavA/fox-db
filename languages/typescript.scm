(function_signature
  name: (identifier) @name) @definition.function

(method_signature
  name: (property_identifier) @name) @definition.method

(abstract_method_signature
  name: (property_identifier) @name) @definition.method

(abstract_class_declaration
  name: (type_identifier) @name) @definition.class

(module
  name: (identifier) @name) @definition.module

(interface_declaration
  name: (type_identifier) @name) @definition.interface

(type_annotation
  (type_identifier) @name) @reference.type

; Named supertypes (`implements Router`, `extends Base`, `interface A extends B`): a reference from the type.
(implements_clause [(type_identifier) @name (generic_type name: (type_identifier) @name)]) @reference.implementation
(extends_clause value: [(identifier) @name (member_expression property: (property_identifier) @name)]) @reference.implementation
(extends_type_clause type: [(type_identifier) @name (generic_type name: (type_identifier) @name)]) @reference.implementation
; A class field: found by name, never a call target.
(public_field_definition name: (property_identifier) @name) @definition.field
